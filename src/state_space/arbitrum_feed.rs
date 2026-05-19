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
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

pub(crate) const ARBITRUM_FEED_WS_URL: &str = "wss://arb1-feed.arbitrum.io/feed";
pub(crate) const ARBITRUM_ONE_L2_OFFSET: u64 = 22_207_817;
pub(crate) const ARBITRUM_FEED_SAFETY_BLOCKS: u64 = 1;
pub(crate) const ARBITRUM_FEED_RETRY_BASE_MS: u64 = 50;
pub(crate) const ARBITRUM_FEED_RETRY_MAX_MS: u64 = 1_000;
const ARBITRUM_FEED_ALERT_RETRY_THRESHOLD: u32 = 8;
const ARBITRUM_FEED_ALERT_RETRY_EVERY: u32 = 20;
pub(crate) const ARBITRUM_FEED_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
struct UnreadableBlockRetryState {
    block: u64,
    retry_attempt: u32,
    next_retry_at: Instant,
}

impl UnreadableBlockRetryState {
    fn new(block: u64) -> Self {
        Self {
            block,
            retry_attempt: 0,
            next_retry_at: Instant::now() + unreadable_retry_delay(0),
        }
    }

    fn bump(self) -> Self {
        let attempt = self.retry_attempt.saturating_add(1);
        Self {
            block: self.block,
            retry_attempt: attempt,
            next_retry_at: Instant::now() + unreadable_retry_delay(attempt),
        }
    }
}

fn unreadable_retry_delay(retry_attempt: u32) -> Duration {
    let shift = retry_attempt.min(5);
    let multiplier = 1u64 << shift;
    let delay_ms = ARBITRUM_FEED_RETRY_BASE_MS
        .saturating_mul(multiplier)
        .min(ARBITRUM_FEED_RETRY_MAX_MS);
    Duration::from_millis(delay_ms)
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
        _query_chunks: &[LogQueryChunk],
        realtime_head: &Arc<AtomicU64>,
        canonical_head: &Arc<AtomicU64>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        applied_log_dedup: &Arc<Mutex<AppliedLogDedupCache>>,
        max_seq: u64,
        unreadable_block: &mut Option<UnreadableBlockRetryState>,
    ) -> Result<Vec<Vec<Address>>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut updates = Vec::new();
        let raw_feed_head = max_seq.saturating_add(ARBITRUM_ONE_L2_OFFSET);
        let candidate_l2_head = raw_feed_head.saturating_sub(ARBITRUM_FEED_SAFETY_BLOCKS);

        loop {
            let synced_head = realtime_head.load(Ordering::Relaxed);
            if candidate_l2_head <= synced_head {
                break;
            }

            let next_block_to_sync = synced_head.saturating_add(1);

            if let Some(state) = unreadable_block.as_ref() {
                if state.block != next_block_to_sync {
                    *unreadable_block = Some(UnreadableBlockRetryState::new(next_block_to_sync));
                } else if Instant::now() < state.next_retry_at {
                    break;
                }
            }

            let filter = Filter::new()
                .from_block(next_block_to_sync)
                .to_block(next_block_to_sync);
            let logs = match provider.get_logs(&filter).await {
                Ok(logs) => logs,
                Err(e) => {
                    let state_err = StateSpaceError::from(e);
                    if Self::is_temporarily_unreadable_block_error(&state_err) {
                        let next = match unreadable_block.take() {
                            Some(cur) if cur.block == next_block_to_sync => cur.bump(),
                            _ => UnreadableBlockRetryState::new(next_block_to_sync),
                        };
                        warn!(
                            unreadable_block = next.block,
                            retry_attempt = next.retry_attempt,
                            retry_delay_ms = unreadable_retry_delay(next.retry_attempt).as_millis(),
                            realtime_head = synced_head,
                            raw_feed_head,
                            candidate_l2_head,
                            safety_blocks = ARBITRUM_FEED_SAFETY_BLOCKS,
                            "Arbitrum feed block not readable yet; scheduling retry"
                        );
                        if next.retry_attempt >= ARBITRUM_FEED_ALERT_RETRY_THRESHOLD
                            && ((next.retry_attempt - ARBITRUM_FEED_ALERT_RETRY_THRESHOLD)
                                % ARBITRUM_FEED_ALERT_RETRY_EVERY
                                == 0)
                        {
                            error!(
                                alert = "arbitrum_feed_block_retry_stuck",
                                unreadable_block = next.block,
                                retry_attempt = next.retry_attempt,
                                retry_delay_ms = unreadable_retry_delay(next.retry_attempt).as_millis(),
                                realtime_head = synced_head,
                                raw_feed_head,
                                candidate_l2_head,
                                safety_blocks = ARBITRUM_FEED_SAFETY_BLOCKS,
                                "ALERT: Arbitrum feed block repeatedly unreadable; fast-retry still failing"
                            );
                        }
                        *unreadable_block = Some(next);
                        break;
                    }
                    return Err(state_err);
                }
            };

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
                applied_log_dedup,
                LogSource::ArbitrumFeedPull,
            )
            .await?;

            if !affected.is_empty() {
                updates.push(affected);
            }
        }

        Ok(updates)
    }

    pub(super) fn subscribe_arbitrum_feed_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<Address>>,
        realtime_head: Arc<AtomicU64>,
        canonical_head: Arc<AtomicU64>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
        query_chunks: Vec<LogQueryChunk>,
        chain_id: u64,
    ) -> impl Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        stream! {
            let mut seq_buf = Vec::with_capacity(64);
            let mut last_seen_seq: Option<u64> = None;
            let mut seq_duplicate_count = 0u64;
            let mut seq_non_monotonic_count = 0u64;
            let mut max_seq = realtime_head
                .load(Ordering::Relaxed)
                .saturating_add(ARBITRUM_FEED_SAFETY_BLOCKS)
                .saturating_sub(ARBITRUM_ONE_L2_OFFSET);
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
                        for affected in results {
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
                        warn!("Initial backfill failed before Arbitrum feed subscribe: {}", e);
                    }
                }

                let connect = connect_async(ARBITRUM_FEED_WS_URL).await;
                let (mut socket, _) = match connect {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Arbitrum feed ws connect failed: {}", e);
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };
                info!(
                    ws_url = ARBITRUM_FEED_WS_URL,
                    "Arbitrum feed connected"
                );

                let mut last_feed_activity = Instant::now();

                loop {
                    match Self::drive_arbitrum_feed_progress(
                        &provider,
                        &state,
                        &hooks,
                        &query_chunks,
                        &realtime_head,
                        &canonical_head,
                        &pending_sync_queue,
                        &applied_log_dedup,
                        max_seq,
                        &mut unreadable_block,
                    )
                    .await
                    {
                        Ok(results) => {
                            for affected in results {
                                if !affected.is_empty() {
                                    yield Ok(affected);
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
                        let raw_feed_head = max_seq.saturating_add(ARBITRUM_ONE_L2_OFFSET);
                        let candidate = raw_feed_head.saturating_sub(ARBITRUM_FEED_SAFETY_BLOCKS);
                        info!(
                            max_seq,
                            raw_feed_head,
                            candidate_l2_head = candidate,
                            safety_blocks = ARBITRUM_FEED_SAFETY_BLOCKS,
                            realtime_head = realtime,
                            head_lag = candidate.saturating_sub(realtime),
                            seq_duplicate_count,
                            seq_non_monotonic_count,
                            "Arbitrum feed realtime heartbeat"
                        );
                        last_metrics_log = Instant::now();
                    }

                    if last_feed_activity.elapsed() > STREAM_IDLE_TIMEOUT {
                        warn!("Arbitrum feed stream timeout, reconnecting");
                        break;
                    }

                    let next = tokio::time::timeout(ARBITRUM_FEED_POLL_INTERVAL, socket.next()).await;
                    let maybe_message_result = match next {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let Some(message_result) = maybe_message_result else {
                        warn!("Arbitrum feed stream ended");
                        break;
                    };

                    let message = match message_result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Arbitrum feed stream receive error: {}", e);
                            break;
                        }
                    };
                    last_feed_activity = Instant::now();

                    let payload = match message {
                        Message::Text(text) => text.to_string(),
                        Message::Binary(bin) => String::from_utf8_lossy(bin.as_ref()).to_string(),
                        Message::Ping(v) => {
                            let _ = socket.send(Message::Pong(v)).await;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Close(_) => break,
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
    fn unreadable_retry_backoff_caps_at_1000ms() {
        assert_eq!(unreadable_retry_delay(0), Duration::from_millis(50));
        assert_eq!(unreadable_retry_delay(1), Duration::from_millis(100));
        assert_eq!(unreadable_retry_delay(2), Duration::from_millis(200));
        assert_eq!(unreadable_retry_delay(3), Duration::from_millis(400));
        assert_eq!(unreadable_retry_delay(4), Duration::from_millis(800));
        assert_eq!(unreadable_retry_delay(5), Duration::from_millis(1000));
        assert_eq!(unreadable_retry_delay(8), Duration::from_millis(1000));
    }

    #[test]
    fn unreadable_state_keeps_same_block_and_increments_attempt() {
        let first = UnreadableBlockRetryState::new(1234);
        assert_eq!(first.block, 1234);
        assert_eq!(first.retry_attempt, 0);
        assert!(first.next_retry_at > Instant::now());

        let second = first.bump();
        assert_eq!(second.block, 1234);
        assert_eq!(second.retry_attempt, 1);

        let third = second.bump();
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
}
