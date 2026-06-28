mod arbitrum_feed;
mod base_pending_logs;
pub mod discovery;
pub mod error;
pub mod filters;
// Retained intentionally for historical reference and probe/debug reuse.
// Base mainnet no longer uses this raw flashblocks parser in the default
// realtime path because the official upstream payload shape changed.
// Current Base realtime syncing is implemented via `pendingLogs`; see
// `base_pending_logs.rs` and `RealtimeSyncSource::BaseFlashblocksRaw` below.
#[allow(dead_code)]
mod flashblocks;
pub mod hooks;
mod maintenance;
pub mod sync_services;
mod ws_logs;
mod xlayer_flashblocks;

use crate::amms::amm::AutomatedMarketMaker;
use crate::amms::amm::{SyncAction, AMM};
use crate::amms::error::AMMError;
use crate::amms::factory::Factory;
use crate::amms::fluid_dex::get_liquidity_layer;
use crate::amms::{
    aerodrome_slipstream::ICustomFeeModule, algebra_integral::IDynamicFeeManager, balancer_v2,
    balancer_v3, ekubo,
};
use crate::state_space::hooks::HookHandle;
use crate::state_space::hooks::HookRegistry;
use crate::state_space::hooks::SnapshotConfig;
use crate::state_space::hooks::StateHook;

use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::network::Network;

use alloy::primitives::{keccak256, Address, Bloom, BloomInput, FixedBytes, B256};
use alloy::providers::Provider;
use alloy::rpc::types::{eth::Log, Filter, FilterSet};
use alloy::sol;
use alloy::sol_types::SolEvent;

use error::StateSpaceError;
use filters::AMMFilter;
use filters::PoolFilter;
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use maintenance::{PendingSyncAction, PendingSyncQueue, PendingSyncReason};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::{future::Future, marker::PhantomData, sync::Arc};
use tokio::sync::{Mutex, Notify, RwLock};

use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

#[derive(Clone, Debug, Default)]
pub enum RealtimeSyncSource {
    #[default]
    Auto,
    // Legacy compatibility knob: non-Base realtime path now follows
    // newHeads + per-block get_logs pull mode.
    WsLogs,
    /// Legacy config name kept for backward compatibility.
    ///
    /// Historically this selected the old Base raw flashblocks parser.
    /// After the upstream Base Flashblocks raw payload format changed,
    /// Base realtime syncing was migrated to `pendingLogs` subscriptions.
    ///
    /// The enum variant name is preserved so downstream users do not need
    /// to immediately change configuration, but it now resolves to the
    /// Base `pendingLogs` implementation internally.
    BaseFlashblocksRaw,
    /// Xlayer flashblocks 流式同步（OP Stack 乐观架构）。
    ///
    /// 与 Base flashblocks 的区别:
    /// - 消息格式不同（完整区块头 vs 精简结构）
    /// - 纯 JSON 传输（无需 Brotli）
    /// - receipt 无 transactionIndex，用累计计数器推导
    XlayerFlashblocksRaw,
    ArbitrumSequencerFeed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolRefreshAction {
    AsyncUpdate,
    Resync,
}

#[derive(Clone, Copy, Debug)]
pub struct RealtimeUpdateMeta {
    pub seq: u64,
    pub block_number: u64,
    pub received_at: Instant,
    pub flashblock_index: Option<u64>,
}

#[derive(Clone)]
pub struct StateSpaceManager<N, P> {
    pub state: Arc<RwLock<StateSpace>>,
    pub block_filter: Filter,
    pub provider: P,
    realtime_ws_endpoints: Option<Vec<String>>,
    pub realtime_head: Arc<AtomicU64>,
    pub canonical_head: Arc<AtomicU64>,
    update_seq: Arc<AtomicU64>,
    pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
    pending_sync_notify: Arc<Notify>,
    applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
    background_started: Arc<std::sync::atomic::AtomicBool>,
    pending_sync_worker_interval: Duration,
    drift_probe_interval: Duration,
    maintenance_interval: Option<Duration>,
    realtime_source: RealtimeSyncSource,
    hooks: HookRegistry<Vec<Address>>,
    phantom: PhantomData<N>,
}

const LOG_ADDRESS_CHUNK_SIZE: usize = 200;
const BASE_CHAIN_ID: u64 = 8453;
const ARBITRUM_CHAIN_ID: u64 = 42161;
const ETHEREUM_MAINNET_CHAIN_ID: u64 = 1;
const XLAYER_CHAIN_ID: u64 = 196;
/// Xlayer Flashblocks WebSocket 端点。
///
/// 端点说明（由用户调研确认）:
/// - wss://ws.xlayer.tech/flashblocks    新加坡 (AWS CloudFront)，默认值
/// - wss://xlayerws.okx.com/flashblocks  加拿大 (Cloudflare CDN)
///
/// 两个端点服务 Local 不完全一致，建议根据部署地理位置就近选择。
/// 可通过环境变量 XLAYER_FLASHBLOCKS_WS 覆盖默认值。
///
/// 测试网:
/// - wss://testws.xlayer.tech/flashblocks
/// - wss://xlayertestws.okx.com/flashblocks
const XLAYER_FLASHBLOCKS_RAW_WS_URL: &str = "wss://ws.xlayer.tech/flashblocks";
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const APPLIED_LOG_DEDUP_CAPACITY: usize = 300_000;

/// XLayer flashblock content_hash 去重集合的 LRU 容量。
///
/// 每个 block 匹配 ~5 条 log，1,000 覆盖 ~200 blocks ≈ 3.3 分钟，
/// 远大于实际重复投递的延迟（实测 ~1 秒）。
const CONTENT_HASH_DEDUP_CAPACITY: usize = 1_000;
const DEFAULT_PENDING_SYNC_WORKER_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_DRIFT_PROBE_INTERVAL: Duration = Duration::from_secs(120);

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogSource {
    RealtimeFlashblock,
    XlayerFlashblock,
    ArbitrumFeedPull,
    NewHeadsPull,
    Maintenance,
}

fn next_realtime_update_seq(update_seq: &Arc<AtomicU64>) -> u64 {
    update_seq.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

fn build_realtime_update_meta(
    update_seq: &Arc<AtomicU64>,
    block_number: u64,
    received_at: Instant,
    flashblock_index: Option<u64>,
) -> RealtimeUpdateMeta {
    RealtimeUpdateMeta {
        seq: next_realtime_update_seq(update_seq),
        block_number,
        received_at,
        flashblock_index,
    }
}

fn log_realtime_update_applied(meta: RealtimeUpdateMeta, affected_pools: usize, log_count: usize) {
    info!(
        seq = meta.seq,
        block = meta.block_number,
        affected_pools,
        log_count,
        ms_recv_to_apply = meta.received_at.elapsed().as_millis(),
        "realtime_update_applied"
    );
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ApplyLogsTiming {
    pub sort_ms: u128,
    pub dedup_ms: u128,
    pub sync_ms: u128,
    pub hooks_ms: u128,
    pub total_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum AppliedLogPosition {
    // Stable block-global log index used by canonical get_logs and providers
    // whose realtime subscriptions expose canonical logIndex semantics.
    LogIndex(u64),
    // Stable event fingerprint used across canonical backfill and realtime
    // flashblock/pendingLogs sources.
    ContentHash(B256),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct AppliedLogKey {
    tx_hash: Option<alloy::primitives::B256>,
    block_number: Option<u64>,
    position: AppliedLogPosition,
    address: Address,
    topic0: Option<FixedBytes<32>>,
}

pub(super) fn build_log_content_hash(log: &Log) -> B256 {
    let mut bytes = Vec::with_capacity(32 * (log.topics().len() + 2) + log.data().data.len());
    bytes.extend_from_slice(log.address().as_slice());

    match log.transaction_hash {
        Some(tx_hash) => {
            bytes.push(1);
            bytes.extend_from_slice(tx_hash.as_slice());
        }
        None => bytes.push(0),
    }

    bytes.extend_from_slice(&log.block_number.unwrap_or_default().to_be_bytes());
    bytes.extend_from_slice(&(log.topics().len() as u64).to_be_bytes());
    for topic in log.topics() {
        bytes.extend_from_slice(topic.as_slice());
    }
    bytes.extend_from_slice(log.data().data.as_ref());
    keccak256(bytes)
}

pub(super) fn build_applied_log_key(log: &Log) -> AppliedLogKey {
    let position = if let Some(log_index) = log.log_index {
        AppliedLogPosition::LogIndex(log_index)
    } else {
        AppliedLogPosition::ContentHash(build_log_content_hash(log))
    };

    AppliedLogKey {
        tx_hash: log.transaction_hash,
        block_number: log.block_number,
        position,
        address: log.address(),
        topic0: log.topics().first().copied(),
    }
}

fn build_applied_log_content_key(log: &Log) -> AppliedLogKey {
    AppliedLogKey {
        tx_hash: log.transaction_hash,
        block_number: log.block_number,
        position: AppliedLogPosition::ContentHash(build_log_content_hash(log)),
        address: log.address(),
        topic0: log.topics().first().copied(),
    }
}

#[derive(Default)]
struct AppliedLogDedupCache {
    seen: HashSet<AppliedLogKey>,
    order: VecDeque<AppliedLogKey>,
    cross_source_content_seen: HashSet<AppliedLogKey>,
    cross_source_content_order: VecDeque<AppliedLogKey>,
    /// XLayer flashblock 专用：基于日志内容哈希 (keccak256) 的重叠计数。
    ///
    /// ## 为什么需要这个计数表
    ///
    /// XLayer flashblock logs 使用本地合成的 receipt-local `log_index`，
    /// canonical `get_logs` 使用 block-global `log_index`。同一条链上事件
    /// 如果先从 XLayer raw stream 处理，稍后又从 canonical overlap/backfill
    /// 处理，稳定位置 key 无法直接命中。
    ///
    /// 这个计数表使用 `build_log_content_hash()` 计算的哈希值做 key。
    /// 该哈希包含了：`address + tx_hash + block_number + topics[] + data`，
    /// 唯一标识一个事件，**不依赖 log_index**。
    /// 用计数而不是集合，是为了避免误杀同一交易内 payload 完全相同的
    /// 多条合法日志。
    ///
    /// 与 `cross_source_content_seen` 的区别：
    /// - `cross_source_content_seen` 用于 canonical -> flashblock 的跨源去重
    /// - `content_hash_seen` 用于 flashblock -> canonical 的跨源去重
    content_hash_seen: HashMap<B256, usize>,
    content_hash_order: VecDeque<B256>,
}

impl AppliedLogDedupCache {
    fn remember(&mut self, key: AppliedLogKey) {
        if self.seen.insert(key.clone()) {
            self.order.push_back(key);
        }
        while self.order.len() > APPLIED_LOG_DEDUP_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }

    fn remember_cross_source_content(&mut self, key: AppliedLogKey) {
        if self.cross_source_content_seen.insert(key.clone()) {
            self.cross_source_content_order.push_back(key);
        }
        while self.cross_source_content_order.len() > APPLIED_LOG_DEDUP_CAPACITY {
            if let Some(old) = self.cross_source_content_order.pop_front() {
                self.cross_source_content_seen.remove(&old);
            }
        }
    }

    /// 记录 content_hash 到 XLayer 专属重叠计数表。
    /// 容量 1,000（~3.3 分钟窗口），远大于实际重复投递的秒级延迟。
    fn remember_content_hash(&mut self, hash: B256) {
        *self.content_hash_seen.entry(hash).or_insert(0) += 1;
        self.content_hash_order.push_back(hash);
        while self.content_hash_order.len() > CONTENT_HASH_DEDUP_CAPACITY {
            if let Some(old) = self.content_hash_order.pop_front() {
                Self::decrement_content_hash_count(&mut self.content_hash_seen, &old);
            }
        }
    }

    fn consume_content_hash(&mut self, hash: &B256) -> bool {
        if !Self::decrement_content_hash_count(&mut self.content_hash_seen, hash) {
            return false;
        }
        if let Some(pos) = self
            .content_hash_order
            .iter()
            .position(|queued| queued == hash)
        {
            self.content_hash_order.remove(pos);
        }
        true
    }

    fn decrement_content_hash_count(counts: &mut HashMap<B256, usize>, hash: &B256) -> bool {
        let Some(count) = counts.get_mut(hash) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            counts.remove(hash);
        }
        true
    }

    fn insert_log_if_new(&mut self, log: &Log, source: LogSource) -> bool {
        let primary = build_applied_log_key(log);
        let content = build_applied_log_content_key(log);

        if matches!(source, LogSource::XlayerFlashblock) {
            // XLayer flashblock logs synthesize receipt-local log_index values,
            // while canonical get_logs returns block-global log_index values.
            // Use only non-XLayer content aliases to suppress overlap with
            // initial backfill; keep XLayer's receipt-local primary key so two
            // legitimate identical-payload logs in one tx are not collapsed.
            if self.cross_source_content_seen.contains(&content) {
                return false;
            }
            if self.seen.contains(&primary) {
                return false;
            }
            let content_hash = build_log_content_hash(log);
            self.remember(primary);
            self.remember_content_hash(content_hash);
            return true;
        }

        // For canonical/provider logs, prefer the stable positional key so two
        // legitimate same-payload events in one transaction are not collapsed.
        if self.seen.contains(&primary) {
            return false;
        }
        // XLayer flashblock logs use synthesized receipt-local log_index values.
        // A later canonical/getLogs overlap for the same event will carry the
        // block-global log_index, so the positional key above cannot match it.
        if self.consume_content_hash(&build_log_content_hash(log)) {
            self.remember(primary);
            return false;
        }
        self.remember(primary);
        // Also record the content alias so a later XLayer overlap can match it.
        self.remember_cross_source_content(content);
        true
    }
}

sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    interface ICLFactoryReader {
        function swapFeeModule() external view returns (address);
    }
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    interface IPancakeV3StateProbe {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    interface IV3StateProbe {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    interface ISlipstreamStateProbe {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
    }
}

#[derive(Clone, Debug)]
enum QueryMode {
    TopicFiltered(Vec<FixedBytes<32>>),
    AddressOnly,
}

#[derive(Clone, Debug)]
struct LogQueryChunk {
    addresses: Vec<Address>,
    mode: QueryMode,
}

impl LogQueryChunk {
    fn ranged_filter(&self, from_block: u64, to_block: u64) -> Filter {
        let mut filter = Filter::new()
            .address(self.addresses.clone())
            .from_block(from_block)
            .to_block(to_block);

        if let QueryMode::TopicFiltered(topics) = &self.mode {
            if !topics.is_empty() {
                filter = filter.event_signature(topics.clone());
            }
        }

        filter
    }
}

#[derive(Clone, Copy, Debug)]
enum SelectedRealtimeSource {
    NewHeadsPull,
    BasePendingLogs,
    XlayerFlashblocksPull,
    ArbitrumFeedPull,
}

impl<N, P> StateSpaceManager<N, P> {
    /// Request internal pending-sync refreshes for specific pools.
    ///
    /// This only enqueues tasks into the internal pending queue; execution still
    /// follows canonical-head gating, in-flight dedup, retry/backoff, and workers.
    pub async fn request_pool_refreshes<I>(&self, addresses: I, action: PoolRefreshAction) -> usize
    where
        I: IntoIterator<Item = Address>,
        P: Provider<N> + Clone,
        N: Network,
    {
        let required_block = self.canonical_head.load(Ordering::Relaxed);
        let (pending_action, reason) = match action {
            PoolRefreshAction::AsyncUpdate => (
                PendingSyncAction::AsyncUpdate,
                PendingSyncReason::AsyncUpdate,
            ),
            PoolRefreshAction::Resync => (PendingSyncAction::Resync, PendingSyncReason::Resync),
        };

        let unique: HashSet<Address> = addresses.into_iter().collect();
        if unique.is_empty() {
            return 0;
        }

        {
            let mut queue = self.pending_sync_queue.lock().await;
            for address in &unique {
                queue.enqueue(*address, pending_action, required_block, reason);
            }
        }
        self.pending_sync_notify.notify_one();

        unique.len()
    }

    /// Registers a hook to be called on every state change.
    pub async fn register_hook(&self, hook: StateHook<Vec<Address>>) -> HookHandle<Vec<Address>> {
        self.hooks.register(hook).await
    }

    async fn ensure_background_tasks(
        &self,
        _query_chunks: Vec<LogQueryChunk>,
        chain_id: u64,
        selected: SelectedRealtimeSource,
    ) -> Result<(), StateSpaceError>
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        if self
            .background_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }

        if (chain_id == BASE_CHAIN_ID
            && matches!(selected, SelectedRealtimeSource::BasePendingLogs))
            || (chain_id == XLAYER_CHAIN_ID
                && matches!(selected, SelectedRealtimeSource::XlayerFlashblocksPull))
        {
            let provider = self.provider.clone();
            let state = self.state.clone();
            let notify = self.pending_sync_notify.clone();
            let canonical_head = self.canonical_head.clone();
            tokio::spawn(async move {
                Self::run_canonical_head_tracker(provider, state, notify, canonical_head).await;
            });
        }

        let provider = self.provider.clone();
        let state = self.state.clone();
        let queue = self.pending_sync_queue.clone();
        let notify = self.pending_sync_notify.clone();
        let canonical_head = self.canonical_head.clone();
        let pending_interval = self.pending_sync_worker_interval;
        tokio::spawn(async move {
            Self::run_pending_sync_worker(
                provider,
                state,
                queue,
                notify,
                canonical_head,
                pending_interval,
            )
            .await;
        });

        let provider = self.provider.clone();
        let state = self.state.clone();
        let queue = self.pending_sync_queue.clone();
        let canonical_head = self.canonical_head.clone();
        let drift_interval = self.drift_probe_interval.max(Duration::from_secs(1));
        tokio::spawn(async move {
            Self::run_silent_drift_probe_task(
                provider,
                state,
                queue,
                canonical_head,
                drift_interval,
            )
            .await;
        });

        if let Some(interval) = self.maintenance_interval {
            let provider = self.provider.clone();
            let state = self.state.clone();
            let queue = self.pending_sync_queue.clone();
            let canonical_head = self.canonical_head.clone();
            tokio::spawn(async move {
                Self::run_maintenance_coverage_scheduler(
                    provider,
                    state,
                    queue,
                    canonical_head,
                    interval,
                )
                .await;
            });
        }

        Ok(())
    }

    /// Subscribes to AMM state changes through a configurable realtime source:
    /// - Base: `pendingLogs` on a Flashblocks-aware WebSocket endpoint by default.
    /// - Other chains: newHeads + logsBloom prefilter + per-block get_logs.
    ///
    /// For Base, the `pendingLogs` path deliberately reuses `build_query_chunks()`,
    /// so all special AMM address/event coverage stays aligned with the canonical
    /// log-pull path instead of introducing a second subscription rule set.
    ///
    /// Note: Base `pendingLogs` also requires explicit realtime WebSocket
    /// endpoints via `StateSpaceBuilder::with_realtime_ws_endpoints(...)`.
    /// These endpoints must support Base flashblock-related subscription
    /// methods, specifically `eth_subscribe` with `pendingLogs`.
    pub async fn subscribe(
        &self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
        StateSpaceError,
    >
    where
        P: Provider<N> + Clone + 'static,
        N: Network,
    {
        let stream = self.subscribe_with_meta().await?;
        Ok(Box::pin(
            stream.map(|item| item.map(|(_, addresses)| addresses)),
        ))
    }

    /// Subscribes to AMM state changes and includes lightweight realtime metadata
    /// for latency attribution in downstream consumers.
    pub async fn subscribe_with_meta(
        &self,
    ) -> Result<
        Pin<
            Box<
                dyn Stream<Item = Result<(RealtimeUpdateMeta, Vec<Address>), StateSpaceError>>
                    + Send,
            >,
        >,
        StateSpaceError,
    >
    where
        P: Provider<N> + Clone + 'static,
        N: Network,
    {
        let provider = self.provider.clone();
        let realtime_head = self.realtime_head.clone();
        let canonical_head = self.canonical_head.clone();
        let state = self.state.clone();
        let hooks = self.hooks.clone();
        let update_seq = self.update_seq.clone();
        let pending_sync_queue = self.pending_sync_queue.clone();
        let pending_sync_notify = self.pending_sync_notify.clone();
        let applied_log_dedup = self.applied_log_dedup.clone();
        let realtime_source = self.realtime_source.clone();

        let chain_id = { state.read().await.chain_id };
        let query_chunks = Self::build_query_chunks(&provider, &state, chain_id).await?;
        let selected = Self::resolve_realtime_source(chain_id, &realtime_source);
        let base_ws_candidates = if matches!(selected, SelectedRealtimeSource::BasePendingLogs) {
            Some(self.realtime_ws_endpoints.clone().ok_or_else(|| {
                StateSpaceError::from(AMMError::Msg(
                    "Base pendingLogs realtime source requires explicit websocket endpoints that support Base flashblock-related subscription methods (specifically `eth_subscribe` with `pendingLogs`). Use StateSpaceBuilder::with_realtime_ws_endpoints(vec![\"wss://...\".into(), ...]).".to_string(),
                ))
            })?)
        } else {
            None
        };
        self.ensure_background_tasks(query_chunks.clone(), chain_id, selected)
            .await?;

        match selected {
            SelectedRealtimeSource::NewHeadsPull => {
                info!(
                    "Starting newHeads + logsBloom + getLogs sync (chain_id={}, {} query chunks)",
                    chain_id,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_new_heads_stream(
                    provider,
                    state,
                    hooks,
                    update_seq,
                    realtime_head,
                    canonical_head,
                    pending_sync_queue,
                    pending_sync_notify,
                    applied_log_dedup,
                    query_chunks,
                    chain_id,
                )))
            }
            SelectedRealtimeSource::BasePendingLogs => {
                let ws_candidates = base_ws_candidates
                    .expect("BasePendingLogs selected must prevalidate ws endpoints");
                info!(
                    "Starting Base pendingLogs sync (chain_id={}, {} query chunks)",
                    chain_id,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_base_pending_logs_stream(
                    provider,
                    state,
                    hooks,
                    update_seq,
                    realtime_head,
                    canonical_head,
                    pending_sync_queue,
                    pending_sync_notify,
                    applied_log_dedup,
                    query_chunks,
                    ws_candidates,
                    chain_id,
                )))
            }
            SelectedRealtimeSource::XlayerFlashblocksPull => {
                info!(
                    "Starting Xlayer flashblocks sync (chain_id={}, ws_url={}, {} query chunks)",
                    chain_id,
                    XLAYER_FLASHBLOCKS_RAW_WS_URL,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_xlayer_flashblocks_stream(
                    provider,
                    state,
                    hooks,
                    update_seq,
                    realtime_head,
                    canonical_head,
                    pending_sync_queue,
                    pending_sync_notify,
                    applied_log_dedup,
                    query_chunks,
                    chain_id,
                )))
            }
            SelectedRealtimeSource::ArbitrumFeedPull => {
                info!(
                    "Starting Arbitrum feed + getLogs sync (chain_id={}, ws_url={}, {} query chunks)",
                    chain_id,
                    arbitrum_feed::ARBITRUM_FEED_WS_URL,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_arbitrum_feed_stream(
                    provider,
                    state,
                    hooks,
                    update_seq,
                    realtime_head,
                    canonical_head,
                    pending_sync_queue,
                    pending_sync_notify,
                    applied_log_dedup,
                    query_chunks,
                    chain_id,
                )))
            }
        }
    }

    fn resolve_realtime_source(
        chain_id: u64,
        source: &RealtimeSyncSource,
    ) -> SelectedRealtimeSource {
        match source {
            RealtimeSyncSource::Auto => {
                if chain_id == BASE_CHAIN_ID {
                    SelectedRealtimeSource::BasePendingLogs
                } else if chain_id == XLAYER_CHAIN_ID {
                    SelectedRealtimeSource::XlayerFlashblocksPull
                } else if chain_id == ARBITRUM_CHAIN_ID {
                    SelectedRealtimeSource::ArbitrumFeedPull
                } else {
                    SelectedRealtimeSource::NewHeadsPull
                }
            }
            RealtimeSyncSource::WsLogs => SelectedRealtimeSource::NewHeadsPull,
            // Backward-compatible mapping: keep the old public config knob, but
            // route it to the new Base `pendingLogs` implementation.
            RealtimeSyncSource::BaseFlashblocksRaw => SelectedRealtimeSource::BasePendingLogs,
            RealtimeSyncSource::XlayerFlashblocksRaw => {
                SelectedRealtimeSource::XlayerFlashblocksPull
            }
            RealtimeSyncSource::ArbitrumSequencerFeed => SelectedRealtimeSource::ArbitrumFeedPull,
        }
    }

    async fn initial_backfill_results(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        chunks: &[LogQueryChunk],
        realtime_head: &Arc<AtomicU64>,
        canonical_head: &Arc<AtomicU64>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Arc<Notify>,
        applied_log_dedup: &Arc<Mutex<AppliedLogDedupCache>>,
        source: LogSource,
        chain_id: u64,
    ) -> Result<Vec<(u64, Vec<Address>)>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let current_synced = realtime_head.load(Ordering::Relaxed);
        if current_synced == 0 {
            return Ok(vec![]);
        }

        let chain_head = provider.get_block_number().await?;
        if chain_head <= current_synced {
            return Ok(vec![]);
        }

        Self::backfill_range(
            provider,
            state,
            hooks,
            chunks,
            current_synced + 1,
            chain_head,
            realtime_head,
            canonical_head,
            pending_sync_queue,
            pending_sync_notify,
            applied_log_dedup,
            source,
            chain_id,
        )
        .await
    }

    #[allow(dead_code)]
    async fn execute_batch_tasks<F, Fut>(
        state: &Arc<RwLock<StateSpace>>,
        amms: Vec<AMM>,
        provider: P,
        log_target: &str,
        task: F,
    ) -> Vec<Address>
    where
        F: Fn(AMM, P) -> Fut,
        Fut: Future<Output = Result<AMM, AMMError>> + Send,
        P: Provider<N> + Clone,
        N: Network,
    {
        if amms.is_empty() {
            return Vec::new();
        }

        let mut futures = FuturesUnordered::new();
        for amm in amms {
            let provider = provider.clone();
            let addr = amm.address();
            let future = task(amm, provider);
            futures.push(async move { (addr, future.await) });
        }

        let mut affected = Vec::new();
        while let Some((addr, res)) = futures.next().await {
            match res {
                Ok(new_amm) => {
                    state.write().await.insert_amm(new_amm);
                    affected.push(addr);
                }
                Err(e) => {
                    error!(target: "state_space::sync", ?addr, task = log_target, "Task failed: {}", e);
                }
            }
        }
        affected
    }

    async fn apply_logs_for_block(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        block_num: u64,
        logs: Vec<Log>,
        realtime_head: &Arc<AtomicU64>,
        canonical_head: &Arc<AtomicU64>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Arc<Notify>,
        applied_log_dedup: &Arc<Mutex<AppliedLogDedupCache>>,
        source: LogSource,
    ) -> Result<Vec<Address>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let (affected, _) = Self::apply_logs_for_block_timed(
            provider,
            state,
            hooks,
            block_num,
            logs,
            realtime_head,
            canonical_head,
            pending_sync_queue,
            pending_sync_notify,
            applied_log_dedup,
            source,
        )
        .await?;
        Ok(affected)
    }

    async fn apply_logs_for_block_timed(
        _provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        block_num: u64,
        mut logs: Vec<Log>,
        realtime_head: &Arc<AtomicU64>,
        canonical_head: &Arc<AtomicU64>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Arc<Notify>,
        applied_log_dedup: &Arc<Mutex<AppliedLogDedupCache>>,
        source: LogSource,
    ) -> Result<(Vec<Address>, ApplyLogsTiming), StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let t_apply_start = Instant::now();
        let mut prev = realtime_head.load(Ordering::Relaxed);
        while block_num > prev
            && realtime_head
                .compare_exchange(prev, block_num, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            prev = realtime_head.load(Ordering::Relaxed);
        }

        // Canonical progress is event-driven from sources that process canonical block logs directly.
        if matches!(
            source,
            LogSource::NewHeadsPull | LogSource::ArbitrumFeedPull
        ) {
            Self::store_monotonic_head(canonical_head, block_num);
            let guard = state.read().await;
            Self::store_monotonic_head(&guard.canonical_head, block_num);
        }

        if logs.is_empty() {
            return Ok((vec![], ApplyLogsTiming::default()));
        }

        let t_sort_start = Instant::now();
        logs.sort_by_key(|log| {
            (
                log.block_number.unwrap_or_default(),
                log.transaction_index.unwrap_or_default(),
                log.log_index.unwrap_or_default(),
            )
        });
        let sort_ms = t_sort_start.elapsed().as_millis();

        let t_dedup_start = Instant::now();
        logs = {
            let mut dedup = applied_log_dedup.lock().await;
            let mut kept = Vec::with_capacity(logs.len());

            for log in logs {
                if dedup.insert_log_if_new(&log, source) {
                    kept.push(log);
                }
            }

            kept
        };
        let dedup_ms = t_dedup_start.elapsed().as_millis();

        if logs.is_empty() {
            return Ok((
                vec![],
                ApplyLogsTiming {
                    sort_ms,
                    dedup_ms,
                    total_ms: t_apply_start.elapsed().as_millis(),
                    ..ApplyLogsTiming::default()
                },
            ));
        }

        let t_sync_start = Instant::now();
        let (affected, needs_resync, needs_async_update) = state.write().await.sync(&logs)?;
        let sync_ms = t_sync_start.elapsed().as_millis();

        if !needs_resync.is_empty() || !needs_async_update.is_empty() {
            let mut queue = pending_sync_queue.lock().await;
            for address in &needs_resync {
                queue.enqueue(
                    *address,
                    PendingSyncAction::Resync,
                    block_num,
                    PendingSyncReason::Resync,
                );
            }
            for address in &needs_async_update {
                queue.enqueue(
                    *address,
                    PendingSyncAction::AsyncUpdate,
                    block_num,
                    PendingSyncReason::AsyncUpdate,
                );
            }
            drop(queue);
            pending_sync_notify.notify_one();
        }

        let t_hooks_start = Instant::now();
        if !affected.is_empty() {
            hooks.notify(&affected).await;
        }
        let hooks_ms = t_hooks_start.elapsed().as_millis();

        Ok((
            affected,
            ApplyLogsTiming {
                sort_ms,
                dedup_ms,
                sync_ms,
                hooks_ms,
                total_ms: t_apply_start.elapsed().as_millis(),
            },
        ))
    }

    async fn collect_logs_for_chunks(
        provider: &P,
        chunks: &[LogQueryChunk],
        from_block: u64,
        to_block: u64,
        bloom: Option<&Bloom>,
    ) -> Result<Vec<Log>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut all_logs = Vec::new();

        for chunk in chunks {
            if let Some(block_bloom) = bloom {
                if !Self::bloom_maybe_has_relevant_logs(block_bloom, chunk) {
                    continue;
                }
            }

            let filter = chunk.ranged_filter(from_block, to_block);
            let logs = provider
                .get_logs(&filter)
                .await
                .map_err(StateSpaceError::from)?;
            all_logs.extend(logs);
        }

        Ok(all_logs)
    }

    async fn backfill_range(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        chunks: &[LogQueryChunk],
        from_block: u64,
        to_block: u64,
        realtime_head: &Arc<AtomicU64>,
        canonical_head: &Arc<AtomicU64>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Arc<Notify>,
        applied_log_dedup: &Arc<Mutex<AppliedLogDedupCache>>,
        source: LogSource,
        chain_id: u64,
    ) -> Result<Vec<(u64, Vec<Address>)>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        if from_block > to_block {
            return Ok(vec![]);
        }

        let mut results = Vec::new();
        let mut start = from_block;
        let window = Self::backfill_window_size(chain_id);

        while start <= to_block {
            let mut end = (start + window - 1).min(to_block);

            loop {
                let logs_res =
                    Self::collect_logs_for_chunks(provider, chunks, start, end, None).await;

                match logs_res {
                    Ok(mut logs) => {
                        logs.sort_by_key(|log| {
                            (
                                log.block_number.unwrap_or_default(),
                                log.transaction_index.unwrap_or_default(),
                                log.log_index.unwrap_or_default(),
                            )
                        });

                        let mut by_block: HashMap<u64, Vec<Log>> = HashMap::new();
                        for log in logs {
                            if let Some(bn) = log.block_number {
                                by_block.entry(bn).or_default().push(log);
                            }
                        }

                        for block_num in start..=end {
                            let block_logs = by_block.remove(&block_num).unwrap_or_default();
                            let affected = Self::apply_logs_for_block(
                                provider,
                                state,
                                hooks,
                                block_num,
                                block_logs,
                                realtime_head,
                                canonical_head,
                                pending_sync_queue,
                                pending_sync_notify,
                                applied_log_dedup,
                                source,
                            )
                            .await?;

                            if !affected.is_empty() {
                                results.push((block_num, affected));
                            }
                        }

                        start = end + 1;
                        break;
                    }
                    Err(e) => {
                        if start == end {
                            return Err(e);
                        }

                        end = start + ((end - start) / 2);
                        warn!(
                            "Backfill window {}..{} failed, shrinking window",
                            start, end
                        );
                    }
                }
            }
        }

        Ok(results)
    }

    async fn build_query_chunks(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        chain_id: u64,
    ) -> Result<Vec<LogQueryChunk>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let guard = state.read().await;

        // This function is the canonical place where realtime log coverage is built.
        // Base `pendingLogs`, generic get_logs backfill/pull, and other log-driven
        // flows all reuse these chunks so we do not drift across protocols.
        let mut topic_addresses = HashSet::new();
        let mut address_only_addresses = HashSet::new();
        let mut topic_signatures: HashSet<FixedBytes<32>> = HashSet::new();
        let mut has_slipstream_pool = false;

        for amm in guard.state.values() {
            let sync_events = amm.sync_events();
            let has_events = !sync_events.is_empty();

            if has_events {
                for event in sync_events {
                    topic_signatures.insert(event);
                }
            }

            match amm.as_ref() {
                AMM::UniswapV4Pool(p) => {
                    if has_events {
                        topic_addresses.insert(p.manager_address);
                    }
                }
                AMM::PancakeInfinityPool(p) => {
                    if has_events {
                        topic_addresses.insert(p.manager_address);
                    }
                }
                AMM::FluidDexPool(p) => {
                    if has_events {
                        topic_addresses.insert(p.address);
                    }
                    if let Some(addr) = get_liquidity_layer(chain_id) {
                        topic_addresses.insert(addr);
                    }
                }
                AMM::BalancerV2Pool(p) => {
                    if has_events {
                        if let Some(vault) = balancer_v2::get_vault_address(chain_id) {
                            topic_addresses.insert(vault);
                        } else {
                            topic_addresses.insert(p.vault_address);
                        }
                    }
                }
                AMM::BalancerV3Pool(p) => {
                    if has_events {
                        if let Some(vault) = balancer_v3::get_vault_address(chain_id) {
                            topic_addresses.insert(vault);
                        } else {
                            topic_addresses.insert(p.vault_address);
                        }
                    }
                }
                AMM::EkuboPool(_) => {
                    if let Some(core) = ekubo::get_core_address(chain_id) {
                        address_only_addresses.insert(core);
                    }
                }
                AMM::AerodromeSlipstreamPool(_) => {
                    has_slipstream_pool = true;
                    if has_events {
                        topic_addresses.insert(amm.address());
                    }
                }
                AMM::AlgebraIntegralPool(p) => {
                    if has_events {
                        topic_addresses.insert(amm.address());
                        // FeeConfiguration is emitted by the plugin contract.
                        if !p.plugin.is_zero() {
                            topic_addresses.insert(p.plugin);
                        }
                    }
                }
                _ => {
                    if has_events {
                        topic_addresses.insert(amm.address());
                    }
                }
            }
        }

        drop(guard);

        if has_slipstream_pool && chain_id == BASE_CHAIN_ID {
            // Collect all Slipstream pool addresses for dynamic FeeModule resolution
            let slipstream_addrs: Vec<Address> = {
                let guard = state.read().await;
                guard
                    .state
                    .values()
                    .filter_map(|amm| match amm.as_ref() {
                        AMM::AerodromeSlipstreamPool(p) => Some(p.address),
                        _ => None,
                    })
                    .collect()
            };
            let fee_modules =
                Self::resolve_slipstream_fee_modules(&slipstream_addrs, provider).await;
            for fm in &fee_modules {
                topic_addresses.insert(*fm);
            }
            if !fee_modules.is_empty() {
                topic_signatures.insert(ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH);
            }
        }

        let mut chunks = Vec::new();

        if !topic_addresses.is_empty() && !topic_signatures.is_empty() {
            let mut topic_addresses: Vec<Address> = topic_addresses.into_iter().collect();
            topic_addresses.sort();

            let mut topic_signatures: Vec<FixedBytes<32>> = topic_signatures.into_iter().collect();
            topic_signatures.sort();

            // Note: topic signatures are unioned across all pools/protocols.
            // This may subscribe somewhat wider than strictly necessary, but it
            // avoids false negatives when different AMM types route through
            // shared manager/vault/plugin contracts.
            for addresses in topic_addresses.chunks(LOG_ADDRESS_CHUNK_SIZE) {
                chunks.push(LogQueryChunk {
                    addresses: addresses.to_vec(),
                    mode: QueryMode::TopicFiltered(topic_signatures.clone()),
                });
            }
        }

        if !address_only_addresses.is_empty() {
            let mut address_only_addresses: Vec<Address> =
                address_only_addresses.into_iter().collect();
            address_only_addresses.sort();

            for addresses in address_only_addresses.chunks(LOG_ADDRESS_CHUNK_SIZE) {
                chunks.push(LogQueryChunk {
                    addresses: addresses.to_vec(),
                    mode: QueryMode::AddressOnly,
                });
            }
        }

        Ok(chunks)
    }

    /// Resolve all unique FeeModule addresses from loaded Slipstream pools.
    /// Each pool may have a different factory, so we collect all unique factory → FeeModule mappings.
    async fn resolve_slipstream_fee_modules(pool_addrs: &[Address], provider: &P) -> Vec<Address>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut fee_modules = std::collections::HashSet::new();
        let mut factory_cache: std::collections::HashMap<Address, Address> =
            std::collections::HashMap::new();

        use crate::amms::aerodrome_slipstream::ICLPool;
        for &pool_addr in pool_addrs {
            let factory_addr = match ICLPool::new(pool_addr, provider.clone())
                .factory()
                .call()
                .await
            {
                Ok(addr) if addr != Address::ZERO => addr,
                _ => continue,
            };
            let fm_addr = if let Some(&cached) = factory_cache.get(&factory_addr) {
                cached
            } else {
                let fm = ICLFactoryReader::new(factory_addr, provider.clone())
                    .swapFeeModule()
                    .call()
                    .await
                    .unwrap_or(Address::ZERO);
                factory_cache.insert(factory_addr, fm);
                fm
            };
            if fm_addr != Address::ZERO {
                fee_modules.insert(fm_addr);
            }
        }
        fee_modules.into_iter().collect()
    }

    fn bloom_maybe_has_relevant_logs(bloom: &Bloom, chunk: &LogQueryChunk) -> bool {
        let address_hit = chunk
            .addresses
            .iter()
            .any(|addr| bloom.contains_input(BloomInput::Raw(addr.as_slice())));

        if !address_hit {
            return false;
        }

        match &chunk.mode {
            QueryMode::AddressOnly => true,
            QueryMode::TopicFiltered(topics) => topics
                .iter()
                .any(|topic| bloom.contains_input(BloomInput::Raw(topic.as_slice()))),
        }
    }

    fn backfill_window_size(chain_id: u64) -> u64 {
        match chain_id {
            ARBITRUM_CHAIN_ID => 200,
            BASE_CHAIN_ID => 100,
            XLAYER_CHAIN_ID => 100,
            ETHEREUM_MAINNET_CHAIN_ID => 50,
            _ => 50,
        }
    }

    fn store_monotonic_head(head: &Arc<AtomicU64>, incoming: u64) {
        let mut prev = head.load(Ordering::Relaxed);
        while incoming > prev {
            match head.compare_exchange(prev, incoming, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
        }
    }

    async fn run_canonical_head_tracker(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        pending_sync_notify: Arc<Notify>,
        canonical_head: Arc<AtomicU64>,
    ) where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        let stream_idle_timeout = Duration::from_secs(15);
        loop {
            match provider.subscribe_blocks().await {
                Ok(sub) => {
                    info!("canonical head tracker subscribed to newHeads");
                    let mut stream = sub.into_stream();

                    loop {
                        match tokio::time::timeout(stream_idle_timeout, stream.next()).await {
                            Ok(Some(header)) => {
                                let block = header.number();
                                Self::store_monotonic_head(&canonical_head, block);
                                {
                                    let guard = state.read().await;
                                    Self::store_monotonic_head(&guard.canonical_head, block);
                                }
                                pending_sync_notify.notify_one();
                            }
                            Ok(None) => {
                                warn!("canonical newHeads stream ended, reconnecting");
                                break;
                            }
                            Err(_) => {
                                warn!("canonical newHeads stream timeout, reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("canonical newHeads subscribe failed: {}", e);
                }
            }
            sleep(STREAM_RECONNECT_DELAY).await;
        }
    }
}

#[derive(Clone)]
pub struct StateSpaceBuilder<N, P> {
    pub provider: P,
    pub initial_block: u64,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
    pub filters: Vec<PoolFilter>,
    pub hooks: Vec<StateHook<Vec<Address>>>,
    pub snapshot_path: Option<PathBuf>,
    pub snapshot_config: Option<SnapshotConfig>,
    /// Periodic sync for data that can drift without reliable events
    /// (e.g. Balancer rates/fees, Fluid limits/price, Slipstream fee).
    pub non_event_sync_interval: Option<Duration>,
    /// Legacy compatibility alias. Prefer `non_event_sync_interval`.
    pub rate_sync_interval: Option<Duration>,
    /// Dedicated interval for Slipstream fee refresh.
    pub slipstream_fee_sync_interval: Option<Duration>,
    pub curve_sync_interval: Option<Duration>,
    pub pending_sync_worker_interval: Duration,
    pub drift_probe_interval: Duration,
    pub maintenance_interval: Option<Duration>,
    pub realtime_source: RealtimeSyncSource,
    pub realtime_ws_endpoints: Option<Vec<String>>,
    phantom: PhantomData<N>,
}

impl<N, P> StateSpaceBuilder<N, P>
where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    pub fn new(provider: P) -> StateSpaceBuilder<N, P> {
        Self {
            provider,
            initial_block: 0,
            factories: vec![],
            amms: vec![],
            filters: vec![],
            phantom: PhantomData,
            snapshot_path: None,
            snapshot_config: None,
            non_event_sync_interval: None,
            rate_sync_interval: None,
            slipstream_fee_sync_interval: None,
            curve_sync_interval: None,
            pending_sync_worker_interval: DEFAULT_PENDING_SYNC_WORKER_INTERVAL,
            drift_probe_interval: DEFAULT_DRIFT_PROBE_INTERVAL,
            maintenance_interval: None,
            realtime_source: RealtimeSyncSource::Auto,
            realtime_ws_endpoints: None,
            hooks: vec![],
        }
    }

    pub fn block(self, block: u64) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            initial_block: block,
            ..self
        }
    }

    pub fn with_factories(self, factories: Vec<Factory>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { factories, ..self }
    }

    pub fn with_amms(self, amms: Vec<AMM>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { amms, ..self }
    }

    pub fn with_filters(self, filters: Vec<PoolFilter>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { filters, ..self }
    }

    pub fn with_hooks(self, hooks: Vec<StateHook<Vec<Address>>>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { hooks, ..self }
    }

    pub fn with_snapshot_path(self, snapshot_path: PathBuf) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            snapshot_path: Some(snapshot_path),
            ..self
        }
    }

    pub fn with_snapshot_enabled(self, config: Option<SnapshotConfig>) -> StateSpaceBuilder<N, P> {
        let config = config.unwrap_or_default();
        StateSpaceBuilder {
            snapshot_config: Some(config),
            ..self
        }
    }

    pub fn with_non_event_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            non_event_sync_interval: Some(interval),
            rate_sync_interval: Some(interval),
            ..self
        }
    }

    /// Backward-compatible alias of `with_non_event_sync_interval`.
    pub fn with_rate_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        self.with_non_event_sync_interval(interval)
    }

    /// Set a dedicated interval for Slipstream fee refresh.
    /// Falls back to `non_event_sync_interval` when not set.
    pub fn with_slipstream_fee_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            slipstream_fee_sync_interval: Some(interval),
            ..self
        }
    }

    pub fn with_maintenance_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            maintenance_interval: Some(interval),
            ..self
        }
    }

    pub fn with_maintenance_coverage_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        self.with_maintenance_interval(interval)
    }

    pub fn with_pending_sync_worker_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            pending_sync_worker_interval: interval,
            ..self
        }
    }

    pub fn with_drift_probe_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            drift_probe_interval: interval,
            ..self
        }
    }

    pub fn with_curve_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            curve_sync_interval: Some(interval),
            ..self
        }
    }

    pub fn with_realtime_source(self, source: RealtimeSyncSource) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            realtime_source: source,
            ..self
        }
    }

    /// Provide explicit WebSocket endpoints for realtime sources that require
    /// their own direct subscription connection.
    ///
    /// This is currently required for Base `pendingLogs`, because that
    /// subscription does not reuse the passed `Provider` transport session.
    /// The provided endpoints must support Base flashblock-related
    /// subscription methods, specifically `eth_subscribe` with `pendingLogs`.
    ///
    /// It is safe for downstream applications to always call this builder
    /// method; non-Base realtime paths currently ignore these endpoints.
    /// Empty / blank entries are ignored.
    pub fn with_realtime_ws_endpoints(self, endpoints: Vec<String>) -> StateSpaceBuilder<N, P> {
        let mut normalized = Vec::new();
        for endpoint in endpoints {
            let trimmed = endpoint.trim();
            if trimmed.is_empty() {
                continue;
            }
            let candidate = trimmed.to_string();
            if !normalized.contains(&candidate) {
                normalized.push(candidate);
            }
        }

        StateSpaceBuilder {
            realtime_ws_endpoints: (!normalized.is_empty()).then_some(normalized),
            ..self
        }
    }

    pub async fn sync(mut self) -> Result<StateSpaceManager<N, P>, AMMError> {
        let mut state_space = StateSpace::default();

        let chain_id = self.provider.get_chain_id().await?;
        info!(target: "state_space::sync", "Syncing AMMs for chain {}", chain_id);

        let chain_tip_u64 = if self.initial_block > 0 {
            self.initial_block
        } else {
            self.provider.get_block_number().await?
        };

        // If block() was not explicitly configured, initialize all runtime heads with chain tip
        if self.initial_block == 0 {
            self.initial_block = chain_tip_u64;
        }

        let chain_tip = BlockId::from(chain_tip_u64);

        let factories = self.factories.clone();
        let mut futures = FuturesUnordered::new();

        // 1. Filter statically loaded AMMs
        let mut valid_amms = Vec::with_capacity(self.amms.len());
        for amm in self.amms {
            if let Some(supported) = amm.supported_chains() {
                if !supported.contains(&chain_id) {
                    warn!(
                        target: "state_space::sync",
                        amm = ?amm.address(),
                        supported = ?supported,
                        current = chain_id,
                        "Skipping AMM due to chain mismatch"
                    );
                    continue;
                }
            }
            valid_amms.push(amm);
        }
        self.amms = valid_amms;

        let mut filter_set = HashSet::new();
        for factory in &self.factories {
            for event in factory.pool_events() {
                filter_set.insert(event);
            }
        }

        for amm in self.amms.iter() {
            for event in amm.sync_events() {
                filter_set.insert(event);
            }
        }

        let block_filter = Filter::new().event_signature(FilterSet::from(
            filter_set.into_iter().collect::<Vec<FixedBytes<32>>>(),
        ));
        let mut amm_variants = HashMap::new();

        for amm in self.amms.into_iter() {
            amm_variants
                .entry(amm.variant())
                .or_insert_with(Vec::new)
                .push(amm);
        }

        for factory in factories {
            let provider = self.provider.clone();
            let filters = self.filters.clone();

            let extension = amm_variants.remove(&factory.variant());
            futures.push(tokio::spawn(async move {
                let mut discovered_amms = factory.discover(chain_tip, provider.clone()).await?;

                info!(
                    target: "state_space::sync",
                    factory = %factory.address(),
                    discovered = discovered_amms.len(),
                    "Discovered AMMs"
                );

                // 2. Filter discovered AMMs based on chain support
                discovered_amms.retain(|amm| {
                    if let Some(supported) = amm.supported_chains() {
                        if !supported.contains(&chain_id) {
                            warn!(
                                target: "state_space::sync",
                                factory = %factory.address(),
                                amm = ?amm.address(),
                                supported = ?supported,
                                current = chain_id,
                                "Filtering discovered AMM due to chain mismatch"
                            );
                            return false;
                        }
                    }
                    true
                });

                if let Some(amms) = extension {
                    discovered_amms.extend(amms);
                }

                // Apply discovery filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Discovery {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Discovery filter"
                        );
                    }
                }

                discovered_amms = factory.sync(discovered_amms, chain_tip, provider).await?;

                // Apply sync filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Sync {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Sync filter"
                        );
                    }
                }

                Ok::<Vec<AMM>, AMMError>(discovered_amms)
            }));
        }

        while let Some(res) = futures.next().await {
            let synced_amms = res??;

            for amm in synced_amms {
                let mut amm = amm;
                amm.set_last_synced_block(chain_tip_u64);
                state_space.insert_amm(amm);
            }
        }

        // Sync remaining AMM variants in batches by variant
        for (variant, remaining_amms) in amm_variants.drain() {
            info!(target: "state_space::sync", variant = ?variant, count = remaining_amms.len(), "Syncing batch");
            let provider = self.provider.clone();
            let synced = variant
                .init_batch::<N, _>(remaining_amms, chain_tip, provider.clone())
                .await?;

            // 仅做通用调度节流；具体批量大小/并发策略由各 AMM init_batch 内部负责。
            sleep(Duration::from_millis(1200)).await;

            for amm in synced {
                let mut amm = amm;
                amm.set_last_synced_block(chain_tip_u64);
                state_space.insert_amm(amm);
            }
        }

        let realtime_head = Arc::new(AtomicU64::new(self.initial_block));
        let canonical_head = Arc::new(AtomicU64::new(self.initial_block));
        state_space.realtime_head = realtime_head.clone();
        state_space.canonical_head = canonical_head.clone();
        state_space.chain_id = chain_id;

        let state_space = Arc::new(RwLock::new(state_space));

        if let Some(snapshot_config) = self.snapshot_config {
            let hook = snapshot_config.into_state_hook(state_space.clone()).await;
            self.hooks.push(hook);
        }

        let non_event_interval = self.non_event_sync_interval.or(self.rate_sync_interval);
        if let Some(interval) = non_event_interval {
            tokio::spawn(sync_services::start_balancer_v2_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
            tokio::spawn(sync_services::start_balancer_v3_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
            // Balancer V3 pools: swap_fee (can be updated by governance, may fail during init)
            tokio::spawn(sync_services::start_balancer_v3_fee_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));

            // Fluid DEX pools: limits and centerPrice (expand over time, drift without events)
            tokio::spawn(sync_services::start_fluid_dex_limits_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
            tokio::spawn(sync_services::start_rocketpool_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
            tokio::spawn(sync_services::start_pendle_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        // Slipstream fee config sync: low-frequency task to refresh DynamicFeeConfig and
        // FeeModuleGlobals (governance changes we don't fully subscribe to via events).
        // Dynamically resolve FeeModule addresses from loaded pools.
        let slipstream_fee_modules: Vec<Address> = {
            let slipstream_addrs: Vec<Address> = {
                let guard = state_space.read().await;
                guard
                    .state
                    .values()
                    .filter_map(|amm| match amm.as_ref() {
                        AMM::AerodromeSlipstreamPool(p) => Some(p.address),
                        _ => None,
                    })
                    .collect()
            };
            StateSpaceManager::<N, P>::resolve_slipstream_fee_modules(
                &slipstream_addrs,
                &self.provider,
            )
            .await
        };

        if !slipstream_fee_modules.is_empty() {
            // Eagerly seed FEE_MODULE_GLOBALS before any event processing.
            if let Some(&first_fm) = slipstream_fee_modules.first() {
                use crate::state_space::sync_services::IDynamicFeeModuleReader;
                let reader = IDynamicFeeModuleReader::new(first_fm, self.provider.clone());
                let (sf, fc, sa) = (
                    reader.defaultScalingFactor().call().await,
                    reader.defaultFeeCap().call().await,
                    reader.secondsAgo().call().await,
                );
                let globals = crate::amms::aerodrome_slipstream::pool::FeeModuleGlobals {
                    default_scaling_factor: sf.ok().map(|v| v.to::<u64>()).unwrap_or(0),
                    default_fee_cap: fc.ok().map(|v| v.to::<u32>()).unwrap_or(50_000),
                    seconds_ago: sa.ok().unwrap_or(600),
                };
                if let Ok(mut g) =
                    crate::amms::aerodrome_slipstream::pool::FEE_MODULE_GLOBALS.lock()
                {
                    *g = globals;
                }
                info!("Slipstream FeeModule globals seeded: scaling_factor={}, fee_cap={}, seconds_ago={}",
                    globals.default_scaling_factor, globals.default_fee_cap, globals.seconds_ago);
            }

            // Spawn low-frequency fee config sync task (default 910s).
            let provider_clone = self.provider.clone();
            let state_clone = state_space.clone();
            let fee_config_interval = self
                .slipstream_fee_sync_interval
                .unwrap_or(Duration::from_secs(910));
            tokio::spawn(sync_services::start_slipstream_fee_config_sync_task(
                state_clone,
                provider_clone,
                fee_config_interval,
                slipstream_fee_modules,
            ));
        } else {
            error!("No Slipstream FeeModule found from loaded pools; fee config sync disabled");
        }

        // Curve pools: StableSwap runtime refresh plus CryptoSwap oracle/runtime refresh
        if let Some(interval) = self.curve_sync_interval.or(non_event_interval) {
            tokio::spawn(sync_services::start_curve_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        Ok(StateSpaceManager {
            realtime_ws_endpoints: self.realtime_ws_endpoints,
            realtime_head,
            canonical_head,
            update_seq: Arc::new(AtomicU64::new(0)),
            state: state_space,
            block_filter,
            provider: self.provider,
            realtime_source: self.realtime_source,
            pending_sync_queue: Arc::new(Mutex::new(PendingSyncQueue::default())),
            pending_sync_notify: Arc::new(Notify::new()),
            applied_log_dedup: Arc::new(Mutex::new(AppliedLogDedupCache::default())),
            background_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_sync_worker_interval: self.pending_sync_worker_interval,
            drift_probe_interval: self.drift_probe_interval,
            maintenance_interval: self.maintenance_interval,
            phantom: PhantomData,
            hooks: HookRegistry::new(self.hooks),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct StateSpace {
    pub state: HashMap<Address, Arc<AMM>>,
    pub realtime_head: Arc<AtomicU64>,
    pub canonical_head: Arc<AtomicU64>,
    pub chain_id: u64,
}

impl StateSpace {
    pub fn get(&self, address: &Address) -> Option<&AMM> {
        self.state.get(address).map(Arc::as_ref)
    }

    pub fn get_shared(&self, address: &Address) -> Option<&Arc<AMM>> {
        self.state.get(address)
    }

    pub fn get_mut_cow(&mut self, address: &Address) -> Option<&mut AMM> {
        self.state.get_mut(address).map(Arc::make_mut)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AMM> {
        self.get_mut_cow(address)
    }

    pub fn insert_amm(&mut self, amm: AMM) {
        let is_curve_legacy = matches!(amm, AMM::CurveLegacyPool(_));
        self.state.insert(amm.address(), Arc::new(amm));
        if is_curve_legacy {
            self.rebuild_curve_legacy_meta_views();
        }
    }

    pub fn insert_shared(&mut self, address: Address, amm: Arc<AMM>) {
        self.state.insert(address, amm);
    }

    pub fn rebuild_curve_legacy_meta_views(&mut self) {
        let base_views: HashMap<Address, Arc<crate::amms::curve_legacy::CurveLegacyBaseView>> =
            self.state
                .iter()
                .filter_map(|(address, amm)| match amm.as_ref() {
                    AMM::CurveLegacyPool(pool) => {
                        pool.build_base_view().map(|view| (*address, view))
                    }
                    _ => None,
                })
                .collect();

        let legacy_addresses: Vec<Address> = self
            .state
            .iter()
            .filter_map(|(address, amm)| match amm.as_ref() {
                AMM::CurveLegacyPool(_) => Some(*address),
                _ => None,
            })
            .collect();

        for address in legacy_addresses {
            let Some(AMM::CurveLegacyPool(pool)) = self.get_mut_cow(&address) else {
                continue;
            };

            let rebuilt_view = pool
                .base_pool_address
                .and_then(|base_addr| base_views.get(&base_addr).cloned());
            if rebuilt_view.is_some() {
                pool.base_pool_view = rebuilt_view;
            }
            pool.update_spot_prices();
        }
    }

    fn resolve_slipstream_fee_event_pool(&self, topics: &[FixedBytes<32>]) -> Option<Address> {
        if topics.len() < 2 {
            return None;
        }

        if topics[0] != ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH {
            return None;
        }

        let pool_address = Address::from_word(topics[1]);
        match self.state.get(&pool_address).map(Arc::as_ref) {
            Some(AMM::AerodromeSlipstreamPool(_)) => Some(pool_address),
            _ => None,
        }
    }

    fn resolve_algebra_plugin_event_pools(
        &self,
        log_address: Address,
        topics: &[FixedBytes<32>],
    ) -> Vec<Address> {
        if topics.is_empty() {
            return vec![];
        }
        if topics[0] != IDynamicFeeManager::FeeConfiguration::SIGNATURE_HASH {
            return vec![];
        }

        self.state
            .iter()
            .filter_map(|(pool_address, amm)| match amm.as_ref() {
                AMM::AlgebraIntegralPool(p) if p.plugin == log_address => Some(*pool_address),
                _ => None,
            })
            .collect()
    }

    pub fn sync(
        &mut self,
        logs: &[Log],
    ) -> Result<(Vec<Address>, Vec<Address>, Vec<Address>), StateSpaceError> {
        // 处理流程：
        // 1) 调用方保证 logs 已按 (block_number, transaction_index, log_index) 有序
        // 2) 逐条应用 log 到对应池子的本地状态
        if logs.is_empty() {
            return Ok((vec![], vec![], vec![]));
        }

        let latest = self.realtime_head.load(Ordering::Relaxed);

        // We do not check for reorgs here using block numbers because partitioned log subscriptions
        // (chunking) can cause logs to arrive out of order or in interleaved batches.
        // A "late" batch from one chunk is not a reorg of the chain.
        // We rely on:
        // 1. Per-AMM `last_synced_block` checks to prevent rewinding individual pools.
        // 2. The maintenance coverage scheduler + canonical reconcile path to repair drift/discrepancies.
        // 3. The `needs_resync` set to handle syncs that fail due to insufficient data (e.g. Curve V1 RemoveLiquidityOne)

        let mut affected_amms = HashSet::new();
        let mut needs_resync = HashSet::new();
        let mut needs_async_update = HashSet::new();
        let mut max_processed_block = latest;

        for log in logs {
            let log_block_number = log
                .block_number
                .ok_or(StateSpaceError::MissingBlockNumber)?;

            // Track the latest block info seen in this batch
            if log_block_number > max_processed_block {
                max_processed_block = log_block_number;
            }

            let address = log.address();
            let direct_hit = self.state.contains_key(&address);

            let mut target_addresses: Vec<Address> = Vec::new();
            if direct_hit {
                target_addresses.push(address);
            } else if log.topics().len() >= 2 {
                if Some(address) == get_liquidity_layer(self.chain_id) {
                    let pool_address = Address::from_word(log.topics()[1]);
                    if matches!(
                        self.state.get(&pool_address).map(Arc::as_ref),
                        Some(AMM::FluidDexPool(_))
                    ) {
                        target_addresses.push(pool_address);
                    }
                } else if Some(address) == balancer_v2::get_vault_address(self.chain_id) {
                    // Balancer V2: poolId is in topics[1]
                    // The first 20 bytes of poolId is the pool address, which is used as the key in StateSpace
                    let pool_id = log.topics()[1];
                    let pool_address = Address::from_slice(&pool_id.as_slice()[0..20]);
                    if matches!(
                        self.state.get(&pool_address).map(Arc::as_ref),
                        Some(AMM::BalancerV2Pool(p)) if p.pool_id == pool_id
                    ) {
                        target_addresses.push(pool_address);
                    }
                } else if Some(address) == balancer_v3::get_vault_address(self.chain_id) {
                    // Balancer V3: pool address is in topics[1]
                    let pool_address = Address::from_word(log.topics()[1]);
                    if self.state.contains_key(&pool_address) {
                        target_addresses.push(pool_address);
                    }
                } else if let Some(pool_address) =
                    self.resolve_slipstream_fee_event_pool(log.topics())
                {
                    target_addresses.push(pool_address);
                } else {
                    let pool_id = log.topics()[1];
                    let virtual_address = Address::from_slice(&pool_id.as_slice()[0..20]);
                    match self.state.get(&virtual_address).map(Arc::as_ref) {
                        Some(AMM::UniswapV4Pool(p)) if p.manager_address == address => {
                            target_addresses.push(virtual_address)
                        }
                        Some(AMM::PancakeInfinityPool(p)) if p.manager_address == address => {
                            target_addresses.push(virtual_address)
                        }
                        _ => {}
                    }
                }
            } else if log.topics().is_empty()
                && Some(address) == ekubo::get_core_address(self.chain_id)
            {
                // Ekubo Log0 events: no topics, pool_id is at data[20..52]
                let data = log.data().data.as_ref();
                if data.len() >= 52 {
                    let pool_id = FixedBytes::<32>::from_slice(&data[20..52]);
                    // Ekubo uses the first 20 bytes of pool_id as the virtual address key in StateSpace
                    let virtual_address = Address::from_slice(&pool_id.as_slice()[0..20]);

                    if matches!(
                        self.state.get(&virtual_address).map(Arc::as_ref),
                        Some(AMM::EkuboPool(p)) if p.pool_id == pool_id
                    ) {
                        target_addresses.push(virtual_address);
                    }
                }
            }

            for pool_address in self.resolve_algebra_plugin_event_pools(address, log.topics()) {
                if !target_addresses.contains(&pool_address) {
                    target_addresses.push(pool_address);
                }
            }

            if target_addresses.is_empty() {
                continue;
            }

            for target_address in target_addresses {
                let Some(amm) = self.get_mut_cow(&target_address) else {
                    continue;
                };

                // 如果 log 区块小于已同步区块，跳过（幂等性）
                if log_block_number < amm.last_synced_block() {
                    continue;
                }

                match amm.sync(log) {
                    Ok(action) => {
                        amm.set_last_synced_block(log_block_number);
                        affected_amms.insert(target_address);

                        match action {
                            SyncAction::None => {}
                            SyncAction::AsyncUpdate => {
                                needs_async_update.insert(target_address);
                            }
                            SyncAction::Resync => {
                                needs_resync.insert(target_address);
                            }
                        }
                    }

                    Err(e) => {
                        error!(target: "state_space::sync", ?address, ?log_block_number, "Failed to sync AMM with log: {}", e);
                        if let Some(tx_hash) = log.transaction_hash {
                            error!(target: "state_space::sync", ?target_address, ?tx_hash, "Marking AMM for resync after sync error");
                        }
                        needs_resync.insert(target_address);
                    }
                }
            }
        }

        if affected_amms.iter().any(|addr| {
            matches!(
                self.state.get(addr).map(Arc::as_ref),
                Some(AMM::CurveLegacyPool(_))
            )
        }) {
            self.rebuild_curve_legacy_meta_views();
        }

        // Update realtime head internally to ensure consistency with state lock
        if max_processed_block > latest {
            self.realtime_head
                .store(max_processed_block, Ordering::Relaxed);
        }

        Ok((
            affected_amms.into_iter().collect(),
            needs_resync.into_iter().collect(),
            needs_async_update.into_iter().collect(),
        ))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializableStateSpace {
    pub state: HashMap<Address, AMM>,
    #[serde(default)]
    pub realtime_head: u64,
    #[serde(default)]
    pub canonical_head: u64,
}

impl From<StateSpace> for SerializableStateSpace {
    fn from(ss: StateSpace) -> Self {
        Self {
            state: ss
                .state
                .into_iter()
                .map(|(address, amm)| (address, amm.as_ref().clone()))
                .collect(),
            realtime_head: ss.realtime_head.load(Ordering::Relaxed),
            canonical_head: ss.canonical_head.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::{algebra_integral::AlgebraIntegralPool, uniswap_v2::UniswapV2Pool};
    use alloy::primitives::{address, Bytes, FixedBytes, LogData};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn make_v2_amm(pool_address: Address, last_synced_block: u64) -> AMM {
        let mut amm = AMM::UniswapV2Pool(UniswapV2Pool::new(pool_address));
        amm.set_last_synced_block(last_synced_block);
        amm
    }

    fn make_test_log(
        tx_hash: Option<B256>,
        log_index: Option<u64>,
        topics: Vec<FixedBytes<32>>,
        data: Vec<u8>,
    ) -> Log {
        let Some(log_data) = LogData::new(topics, Bytes::from(data)) else {
            panic!("failed to build test log data");
        };

        Log {
            inner: alloy::primitives::Log {
                address: address!("3333333333333333333333333333333333333333"),
                data: log_data,
            },
            block_hash: None,
            block_number: Some(123),
            block_timestamp: Some(1),
            transaction_hash: tx_hash,
            transaction_index: Some(0),
            log_index,
            removed: false,
        }
    }

    #[test]
    fn bloom_prefilter_address_only_matches() {
        let address = address!("1111111111111111111111111111111111111111");
        let mut bloom = Bloom::ZERO;
        bloom.accrue(BloomInput::Raw(address.as_slice()));

        let chunk = LogQueryChunk {
            addresses: vec![address],
            mode: QueryMode::AddressOnly,
        };

        assert!(StateSpaceManager::<(), ()>::bloom_maybe_has_relevant_logs(
            &bloom, &chunk
        ));
    }

    #[test]
    fn bloom_prefilter_topic_filtered_requires_topic_hit() {
        let address = address!("2222222222222222222222222222222222222222");
        let hit_topic = FixedBytes::<32>::from([0x11u8; 32]);
        let miss_topic = FixedBytes::<32>::from([0x22u8; 32]);

        let mut bloom = Bloom::ZERO;
        bloom.accrue(BloomInput::Raw(address.as_slice()));
        bloom.accrue(BloomInput::Raw(hit_topic.as_slice()));

        let hit_chunk = LogQueryChunk {
            addresses: vec![address],
            mode: QueryMode::TopicFiltered(vec![hit_topic]),
        };

        let miss_chunk = LogQueryChunk {
            addresses: vec![address],
            mode: QueryMode::TopicFiltered(vec![miss_topic]),
        };

        assert!(StateSpaceManager::<(), ()>::bloom_maybe_has_relevant_logs(
            &bloom, &hit_chunk
        ));
        assert!(!StateSpaceManager::<(), ()>::bloom_maybe_has_relevant_logs(
            &bloom,
            &miss_chunk
        ));
    }

    #[test]
    fn backfill_window_size_is_chain_specific() {
        assert_eq!(
            StateSpaceManager::<(), ()>::backfill_window_size(42161),
            200
        );
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(8453), 100);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(196), 100);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(1), 50);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(10), 50);
    }

    #[test]
    fn slipstream_custom_fee_event_routes_to_pool_topic1() {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        let mut state = StateSpace::default();
        state.insert_amm(AMM::AerodromeSlipstreamPool(
            crate::amms::aerodrome_slipstream::AerodromeSlipstreamPool::new(pool_address),
        ));

        let topics = vec![
            ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH,
            pool_address.into_word(),
        ];

        assert_eq!(
            state.resolve_slipstream_fee_event_pool(&topics),
            Some(pool_address)
        );
    }

    #[test]
    fn slipstream_custom_fee_event_ignores_unknown_pool() {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        let state = StateSpace::default();
        let topics = vec![
            ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH,
            pool_address.into_word(),
        ];

        assert_eq!(state.resolve_slipstream_fee_event_pool(&topics), None);
    }

    #[test]
    fn algebra_fee_config_event_routes_to_all_pools_for_plugin() {
        let plugin = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let pool1 = address!("1111111111111111111111111111111111111111");
        let pool2 = address!("2222222222222222222222222222222222222222");
        let pool_other = address!("3333333333333333333333333333333333333333");
        let other_plugin = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let mut state = StateSpace::default();
        let mut algebra_1 = AlgebraIntegralPool::new(pool1);
        algebra_1.plugin = plugin;
        state.insert_amm(AMM::AlgebraIntegralPool(algebra_1));

        let mut algebra_2 = AlgebraIntegralPool::new(pool2);
        algebra_2.plugin = plugin;
        state.insert_amm(AMM::AlgebraIntegralPool(algebra_2));

        let mut algebra_3 = AlgebraIntegralPool::new(pool_other);
        algebra_3.plugin = other_plugin;
        state.insert_amm(AMM::AlgebraIntegralPool(algebra_3));

        let topics = vec![IDynamicFeeManager::FeeConfiguration::SIGNATURE_HASH];
        let mut routed = state.resolve_algebra_plugin_event_pools(plugin, &topics);
        routed.sort_unstable();

        assert_eq!(routed, vec![pool1, pool2]);
    }

    #[test]
    fn algebra_non_fee_event_does_not_route_from_plugin() {
        let plugin = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let pool = address!("1111111111111111111111111111111111111111");
        let mut state = StateSpace::default();
        let mut algebra = AlgebraIntegralPool::new(pool);
        algebra.plugin = plugin;
        state.insert_amm(AMM::AlgebraIntegralPool(algebra));

        let topics = vec![ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH];
        assert!(state
            .resolve_algebra_plugin_event_pools(plugin, &topics)
            .is_empty());
    }

    #[test]
    fn applied_log_builder_drops_same_event_when_indices_differ_across_sources() {
        let tx_hash = alloy::primitives::B256::from([0x88u8; 32]);
        let topic0 = FixedBytes::<32>::from([0x99u8; 32]);
        let canonical_log = make_test_log(Some(tx_hash), Some(77), vec![topic0], vec![0xde, 0xad]);
        let mut realtime_log =
            make_test_log(Some(tx_hash), Some(0), vec![topic0], vec![0xde, 0xad]);
        realtime_log.transaction_index = Some(12);

        let mut dedup = AppliedLogDedupCache::default();
        assert!(dedup.insert_log_if_new(&canonical_log, LogSource::NewHeadsPull));
        assert!(!dedup.insert_log_if_new(&realtime_log, LogSource::XlayerFlashblock));
    }

    #[test]
    fn applied_log_builder_drops_canonical_overlap_after_xlayer_local_index() {
        let tx_hash = alloy::primitives::B256::from([0x89u8; 32]);
        let topic0 = FixedBytes::<32>::from([0x9au8; 32]);
        let realtime_log = make_test_log(Some(tx_hash), Some(7), vec![topic0], vec![0xde, 0xad]);
        let mut canonical_log =
            make_test_log(Some(tx_hash), Some(79), vec![topic0], vec![0xde, 0xad]);
        canonical_log.transaction_index = Some(12);

        let mut dedup = AppliedLogDedupCache::default();
        assert!(dedup.insert_log_if_new(&realtime_log, LogSource::XlayerFlashblock));
        assert!(!dedup.insert_log_if_new(&canonical_log, LogSource::NewHeadsPull));
    }

    #[test]
    fn applied_log_builder_keeps_same_payload_events_when_canonical_indices_differ() {
        let tx_hash = alloy::primitives::B256::from([0x12u8; 32]);
        let topic0 = FixedBytes::<32>::from([0x34u8; 32]);
        let log_a = make_test_log(Some(tx_hash), Some(77), vec![topic0], vec![0xde, 0xad]);
        let log_b = make_test_log(Some(tx_hash), Some(78), vec![topic0], vec![0xde, 0xad]);

        let mut dedup = AppliedLogDedupCache::default();
        assert!(dedup.insert_log_if_new(&log_a, LogSource::NewHeadsPull));
        assert!(dedup.insert_log_if_new(&log_b, LogSource::NewHeadsPull));
    }

    #[test]
    fn applied_log_builder_keeps_same_payload_events_when_xlayer_local_indices_differ() {
        let tx_hash = alloy::primitives::B256::from([0x56u8; 32]);
        let topic0 = FixedBytes::<32>::from([0x78u8; 32]);
        let log_a = make_test_log(Some(tx_hash), Some(0), vec![topic0], vec![0xde, 0xad]);
        let log_b = make_test_log(Some(tx_hash), Some(1), vec![topic0], vec![0xde, 0xad]);

        let mut dedup = AppliedLogDedupCache::default();
        assert!(dedup.insert_log_if_new(&log_a, LogSource::XlayerFlashblock));
        assert!(dedup.insert_log_if_new(&log_b, LogSource::XlayerFlashblock));
    }

    #[test]
    fn applied_log_dedup_keeps_distinct_missing_log_index_events_in_same_tx() {
        let tx_hash = alloy::primitives::B256::from([0x44u8; 32]);
        let topic0 = FixedBytes::<32>::from([0x55u8; 32]);
        let log_a = make_test_log(
            Some(tx_hash),
            None,
            vec![topic0, FixedBytes::<32>::from([0x01u8; 32])],
            vec![0xaa],
        );
        let log_b = make_test_log(
            Some(tx_hash),
            None,
            vec![topic0, FixedBytes::<32>::from([0x02u8; 32])],
            vec![0xaa],
        );
        let mut dedup = AppliedLogDedupCache::default();

        assert!(dedup.insert_log_if_new(&log_a, LogSource::NewHeadsPull));
        assert!(dedup.insert_log_if_new(&log_b, LogSource::NewHeadsPull));
    }

    #[test]
    fn applied_log_dedup_drops_identical_missing_log_index_duplicates() {
        let tx_hash = alloy::primitives::B256::from([0x66u8; 32]);
        let topic0 = FixedBytes::<32>::from([0x77u8; 32]);
        let log = make_test_log(
            Some(tx_hash),
            None,
            vec![topic0, FixedBytes::<32>::from([0x03u8; 32])],
            vec![0xde, 0xad],
        );
        let mut dedup = AppliedLogDedupCache::default();

        assert!(dedup.insert_log_if_new(&log, LogSource::NewHeadsPull));
        assert!(!dedup.insert_log_if_new(&log, LogSource::NewHeadsPull));
    }

    #[test]
    fn get_mut_cow_keeps_pointer_when_uniquely_owned() {
        let pool_address = address!("1000000000000000000000000000000000000001");
        let mut state = StateSpace::default();
        state.insert_amm(make_v2_amm(pool_address, 10));

        let arc_before = state.get_shared(&pool_address).unwrap();
        assert_eq!(Arc::strong_count(arc_before), 1);
        let ptr_before = Arc::as_ptr(arc_before);

        let amm_mut = state.get_mut_cow(&pool_address).unwrap();
        amm_mut.set_last_synced_block(42);

        let arc_after = state.get_shared(&pool_address).unwrap();
        let ptr_after = Arc::as_ptr(arc_after);
        assert_eq!(ptr_before, ptr_after);
        assert_eq!(state.get(&pool_address).unwrap().last_synced_block(), 42);
    }

    #[test]
    fn get_mut_cow_clones_when_snapshot_holds_reference() {
        let pool_address = address!("2000000000000000000000000000000000000002");
        let mut state = StateSpace::default();
        state.insert_amm(make_v2_amm(pool_address, 15));

        let snapshot_arc = Arc::clone(state.get_shared(&pool_address).unwrap());
        let ptr_before = Arc::as_ptr(state.get_shared(&pool_address).unwrap());
        assert!(Arc::strong_count(state.get_shared(&pool_address).unwrap()) > 1);

        let amm_mut = state.get_mut_cow(&pool_address).unwrap();
        amm_mut.set_last_synced_block(77);

        let ptr_after = Arc::as_ptr(state.get_shared(&pool_address).unwrap());
        assert_ne!(ptr_before, ptr_after);
        assert_eq!(snapshot_arc.as_ref().last_synced_block(), 15);
        assert_eq!(state.get(&pool_address).unwrap().last_synced_block(), 77);
    }

    #[test]
    fn serializable_state_space_roundtrip_preserves_state() {
        let pool_address = address!("3000000000000000000000000000000000000003");
        let mut state = StateSpace::default();
        state.insert_amm(make_v2_amm(pool_address, 123));
        state.realtime_head.store(300, Ordering::Relaxed);
        state.canonical_head.store(290, Ordering::Relaxed);

        let serializable = SerializableStateSpace::from(state);
        let restored = StateSpace::from(serializable);

        assert_eq!(restored.state.len(), 1);
        assert_eq!(
            restored.get(&pool_address).unwrap().last_synced_block(),
            123
        );
        assert_eq!(restored.realtime_head.load(Ordering::Relaxed), 300);
        assert_eq!(restored.canonical_head.load(Ordering::Relaxed), 290);
    }
}

impl From<SerializableStateSpace> for StateSpace {
    fn from(val: SerializableStateSpace) -> Self {
        let inferred_head = val
            .state
            .values()
            .map(AutomatedMarketMaker::last_synced_block)
            .max()
            .unwrap_or_default();
        let realtime_head = if val.realtime_head == 0 {
            inferred_head
        } else {
            val.realtime_head
        };
        let canonical_head = if val.canonical_head == 0 {
            inferred_head.max(realtime_head)
        } else {
            val.canonical_head
        };
        let state = val
            .state
            .into_iter()
            .map(|(address, amm)| (address, Arc::new(amm)))
            .collect();
        StateSpace {
            state,
            realtime_head: Arc::new(AtomicU64::new(realtime_head)),
            canonical_head: Arc::new(AtomicU64::new(canonical_head)),
            chain_id: 0,
        }
    }
}

#[macro_export]
macro_rules! sync {
    // Sync factories with provider
    ($factories:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .sync()
            .await?
    }};

    // Sync factories with filters
    ($factories:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_filters($filters)
            .sync()
            .await?
    }};

    ($factories:expr, $amms:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_amms($amms)
            .with_filters($filters)
            .sync()
            .await?
    }};
}
