mod arbitrum_feed;
mod base_pending_logs;
mod bsc_logs_push;
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
pub mod titan_consumer;
pub mod titan_stream;
mod ws_logs;
mod xlayer_flashblocks;

use crate::amms::amm::AutomatedMarketMaker;
use crate::amms::amm::{SyncAction, AMM};
use crate::amms::caliber_prop::CaliberPropPool;
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

use alloy::primitives::{keccak256, Address, Bloom, BloomInput, FixedBytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{eth::Log, Filter, FilterSet};
use alloy::sol;
use alloy::sol_types::SolEvent;

use error::StateSpaceError;
use filters::AMMFilter;
use filters::PoolFilter;
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use maintenance::{PendingSyncAction, PendingSyncQueue, PendingSyncReason};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::{future::Future, marker::PhantomData, sync::Arc};
use titan_stream::TitanPammStreamConfig;
use tokio::sync::{Mutex, Notify, RwLock};

use crate::amms::caliber_prop::{decode_caliber_swap_log, CaliberSwapEvent, CALIBER_SWAP_EVENT};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};
use xlayer_flashblocks::{CaliberTxEvent, ElfomoTxEvent};

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
    /// BSC 主网实时同步：标准 `eth_subscribe("logs")` push 订阅 + canonical 对账兜底。
    ///
    /// BSC 无 flashblocks 端点（0.45s 原生块的 PoSA L1），也没有 sequencer feed。
    /// 实时通道用标准 geth `logs` 订阅推送已打包块日志；漏推由断线重连的
    /// getLogs 补拉 + drift 状态级对账兜底（Base 同款体系）。
    /// 要求 `StateSpaceBuilder::with_realtime_ws_endpoints(...)` 提供支持
    /// `eth_subscribe("logs")` 的 WSS 端点。
    BscMainnetLogsPush,
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
    titan_stream_config: Option<TitanPammStreamConfig>,
    hooks: HookRegistry<Vec<Address>>,
    phantom: PhantomData<N>,
}

const LOG_ADDRESS_CHUNK_SIZE: usize = 200;
const BASE_CHAIN_ID: u64 = 8453;
const BSC_MAINNET_CHAIN_ID: u64 = 56;
const ARBITRUM_CHAIN_ID: u64 = 42161;
const ETHEREUM_MAINNET_CHAIN_ID: u64 = 1;
const XLAYER_CHAIN_ID: u64 = 196;
const ROBINHOOD_CHAIN_ID: u64 = 4663;
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
/// 实时交易驱动下 caliber 周期对账默认间隔（断流/储备变动/漏更新的兜底延迟上界）。
///
/// 断流期间报价滞后上界由此间隔决定：30s 兼顾 RPC 限流（`eth_getStorageAt`
/// 批量刷新）与陈旧报价窗口；正常时 flashblocks 实时交易流保证新鲜度，
/// 对账仅作低频兜底。后续应改为配置化独立 HTTP RPC 端点后恢复更细粒度。
const DEFAULT_CALIBER_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogSource {
    RealtimeFlashblock,
    XlayerFlashblock,
    ArbitrumFeedPull,
    NewHeadsPull,
    BscLogsPush,
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

// FoT BuySell swapBack 自持余额启动快照用 balanceOf probe
// （owner = token 合约自身；仅 sync() 初始化时调用一次）。
sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    interface IERC20BalanceOfProbe {
        function balanceOf(address account) external view returns (uint256);
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

/// 将查询块序列化为 `eth_subscribe("logs"|"pendingLogs")` 的 filter JSON。
///
/// Base `pendingLogs` 与 BSC `logs` 订阅共用同一 filter 结构
/// （`address` + `topics[0]` OR 列表），保证 push 订阅与 canonical
/// getLogs 路径的覆盖完全一致。
pub(super) fn chunk_to_subscription_filter(chunk: &LogQueryChunk) -> Value {
    let addresses: Vec<String> = chunk
        .addresses
        .iter()
        .map(|addr| format!("{addr:?}"))
        .collect();

    match &chunk.mode {
        QueryMode::TopicFiltered(topics) => {
            let topic0: Vec<String> = topics.iter().map(|topic| format!("{topic:?}")).collect();
            json!({
                "address": addresses,
                "topics": [topic0],
            })
        }
        QueryMode::AddressOnly => {
            json!({
                "address": addresses,
            })
        }
    }
}

/// push 订阅本地预去重：多个 chunk 订阅可合法重叠（共享基础设施合约），
/// 重连后节点也可能重复投递；在进入全局 `AppliedLogDedupCache` 前先过滤。
#[derive(Default)]
pub(super) struct PendingLogDedupCache {
    seen: HashSet<AppliedLogKey>,
    order: VecDeque<AppliedLogKey>,
}

impl PendingLogDedupCache {
    pub(super) fn insert_if_new(&mut self, key: AppliedLogKey) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > 300_000 {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug)]
enum SelectedRealtimeSource {
    NewHeadsPull,
    BasePendingLogs,
    BscLogsPush,
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
            || (chain_id == BSC_MAINNET_CHAIN_ID
                && matches!(selected, SelectedRealtimeSource::BscLogsPush))
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

        // Titan pAMM 流消费（M4）：Ethereum 主网且启用时挂载。
        // 启用判定：
        // - 显式 `with_titan_pamm_stream(Some(config))` → 强制启用（用户给定参数）；
        // - 默认 None → 自动检测：state 中存在需要 Titan 实时流的 PropAMM 池
        //   （Fermi 等，见 `titan_consumer::pool_requires_titan_stream`）时，
        //   以 `TitanPammStreamConfig::default()`（eu 区域）自动启用；
        // 与 realtime 日志路径解耦：独立 slot 守卫/重连/校准，快照经
        // `apply_titan_snapshot` 应用到 Fermi pools 后触发 hooks 通知。
        let titan_config = match self.titan_stream_config.clone() {
            Some(config) => Some(config),
            None => {
                let has_titan_pamm = {
                    let state = self.state.read().await;
                    state
                        .state
                        .values()
                        .any(|amm| titan_consumer::pool_requires_titan_stream(amm.as_ref()))
                };
                if has_titan_pamm {
                    info!(
                        target: "state_space::titan_consumer",
                        "detected PropAMM pools requiring Titan stream, auto-enabling with default config"
                    );
                    Some(TitanPammStreamConfig::default())
                } else {
                    None
                }
            }
        };
        if chain_id == ETHEREUM_MAINNET_CHAIN_ID {
            if let Some(titan_config) = titan_config {
                let state = self.state.clone();
                let hooks = self.hooks.clone();
                let provider = self.provider.clone();
                info!(
                    target: "state_space::titan_consumer",
                    ws = %titan_config.ws_url,
                    rpc = %titan_config.rpc_url,
                    reconcile_s = titan_config.reconcile_interval.as_secs(),
                    "Titan pAMM stream consumer enabled"
                );
                tokio::spawn(async move {
                    titan_consumer::run_titan_pamm_stream_task::<N, P>(
                        titan_config,
                        state,
                        hooks,
                        provider,
                    )
                    .await;
                });
            }
        }

        Ok(())
    }

    /// Subscribes to AMM state changes through a configurable realtime source:
    /// - Base: `pendingLogs` on a Flashblocks-aware WebSocket endpoint by default.
    /// - BSC: standard `logs` subscription on an explicit WSS endpoint (push).
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
        self.build_realtime_stream().await
    }

    /// 自动启动完备的 realtime 状态同步（内部消费流），**不产出 affected pools 通知**。
    ///
    /// 用于"只维护状态、不触发下游检测"的场景（如 core 的 pending-only 模式）：
    /// 状态应用在流内部完成（含按链实时源 + `pending_sync_worker` /
    /// `silent_drift_probe` 等后台任务，见 `ensure_background_tasks`），
    /// 本方法只负责消费流，affected pools 仅用于日志，不转发下游。
    ///
    /// 流自带重连 loop，任务长期运行；返回 `JoinHandle` 供调用方监控
    /// （与外部消费路径的 `subscribe_with_meta` 互斥：二选一启动，不可同时消费）。
    /// 幂等：重复调用安全（`ensure_background_tasks` 内部 AtomicBool；流构造无副作用）。
    pub async fn spawn_realtime_state_sync(
        &self,
    ) -> Result<tokio::task::JoinHandle<()>, StateSpaceError>
    where
        P: Provider<N> + Clone + 'static,
        N: Network,
    {
        let stream = self.build_realtime_stream().await?;
        Ok(tokio::spawn(async move {
            let mut stream = stream;
            while let Some(item) = stream.next().await {
                match item {
                    Ok((meta, addrs)) => {
                        // 状态应用已在流内部完成；affected pools 仅用于日志，不转发下游
                        debug!(
                            seq = meta.seq,
                            block = meta.block_number,
                            affected = addrs.len(),
                            "realtime state sync (no downstream notification)"
                        );
                    }
                    Err(e) => {
                        debug!(error = ?e, "realtime state sync stream item error");
                    }
                }
            }
            warn!("realtime state sync stream ended");
        }))
    }

    /// 构造 realtime 状态流（不消费）：按链 resolve source + ensure_background_tasks
    /// + match 分发四种 subscribe_*_stream。
    ///
    /// `subscribe_with_meta`（外部消费，产出 affected pools 通知）与
    /// `spawn_realtime_state_sync`（内部消费，只维护状态）共用同一构造路径，
    /// 保证两种模式下实时同步覆盖与后台任务完全一致。
    async fn build_realtime_stream(
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
        let push_ws_candidates = if matches!(selected, SelectedRealtimeSource::BasePendingLogs)
            || matches!(selected, SelectedRealtimeSource::BscLogsPush)
        {
            Some(self.realtime_ws_endpoints.clone().ok_or_else(|| {
                StateSpaceError::from(AMMError::Msg(
                    "push realtime source (Base pendingLogs / BSC logs) requires explicit websocket endpoints that support `eth_subscribe` (`pendingLogs` / `logs`). Use StateSpaceBuilder::with_realtime_ws_endpoints(vec![\"wss://...\".into(), ...]).".to_string(),
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
                let ws_candidates = push_ws_candidates
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
            SelectedRealtimeSource::BscLogsPush => {
                let ws_candidates =
                    push_ws_candidates.expect("BscLogsPush selected must prevalidate ws endpoints");
                info!(
                    "Starting BSC logs push sync (chain_id={}, {} query chunks)",
                    chain_id,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_bsc_logs_push_stream(
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
                    "Starting Nitro feed + getLogs sync (chain_id={}, ws_url={}, {} query chunks)",
                    chain_id,
                    arbitrum_feed::feed_ws_url(chain_id),
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
                } else if chain_id == ARBITRUM_CHAIN_ID || chain_id == ROBINHOOD_CHAIN_ID {
                    SelectedRealtimeSource::ArbitrumFeedPull
                } else if chain_id == BSC_MAINNET_CHAIN_ID {
                    // BSC 实时同步与 Ethereum 主网一致走 NewHeadsPull：
                    // newHeads + 整块 get_logs 按块边界应用，避免 logs-push
                    // 同块多批推送产生的中间态幻影机会（P1 已砸/P2 未砸）。
                    SelectedRealtimeSource::NewHeadsPull
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
            RealtimeSyncSource::BscMainnetLogsPush => SelectedRealtimeSource::BscLogsPush,
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

        // FoT BuySell swapBack 自持余额事件驱动同步：
        // 从日志流中提取监控 token 的 ERC20 Transfer 事件，驱动累加器
        // （to==token → +=v 税收入；from==token → =0 swapBack dump 归零；
        // 水位 (block, txIndex, logIndex) 三元组防重放，同块多事件全部
        // 按序应用——块粒度水位会误丢同块第二条事件，67617009 取证修复）。
        // 过滤与提取统一走 fot 公开 API（与验证/回放脚本同源，避免手动
        // 复制逻辑漂移）。随后从 logs 移除——这些日志不属于任何池子 sync
        // 事件，避免污染 state.sync 的未知地址静默跳过路径。
        let mut fot_logs: Vec<alloy::rpc::types::Log> = Vec::new();
        logs.retain(|l| {
            if crate::amms::fot::is_swap_back_transfer_log(l) {
                fot_logs.push(l.clone());
                false
            } else {
                true
            }
        });
        for l in &fot_logs {
            crate::amms::fot::apply_swap_back_transfer_log(l, block_num);
        }

        // Caliber swap 事件断流回补（XLayer）：get_logs/backfill 拉回的
        // caliber swap 日志地址是合约地址而非池子地址，通用 sync() 路由
        // 会静默丢弃；在此显式提取并应用。仅对非实时来源生效：backfill/
        // canonical get_logs（NewHeadsPull）天然只含已确认交易日志；实时
        // XlayerFlashblock 路径的 caliber swap 已由预提取循环以 receipt
        // status==0x1 过滤处理，此处再提取会把 status!=0x1（回滚/未确认）
        // 日志当已确认应用，产生幻影消费（P0）。
        // 日志已在上方 applied_log_dedup 占位，跨路径不会重放。
        let mut caliber_swap_affected: Vec<Address> = Vec::new();
        if !matches!(source, LogSource::XlayerFlashblock) && !logs.is_empty() {
            let caliber_contracts: HashSet<Address> = {
                let guard = state.read().await;
                guard
                    .state
                    .values()
                    .filter_map(|amm| match amm.as_ref() {
                        AMM::CaliberPropPool(p) => Some(p.contract_address),
                        _ => None,
                    })
                    .collect()
            };
            if !caliber_contracts.is_empty() {
                let (caliber_swap_logs, other_logs): (Vec<Log>, Vec<Log>) =
                    logs.into_iter().partition(|l| {
                        caliber_contracts.contains(&l.inner.address)
                            && l.inner.data.topics().first() == Some(&CALIBER_SWAP_EVENT)
                    });
                logs = other_logs;
                let swaps: Vec<CaliberSwapEvent> = caliber_swap_logs
                    .iter()
                    .filter_map(|l| {
                        let topics = l.inner.data.topics();
                        let mut ev = decode_caliber_swap_log(topics, l.inner.data.data.as_ref())?;
                        ev.contract = l.inner.address;
                        ev.tx_index = l.transaction_index.unwrap_or_default();
                        Some(ev)
                    })
                    .collect();
                if !swaps.is_empty() {
                    match Self::apply_caliber_swaps_for_block(
                        state,
                        block_num,
                        swaps,
                        realtime_head,
                    )
                    .await
                    {
                        Ok(affected_swaps) => caliber_swap_affected.extend(affected_swaps),
                        Err(e) => {
                            error!(
                                "Caliber swap backfill apply failed at block {}: {}",
                                block_num, e
                            );
                        }
                    }
                }
            }
        }

        if logs.is_empty() {
            return Ok((
                caliber_swap_affected,
                ApplyLogsTiming {
                    sort_ms,
                    dedup_ms,
                    total_ms: t_apply_start.elapsed().as_millis(),
                    ..ApplyLogsTiming::default()
                },
            ));
        }

        let t_sync_start = Instant::now();
        let (mut affected, needs_resync, needs_async_update) = state.write().await.sync(&logs)?;
        affected.extend(caliber_swap_affected);
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

    /// 应用 Xlayer flashblocks 提取的 caliber 报价更新事件。
    ///
    /// caliber 更新交易不 emit 任何事件，只能从原始交易 calldata 提取；
    /// 本函数与 `apply_logs_for_block_timed` 同锁同序（同一 RwLock、
    /// 同一 realtime_head 推进语义）：
    /// - 提取侧已按 receipt status 过滤回滚/未确认交易（仅成功更新到达本函数）；
    /// - 块内按 `tx_index` 排序（EVM 语义：后者覆盖前者）；
    /// - `pairId + 合约地址 → virtual_address` 路由，命中本地池子后
    ///   `apply_batch_update` 增量刷新 field0/field1/deadline；
    /// - pairId 不在本地 / 块号落后于池子已同步块 → 静默跳过（对账兜底）。
    async fn apply_caliber_updates_for_block(
        state: &Arc<RwLock<StateSpace>>,
        block_num: u64,
        updates: Vec<CaliberTxEvent>,
        realtime_head: &Arc<AtomicU64>,
    ) -> Result<Vec<Address>, StateSpaceError> {
        // 与 apply_logs_for_block_timed 相同的 head 推进语义
        let mut prev = realtime_head.load(Ordering::Relaxed);
        while block_num > prev
            && realtime_head
                .compare_exchange(prev, block_num, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            prev = realtime_head.load(Ordering::Relaxed);
        }

        if updates.is_empty() {
            return Ok(vec![]);
        }

        let mut guard = state.write().await;
        Ok(guard.apply_caliber_updates(&updates, block_num))
    }

    /// 应用 Xlayer flashblocks 提取的 caliber swap 事件（日志驱动实时消费同步）。
    ///
    /// 与 `apply_caliber_updates_for_block` 同锁同序（同一 RwLock、同一
    /// realtime_head 推进语义）：
    /// - 提取侧已按 `receipt.status == 0x1` 过滤回滚/未确认交易；
    /// - 块内按 `tx_index` 排序（EVM 语义：后者覆盖前者）；
    /// - `pairId + 合约地址 → virtual_address` 路由，命中本地池子后
    ///   `apply_chain_swap` 增量更新储备 + pos（ladder 消费）；
    /// - pairId 不在本地 / 块号落后于池子已同步块 → 静默跳过（对账兜底）。
    async fn apply_caliber_swaps_for_block(
        state: &Arc<RwLock<StateSpace>>,
        block_num: u64,
        swaps: Vec<CaliberSwapEvent>,
        realtime_head: &Arc<AtomicU64>,
    ) -> Result<Vec<Address>, StateSpaceError> {
        let mut prev = realtime_head.load(Ordering::Relaxed);
        while block_num > prev
            && realtime_head
                .compare_exchange(prev, block_num, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            prev = realtime_head.load(Ordering::Relaxed);
        }

        if swaps.is_empty() {
            return Ok(vec![]);
        }

        let mut guard = state.write().await;
        Ok(guard.apply_caliber_swaps(&swaps, block_num))
    }

    /// 应用 Xlayer flashblocks 提取的 ElfomoFi `updatePrices` 原始交易
    /// （本地直算通道，零 RPC）。
    ///
    /// 与 `apply_caliber_updates_for_block` 同锁同序（同一 RwLock、同一
    /// realtime_head 推进语义）：
    /// - 提取侧已按 receipt status 过滤回滚/未确认交易（仅成功更新到达本函数）；
    /// - calldata 携带价格种子，`apply_price_seed` 按本地金库余额重算 orderbook；
    /// - 同块 ElfomoTrade 日志先于本步应用（金库已递减），更新用递减后的余额
    ///   重算，最终态与链上逐笔顺序一致（重算是 (seed, vault) 的纯函数）。
    async fn apply_elfomo_updates_for_block(
        state: &Arc<RwLock<StateSpace>>,
        block_num: u64,
        updates: Vec<ElfomoTxEvent>,
        realtime_head: &Arc<AtomicU64>,
    ) -> Result<Vec<Address>, StateSpaceError> {
        let mut prev = realtime_head.load(Ordering::Relaxed);
        while block_num > prev
            && realtime_head
                .compare_exchange(prev, block_num, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            prev = realtime_head.load(Ordering::Relaxed);
        }

        if updates.is_empty() {
            return Ok(vec![]);
        }

        let mut guard = state.write().await;
        Ok(guard.apply_elfomo_updates(&updates, block_num))
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

    /// BSC NewHeadsPull 实时路径专用：整块单查 + 本地过滤（仅 chain_id == 56）。
    ///
    /// 生产实测（us-east-1，两批 532 块，Chainstack WS，`--full-block` 探针）：
    ///   整块单查  med ~14ms / p90 ~26ms / max ~240ms
    ///   并发 chunk med ~14ms / p90 ~107ms / max ~370ms
    ///   串行 chunk med ~78ms / p90 ~330ms
    /// 整块单查尾部延迟显著更优、只发 1 个请求，无 chunk 表维护/并发挂起风险。
    /// 仅 BSC 实时单块路径使用；backfill 长区间仍走 chunked（需要 address
    /// 过滤控制数据量），其他链实时路径不变。
    async fn collect_logs_for_block_bsc_full(
        provider: &P,
        chunks: &[LogQueryChunk],
        block_num: u64,
        bloom: Option<&Bloom>,
    ) -> Result<Vec<Log>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        // 全局 bloom 预筛：与 chunked 路径「是否会查询任何 chunk」等价，
        // 0 命中块直接跳过，避免无谓拉取整块日志。
        if let Some(block_bloom) = bloom {
            let any_relevant = chunks
                .iter()
                .any(|chunk| Self::bloom_maybe_has_relevant_logs(block_bloom, chunk));
            if !any_relevant {
                return Ok(vec![]);
            }
        }

        let filter = Filter::new().from_block(block_num).to_block(block_num);
        let logs = provider
            .get_logs(&filter)
            .await
            .map_err(StateSpaceError::from)?;

        Ok(Self::filter_logs_for_chunks(logs, chunks))
    }

    /// 本地过滤：语义与 chunked 服务端过滤完全一致。
    ///
    /// - `TopicFiltered` chunk：地址命中且 topic0 ∈ 全局 topic 联合
    /// - `AddressOnly` chunk：仅地址命中（任意 topic，如 FoT swapBack /
    ///   Ekubo Log0 / Caliber）
    ///
    /// 与 `ranged_filter`/`chunk_to_subscription_filter` 覆盖相同，保证整块
    /// 单查送入 apply 的日志集合与原 chunked 路径一致——避免池子地址收到
    /// 非 sync 事件日志进入 `amm.sync` 触发误 resync。
    fn filter_logs_for_chunks(logs: Vec<Log>, chunks: &[LogQueryChunk]) -> Vec<Log> {
        let mut topic_addresses = HashSet::new();
        let mut topics = HashSet::new();
        let mut address_only = HashSet::new();
        for chunk in chunks {
            match &chunk.mode {
                QueryMode::TopicFiltered(t) => {
                    topic_addresses.extend(chunk.addresses.iter().copied());
                    topics.extend(t.iter().copied());
                }
                QueryMode::AddressOnly => {
                    address_only.extend(chunk.addresses.iter().copied());
                }
            }
        }

        logs.into_iter()
            .filter(|log| {
                let addr = log.address();
                address_only.contains(&addr)
                    || (topic_addresses.contains(&addr)
                        && log.topics().first().map_or(false, |t| topics.contains(t)))
            })
            .collect()
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
                AMM::BinaryFiPropPool(p) => {
                    if has_events {
                        topic_addresses.insert(p.pool_address);
                        topic_addresses.insert(p.engine_address);
                    }
                }
                AMM::CaliberPropPool(p) => {
                    // Caliber 无 sync_events（batchUpdateParameters 0 日志），
                    // 但 Swap 事件是断流 gap catch-up 唯一可回补的消费同步来源。
                    // 把合约地址 + CALIBER_SWAP_EVENT 注册进 query chunks，
                    // 让 get_logs/backfill 能拉回 caliber swap 日志；
                    // 实时提取层（xlayer_flashblocks）先占用 dedup 键，
                    // apply_logs_for_block_timed 内显式提取应用，不会重复处理。
                    topic_addresses.insert(p.contract_address);
                    topic_signatures.insert(CALIBER_SWAP_EVENT);
                }
                AMM::ElfomoFiPropPool(p) => {
                    // ElfomoTrade 由 Router emit、updatePrices 空事件由 Pool emit，
                    // 两者都必须注册（默认分支只注册 amm.address() = pool_address）。
                    if has_events {
                        topic_addresses.insert(p.pool_address);
                        topic_addresses.insert(p.router_address);
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

        // FoT BuySell swapBack 监控 token（self-hold balance 事件驱动同步）：
        // 独立 chunk 订阅 token 合约全部日志（AddressOnly，避免把 ERC20 Transfer
        // 签名污染全局 topic union）。RTX 等 token 合约只 emit Transfer，量小，
        // 对实时流/backfill 开销可忽略；不进 chunks 会被 Xlayer flashblocks
        // matcher 预筛丢弃（xlayer_flashblocks.rs），必须在此显式注册。
        let swapback_tokens = crate::amms::fot::swap_back_monitored_tokens();
        if !swapback_tokens.is_empty() {
            let mut swapback_tokens = swapback_tokens;
            swapback_tokens.sort();
            for addresses in swapback_tokens.chunks(LOG_ADDRESS_CHUNK_SIZE) {
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
            BSC_MAINNET_CHAIN_ID => 300,
            XLAYER_CHAIN_ID => 100,
            ROBINHOOD_CHAIN_ID => 100,
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
    /// Dedicated interval for Caliber propAMM Ladder refresh.
    pub caliber_ladder_sync_interval: Option<Duration>,
    /// 实时交易驱动开关：true（默认）时 caliber 报价更新由 Xlayer flashblocks
    /// 原始交易流驱动，周期任务降频为对账/兜底；false 时退回纯周期拉取。
    pub caliber_realtime_sync: bool,
    /// 实时模式下的 caliber 对账/兜底间隔（默认 45s）。
    pub caliber_reconcile_interval: Option<Duration>,
    /// Dedicated interval for BinaryFi propAMM full-snapshot re-anchor
    /// (cap/disabled states are not observable from events).
    pub binaryfi_sync_interval: Option<Duration>,
    /// Dedicated interval for ElfomoFi propAMM orderbook re-anchor
    /// (L1 updatePrices 事件断流时的最后兜底)。
    pub elfomo_sync_interval: Option<Duration>,
    pub pending_sync_worker_interval: Duration,
    pub drift_probe_interval: Duration,
    pub maintenance_interval: Option<Duration>,
    pub realtime_source: RealtimeSyncSource,
    pub realtime_ws_endpoints: Option<Vec<String>>,
    /// 初始化（init_batch）阶段可选的独立 HTTP RPC 端点。
    /// 配置后，state 同步的批量初始化改用 HTTP provider（多连接并发，突破 WS
    /// 单连接吞吐限制）；实时事件订阅仍使用传入的 WS provider。不配置则保持原行为。
    pub init_http_endpoint: Option<String>,
    /// Titan pAMM 流消费配置（M4；Ethereum 主网 + Fermi 时启用）。
    pub titan_stream_config: Option<TitanPammStreamConfig>,
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
            caliber_ladder_sync_interval: None,
            caliber_realtime_sync: true,
            caliber_reconcile_interval: None,
            binaryfi_sync_interval: None,
            elfomo_sync_interval: None,
            pending_sync_worker_interval: DEFAULT_PENDING_SYNC_WORKER_INTERVAL,
            drift_probe_interval: DEFAULT_DRIFT_PROBE_INTERVAL,
            maintenance_interval: None,
            realtime_source: RealtimeSyncSource::Auto,
            realtime_ws_endpoints: None,
            init_http_endpoint: None,
            titan_stream_config: None,
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

    /// Set a dedicated interval for Caliber propAMM Ladder refresh.
    ///
    /// 实时交易驱动关闭（`with_caliber_realtime_sync(false)`）时的周期拉取间隔；
    /// 未设置时回退到 `non_event_sync_interval`。实时模式开启时此配置被
    /// `caliber_reconcile_interval`（对账间隔）取代。
    pub fn with_caliber_ladder_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            caliber_ladder_sync_interval: Some(interval),
            ..self
        }
    }

    /// 启用/关闭 caliber 实时交易驱动同步（默认开启）。
    ///
    /// 开启：报价更新由 Xlayer flashblocks 原始交易流（`batchUpdateParameters`
    /// calldata）驱动，周期任务降频为对账/兜底；关闭：退回纯周期拉取，便于灰度。
    pub fn with_caliber_realtime_sync(mut self, enabled: bool) -> StateSpaceBuilder<N, P> {
        self.caliber_realtime_sync = enabled;
        self
    }

    /// 设置实时模式下的 caliber 对账/兜底间隔（默认 45s）。
    ///
    /// 对账任务负责冷启动、flashblocks 断流回填、储备/pos 低频变动与漏更新纠正；
    /// 对账间隔即断流期间报价滞后的上界。
    pub fn with_caliber_reconcile_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            caliber_reconcile_interval: Some(interval),
            ..self
        }
    }

    /// Set a dedicated interval for BinaryFi propAMM full-snapshot re-anchor.
    /// Falls back to `non_event_sync_interval` when not set.
    pub fn with_binaryfi_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            binaryfi_sync_interval: Some(interval),
            ..self
        }
    }

    /// Set a dedicated interval for ElfomoFi propAMM orderbook re-anchor.
    ///
    /// L1 事件（Pool `updatePrices` 空事件）为块级实时主通道；本任务仅作
    /// flashblocks 断流/重连/漏块时的最后兜底。未设置时回退到
    /// `non_event_sync_interval`。
    pub fn with_elfomo_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            elfomo_sync_interval: Some(interval),
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
    /// Required for push-subscription realtime sources that use their own
    /// direct WebSocket connection:
    /// - Base `pendingLogs` (`eth_subscribe` with `pendingLogs`);
    /// - BSC logs push (`eth_subscribe` with `logs`).
    ///
    /// It is safe for downstream applications to always call this builder
    /// method; other realtime paths currently ignore these endpoints.
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

    /// 指定初始化阶段使用的独立 HTTP RPC 端点（可选）。
    /// 见 [`StateSpaceBuilder::init_http_endpoint`] 说明。
    pub fn with_init_http_endpoint(mut self, url: String) -> StateSpaceBuilder<N, P> {
        self.init_http_endpoint = Some(url);
        self
    }

    /// 配置 Titan pAMM 流消费（M4，可选）。
    ///
    /// - `None`（默认）：**自动检测**——state 中存在需要 Titan 实时流的 PropAMM 池
    ///   （如 Fermi）时，以 `TitanPammStreamConfig::default()`（eu 区域端点）自动启用，
    ///   无需手动调用；
    /// - `Some(config)`：显式配置并强制启用（可定制区域/空闲超时/校准周期）。
    pub fn with_titan_pamm_stream(
        mut self,
        config: Option<TitanPammStreamConfig>,
    ) -> StateSpaceBuilder<N, P> {
        self.titan_stream_config = config;
        self
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

        // 初始化批量读取使用独立 provider：配置了 HTTP 端点则用 HTTP（多连接并发，
        // 突破 WS 单连接吞吐限制）；否则退回 WS provider，保持原行为。
        let init_provider: DynProvider<N> = match &self.init_http_endpoint {
            Some(url) => DynProvider::new(
                ProviderBuilder::<alloy::providers::Identity, alloy::providers::Identity, N>::default()
                    .network::<N>()
                    .connect_http(url.parse().map_err(|e| AMMError::Msg(format!(
                        "invalid init_http_endpoint: {e}"
                    )))?),
            ),
            None => DynProvider::new(self.provider.clone()),
        };

        // Sync remaining AMM variants in batches by variant
        for (variant, mut remaining_amms) in amm_variants.drain() {
            info!(target: "state_space::sync", variant = ?variant, count = remaining_amms.len(), "Syncing batch");
            let provider = init_provider.clone();

            // PancakeV3 初始化请求量巨大（spacing=1 池需全量扫 bitmap + tickdata，
            // 726 池一次打出去会把 RPC 单连接打爆：-32603 / -32000）。
            // 按小批初始化（HTTP 多连接并发 + 批间 sleep 节流）规避连接保护；
            // 批大小与 init_batch 内部 POOLS_STEP=30 对齐（每阶段恰 1 组），
            // 其余 variant 一次全量。
            let batch_size = if matches!(variant, crate::amms::amm::Variant::PancakeV3Pool) {
                30
            } else {
                remaining_amms.len()
            };

            while !remaining_amms.is_empty() {
                let take = batch_size.min(remaining_amms.len());
                let batch: Vec<AMM> = remaining_amms.drain(0..take).collect();
                // 所有 init_batch 必须与 realtime_head（=chain_tip_u64）在同一块快照：
                // 事件流初始 backfill 从 realtime_head+1 开始，若各批使用更新的
                // batch_block，会被 backfill 重复应用已包含的块（Mint 双计 /
                // Burn underflow → 启动期 Resync 风暴）。固定快照块后 backfill
                // [tip+1..head] 只应用 init 期间产生的新块，无重叠无缺口。
                let batch_block = chain_tip_u64;
                let synced = variant
                    .init_batch::<N, _>(batch, BlockId::from(batch_block), provider.clone())
                    .await?;

                // 仅做通用调度节流（HTTP 多连接 + in-flight 并发已控速，300ms 足够）；
                // 具体批量大小/并发策略由各 AMM init_batch 内部负责。
                sleep(Duration::from_millis(300)).await;

                for amm in synced {
                    let mut amm = amm;
                    amm.set_last_synced_block(chain_tip_u64);
                    state_space.insert_amm(amm);
                }
                info!(target: "state_space::sync", variant = ?variant, batch_done = take, remaining = remaining_amms.len(), "Batch initialized");
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

        // FoT BuySell token swapBack 自持余额：启动时一次链上快照（chain tip
        // 块末状态），此后完全由事件驱动增量同步（apply_logs_for_block_timed
        // 提取 token 合约 Transfer 事件：税收入 +=v、swapBack dump 全额转出
        // =0 天然强制对齐点）。事件流初始 backfill 从 realtime_head+1 =
        // chain_tip+1 开始，与快照块精确对齐 → 增量无重叠无缺口。已移除
        // 原 1s 轮询任务（见 sync_services 删除记录）。
        let fot_tokens = crate::amms::fot::swap_back_monitored_tokens();
        if !fot_tokens.is_empty() {
            for token in &fot_tokens {
                match IERC20BalanceOfProbe::new(*token, self.provider.clone())
                    .balanceOf(*token)
                    .call()
                    .block(chain_tip)
                    .await
                {
                    Ok(balance) => {
                        crate::amms::fot::init_swap_back_balance_snapshot(
                            *token,
                            balance,
                            chain_tip_u64,
                        );
                        info!(
                            target: "state_space::sync",
                            token = ?token,
                            balance = ?balance,
                            block = chain_tip_u64,
                            "Swap-back balance snapshot initialized (event-driven sync)"
                        );
                    }
                    Err(e) => {
                        // 失败保留无缓存：swap_back_balance 返回 0 → 模拟"不 dump"→
                        // 输出高估 → 若链上实际余额 >= threshold（dump 常态）可能误判
                        // profitable → revert 风险（与 67598082 事故同向）。
                        // 缓解：sync 成功时 RPC 已健康（失败概率低）+ 事件流首个
                        // Transfer 后即修正 + warn 可观测。真正保守方向（模拟"会
                        // dump"）需模拟层 stale 降级，超出本次范围。
                        warn!(
                            target: "state_space::sync",
                            token = ?token,
                            error = ?e,
                            "Failed to snapshot swap-back balance; swapBack simulated as inactive (revert-risk window until first Transfer event)"
                        );
                    }
                }
            }
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

        // Caliber propAMM pools: 实时交易驱动 + 低频对账/兜底。
        // 实时模式（默认）：报价更新由 flashblocks 原始交易流（batchUpdateParameters
        // calldata）驱动，周期任务只负责冷启动/断流回填/储备 pos 变动/漏更新纠正；
        // 关闭时退回纯周期拉取（legacy caliber_ladder_sync_interval 语义）。
        let caliber_interval = if self.caliber_realtime_sync {
            self.caliber_reconcile_interval
                .or(Some(DEFAULT_CALIBER_RECONCILE_INTERVAL))
        } else {
            self.caliber_ladder_sync_interval.or(non_event_interval)
        };
        if let Some(interval) = caliber_interval {
            tokio::spawn(sync_services::start_caliber_prop_ladder_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        // BinaryFi propAMM pools: periodic full-snapshot re-anchor
        // (maxIn/maxOut & disabled states are not observable from calldata/events)
        if let Some(interval) = self.binaryfi_sync_interval.or(non_event_interval) {
            tokio::spawn(sync_services::start_binaryfi_prop_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        // ElfomoFi propAMM pools: 周期 orderbook 快照（最后兜底）。
        // 主通道是 L1 事件 + L3 flashblocks raw-tx（每块 updatePrices 实时驱动），
        // 本任务只覆盖事件流断供场景。
        if let Some(interval) = self.elfomo_sync_interval.or(non_event_interval) {
            tokio::spawn(sync_services::start_elfomo_prop_sync_task(
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
            titan_stream_config: self.titan_stream_config,
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

    /// 解析 BinaryFi 事件（Swap / Update）命中的虚拟子池地址集合。
    ///
    /// 虚拟化后 StateSpace 中不再有真实地址 key：Swap 事件（topics[2]/topics[3] 为
    /// tokenIn/tokenOut）命中"暴露 pair 含任一交易资产"的虚拟子池；Update 事件
    /// （topics[1] 为全局 asset_idx）命中"暴露 pair 含该资产"的虚拟子池。
    /// 返回 None 表示非本 StateSpace 中 BinaryFi 部署的事件，交由其他分支解析。
    fn resolve_binaryfi_targets(
        &self,
        log_address: Address,
        topics: &[FixedBytes<32>],
    ) -> Option<Vec<Address>> {
        use crate::amms::binaryfi_prop::{
            BINARYFI_FEE_ACCOUNT_EVENT, BINARYFI_FEE_EVENT, BINARYFI_SWAP_EVENT,
            BINARYFI_UPDATE_EVENT,
        };

        let mut instances: Vec<(
            Address,
            Option<(usize, usize)>,
            Vec<Address>,
            Address,
            Address,
            Address,
        )> = Vec::new();
        let mut has_pool_match = false;
        let mut has_engine_match = false;
        for (key, amm) in self.state.iter() {
            if let AMM::BinaryFiPropPool(p) = amm.as_ref() {
                has_pool_match |= p.pool_address == log_address;
                has_engine_match |= p.engine_address == log_address;
                let assets = p.assets.iter().map(|t| t.address).collect::<Vec<_>>();
                instances.push((
                    *key,
                    p.exposed_pair,
                    assets,
                    p.pool_address,
                    p.engine_address,
                    p.fee_recipient(),
                ));
            }
        }

        match topics.first() {
            Some(topic) if *topic == BINARYFI_SWAP_EVENT && has_pool_match && topics.len() >= 4 => {
                let token_in = Address::from_word(topics[2]);
                let token_out = Address::from_word(topics[3]);
                let targets = instances
                    .into_iter()
                    .filter_map(|(key, exposed, assets, pool_addr, _, _)| {
                        // 只命中本部署（同一 pool 地址）的虚拟子池，避免多部署串扰
                        if pool_addr != log_address {
                            return None;
                        }
                        let Some((a, b)) = exposed else {
                            return None;
                        };
                        // 共享金库是部署级：Swap 只要涉及本实例任一资产（含 USDT0
                        // 这类全局共享资产），就必须路由到该实例更新金库账本
                        // （reserves/buy_ladder_remaining）。费率锚定仍限本 pair，
                        // 由 sync() L1 内层守卫保证，不在此过滤。
                        let touches = match (assets.get(a), assets.get(b)) {
                            (Some(ta), Some(tb)) => {
                                ta == &token_in
                                    || tb == &token_in
                                    || ta == &token_out
                                    || tb == &token_out
                            }
                            _ => false,
                        };
                        touches.then_some(key)
                    })
                    .collect();
                Some(targets)
            }
            Some(topic)
                if *topic == BINARYFI_UPDATE_EVENT && has_engine_match && topics.len() >= 2 =>
            {
                let asset_idx = U256::from_be_bytes(topics[1].0).to::<usize>();
                let targets = instances
                    .into_iter()
                    .filter_map(|(key, exposed, _, _, engine_addr, _)| {
                        // 只命中本部署（同一 engine 地址）的虚拟子池，避免多部署串扰
                        if engine_addr != log_address {
                            return None;
                        }
                        match exposed {
                            Some((a, b)) if a == asset_idx || b == asset_idx => Some(key),
                            _ => None,
                        }
                    })
                    .collect();
                Some(targets)
            }
            // 引擎 FeeUpdated 事件（topics.len()==1，data 前 32 字节 = 新 fee ppm）：
            // 全局费率变更，同一 engine 的全部虚拟子池一起更新（AsyncUpdate
            // 重拉快照时按各自 fee_recipient 校正 per-account 覆盖）。
            Some(topic) if *topic == BINARYFI_FEE_EVENT && has_engine_match => {
                let targets = instances
                    .into_iter()
                    .filter_map(|(key, _, _, _, engine_addr, _)| {
                        (engine_addr == log_address).then_some(key)
                    })
                    .collect();
                Some(targets)
            }
            // 引擎黑名单事件（BlacklistSet(account, status)，topic1 = 账户，
            // data = bool）：拉黑/解除拉黑都会发该事件，被拉黑账户 getFee=1e6、
            // quote 全 0。仅费率生效账户（fee_recipient，实际执行合约）==
            // 事件账户 的实例路由（触发 AsyncUpdate 重拉该账户口径快照）。
            Some(topic)
                if *topic == BINARYFI_FEE_ACCOUNT_EVENT
                    && has_engine_match
                    && topics.len() >= 2 =>
            {
                let account = Address::from_word(topics[1]);
                let targets = instances
                    .into_iter()
                    .filter_map(|(key, _, _, _, engine_addr, fee_recipient)| {
                        (engine_addr == log_address && fee_recipient == account).then_some(key)
                    })
                    .collect();
                Some(targets)
            }
            _ => None,
        }
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
            } else if log.topics().len() == 1 {
                // 单 topic 事件（如 BinaryFi FeeUpdated）：非池地址直命中，
                // 需单独尝试 BinaryFi engine 事件路由（Update/Swap 均在 >=2 分支）
                if let Some(binaryfi_targets) = self.resolve_binaryfi_targets(address, log.topics())
                {
                    target_addresses.extend(binaryfi_targets);
                }
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
                } else if let Some(binaryfi_targets) =
                    self.resolve_binaryfi_targets(address, log.topics())
                {
                    target_addresses.extend(binaryfi_targets);
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

    /// 应用 caliber 报价更新事件（XLayer flashblocks 原始交易驱动）。
    ///
    /// 与 `sync()` 同锁调用；块内按 `tx_index` 排序（EVM 语义：后者覆盖前者），
    /// `pairId + 合约地址 → virtual_address` 路由，命中本地池子后
    /// `apply_batch_update` 增量刷新 field0/field1/deadline。
    /// pairId 不在本地 / 块号落后于池子已同步块 → 静默跳过（对账兜底）。
    fn apply_caliber_updates(
        &mut self,
        updates: &[CaliberTxEvent],
        block_num: u64,
    ) -> Vec<Address> {
        if updates.is_empty() {
            return vec![];
        }

        let mut sorted: Vec<&CaliberTxEvent> = updates.iter().collect();
        sorted.sort_by_key(|u| u.tx_index);

        let mut affected_set = HashSet::new();
        for event in sorted {
            let virtual_address =
                CaliberPropPool::virtual_address_from_pair_id(event.update.pair_id, event.contract);
            let Some(amm) = self.get_mut_cow(&virtual_address) else {
                continue;
            };
            let AMM::CaliberPropPool(pool) = amm else {
                continue;
            };
            // 幂等保护：与 sync() 相同语义，禁止回卷池子状态
            if block_num < pool.last_synced_block() {
                continue;
            }
            pool.apply_batch_update(&event.update, block_num);
            affected_set.insert(virtual_address);
        }
        affected_set.into_iter().collect()
    }

    /// 应用 caliber swap 事件（XLayer flashblocks 日志驱动实时消费同步）。
    ///
    /// 与 `apply_caliber_updates` 同锁调用；块内按 `tx_index` 排序
    /// （EVM 语义：后者覆盖前者），`pairId + 合约地址 → virtual_address`
    /// 路由，命中本地池子后 `apply_chain_swap` 增量更新储备 + pos
    /// （ladder 消费）。pairId 不在本地 / 块号落后于池子已同步块 →
    /// 静默跳过（对账兜底）。
    fn apply_caliber_swaps(&mut self, swaps: &[CaliberSwapEvent], block_num: u64) -> Vec<Address> {
        if swaps.is_empty() {
            return vec![];
        }

        let mut sorted: Vec<&CaliberSwapEvent> = swaps.iter().collect();
        sorted.sort_by_key(|s| s.tx_index);

        let mut affected_set = HashSet::new();
        for event in sorted {
            let virtual_address =
                CaliberPropPool::virtual_address_from_pair_id(event.pair_id, event.contract);
            let Some(amm) = self.get_mut_cow(&virtual_address) else {
                continue;
            };
            let AMM::CaliberPropPool(pool) = amm else {
                continue;
            };
            // 幂等保护：与 sync() 相同语义，禁止回卷池子状态
            if block_num < pool.last_synced_block() {
                continue;
            }
            pool.apply_chain_swap(event, block_num);
            affected_set.insert(virtual_address);
        }
        affected_set.into_iter().collect()
    }

    /// 应用 ElfomoFi `updatePrices` 原始交易（本地直算通道，零 RPC）。
    ///
    /// 与 `apply_caliber_updates` 同锁调用；块内按 `tx_index` 排序，按
    /// `pool_address` 路由，命中本地池子后 `apply_price_seed` 用本地金库余额
    /// 重算整本 orderbook（读时纯函数，逐位一致）。池子不在本地 / 块号落后
    /// 于池子已同步块 → 静默跳过（对账兜底）。
    fn apply_elfomo_updates(&mut self, updates: &[ElfomoTxEvent], block_num: u64) -> Vec<Address> {
        if updates.is_empty() {
            return vec![];
        }

        let mut sorted: Vec<&ElfomoTxEvent> = updates.iter().collect();
        sorted.sort_by_key(|u| u.tx_index);

        let mut affected_set = HashSet::new();
        for event in sorted {
            let Some(amm) = self.get_mut_cow(&event.pool) else {
                continue;
            };
            let AMM::ElfomoFiPropPool(pool) = amm else {
                continue;
            };
            // 幂等保护：与 sync() 相同语义，禁止回卷池子状态
            if block_num < pool.last_synced_block() {
                continue;
            }
            pool.apply_price_seed(event.seed, block_num);
            affected_set.insert(event.pool);
        }
        affected_set.into_iter().collect()
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
    fn bsc_full_block_local_filter_matches_chunked_semantics() {
        let pool_a = address!("1111111111111111111111111111111111111111");
        let pool_b = address!("2222222222222222222222222222222222222222");
        let addr_only = address!("4444444444444444444444444444444444444444");
        let sync_topic = FixedBytes::<32>::from([0x01u8; 32]);
        let other_topic = FixedBytes::<32>::from([0x02u8; 32]);

        let chunks = vec![
            LogQueryChunk {
                addresses: vec![pool_a, pool_b],
                mode: QueryMode::TopicFiltered(vec![sync_topic]),
            },
            LogQueryChunk {
                addresses: vec![addr_only],
                mode: QueryMode::AddressOnly,
            },
        ];

        let make_log = |addr: Address, topic: FixedBytes<32>| Log {
            inner: alloy::primitives::Log {
                address: addr,
                data: LogData::new(vec![topic], Bytes::new()).unwrap(),
            },
            block_hash: None,
            block_number: Some(1),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        };

        let logs = vec![
            make_log(pool_a, sync_topic),
            make_log(pool_a, other_topic),
            make_log(pool_b, sync_topic),
            make_log(addr_only, other_topic),
            make_log(
                address!("5555555555555555555555555555555555555555"),
                sync_topic,
            ),
        ];

        let filtered = StateSpaceManager::<(), ()>::filter_logs_for_chunks(logs, &chunks);

        // pool_a+other_topic 必须被过滤（topic 不在全局联合，与 chunked 语义一致）
        assert_eq!(filtered.len(), 3);
        assert!(filtered.iter().all(|l| {
            l.address() == pool_a || l.address() == pool_b || l.address() == addr_only
        }));
        assert!(!filtered
            .iter()
            .any(|l| { l.address() == pool_a && l.topics().first() == Some(&other_topic) }));
        // AddressOnly 地址任意 topic 保留
        assert!(filtered
            .iter()
            .any(|l| l.address() == addr_only && l.topics().first() == Some(&other_topic)));
    }

    #[test]
    fn backfill_window_size_is_chain_specific() {
        assert_eq!(
            StateSpaceManager::<(), ()>::backfill_window_size(42161),
            200
        );
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(8453), 100);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(196), 100);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(56), 300);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(1), 50);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(10), 50);
    }

    #[test]
    fn bsc_realtime_source_resolves_to_new_heads_pull() {
        assert!(matches!(
            StateSpaceManager::<(), ()>::resolve_realtime_source(56, &RealtimeSyncSource::Auto),
            SelectedRealtimeSource::NewHeadsPull
        ));
        // 显式配置仍走旧 logs-push 路径（向后兼容，生产未使用）。
        assert!(matches!(
            StateSpaceManager::<(), ()>::resolve_realtime_source(
                56,
                &RealtimeSyncSource::BscMainnetLogsPush
            ),
            SelectedRealtimeSource::BscLogsPush
        ));
        assert!(matches!(
            StateSpaceManager::<(), ()>::resolve_realtime_source(1, &RealtimeSyncSource::Auto),
            SelectedRealtimeSource::NewHeadsPull
        ));
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
    fn binaryfi_fee_event_routes_to_all_pools_of_engine() {
        use crate::amms::binaryfi_prop::{
            BinaryFiPropPool, BINARYFI_FEE_EVENT, BINARYFI_UPDATE_EVENT,
        };

        let engine = BinaryFiPropPool::default().engine_address;
        let other_engine = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let pool1 = address!("1111111111111111111111111111111111111111");
        let pool2 = address!("2222222222222222222222222222222222222222");
        let pool_other = address!("3333333333333333333333333333333333333333");

        let mut state = StateSpace::default();
        let mut p1 = BinaryFiPropPool::default();
        p1.pool_address = pool1;
        p1.exposed_pair = Some((3, 0));
        let mut p2 = BinaryFiPropPool::default();
        p2.pool_address = pool2;
        p2.exposed_pair = Some((3, 0));
        let mut p3 = BinaryFiPropPool::default();
        p3.pool_address = pool_other;
        p3.engine_address = other_engine;
        state.insert_amm(AMM::BinaryFiPropPool(p1));
        state.insert_amm(AMM::BinaryFiPropPool(p2));
        state.insert_amm(AMM::BinaryFiPropPool(p3));

        // FeeUpdated（单 topic）→ 命中同一 engine 的全部子池（费率 per-account 共享）
        let mut routed = state
            .resolve_binaryfi_targets(engine, &[BINARYFI_FEE_EVENT])
            .expect("fee event must route");
        routed.sort_unstable();
        assert_eq!(routed, vec![pool1, pool2]);

        // 非 fee 单 topic 事件 → 不路由（交由其他分支）
        assert!(state
            .resolve_binaryfi_targets(engine, &[B256::ZERO])
            .is_none());
        // fee 事件来自另一 engine → 只路由到该 engine 的池子
        let mut routed_other = state
            .resolve_binaryfi_targets(other_engine, &[BINARYFI_FEE_EVENT])
            .expect("fee event must route for its own engine");
        routed_other.sort_unstable();
        assert_eq!(routed_other, vec![pool_other]);
        // 既有 Update 事件路由不受影响
        let asset_idx = B256::from(U256::from(3u64));
        let mut routed_update = state
            .resolve_binaryfi_targets(engine, &[BINARYFI_UPDATE_EVENT, asset_idx])
            .expect("update event must route");
        routed_update.sort_unstable();
        assert_eq!(routed_update, vec![pool1, pool2]);
    }

    #[test]
    fn binaryfi_swap_routes_to_instances_touching_shared_asset() {
        use crate::amms::binaryfi_prop::{BinaryFiPropPool, BINARYFI_SWAP_EVENT};
        use crate::amms::Token;

        let pool_addr = address!("1111111111111111111111111111111111111111");
        let v1 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let v2 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let usdt0 = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");
        let a = address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56");
        let b = address!("0x68fa48b1c2fe52b3d776e1953e0e782b5044ce28");
        let c = address!("0x535aabfaf9a6fbc1a04317b98885706dc6bf1650");

        let mk = |virt: Address, pair: (usize, usize)| {
            let mut p = BinaryFiPropPool::default();
            p.pool_address = pool_addr;
            p.virtual_address = virt;
            p.exposed_pair = Some(pair);
            p.assets = vec![
                Token::new_with_decimals(usdt0, 6),
                Token::new_with_decimals(a, 18),
                Token::new_with_decimals(b, 18),
                Token::new_with_decimals(c, 18),
            ];
            p
        };
        let mut state = StateSpace::default();
        state.insert_amm(AMM::BinaryFiPropPool(mk(v1, (0, 1))));
        state.insert_amm(AMM::BinaryFiPropPool(mk(v2, (0, 2))));

        let swap_topics = |tin: Address, tout: Address| {
            vec![
                BINARYFI_SWAP_EVENT,
                tin.into_word(),
                tin.into_word(),
                tout.into_word(),
            ]
        };

        // 含 USDT0（全局共享资产）的 swap → 命中全部实例（USDT0 在每个 pair 中）
        let mut routed = state
            .resolve_binaryfi_targets(pool_addr, &swap_topics(a, usdt0))
            .expect("swap must route");
        routed.sort_unstable();
        assert_eq!(routed, vec![v1, v2]);

        // 跨资产 swap（A→B）：A 在 v1 pair、B 在 v2 pair → 命中 2 实例
        let mut routed2 = state
            .resolve_binaryfi_targets(pool_addr, &swap_topics(a, b))
            .expect("swap must route");
        routed2.sort_unstable();
        assert_eq!(routed2, vec![v1, v2]);

        // 只触碰单一 pair 资产的 swap（A→C，C 不在任何 pair）→ 只命中 v1
        let mut routed3 = state
            .resolve_binaryfi_targets(pool_addr, &swap_topics(a, c))
            .expect("swap must route");
        routed3.sort_unstable();
        assert_eq!(routed3, vec![v1]);

        // 其他部署的 swap 事件不路由（pool 地址不同）
        let other_pool = address!("2222222222222222222222222222222222222222");
        assert!(state
            .resolve_binaryfi_targets(other_pool, &swap_topics(a, usdt0))
            .is_none());
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

    #[test]
    fn caliber_updates_route_apply_in_tx_order_and_skip_unknown() {
        use crate::amms::caliber_prop::{CaliberBatchUpdate, CaliberPropPool};
        use crate::amms::Token;

        let contract: Address = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
            .parse()
            .unwrap();
        let pair_id = B256::from([0x11u8; 32]);
        let virtual_address = CaliberPropPool::virtual_address_from_pair_id(pair_id, contract);

        let mut state = StateSpace::default();
        state.insert_amm(AMM::CaliberPropPool(CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address,
            token_x: Address::from([1u8; 20]),
            token_y: Address::from([2u8; 20]),
            token_a: Token::new_with_decimals(Address::from([1u8; 20]), 18),
            token_b: Token::new_with_decimals(Address::from([2u8; 20]), 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1_000),
            reserve_b: U256::from(1_000),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        }));

        let mk_event = |pair: B256, tx_index: u64, price: u64, flags: u32| CaliberTxEvent {
            contract,
            tx_index,
            update: CaliberBatchUpdate {
                pair_id: pair,
                price: U256::from(price),
                flags,
                deadline: 1_786_098_592,
            },
        };

        // 乱序传入：排序后 tx_index=0 先应用、tx_index=1 覆盖（EVM 语义）
        let updates = vec![mk_event(pair_id, 1, 100, 10), mk_event(pair_id, 0, 200, 20)];
        let affected = state.apply_caliber_updates(&updates, 67_329_558);
        assert_eq!(affected, vec![virtual_address]);
        let pool = match state.get(&virtual_address).unwrap() {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        };
        // tx_index=1（price=100）最后应用，覆盖 tx_index=0（price=200）
        assert_eq!(pool.ladder.field0, U256::from(100u64));
        assert_eq!(pool.ladder.field1, U256::from(10u64));
        assert_eq!(pool.last_synced_block, 67_329_558);

        // 未知 pairId → 路由无池子，静默跳过
        let unknown = mk_event(B256::from([0x22u8; 32]), 2, 999, 1);
        let affected = state.apply_caliber_updates(&[unknown], 67_329_559);
        assert!(affected.is_empty());
        let pool = match state.get(&virtual_address).unwrap() {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        };
        assert_eq!(pool.ladder.field0, U256::from(100u64));

        // 块号落后于池子已同步块 → 跳过（幂等保护）
        let stale = mk_event(pair_id, 3, 300, 30);
        let affected = state.apply_caliber_updates(&[stale], 67_329_557);
        assert!(affected.is_empty());
    }

    #[test]
    fn caliber_swaps_route_apply_in_tx_order_and_skip_unknown() {
        use crate::amms::caliber_prop::{CaliberPropPool, CaliberSwapEvent};
        use crate::amms::Token;

        let contract: Address = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
            .parse()
            .unwrap();
        let pair_id = B256::from([0x11u8; 32]);
        let virtual_address = CaliberPropPool::virtual_address_from_pair_id(pair_id, contract);
        let token_x = Address::from([0x01u8; 20]); // token_a（地址较小）
        let token_y = Address::from([0x02u8; 20]); // token_b

        let mut state = StateSpace::default();
        state.insert_amm(AMM::CaliberPropPool(CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address,
            token_x,
            token_y,
            token_a: Token::new_with_decimals(token_x, 18),
            token_b: Token::new_with_decimals(token_y, 6),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1_000_000),
            reserve_b: U256::from(2_000_000),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        }));

        let mk_swap =
            |pair: B256, tx_index: u64, tin: Address, tout: Address, ain: u64, aout: u64| {
                CaliberSwapEvent {
                    contract,
                    tx_index,
                    pair_id: pair,
                    token_in: tin,
                    token_out: tout,
                    amount_in: U256::from(ain),
                    amount_out: U256::from(aout),
                }
            };

        // 乱序传入两笔正向 swap（x→y）：排序后按 tx_index 应用，pos_forward 累计
        let swaps = vec![
            mk_swap(pair_id, 2, token_x, token_y, 10, 5),
            mk_swap(pair_id, 0, token_x, token_y, 100, 30),
        ];
        let affected = state.apply_caliber_swaps(&swaps, 67_329_558);
        assert_eq!(affected, vec![virtual_address]);
        let pool = match state.get(&virtual_address).unwrap() {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        };
        // 正向 x→y：token_x==token_a 为输入 → reserve_a += in、reserve_b -= out
        // tx0: +100 / -30；tx2: +10 / -5
        assert_eq!(pool.reserve_a, U256::from(1_000_000u64 + 100 + 10));
        assert_eq!(pool.reserve_b, U256::from(2_000_000u64 - 30 - 5));
        assert_eq!(pool.ladder.pos_forward, U256::from(35u64));
        assert_eq!(pool.ladder.pos_reverse, U256::ZERO);
        assert_eq!(pool.last_synced_block, 67_329_558);

        // 反向 swap（y→x）：pos_reverse 累计"扣费后的输入"（fee_rate=0 时为
        // 全额 amountIn，链上 cfg+7 mid96 语义）、pos_forward 归零
        let rev = mk_swap(pair_id, 3, token_y, token_x, 7, 4);
        let affected = state.apply_caliber_swaps(&[rev], 67_329_558);
        assert_eq!(affected, vec![virtual_address]);
        let pool = match state.get(&virtual_address).unwrap() {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        };
        // 反向 y→x：token_y==token_b 为输入 → reserve_b += in、reserve_a -= out
        assert_eq!(pool.reserve_a, U256::from(1_000_000u64 + 100 + 10 - 4));
        assert_eq!(pool.reserve_b, U256::from(2_000_000u64 - 30 - 5 + 7));
        assert_eq!(pool.ladder.pos_reverse, U256::from(7u64));
        assert_eq!(pool.ladder.pos_forward, U256::ZERO);

        // 未知 pairId → 静默跳过
        let unknown = mk_swap(B256::from([0x22u8; 32]), 4, token_x, token_y, 1, 1);
        let affected = state.apply_caliber_swaps(&[unknown], 67_329_559);
        assert!(affected.is_empty());

        // 块号落后于池子已同步块 → 跳过（幂等保护）
        let stale = mk_swap(pair_id, 5, token_x, token_y, 1, 1);
        let affected = state.apply_caliber_swaps(&[stale], 67_329_557);
        assert!(affected.is_empty());
        let pool = match state.get(&virtual_address).unwrap() {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        };
        assert_eq!(pool.ladder.pos_forward, U256::ZERO);
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
