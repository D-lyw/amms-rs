use super::{
    build_applied_log_key, AppliedLogDedupCache, HookRegistry, LogQueryChunk, LogSource,
    PendingLogDedupCache, PendingSyncQueue, StateSpace, StateSpaceError, StateSpaceManager,
};
use crate::state_space::{QueryMode, STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY};
use alloy::network::Network;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::{eth::Log, Filter};
use async_stream::stream;
use futures::{Stream, StreamExt};
use std::mem;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::sleep;
use tracing::{error, info, warn};

// Robinhood 实时同步（标准 `eth_subscribe("logs")`，走传入 provider 的 pubsub）。
//
// 与 BSC 的 logs-push（`bsc_logs_push.rs`，裸 WS + realtime_ws_endpoints）不同，
// 本管线直接复用调用方传入的 alloy Provider（`subscribe_logs`）：
// - 不需要 `StateSpaceBuilder::with_realtime_ws_endpoints`；
// - canonical newHeads 也走同一 provider（`ensure_background_tasks` 的 tracker）。
//
// 实测（生产 27 地址过滤集，chainstack/alchemy 一致）：Robinhood 的 logs 订阅
// 是**逐条日志推送**——每块约 30 条日志 = 约 30 次通知，但整块日志集中在
// p50≈4ms / p95≈14ms 内到达，且块间严格有序无交错。因此消费侧必须做
// **块级聚合 + settle 后原子应用**：
// - 逐条立即 apply 会让下游看到同块中间态（P1 已砸/P2 未砸）的幻影机会；
// - settle 10ms 覆盖 p95 推送窗口，代价远小于 100ms 块时间；
// - “收到更大块日志立即 flush 旧块”作为长尾加速与兜底（实测块间 0 交错，
//   该条件安全且不会漏块尾）。
//
// 断线/漏推兜底与 Base/BSC push 管线一致：外层每次重建前先
// `initial_backfill_results` getLogs 回补 + 全局位置去重；状态级由
// `run_silent_drift_probe_task` / maintenance coverage 对账。

/// 块级聚合 settle 窗口：当前块最后一条日志到达后等待该时长，若再无同块
/// 新日志则整块原子应用。10ms ≈ Robinhood 块内推送跨度 p95。
const LOGS_PUSH_SETTLE_MS: Duration = Duration::from_millis(10);
/// 连接静默兜底：超过该时长无任何日志推送视为断流，强制重建订阅（重建前
/// getLogs 回补 gap）。Robinhood 每块几乎都有监控日志，60s 无消息=异常。
const LOGS_PUSH_SILENT_REBUILD: Duration = STREAM_IDLE_TIMEOUT;

#[derive(Debug)]
enum FeedOutcome {
    /// 已并入当前缓冲块。
    Buffered,
    /// 收到更大块的日志：旧块整块原子应用。
    FlushBlock(u64, Vec<Log>),
    /// 迟到的旧块日志（长尾块 flush 后仍到达的尾巴）：单独小批量补应用。
    LatePatch(Log),
}

/// 块级日志聚合器（纯逻辑，便于单测）。
///
/// settle 计时基于 **tokio Instant 的绝对 deadline**（`last_feed_at + settle`），
/// 而非“每轮循环起点 + settle”：Robinhood 同块 30 条日志在 ~4ms 内到达
/// （事件间隔 <1ms），若用相对计时，settle 会被持续到达的事件不断重置、
/// 永不触发，整块 flush 只能等到下一块首条到达（≈+90ms，破坏实时性）。
#[derive(Default)]
struct BlockLogAccumulator {
    cur_block: Option<u64>,
    buf: Vec<Log>,
    last_feed_at: Option<tokio::time::Instant>,
}

impl BlockLogAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn feed(&mut self, block_num: u64, log: Log, now: tokio::time::Instant) -> FeedOutcome {
        self.last_feed_at = Some(now);
        match self.cur_block {
            None => {
                self.cur_block = Some(block_num);
                self.buf.push(log);
                FeedOutcome::Buffered
            }
            Some(cur) if block_num == cur => {
                self.buf.push(log);
                FeedOutcome::Buffered
            }
            Some(cur) if block_num > cur => {
                let flushed = (cur, mem::take(&mut self.buf));
                self.cur_block = Some(block_num);
                self.buf.push(log);
                FeedOutcome::FlushBlock(flushed.0, flushed.1)
            }
            Some(_) => FeedOutcome::LatePatch(log),
        }
    }

    /// settle 到期时取走当前块（若存在）。
    fn take(&mut self) -> Option<(u64, Vec<Log>)> {
        let block = self.cur_block.take()?;
        self.last_feed_at = None;
        Some((block, mem::take(&mut self.buf)))
    }

    /// 滑动 settle deadline：最后一条日志到达时刻 + settle；无缓冲块时 arm 到
    /// 远端（不空转）。
    fn settle_deadline(&self) -> tokio::time::Instant {
        self.last_feed_at
            .map(|t| t + LOGS_PUSH_SETTLE_MS)
            .unwrap_or_else(|| tokio::time::Instant::now() + FAR_FUTURE)
    }
}

/// 无缓冲块时 settle 的远端 deadline（避免空转 busy loop）。
const FAR_FUTURE: Duration = Duration::from_secs(3600);

fn subscription_filter_for_chunk(chunk: &LogQueryChunk) -> Filter {
    let mut filter = Filter::new().address(chunk.addresses.clone());
    if let QueryMode::TopicFiltered(topics) = &chunk.mode {
        if !topics.is_empty() {
            filter = filter.event_signature(topics.clone());
        }
    }
    filter
}

impl<N, P> StateSpaceManager<N, P> {
    /// 标准 `eth_subscribe("logs")` 实时同步（provider pubsub，块级聚合 settle）。
    ///
    /// - 订阅过滤集与 canonical getLogs 路径共用 `query_chunks`，覆盖一致；
    /// - 启动/重建前 `initial_backfill_results` 从 `realtime_head` 之后 getLogs 回补
    ///   （期间更新抑制，不产出下游通知）；
    /// - 消费侧块缓冲 + 10ms settle + “新块即 flush”，整块一次原子 apply、
    ///   每块至多一次下游通知；
    /// - canonical head 不由此路径推进，由 `ensure_background_tasks` 的
    ///   newHeads tracker 独立维护（与 Base/BSC push 链一致）。
    pub(super) fn subscribe_logs_push_stream(
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
            let mut pending_log_dedup = PendingLogDedupCache::default();

            loop {
                // 启动/重建 catch-up：`realtime_head` 之后已确认块 getLogs 补回，
                // 期间更新抑制（不产出下游通知）。
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
                    LogSource::LogsPush,
                    chain_id,
                )
                .await
                {
                    Ok(results) => {
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
                        warn!("Initial backfill failed before logs push subscribe: {}", e);
                    }
                }

                // 每 query chunk 一条 `eth_subscribe("logs")` 订阅（provider pubsub）。
                let mut subscriptions = Vec::new();
                let mut last_err = None;
                for chunk in &query_chunks {
                    let filter = subscription_filter_for_chunk(chunk);
                    match provider.subscribe_logs(&filter).await {
                        Ok(sub) => subscriptions.push(sub.into_stream()),
                        Err(e) => {
                            warn!(
                                addresses = chunk.addresses.len(),
                                "logs push subscribe failed: {}",
                                e
                            );
                            last_err = Some(format!("{e}"));
                            break;
                        }
                    }
                }

                // 任一 chunk 订阅失败即整体重建（避免静默缺 chunk 覆盖）；
                // 重建前 getLogs 回补 + 全局去重兜底。
                if last_err.is_some() || subscriptions.is_empty() {
                    error!(
                        chain_id,
                        error = last_err.unwrap_or_else(|| "<unknown>".to_string()),
                        "logs push failed to establish full subscription set; reconnecting"
                    );
                    sleep(STREAM_RECONNECT_DELAY).await;
                    continue;
                }

                info!(
                    chain_id,
                    subscriptions = subscriptions.len(),
                    "logs push subscriptions established (provider pubsub)"
                );

                let mut merged = futures::stream::select_all(subscriptions);
                let mut accum = BlockLogAccumulator::new();
                let mut last_activity = Instant::now();

                loop {
                    // settle：绝对 deadline = 当前块最后一条日志到达 + 10ms。
                    // 同块事件密集到达时 deadline 随之顺延，末条日志后 10ms
                    // 必然触发——不受事件到达频率影响。
                    let settle_fut = tokio::time::sleep_until(accum.settle_deadline());
                    tokio::pin!(settle_fut);
                    // 静默兜底：无任何推送超时 → 重建（重建前 getLogs 回补）。
                    let idle_fut = tokio::time::sleep(LOGS_PUSH_SILENT_REBUILD);
                    tokio::pin!(idle_fut);

                    tokio::select! {
                        maybe_log = merged.next() => {
                            let Some(log) = maybe_log else {
                                warn!(chain_id, "logs push subscription stream ended; reconnecting");
                                break;
                            };
                            last_activity = Instant::now();

                            // 标准 logs 订阅应返回已打包块日志（blockNumber 必填）；
                            // 防御性丢弃 pending/半成品与 reorg 回滚标记通知。
                            let Some(block_num) = log.block_number else {
                                continue;
                            };
                            if log.removed {
                                continue;
                            }

                            // 多个 chunk 订阅可能重叠（共享基础设施合约），
                            // 进入全局去重层前先本地预去重。
                            let prededup_key = build_applied_log_key(&log);
                            if !pending_log_dedup.insert_if_new(prededup_key) {
                                continue;
                            }

                            match accum.feed(block_num, log, tokio::time::Instant::now()) {
                                FeedOutcome::Buffered => {}
                                FeedOutcome::FlushBlock(block, logs) => {
                                    let affected = Self::apply_logs_for_block(
                                        &provider,
                                        &state,
                                        &hooks,
                                        block,
                                        logs,
                                        &realtime_head,
                                        &canonical_head,
                                        &pending_sync_queue,
                                        &pending_sync_notify,
                                        &applied_log_dedup,
                                        LogSource::LogsPush,
                                    )
                                    .await;
                                    match affected {
                                        Ok(affected) => {
                                            if !affected.is_empty() {
                                                let meta = super::build_realtime_update_meta(
                                                    &update_seq,
                                                    block,
                                                    Instant::now(),
                                                    None,
                                                );
                                                super::log_realtime_update_applied(meta, affected.len(), 1);
                                                yield Ok((meta, affected));
                                            }
                                        }
                                        Err(e) => {
                                            error!("logs push block {} apply failed: {}", block, e);
                                        }
                                    }
                                }
                                FeedOutcome::LatePatch(log) => {
                                    // 长尾块（超过 settle 才推完）flush 后仍到达的同块
                                    // 尾巴：单独补应用，不阻塞主路径。
                                    let affected = Self::apply_logs_for_block(
                                        &provider,
                                        &state,
                                        &hooks,
                                        block_num,
                                        vec![log],
                                        &realtime_head,
                                        &canonical_head,
                                        &pending_sync_queue,
                                        &pending_sync_notify,
                                        &applied_log_dedup,
                                        LogSource::LogsPush,
                                    )
                                    .await;
                                    match affected {
                                        Ok(affected) => {
                                            if !affected.is_empty() {
                                                let meta = super::build_realtime_update_meta(
                                                    &update_seq,
                                                    block_num,
                                                    Instant::now(),
                                                    None,
                                                );
                                                super::log_realtime_update_applied(meta, affected.len(), 1);
                                                yield Ok((meta, affected));
                                            }
                                        }
                                        Err(e) => {
                                            error!("logs push late patch block {} apply failed: {}", block_num, e);
                                        }
                                    }
                                }
                            }
                        }
                        _ = settle_fut => {
                            if let Some((block, logs)) = accum.take() {
                                let affected = Self::apply_logs_for_block(
                                    &provider,
                                    &state,
                                    &hooks,
                                    block,
                                    logs,
                                    &realtime_head,
                                    &canonical_head,
                                    &pending_sync_queue,
                                    &pending_sync_notify,
                                    &applied_log_dedup,
                                    LogSource::LogsPush,
                                )
                                .await;
                                match affected {
                                    Ok(affected) => {
                                        if !affected.is_empty() {
                                            let meta = super::build_realtime_update_meta(
                                                &update_seq,
                                                block,
                                                Instant::now(),
                                                None,
                                            );
                                            super::log_realtime_update_applied(meta, affected.len(), 1);
                                            yield Ok((meta, affected));
                                        }
                                    }
                                    Err(e) => {
                                        error!("logs push settle flush block {} apply failed: {}", block, e);
                                    }
                                }
                            }
                        }
                        _ = idle_fut => {
                            warn!(
                                chain_id,
                                silent_ms = LOGS_PUSH_SILENT_REBUILD.as_millis(),
                                "logs push stream silent; rebuilding subscriptions"
                            );
                            break;
                        }
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
    use alloy::primitives::FixedBytes;
    use serde_json::json;

    fn sample_log(block: u64, tx_index: u64, log_index: u64) -> Log {
        serde_json::from_value(json!({
            "address": "0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE",
            "topics": ["0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"],
            "data": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "blockNumber": format!("{block:#x}"),
            "transactionHash": format!("0x{:064x}", tx_index + 1),
            "transactionIndex": format!("{tx_index:#x}"),
            "blockHash": format!("0x{:064x}", block),
            "logIndex": format!("{log_index:#x}"),
            "removed": false
        }))
        .unwrap()
    }

    #[test]
    fn accumulator_same_block_buffers_and_new_block_flushes() {
        let mut acc = BlockLogAccumulator::new();
        let now = tokio::time::Instant::now();
        assert!(matches!(
            acc.feed(10, sample_log(10, 0, 0), now),
            FeedOutcome::Buffered
        ));
        assert!(matches!(
            acc.feed(10, sample_log(10, 1, 1), now),
            FeedOutcome::Buffered
        ));
        // 新块到达 → flush 整块（两条日志原子交给下游）
        match acc.feed(11, sample_log(11, 0, 0), now) {
            FeedOutcome::FlushBlock(block, logs) => {
                assert_eq!(block, 10);
                assert_eq!(logs.len(), 2);
            }
            other => panic!("expected FlushBlock, got {other:?}"),
        }
        // 当前块已切到 11
        assert_eq!(acc.take().map(|(b, _)| b), Some(11));
    }

    #[test]
    fn accumulator_settle_take_is_atomic() {
        let mut acc = BlockLogAccumulator::new();
        let now = tokio::time::Instant::now();
        acc.feed(10, sample_log(10, 0, 0), now);
        acc.feed(10, sample_log(10, 1, 1), now);
        let (block, logs) = acc.take().expect("buffered block should be takable");
        assert_eq!(block, 10);
        assert_eq!(logs.len(), 2);
        // 取走后为空：settle deadline 顺延（远端），且新 feed 从新块重新计时
        assert!(acc.take().is_none());
        let deadline = acc.settle_deadline();
        assert!(deadline - tokio::time::Instant::now() > FAR_FUTURE - Duration::from_secs(1));
    }

    #[test]
    fn accumulator_late_patch_routes_old_block_log() {
        let mut acc = BlockLogAccumulator::new();
        let now = tokio::time::Instant::now();
        acc.feed(10, sample_log(10, 0, 0), now);
        acc.feed(11, sample_log(11, 0, 0), now); // flush 10, cur=11
        assert!(matches!(
            acc.feed(10, sample_log(10, 5, 5), now),
            FeedOutcome::LatePatch(_)
        ));
    }

    #[test]
    fn settle_deadline_slides_with_last_feed() {
        let mut acc = BlockLogAccumulator::new();
        let t0 = tokio::time::Instant::now();
        acc.feed(10, sample_log(10, 0, 0), t0);
        // 事件密集（1ms 后同块第二条）：deadline 顺延到第二条 + 10ms，而非固定
        // “循环起点 + 10ms”（否则密集到达会让 settle 永不触发）。
        let t1 = t0 + Duration::from_millis(1);
        acc.feed(10, sample_log(10, 1, 1), t1);
        let deadline = acc.settle_deadline();
        let expect = t1 + LOGS_PUSH_SETTLE_MS;
        assert!(
            deadline >= expect,
            "deadline {deadline:?} should slide to {expect:?}"
        );
        assert!(deadline - expect < Duration::from_millis(5));
        // 新块开始后 deadline 以新块首批为准
        let t2 = t1 + Duration::from_millis(1);
        acc.feed(11, sample_log(11, 0, 0), t2);
        let deadline2 = acc.settle_deadline();
        let expect2 = t2 + LOGS_PUSH_SETTLE_MS;
        assert!(deadline2 >= expect2 && deadline2 - expect2 < Duration::from_millis(5));
    }

    #[test]
    fn subscription_filter_shape_matches_chunk() {
        let addr: Address = "0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE"
            .parse()
            .unwrap();
        let chunk = LogQueryChunk {
            addresses: vec![addr],
            mode: QueryMode::TopicFiltered(vec![FixedBytes::from([0xabu8; 32])]),
        };
        let f = subscription_filter_for_chunk(&chunk);
        // 订阅 filter 必须带 address + topics，且不得携带块范围
        let json = serde_json::to_value(&f).unwrap();
        assert!(json.get("address").is_some());
        assert!(json.get("topics").is_some());
        assert!(json.get("fromBlock").is_none());
        assert!(json.get("toBlock").is_none());
    }
}
