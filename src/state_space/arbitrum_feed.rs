use super::{
    AppliedLogDedupCache, HookRegistry, LogQueryChunk, LogSource, PendingSyncQueue, StateSpace,
    StateSpaceError, StateSpaceManager, STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY,
};
use alloy::network::Network;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use async_stream::stream;
use futures::{SinkExt, Stream, StreamExt};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant as StdInstant;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

pub(crate) const ARBITRUM_FEED_WS_URL: &str = "wss://arb1-feed.arbitrum.io/feed";
pub(crate) const ROBINHOOD_FEED_WS_URL: &str = "wss://feed.mainnet.chain.robinhood.com";
pub(crate) const ARBITRUM_ONE_L2_OFFSET: u64 = 22_207_817;
pub(crate) const ROBINHOOD_L2_OFFSET: u64 = 0;
pub(crate) const ARBITRUM_FEED_SAFETY_BLOCKS: u64 = 1;
pub(crate) const ARBITRUM_FEED_RETRY_BASE_MS: u64 = 50;
pub(crate) const ARBITRUM_FEED_RETRY_MAX_MS: u64 = 1_000;
const ARBITRUM_FEED_ALERT_RETRY_THRESHOLD: u32 = 8;
const ARBITRUM_FEED_ALERT_RETRY_EVERY: u32 = 20;
pub(crate) const ARBITRUM_FEED_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 落后阈值：realtime_head 落后 candidate（feed 前沿）超过该块数时，
/// drive 改用批量窗口（backfill_range 区间 get_logs）追平，而不是逐块
/// 单次 get_logs —— 单次 RPC 固定成本 ~100ms+ 已超过 Robinhood ~100ms
/// 出块间隔，积压/回放时逐块追赶永远追不上。
pub(crate) const ARBITRUM_FEED_CATCHUP_BATCH_THRESHOLD: u64 = 8;
/// 消费积压帧：回放/重连时 feed 帧到达可能远快于单帧处理节奏，
/// 用短轮询把 socket 已就绪帧一次排空，使 max_seq 贴近真实前沿。
const FEED_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);
const MAX_FEED_DRAIN_FRAMES_PER_ROUND: u64 = 16_384;

pub(crate) fn feed_ws_url(chain_id: u64) -> &'static str {
    match chain_id {
        4663 => ROBINHOOD_FEED_WS_URL,
        _ => ARBITRUM_FEED_WS_URL,
    }
}

fn feed_l2_offset(chain_id: u64) -> u64 {
    match chain_id {
        4663 => ROBINHOOD_L2_OFFSET,
        _ => ARBITRUM_ONE_L2_OFFSET,
    }
}

fn feed_chain_label(chain_id: u64) -> &'static str {
    match chain_id {
        4663 => "robinhood",
        _ => "arbitrum",
    }
}

#[derive(Clone, Debug)]
struct UnreadableBlockRetryState {
    block: u64,
    retry_attempt: u32,
    next_retry_at: Instant,
}

/// 不可读块重试策略。按链配置：各链出块节奏不同，禁止跨链混用同一套重试参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeedRetryPolicy {
    /// 固定间隔重试：失败后恒定间隔再试，不倍增、不空转。
    /// 用于 ~100ms 出块的链（如 Robinhood 4663），tip 块落地后下一次重试即命中。
    Fixed { interval_ms: u64 },
    /// 指数退避：base 起倍增，封顶 max。Arbitrum One（42161）等历史默认行为。
    ExponentialBackoff { base_ms: u64, max_ms: u64 },
}

impl FeedRetryPolicy {
    fn delay(self, retry_attempt: u32) -> Duration {
        match self {
            Self::Fixed { interval_ms } => Duration::from_millis(interval_ms),
            Self::ExponentialBackoff { base_ms, max_ms } => {
                let shift = retry_attempt.min(5);
                let delay_ms = base_ms.saturating_mul(1u64 << shift).min(max_ms);
                Duration::from_millis(delay_ms)
            }
        }
    }
}

fn feed_safety_blocks(chain_id: u64) -> u64 {
    match chain_id {
        // Robinhood：直接盯 feed tip（safety=0），不可读由固定 50ms 重试兜底。
        4663 => 0,
        // Arbitrum One 及未登记链保持历史行为（safety buffer = 1）。
        _ => ARBITRUM_FEED_SAFETY_BLOCKS,
    }
}

fn feed_retry_policy(chain_id: u64) -> FeedRetryPolicy {
    match chain_id {
        // Robinhood ~100ms/块：tip 不可读时固定 50ms 重试。
        4663 => FeedRetryPolicy::Fixed { interval_ms: 50 },
        // Arbitrum One（~250ms/块）沿用历史指数退避 50ms..=1000ms。
        _ => FeedRetryPolicy::ExponentialBackoff {
            base_ms: ARBITRUM_FEED_RETRY_BASE_MS,
            max_ms: ARBITRUM_FEED_RETRY_MAX_MS,
        },
    }
}

impl UnreadableBlockRetryState {
    fn new(block: u64, policy: FeedRetryPolicy) -> Self {
        Self {
            block,
            retry_attempt: 0,
            next_retry_at: Instant::now() + policy.delay(0),
        }
    }

    fn bump(self, policy: FeedRetryPolicy) -> Self {
        let attempt = self.retry_attempt.saturating_add(1);
        Self {
            block: self.block,
            retry_attempt: attempt,
            next_retry_at: Instant::now() + policy.delay(attempt),
        }
    }
}

fn parse_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).ok()
            } else {
                s.parse::<u64>().ok()
            }
        }
        _ => None,
    }
}

fn extract_sequences(payload: &Value, out: &mut Vec<u64>) {
    let Some(messages) = payload.get("messages").and_then(|v| v.as_array()) else {
        return;
    };
    for msg in messages {
        if let Some(seq) = msg.get("sequenceNumber").and_then(parse_u64) {
            out.push(seq);
        }
    }
}

fn update_seq_counters(
    seq: u64,
    last_seen_seq: &mut Option<u64>,
    max_seq: &mut u64,
    seq_duplicate_count: &mut u64,
    seq_non_monotonic_count: &mut u64,
) {
    if let Some(prev_seen) = *last_seen_seq {
        if seq < prev_seen {
            *seq_non_monotonic_count = seq_non_monotonic_count.saturating_add(1);
        }
    }

    if seq <= *max_seq {
        *seq_duplicate_count = seq_duplicate_count.saturating_add(1);
    } else {
        *max_seq = seq;
    }

    *last_seen_seq = Some(seq);
}

impl<N, P> StateSpaceManager<N, P> {
    fn is_temporarily_unreadable_block_error(err: &StateSpaceError) -> bool {
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("block not found")
            || msg.contains("header not found")
            || msg.contains("requested to block")
            || msg.contains("invalid block range")
            // Some RPC backends surface transient getLogs failures as -32603 Internal error.
            || msg.contains("error code -32603")
            || msg.contains("internal error")
    }

    async fn drive_arbitrum_feed_progress(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        query_chunks: &[LogQueryChunk],
        update_seq: &Arc<AtomicU64>,
        realtime_head: &Arc<AtomicU64>,
        canonical_head: &Arc<AtomicU64>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Arc<Notify>,
        applied_log_dedup: &Arc<Mutex<AppliedLogDedupCache>>,
        max_seq: u64,
        chain_id: u64,
        unreadable_block: &mut Option<UnreadableBlockRetryState>,
    ) -> Result<Vec<(super::RealtimeUpdateMeta, Vec<Address>)>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut updates = Vec::new();
        let safety_blocks = feed_safety_blocks(chain_id);
        let retry_policy = feed_retry_policy(chain_id);
        let raw_feed_head = max_seq.saturating_add(feed_l2_offset(chain_id));
        let candidate_l2_head = raw_feed_head.saturating_sub(safety_blocks);

        // 监控地址并集（chunk 间地址可重叠：共享 manager/vault/plugin 合约）。
        // 单块逐块路径据此做服务端 eth_getLogs 地址过滤（见下方 Filter），
        // 与 backfill/NewHeadsPull 的"服务端过滤"语义对齐；不设 topic 以
        // 兼容 AddressOnly chunk（FoT token 等非 sync 事件），本地 apply
        // 按原逻辑路由/容忍无关事件（与整块全量路径处理等价）。
        let mut watched_addresses: Vec<Address> = Vec::new();
        {
            let mut seen = std::collections::HashSet::new();
            for chunk in query_chunks {
                for addr in &chunk.addresses {
                    if seen.insert(*addr) {
                        watched_addresses.push(*addr);
                    }
                }
            }
        }

        loop {
            let synced_head = realtime_head.load(Ordering::Relaxed);
            if candidate_l2_head <= synced_head {
                break;
            }

            // 落后较多：批量窗口追赶。逐块单次 get_logs 的固定 RPC 成本
            // （~100ms+）已超过 ~100ms 出块间隔，积压/回放时永远追不上；
            // 区间 get_logs（backfill_range）一次请求覆盖多块，摊销成本后
            // 每块 <1ms，可快速贴回 feed 前沿。追赶阶段只应用状态、不向下游
            // 发出可交易信号（与 initial backfill 语义一致），避免把回放旧块
            // 当成实时机会。
            if candidate_l2_head - synced_head > ARBITRUM_FEED_CATCHUP_BATCH_THRESHOLD {
                *unreadable_block = None;
                info!(
                    feed = feed_chain_label(chain_id),
                    from_block = synced_head.saturating_add(1),
                    to_block = candidate_l2_head,
                    blocks = candidate_l2_head - synced_head,
                    "Feed head lag exceeds threshold; batch catch-up via ranged get_logs"
                );
                Self::backfill_range(
                    provider,
                    state,
                    hooks,
                    query_chunks,
                    synced_head.saturating_add(1),
                    candidate_l2_head,
                    realtime_head,
                    canonical_head,
                    pending_sync_queue,
                    pending_sync_notify,
                    applied_log_dedup,
                    LogSource::ArbitrumFeedPull,
                    chain_id,
                )
                .await?;
                continue;
            }

            let next_block_to_sync = synced_head.saturating_add(1);

            if let Some(state) = unreadable_block.as_ref() {
                if state.block != next_block_to_sync {
                    *unreadable_block = Some(UnreadableBlockRetryState::new(
                        next_block_to_sync,
                        retry_policy,
                    ));
                } else if Instant::now() < state.next_retry_at {
                    break;
                }
            }

            let mut filter = Filter::new()
                .from_block(next_block_to_sync)
                .to_block(next_block_to_sync);
            if !watched_addresses.is_empty() {
                filter = filter.address(watched_addresses.clone());
            }
            let received_at = StdInstant::now();
            let logs = match provider.get_logs(&filter).await {
                Ok(logs) => logs,
                Err(e) => {
                    let state_err = StateSpaceError::from(e);
                    if Self::is_temporarily_unreadable_block_error(&state_err) {
                        let next = match unreadable_block.take() {
                            Some(cur) if cur.block == next_block_to_sync => cur.bump(retry_policy),
                            _ => UnreadableBlockRetryState::new(next_block_to_sync, retry_policy),
                        };
                        warn!(
                            feed = feed_chain_label(chain_id),
                            unreadable_block = next.block,
                            retry_attempt = next.retry_attempt,
                            retry_delay_ms = retry_policy.delay(next.retry_attempt).as_millis(),
                            realtime_head = synced_head,
                            raw_feed_head,
                            candidate_l2_head,
                            safety_blocks,
                            "Feed block not readable yet; scheduling retry"
                        );
                        if next.retry_attempt >= ARBITRUM_FEED_ALERT_RETRY_THRESHOLD
                            && ((next.retry_attempt - ARBITRUM_FEED_ALERT_RETRY_THRESHOLD)
                                % ARBITRUM_FEED_ALERT_RETRY_EVERY
                                == 0)
                        {
                            error!(
                                alert = "arbitrum_feed_block_retry_stuck",
                                feed = feed_chain_label(chain_id),
                                unreadable_block = next.block,
                                retry_attempt = next.retry_attempt,
                                retry_delay_ms = retry_policy.delay(next.retry_attempt).as_millis(),
                                realtime_head = synced_head,
                                raw_feed_head,
                                candidate_l2_head,
                                safety_blocks,
                                "ALERT: feed block repeatedly unreadable; fast-retry still failing"
                            );
                        }
                        *unreadable_block = Some(next);
                        break;
                    }
                    return Err(state_err);
                }
            };
            let log_count = logs.len();

            *unreadable_block = None;

            let affected = Self::apply_logs_for_block(
                provider,
                state,
                hooks,
                next_block_to_sync,
                logs,
                realtime_head,
                canonical_head,
                pending_sync_queue,
                pending_sync_notify,
                applied_log_dedup,
                LogSource::ArbitrumFeedPull,
            )
            .await?;

            if !affected.is_empty() {
                let meta = super::build_realtime_update_meta(
                    update_seq,
                    next_block_to_sync,
                    received_at,
                    None,
                );
                super::log_realtime_update_applied(meta, affected.len(), log_count);
                updates.push((meta, affected));
            }
        }

        Ok(updates)
    }

    pub(super) fn subscribe_arbitrum_feed_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<Address>>,
        update_seq: Arc<AtomicU64>,
        realtime_head: Arc<AtomicU64>,
        canonical_head: Arc<AtomicU64>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: Arc<Notify>,
        applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
        query_chunks: Vec<LogQueryChunk>,
        chain_id: u64,
    ) -> impl Stream<Item = Result<(super::RealtimeUpdateMeta, Vec<Address>), StateSpaceError>> + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        stream! {
            let mut seq_buf = Vec::with_capacity(64);
            let mut last_seen_seq: Option<u64> = None;
            let mut seq_duplicate_count = 0u64;
            let mut seq_non_monotonic_count = 0u64;
            let feed_offset = feed_l2_offset(chain_id);
            let feed_url = feed_ws_url(chain_id);
            let feed_label = feed_chain_label(chain_id);
            let feed_safety_blocks = feed_safety_blocks(chain_id);
            let mut max_seq = realtime_head
                .load(Ordering::Relaxed)
                .saturating_add(feed_safety_blocks)
                .saturating_sub(feed_offset);
            let mut last_metrics_log = Instant::now();
            let mut unreadable_block: Option<UnreadableBlockRetryState> = None;

            loop {
                match Self::initial_backfill_results(
                    &provider,
                    &state,
                    &hooks,
                    &query_chunks,
                    &realtime_head,
                    &canonical_head,
                    &pending_sync_queue,
                    &pending_sync_notify,
                    &applied_log_dedup,
                    LogSource::ArbitrumFeedPull,
                    chain_id,
                )
                .await
                {
                    Ok(results) => {
                        // Catch-up stage: apply state updates only; do not emit tradable updates downstream.
                        let mut non_empty_batches = 0usize;
                        let mut affected_pools = 0usize;
                        for (_, affected) in results {
                            if !affected.is_empty() {
                                non_empty_batches += 1;
                                affected_pools += affected.len();
                            }
                        }
                        if non_empty_batches > 0 {
                            info!(
                                non_empty_batches,
                                affected_pools,
                                "Initial catch-up completed (updates suppressed during catch-up stage)"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(feed = feed_label, "Initial backfill failed before feed subscribe: {}", e);
                    }
                }

                let connect = connect_async(feed_url).await;
                let (mut socket, _) = match connect {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(feed = feed_label, "Feed ws connect failed: {}", e);
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };
                info!(
                    feed = feed_label,
                    ws_url = feed_url,
                    "Feed connected"
                );

                let mut last_feed_activity = Instant::now();

                loop {
                    match Self::drive_arbitrum_feed_progress(
                        &provider,
                        &state,
                        &hooks,
                        &query_chunks,
                        &update_seq,
                        &realtime_head,
                        &canonical_head,
                        &pending_sync_queue,
                        &pending_sync_notify,
                        &applied_log_dedup,
                        max_seq,
                        chain_id,
                        &mut unreadable_block,
                    )
                    .await
                    {
                        Ok(results) => {
                            for (meta, affected) in results {
                                if !affected.is_empty() {
                                    yield Ok((meta, affected));
                                }
                            }
                        }
                        Err(e) => {
                            error!("Arbitrum feed progress apply failed: {}", e);
                            break;
                        }
                    }

                    if last_metrics_log.elapsed() >= Duration::from_secs(5) {
                        let realtime = realtime_head.load(Ordering::Relaxed);
                        let raw_feed_head = max_seq.saturating_add(feed_offset);
                        let candidate = raw_feed_head.saturating_sub(feed_safety_blocks);
                        info!(
                            feed = feed_label,
                            max_seq,
                            raw_feed_head,
                            candidate_l2_head = candidate,
                            safety_blocks = feed_safety_blocks,
                            realtime_head = realtime,
                            head_lag = candidate.saturating_sub(realtime),
                            seq_duplicate_count,
                            seq_non_monotonic_count,
                            "Feed realtime heartbeat"
                        );
                        last_metrics_log = Instant::now();
                    }

                    if last_feed_activity.elapsed() > STREAM_IDLE_TIMEOUT {
                        warn!(feed = feed_label, "Feed stream timeout, reconnecting");
                        break;
                    }

                    // 一次性排空 socket 已就绪的积压帧（回放/断线追赶时帧到达
                    // 可能远快于单帧处理节奏）。逐帧消费会让 max_seq 只随已消费帧
                    // 缓慢增长、无法反映真实前沿，批量追赶也无从谈起。
                    let mut feed_ended = false;
                    let mut drained_frames = 0u64;
                    let mut first_wait = ARBITRUM_FEED_POLL_INTERVAL;
                    loop {
                        let next = tokio::time::timeout(first_wait, socket.next()).await;
                        first_wait = FEED_DRAIN_POLL_INTERVAL;
                        let maybe_message_result = match next {
                            Ok(v) => v,
                            Err(_) => break,
                        };

                        let Some(message_result) = maybe_message_result else {
                            warn!(feed = feed_label, "Feed stream ended");
                            feed_ended = true;
                            break;
                        };

                        let message = match message_result {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(feed = feed_label, "Feed stream receive error: {}", e);
                                feed_ended = true;
                                break;
                            }
                        };
                        last_feed_activity = Instant::now();

                        let payload = match message {
                            Message::Text(text) => text.to_string(),
                            Message::Binary(bin) => {
                                String::from_utf8_lossy(bin.as_ref()).to_string()
                            }
                            Message::Ping(v) => {
                                let _ = socket.send(Message::Pong(v)).await;
                                continue;
                            }
                            Message::Pong(_) => continue,
                            Message::Close(_) => {
                                feed_ended = true;
                                break;
                            }
                            Message::Frame(_) => continue,
                        };

                        let value: Value = match serde_json::from_str(&payload) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        seq_buf.clear();
                        extract_sequences(&value, &mut seq_buf);
                        for seq in &seq_buf {
                            update_seq_counters(
                                *seq,
                                &mut last_seen_seq,
                                &mut max_seq,
                                &mut seq_duplicate_count,
                                &mut seq_non_monotonic_count,
                            );
                        }

                        drained_frames += 1;
                        if drained_frames >= MAX_FEED_DRAIN_FRAMES_PER_ROUND {
                            break;
                        }
                    }

                    if feed_ended {
                        break;
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_tracker_is_monotonic_with_out_of_order_inputs() {
        let mut last_seen = None;
        let mut max_seq = 10u64;
        let mut dup = 0u64;
        let mut non_mono = 0u64;

        for seq in [11u64, 11, 13, 12, 14, 14, 13] {
            update_seq_counters(seq, &mut last_seen, &mut max_seq, &mut dup, &mut non_mono);
        }

        assert_eq!(max_seq, 14);
        assert_eq!(dup, 4);
        assert_eq!(non_mono, 2);
        let raw_feed_head = max_seq.saturating_add(ARBITRUM_ONE_L2_OFFSET);
        let candidate_l2_head = raw_feed_head.saturating_sub(ARBITRUM_FEED_SAFETY_BLOCKS);
        assert_eq!(raw_feed_head, 14 + ARBITRUM_ONE_L2_OFFSET);
        assert_eq!(
            candidate_l2_head,
            14 + ARBITRUM_ONE_L2_OFFSET - ARBITRUM_FEED_SAFETY_BLOCKS
        );
    }

    #[test]
    fn arbitrum_default_retry_backoff_caps_at_1000ms() {
        // 42161 及未登记链保持历史指数退避 50ms..=1000ms。
        let policy = feed_retry_policy(42161);
        assert_eq!(policy.delay(0), Duration::from_millis(50));
        assert_eq!(policy.delay(1), Duration::from_millis(100));
        assert_eq!(policy.delay(2), Duration::from_millis(200));
        assert_eq!(policy.delay(3), Duration::from_millis(400));
        assert_eq!(policy.delay(4), Duration::from_millis(800));
        assert_eq!(policy.delay(5), Duration::from_millis(1000));
        assert_eq!(policy.delay(8), Duration::from_millis(1000));
        assert_eq!(feed_safety_blocks(42161), ARBITRUM_FEED_SAFETY_BLOCKS);
    }

    #[test]
    fn robinhood_retry_is_fixed_50ms_with_no_safety_buffer() {
        // 4663：safety=0 直盯 tip，不可读时固定 50ms 重试（不倍增）。
        let policy = feed_retry_policy(4663);
        assert_eq!(policy, FeedRetryPolicy::Fixed { interval_ms: 50 });
        assert_eq!(policy.delay(0), Duration::from_millis(50));
        assert_eq!(policy.delay(5), Duration::from_millis(50));
        assert_eq!(policy.delay(100), Duration::from_millis(50));
        assert_eq!(feed_safety_blocks(4663), 0);
    }

    #[test]
    fn unreadable_state_keeps_same_block_and_increments_attempt() {
        let policy = feed_retry_policy(42161);
        let first = UnreadableBlockRetryState::new(1234, policy);
        assert_eq!(first.block, 1234);
        assert_eq!(first.retry_attempt, 0);
        assert!(first.next_retry_at > Instant::now());

        let second = first.bump(policy);
        assert_eq!(second.block, 1234);
        assert_eq!(second.retry_attempt, 1);

        let third = second.bump(policy);
        assert_eq!(third.block, 1234);
        assert_eq!(third.retry_attempt, 2);
        assert!(third.next_retry_at > Instant::now());
    }

    #[test]
    fn invalid_block_range_is_treated_as_temporarily_unreadable() {
        let err = StateSpaceError::AMMError(crate::amms::error::AMMError::Msg(
            "server returned an error response: error code -32000: invalid block range params"
                .to_string(),
        ));
        assert!(
            StateSpaceManager::<(), ()>::is_temporarily_unreadable_block_error(&err),
            "invalid block range params should trigger fast retry path"
        );
    }

    #[test]
    fn rpc_internal_error_is_treated_as_temporarily_unreadable() {
        let err = StateSpaceError::AMMError(crate::amms::error::AMMError::Msg(
            "server returned an error response: error code -32603: Internal error".to_string(),
        ));
        assert!(
            StateSpaceManager::<(), ()>::is_temporarily_unreadable_block_error(&err),
            "rpc internal errors should trigger fast retry path"
        );
    }

    #[test]
    fn robinhood_feed_parameters() {
        assert_eq!(feed_ws_url(4663), ROBINHOOD_FEED_WS_URL);
        assert_eq!(feed_l2_offset(4663), 0);
        assert_eq!(feed_chain_label(4663), "robinhood");
        assert_eq!(feed_safety_blocks(4663), 0);
        assert_eq!(
            feed_retry_policy(4663),
            FeedRetryPolicy::Fixed { interval_ms: 50 }
        );
    }

    #[test]
    fn robinhood_offset_zero_raw_feed_head_equals_max_seq() {
        // Robinhood sequenceNumber == real L2 block number, offset = 0, safety_blocks = 0.
        // verify that raw_feed_head == max_seq and candidate == max_seq.
        let max_seq = 100u64;
        let offset = ROBINHOOD_L2_OFFSET;
        let safety = feed_safety_blocks(4663);

        let raw_feed_head = max_seq.saturating_add(offset);
        let candidate_l2_head = raw_feed_head.saturating_sub(safety);

        assert_eq!(raw_feed_head, 100);
        assert_eq!(candidate_l2_head, 100);
    }

    #[test]
    fn robinhood_max_seq_initialization_skips_already_synced() {
        // Simulate subscribe_arbitrum_feed_stream's max_seq initialization
        // for Robinhood (offset=0, safety_blocks=0).
        // realtime_head already at block 50 → max_seq = 50 + 0 - 0 = 50.
        // Then raw_feed_head = 50 + 0 = 50, candidate = 50.
        // candidate <= realtime_head → drive_arbitrum_feed_progress should skip.
        let realtime_head = 50u64;
        let offset = ROBINHOOD_L2_OFFSET;
        let safety = feed_safety_blocks(4663);

        let max_seq = realtime_head.saturating_add(safety).saturating_sub(offset);
        let raw_feed_head = max_seq.saturating_add(offset);
        let candidate_l2_head = raw_feed_head.saturating_sub(safety);

        assert_eq!(max_seq, 50);
        assert_eq!(raw_feed_head, 50);
        assert_eq!(candidate_l2_head, 50);
        assert!(candidate_l2_head <= realtime_head);
    }
}
