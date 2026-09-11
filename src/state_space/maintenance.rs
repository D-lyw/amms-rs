use super::{StateSpace, StateSpaceManager};
use crate::amms::aerodrome_slipstream::pool::GetAerodromeSlipstreamProbeBatchRequest;
use crate::amms::amm::{AutomatedMarketMaker, SyncAction, Variant, AMM};
use crate::amms::curve_ng::{
    CurveNGFactory, CurveNGPool, GetCurveNGTriCryptoRuntimeDataBatchRequest,
    GetCurveNGTwoCryptoRuntimeDataBatchRequest, TriCryptoRuntimeData, TwoCryptoRuntimeData,
};
use crate::amms::error::AMMError;
use crate::amms::pancake_v3::GetPancakeV3PoolSlot0BatchRequest;
use crate::amms::uniswap_v2::GetV2LikeReservesProbeBatchRequest;
use crate::amms::uniswap_v3::GetUniswapV3PoolSlot0BatchRequest;
use crate::amms::uniswap_v4::GetV4LitePoolStateBatchRequest;
use crate::state_space::error::StateSpaceError;
use alloy::eips::BlockId;
use alloy::network::Network;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolValue;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

pub(super) const DRIFT_HOT_POOL_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const DRIFT_COLD_POOL_INTERVAL: Duration = Duration::from_secs(300);
pub(super) const DRIFT_MAX_POOLS_PER_TICK: usize = 100;
pub(super) const DRIFT_HOT_WINDOW_BLOCKS: u64 = 60;
pub(super) const MAINT_COVERAGE_BATCH_SIZE: usize = 80;
const DRIFT_V3_SLOT0_BATCH_STEP: usize = DRIFT_MAX_POOLS_PER_TICK;
const DRIFT_SLIPSTREAM_PROBE_BATCH_STEP: usize = DRIFT_MAX_POOLS_PER_TICK;
const DRIFT_CANDIDATE_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClProbeSnapshot {
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
    // Only Slipstream currently needs dynamic fee drift checks.
    fee: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CurveNGStableProbeSnapshot {
    balances: Vec<U256>,
    admin_balances: Vec<U256>,
    rates: Option<Vec<U256>>,
    /// Per-rate asset type (0=Standard, 1=Oracle, 2=Rebasing, 3=ERC4626).
    /// Only populated when rates is Some. None means asset type unknown → compare all rates.
    rates_asset_types: Option<Vec<u8>>,
    amp: Option<U256>,
    fee: U256,
    admin_fee: U256,
    offpeg_fee_multiplier: U256,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CurveNGCryptoProbeSnapshot {
    balances: Vec<U256>,
    price_scale: Vec<U256>,
    d: Option<U256>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct V2LikeProbeSnapshot {
    reserve_0: u128,
    reserve_1: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct V4LiteProbeSnapshot {
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriftProbeKind {
    V2Like,
    V3Like,
    V4Like,
    Slipstream,
    CurveNGStable,
    CurveNGCrypto,
}

impl DriftProbeKind {
    fn next(self) -> Self {
        match self {
            DriftProbeKind::V2Like => DriftProbeKind::V3Like,
            DriftProbeKind::V3Like => DriftProbeKind::V4Like,
            DriftProbeKind::V4Like => DriftProbeKind::Slipstream,
            DriftProbeKind::Slipstream => DriftProbeKind::CurveNGStable,
            DriftProbeKind::CurveNGStable => DriftProbeKind::CurveNGCrypto,
            DriftProbeKind::CurveNGCrypto => DriftProbeKind::V2Like,
        }
    }
}

// Drift classification for CurveNG StableSwap pools.
// Balances drift → Resync (event-driven, mismatch indicates real state error).
// Admin balances/rates/amp drift → AsyncUpdate (runtime refresh is sufficient).
// ERC4626 rates (type 3) are skipped — yield accrues every block via convertToAssets(),
// comparison always produces false positives. Handled by rate sync task instead.
fn classify_curve_ng_stable_drift(
    local: &CurveNGStableProbeSnapshot,
    remote: &CurveNGStableProbeSnapshot,
) -> Option<PendingSyncAction> {
    if local == remote {
        return None;
    }
    if local.balances != remote.balances {
        return Some(PendingSyncAction::Resync);
    }
    if local.admin_balances != remote.admin_balances {
        return Some(PendingSyncAction::AsyncUpdate);
    }
    if local.fee != remote.fee
        || local.admin_fee != remote.admin_fee
        || local.offpeg_fee_multiplier != remote.offpeg_fee_multiplier
    {
        return Some(PendingSyncAction::AsyncUpdate);
    }
    // Non-event-driven fields: silent drift is normal, lightweight refresh is sufficient.
    // rates: accrues via interest (rebasing tokens like stETH/weETH)
    // Skip rates comparison for ERC4626 tokens (type 3) — their rates change every block
    // via convertToAssets() as yield accrues, making comparison always produce false positives.
    if let (Some(local_rates), Some(remote_rates)) = (&local.rates, &remote.rates) {
        let asset_types = local.rates_asset_types.as_deref();
        let all_match =
            local_rates
                .iter()
                .zip(remote_rates.iter())
                .enumerate()
                .all(|(i, (l, r))| {
                    // Skip comparison for ERC4626 tokens (type 3)
                    if asset_types.map(|at| at.get(i) == Some(&3)).unwrap_or(false) {
                        return true;
                    }
                    l == r
                });
        if !all_match {
            return Some(PendingSyncAction::AsyncUpdate);
        }
    }
    // amp: changes every block during a RampA period without per-block events
    if let (Some(local_amp), Some(remote_amp)) = (local.amp, remote.amp) {
        if local_amp != remote_amp {
            return Some(PendingSyncAction::AsyncUpdate);
        }
    }
    None
}

// Drift classification for CurveNG CryptoSwap pools (TwoCrypto/TriCrypto).
// No ERC4626/Oracle complexity — CryptoSwap uses price_scale (embedded in events)
// instead of rates. Balances, price_scale and D all drift per-swap without events,
// so any field mismatch triggers AsyncUpdate (cheap multicall refresh).
fn classify_curve_ng_crypto_drift(
    local: &CurveNGCryptoProbeSnapshot,
    remote: &CurveNGCryptoProbeSnapshot,
) -> Option<PendingSyncAction> {
    if local == remote {
        return None;
    }
    if local.balances != remote.balances {
        return Some(PendingSyncAction::Resync);
    }
    // price_scale or D changes are non-event-driven (per-swap updates), lightweight refresh.
    if local.price_scale != remote.price_scale || local.d != remote.d {
        return Some(PendingSyncAction::AsyncUpdate);
    }
    None
}

fn should_skip_async_apply(
    existing_last_synced_block: u64,
    snapshot_last_synced_block: u64,
) -> bool {
    existing_last_synced_block > snapshot_last_synced_block
}

/// Resync 的块级水位闸门是否必须跳过（改走「锁外 fetch → 锁内 merge」）。
///
/// 判据：该池型的 `last_synced_block` 是否被**每块必发的 raw-tx** 顶到
/// flashblock 乐观头。这类池型的通用闸门 `last_synced_block() > target_block`
/// 在健康期恒真——Resync 的 `required_block` 取自请求时的 canonical head，
/// 落后乐观头 ≥1 块——于是任务被 `postpone` 无限后移、纠错与覆盖对账永远
/// 排不上（原则文档 §3 规则 2 的典型反模式，2026-09-11 事故）。
///
/// - **Caliber**：`apply_batch_update`（`batchUpdateParameters`）/`apply_chain_swap`
///   每块推进水位。
/// - **Elfomo**：`updatePrices` 每块推进水位。
/// - 其余池型闸门保留：BinaryFi 的水位只由快照 `apply_snapshot` 推进，
///   不会高于它自己的快照目标块。
fn resync_skips_block_watermark_gate(amm: &AMM) -> bool {
    matches!(amm, AMM::CaliberPropPool(_) | AMM::ElfomoFiPropPool(_))
}

/// AsyncUpdate 快照写回前是否因"本地已更新"竞态丢弃。
///
/// - `AMM::BinaryFiPropPool` 不放宽：其 AsyncUpdate 快照携带事件流无法提供的
///   quote/bid/容量/费率（L2 update 日志只有 price/ladder/点差），而该链每块
///   都有 MM update 交易，`last_synced_block` 恒新；若按 last_synced_block
///   竞态丢弃，恢复通道会完全失效（快照永远被 SkippedStale 丢弃）。价格回退
///   已由 `apply_snapshot` 内部 `price_updated_block >= snap_block 不覆盖`
///   保鲜判断兜底，容量以链上 quote 观测为权威（与周期 re-anchor 同语义）。
/// - 其余 AMM 保持原语义：existing 已新于快照克隆块 → 快照视为过期。
fn should_skip_async_apply_for(
    amm: &AMM,
    existing_last_synced_block: u64,
    snapshot_last_synced_block: u64,
) -> bool {
    if matches!(amm, AMM::BinaryFiPropPool(_)) {
        return false;
    }
    should_skip_async_apply(existing_last_synced_block, snapshot_last_synced_block)
}

/// 把 BinaryFi 快照合并进**写锁内当时的** existing 实例（绝不整只替换）。
///
/// - 水位 `max(prev, snap_block)`：快照落地不得回卷本地已推进的块号，否则后续旧块
///   日志会被重新应用（重复消费容量/双计）。
/// - `clear_stale_pairs` 只清快照实际覆盖的 pair：否则 RPC 窗口内新标记的 stale 会
///   一直残留，之后每次 AsyncUpdate 都退化成全量重拉（RPC 放大）。
fn merge_binaryfi_snapshot(
    existing: &mut crate::amms::binaryfi_prop::BinaryFiPropPool,
    snap: &crate::amms::binaryfi_prop::Snapshot,
    snap_block: u64,
    refreshed_pairs: &[usize],
) {
    let prev_last_synced = existing.last_synced_block();
    let mut refreshed = existing.clone();
    refreshed.apply_snapshot(snap, snap_block);
    refreshed.clear_stale_pairs(refreshed_pairs);
    refreshed.set_last_synced_block(prev_last_synced.max(snap_block));
    *existing = refreshed;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum PendingSyncAction {
    AsyncUpdate,
    Resync,
}

impl PendingSyncAction {
    fn priority(self) -> u8 {
        match self {
            PendingSyncAction::AsyncUpdate => 1,
            PendingSyncAction::Resync => 2,
        }
    }
}

impl From<SyncAction> for PendingSyncAction {
    fn from(value: SyncAction) -> Self {
        match value {
            SyncAction::Resync => PendingSyncAction::Resync,
            SyncAction::AsyncUpdate | SyncAction::None => PendingSyncAction::AsyncUpdate,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum PendingSyncReason {
    AsyncUpdate,
    Resync,
    SyncError,
    DriftProbe,
    MaintenanceCoverage,
}

impl PendingSyncReason {
    fn priority(self) -> u8 {
        match self {
            PendingSyncReason::MaintenanceCoverage => 0,
            PendingSyncReason::AsyncUpdate | PendingSyncReason::Resync => 1,
            PendingSyncReason::SyncError | PendingSyncReason::DriftProbe => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct PendingSyncTask {
    pub(super) action: PendingSyncAction,
    pub(super) required_block: u64,
    pub(super) reason: PendingSyncReason,
    pub(super) retry_count: u32,
    pub(super) next_retry_at: Instant,
    pub(super) first_seen_at: Instant,
}

#[derive(Default)]
struct PendingSyncAddressQueue {
    tasks: VecDeque<PendingSyncTask>,
}

#[derive(Default)]
pub(super) struct PendingSyncQueue {
    tasks: HashMap<Address, PendingSyncAddressQueue>,
    in_flight: HashSet<Address>,
}

enum PendingExecutionOutcome {
    Applied,
    SkippedStale,
    DeferredStale(u64),
    /// 目标块超前于存储 RPC 头部，本次纠错未覆盖目标块：任务保持排队，
    /// 以相同 required_block 立即重试，直到 RPC 收录目标块（不假装成功）。
    RetryLater(u64),
    MissingPool,
}

impl PendingSyncQueue {
    fn merge_task(
        existing: &mut PendingSyncTask,
        action: PendingSyncAction,
        reason: PendingSyncReason,
    ) {
        if action.priority() > existing.action.priority() {
            existing.action = action;
        }
        if reason.priority() >= existing.reason.priority() {
            existing.reason = reason;
        }
    }

    fn new_task(
        action: PendingSyncAction,
        required_block: u64,
        reason: PendingSyncReason,
        now: Instant,
    ) -> PendingSyncTask {
        PendingSyncTask {
            action,
            required_block,
            reason,
            retry_count: 0,
            next_retry_at: now,
            first_seen_at: now,
        }
    }

    pub(super) fn enqueue(
        &mut self,
        address: Address,
        action: PendingSyncAction,
        required_block: u64,
        reason: PendingSyncReason,
    ) {
        let now = Instant::now();
        let in_flight = self.in_flight.contains(&address);
        match self.tasks.get_mut(&address) {
            Some(queue) => {
                let start_idx = usize::from(in_flight);
                if let Some(existing) = queue
                    .tasks
                    .iter_mut()
                    .skip(start_idx)
                    .find(|task| task.required_block == required_block)
                {
                    Self::merge_task(existing, action, reason);
                    return;
                }

                let insert_at = queue
                    .tasks
                    .iter()
                    .enumerate()
                    .skip(start_idx)
                    .find_map(|(idx, task)| (required_block < task.required_block).then_some(idx))
                    .unwrap_or(queue.tasks.len());

                queue.tasks.insert(
                    insert_at,
                    Self::new_task(action, required_block, reason, now),
                );
            }
            None => {
                let mut queue = PendingSyncAddressQueue::default();
                queue
                    .tasks
                    .push_back(Self::new_task(action, required_block, reason, now));
                self.tasks.insert(address, queue);
            }
        }
    }

    fn claim_due_filtered<F>(
        &mut self,
        canonical_head: u64,
        max_items: usize,
        mut filter: F,
    ) -> Vec<(Address, PendingSyncTask)>
    where
        F: FnMut(&PendingSyncTask) -> bool,
    {
        let now = Instant::now();
        let mut due: Vec<(Address, PendingSyncTask)> = self
            .tasks
            .iter()
            .filter_map(|(addr, queue)| {
                let task = queue.tasks.front()?;
                (!self.in_flight.iter().any(|in_flight| *in_flight == *addr)
                    && task.required_block <= canonical_head
                    && task.next_retry_at <= now
                    && filter(task))
                .then_some((*addr, task.clone()))
            })
            .collect();

        due.sort_by_key(|(_, task)| {
            (
                Reverse(task.action.priority()),
                task.first_seen_at,
                Reverse(task.required_block),
            )
        });
        due.truncate(max_items);
        for (addr, _) in &due {
            self.in_flight.insert(*addr);
        }

        due
    }

    pub(super) fn claim_due_non_coverage(
        &mut self,
        canonical_head: u64,
        max_items: usize,
    ) -> Vec<(Address, PendingSyncTask)> {
        self.claim_due_filtered(canonical_head, max_items, |task| {
            task.reason != PendingSyncReason::MaintenanceCoverage
        })
    }

    pub(super) fn claim_due_coverage(
        &mut self,
        canonical_head: u64,
        max_items: usize,
    ) -> Vec<(Address, PendingSyncTask)> {
        self.claim_due_filtered(canonical_head, max_items, |task| {
            task.reason == PendingSyncReason::MaintenanceCoverage
        })
    }

    pub(super) fn complete_success(&mut self, address: Address, executed_block: u64) {
        self.in_flight.remove(&address);
        let mut should_remove = false;
        if let Some(queue) = self.tasks.get_mut(&address) {
            if let Some(front) = queue.tasks.front() {
                if front.required_block > executed_block {
                    if let Some(front_mut) = queue.tasks.front_mut() {
                        front_mut.retry_count = 0;
                        front_mut.next_retry_at = Instant::now();
                    }
                    return;
                }
            }
            queue.tasks.pop_front();
            should_remove = queue.tasks.is_empty();
        }
        if should_remove {
            self.tasks.remove(&address);
        }
    }

    pub(super) fn drop_task(&mut self, address: Address) {
        self.in_flight.remove(&address);
        self.tasks.remove(&address);
    }

    pub(super) fn on_failure(&mut self, address: Address, maybe_next_required_block: Option<u64>) {
        self.in_flight.remove(&address);
        if let Some(task) = self
            .tasks
            .get_mut(&address)
            .and_then(|queue| queue.tasks.front_mut())
        {
            task.retry_count = task.retry_count.saturating_add(1);
            let exp = task.retry_count.min(5);
            let delay_ms = (200u64).saturating_mul(2u64.saturating_pow(exp));
            task.next_retry_at = Instant::now() + Duration::from_millis(delay_ms.min(10_000));
            if let Some(required) = maybe_next_required_block {
                if required == task.required_block {
                    task.next_retry_at = Instant::now();
                }
            }
        }
    }

    pub(super) fn defer_task(&mut self, address: Address, required_block: u64) {
        self.in_flight.remove(&address);
        let mut enqueue_follow_up = None;
        if let Some(queue) = self.tasks.get_mut(&address) {
            if let Some(task) = queue.tasks.front_mut() {
                if task.required_block == required_block {
                    task.next_retry_at = Instant::now();
                    return;
                }
                enqueue_follow_up = Some((task.action, task.reason));
            }
        }
        if let Some((action, reason)) = enqueue_follow_up {
            self.enqueue(address, action, required_block, reason);
        }
    }

    /// 把队首任务推迟到更晚的 `required_block`（本地状态已新于目标块时使用）。
    /// 与 `defer_task` 不同：移除旧队首并以新 `required_block` 重新入队，
    /// 避免 canonical 落后期间同一任务被反复认领、反复执行链上读取。
    pub(super) fn postpone(
        &mut self,
        address: Address,
        action: PendingSyncAction,
        reason: PendingSyncReason,
        required_block: u64,
    ) {
        self.in_flight.remove(&address);
        let same_block = self
            .tasks
            .get(&address)
            .and_then(|queue| queue.tasks.front())
            .map(|task| task.required_block == required_block)
            .unwrap_or(false);
        if same_block {
            if let Some(task) = self
                .tasks
                .get_mut(&address)
                .and_then(|queue| queue.tasks.front_mut())
            {
                task.next_retry_at = Instant::now();
                task.retry_count = 0;
            }
            return;
        }
        if let Some(queue) = self.tasks.get_mut(&address) {
            queue.tasks.pop_front();
        }
        self.enqueue(address, action, required_block, reason);
    }

    /// `BlockNotAvailable` 重试：保持 `required_block` 不变，等存储 RPC 收录
    /// 目标块。与 `postpone` 不同，不立即重试——RPC 落后窗口内（通常 ≤1 块、
    /// <2s 追平）立即重试只会空转热循环；用 250ms→2s 短退避，既及时重试又
    /// 不刷 RPC、不空转。
    pub(super) fn retry_later(&mut self, address: Address, required_block: u64) {
        self.in_flight.remove(&address);
        let Some(queue) = self.tasks.get_mut(&address) else {
            return;
        };
        let Some(task) = queue.tasks.front_mut() else {
            return;
        };
        if task.required_block == required_block {
            task.retry_count = task.retry_count.saturating_add(1);
            let exp = task.retry_count.min(3);
            let delay_ms = 250u64.saturating_mul(1u64 << exp).min(2_000);
            task.next_retry_at = Instant::now() + Duration::from_millis(delay_ms);
        } else {
            // 队首任务目标块已变化（如新鲜度守卫推迟到更晚块）：旧任务按新块重排。
            let (action, reason) = (task.action, task.reason);
            queue.tasks.pop_front();
            self.enqueue(address, action, required_block, reason);
        }
    }
}

impl<N, P> StateSpaceManager<N, P> {
    async fn diagnose_v3_probe_failures(
        provider: &P,
        addresses: &[Address],
        block: u64,
    ) -> Vec<Address>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut failed = Vec::new();
        let block = BlockId::from(block);
        for &address in addresses {
            let probe = super::IV3StateProbe::new(address, provider.clone());
            let slot0_ok = probe.slot0().block(block).call().await.is_ok();
            let liq_ok = probe.liquidity().block(block).call().await.is_ok();
            if !(slot0_ok && liq_ok) {
                failed.push(address);
            }
        }
        failed
    }

    async fn diagnose_slipstream_probe_failures(
        provider: &P,
        addresses: &[Address],
        block: u64,
    ) -> Vec<Address>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut failed = Vec::new();
        let block = BlockId::from(block);
        for &address in addresses {
            let probe = super::ISlipstreamStateProbe::new(address, provider.clone());
            let slot0_ok = probe.slot0().block(block).call().await.is_ok();
            let liq_ok = probe.liquidity().block(block).call().await.is_ok();
            let fee_ok = probe.fee().block(block).call().await.is_ok();
            if !(slot0_ok && liq_ok && fee_ok) {
                failed.push(address);
            }
        }
        failed
    }

    async fn drain_maintenance_coverage_batch(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        canonical_head: &Arc<AtomicU64>,
        max_items: usize,
    ) -> Result<(), StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let canonical = canonical_head.load(Ordering::Relaxed);
        if canonical == 0 {
            return Ok(());
        }

        let due_tasks = {
            let mut queue = pending_sync_queue.lock().await;
            queue.claim_due_coverage(canonical, max_items)
        };
        if due_tasks.is_empty() {
            return Ok(());
        }

        let mut by_variant: HashMap<crate::amms::amm::Variant, Vec<AMM>> = HashMap::new();
        let mut variant_addresses: HashMap<crate::amms::amm::Variant, Vec<Address>> =
            HashMap::new();
        let mut drop_addresses = Vec::new();
        let mut deferred_addresses: Vec<(Address, u64)> = Vec::new();
        {
            let guard = state.read().await;
            for (address, _) in &due_tasks {
                if let Some(amm) = guard.state.get(address) {
                    if amm.last_synced_block() > canonical {
                        deferred_addresses.push((*address, amm.last_synced_block()));
                        continue;
                    }
                    by_variant
                        .entry(amm.variant())
                        .or_default()
                        .push(amm.as_ref().clone());
                    variant_addresses
                        .entry(amm.variant())
                        .or_default()
                        .push(*address);
                } else {
                    drop_addresses.push(*address);
                }
            }
        }

        let chain_tip = BlockId::from(canonical);
        let mut synced_pools = Vec::new();
        let mut failed_addresses = Vec::new();

        for (variant, amms) in by_variant {
            let requested = variant_addresses.remove(&variant).unwrap_or_default();
            match variant
                .sync_all_pools::<N, _>(amms, chain_tip, provider.clone())
                .await
            {
                Ok(mut pools) => {
                    let mut returned = HashSet::new();
                    for pool in &pools {
                        returned.insert(pool.address());
                    }
                    for address in requested {
                        if !returned.contains(&address) {
                            failed_addresses.push(address);
                        }
                    }
                    synced_pools.append(&mut pools);
                }
                // flashblocks 乐观头领先存储 RPC 时，coverage 读到超前块是预期
                // 状态（下一轮 tick 再试），不打 warn 刷屏。
                Err(AMMError::BlockNotAvailable {
                    requested_block,
                    storage_head,
                }) => {
                    debug!(
                        ?variant,
                        count = requested.len(),
                        requested_block,
                        storage_head,
                        "Maintenance coverage batch sync deferred: storage RPC head behind required block"
                    );
                    failed_addresses.extend(requested);
                }
                Err(e) => {
                    warn!(
                        ?variant,
                        count = requested.len(),
                        "Maintenance coverage batch sync failed: {}",
                        e
                    );
                    failed_addresses.extend(requested);
                }
            }
        }

        let mut success_addresses = Vec::new();
        {
            let mut guard = state.write().await;
            for mut pool in synced_pools {
                let address = pool.address();
                if let Some(existing) = guard.state.get(&address) {
                    if existing.last_synced_block() > canonical {
                        deferred_addresses.push((address, existing.last_synced_block()));
                        continue;
                    }
                }
                pool.set_last_synced_block(canonical);
                guard.insert_amm(pool);
                success_addresses.push(address);
            }
        }

        let mut queue = pending_sync_queue.lock().await;
        for address in drop_addresses {
            queue.drop_task(address);
        }
        for (address, required_block) in deferred_addresses {
            queue.defer_task(address, required_block);
        }
        for address in success_addresses {
            queue.complete_success(address, canonical);
        }
        for address in failed_addresses {
            queue.on_failure(address, None);
        }

        Ok(())
    }

    /// BinaryFi 的 AsyncUpdate：三段式写回（锁外 fetch → 写锁内对 current existing
    /// 合并），与 `sync_services::start_binaryfi_prop_sync_task` 同形状。
    ///
    /// `probe` 只用于取发起 fetch 时的 stale 集合与 assets/decimals/fee_recipient，
    /// **不参与写回**。
    async fn execute_binaryfi_async_update(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        address: Address,
        probe: &AMM,
    ) -> Result<PendingExecutionOutcome, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let AMM::BinaryFiPropPool(probe) = probe else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        // 块号显式钉死：快照 quote、日志保鲜（price_updated_block）与 ledger rebase
        // 的自变量必须指向同一块。
        let snap_block = provider.get_block_number().await?;
        let snap = probe
            .fetch_stale_snapshot::<N, _>(provider.clone(), BlockId::from(snap_block))
            .await?;
        let refreshed_pairs: Vec<usize> = snap.quotePairs.iter().map(|p| p.to::<usize>()).collect();

        let mut guard = state.write().await;
        let Some(existing_amm) = guard.get_mut_cow(&address) else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        let AMM::BinaryFiPropPool(existing) = existing_amm else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        merge_binaryfi_snapshot(existing, &snap, snap_block, &refreshed_pairs);
        Ok(PendingExecutionOutcome::Applied)
    }

    /// Elfomo 的 AsyncUpdate / Resync：三段式写回（锁外按目标块 fetch → 写锁内对
    /// **current existing** `merge_snapshot`），与
    /// `sync_services::start_elfomo_prop_sync_task` 同形状。
    ///
    /// 为什么必须特判、不能走通用路径：
    /// - **通用路径**（锁外克隆整池 → RPC → 写锁内整只 `insert_amm`）在 RPC 窗口内
    ///   会把实时流刚落到 live 池子上的 `ElfomoTrade` 金库增量丢掉；而用
    ///   `last_synced_block` 做竞态闸门又会恒为 `SkippedStale`——Elfomo 的块级水位
    ///   被每块一笔 raw-tx（`updatePrices`）顶到 flashblock 乐观头，快照读的是规范头/
    ///   目标块，健康期就必然"本地更新"（原则文档 §3 规则 2 的反模式）。
    /// - **本分支**：`merge_snapshot` 自带 `VaultDeltaLedger` rebase 语义
    ///   （`current = 快照(S) + Σ_{块 > S} 增量`），快照永远能安全落地，RPC 窗口内
    ///   落在 live 池子上的成交一个不丢；水位由池内取 `max` 只作幂等守卫。
    async fn execute_elfomo_snapshot_reconcile(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        address: Address,
        target_block: u64,
    ) -> Result<PendingExecutionOutcome, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let probe = {
            let guard = state.read().await;
            match guard.get(&address) {
                Some(AMM::ElfomoFiPropPool(p)) => p.clone(),
                _ => return Ok(PendingExecutionOutcome::MissingPool),
            }
        };
        let (snap, snap_block) = match probe
            .fetch_snapshot_at::<N, _>(provider.clone(), BlockId::from(target_block))
            .await
        {
            Ok(v) => v,
            // 目标块超前于存储 RPC 头部（flashblocks 乐观头 vs HTTP 节点落后）：
            // 留队重试，不降级读取旧块数据。
            Err(AMMError::BlockNotAvailable { .. }) => {
                return Ok(PendingExecutionOutcome::RetryLater(target_block));
            }
            Err(e) => return Err(e),
        };

        let mut guard = state.write().await;
        let Some(existing) = guard.get_mut_cow(&address) else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        let AMM::ElfomoFiPropPool(existing) = existing else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        // 返回 false = 快照早于账本锚点块（无法重建 (S, anchor] 增量）→ 保持本地真值。
        existing.merge_snapshot(snap, snap_block);
        Ok(PendingExecutionOutcome::Applied)
    }

    /// Caliber 的 AsyncUpdate / Resync：三段式写回（锁外按目标块 fetch → 写锁内对
    /// **current existing** `apply_snapshot_merged`），与
    /// `sync_services::start_caliber_prop_sync_task` 的锁内合并同形状。
    ///
    /// 为什么必须特判、不能走通用路径（与 Elfomo 同一形态，见其注释）：
    /// - **块级水位闸门恒真**：Caliber 的 `batchUpdateParameters` 是 raw-tx 更新，
    ///   水位被 flashblock 乐观头顶高（`apply_batch_update` / `apply_swap` 取 max）；
    ///   Resync 的 `required_block` 却取自请求时的 canonical head（落后乐观头 ≥1 块）
    ///   → 通用闸门 `last_synced_block() > target_block` 每次都 `DeferredStale`，
    ///   `postpone` 无限后移，**纠错通道被永久饿死**（原则文档 §3 规则 2 的反模式）。
    ///   这也解释了日志里 `Pending sync task deferred: local state newer than
    ///   target block ... deferred_to_block=…` 的死循环。
    /// - **整只覆盖丢增量**：通用路径"锁外克隆整池 → RPC → `insert_amm`"会丢掉
    ///   RPC 窗口内实时流落在 live 池子上的 swap/报价更新（连同事件账本一起）。
    ///   `apply_snapshot_merged` 自带 rebase 合并（累积量 = 快照(S) + Σ(块 > S)
    ///   增量）、B 类字段（field0/field1/deadline）字段级水位保鲜、C 类低频字段
    ///   直接覆盖，快照永远能安全落地，水位由池内 `max` 只作幂等守卫。
    async fn execute_caliber_snapshot_reconcile(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        address: Address,
        target_block: u64,
    ) -> Result<PendingExecutionOutcome, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let probe = {
            let guard = state.read().await;
            match guard.get(&address) {
                Some(AMM::CaliberPropPool(p)) => p.clone(),
                _ => return Ok(PendingExecutionOutcome::MissingPool),
            }
        };
        let snap = match crate::amms::caliber_prop::fetch_exact_snapshot(
            provider,
            probe.contract_address,
            probe.pair_id,
            probe.token_x,
            probe.token_y,
            BlockId::from(target_block),
        )
        .await
        {
            Ok(v) => v,
            // 目标块超前于存储 RPC 头部（flashblocks 乐观头 vs HTTP 节点落后）：
            // 留队重试，不降级读取旧块数据。
            Err(AMMError::BlockNotAvailable { .. }) => {
                return Ok(PendingExecutionOutcome::RetryLater(target_block));
            }
            Err(e) => return Err(e),
        };

        let mut guard = state.write().await;
        let Some(existing) = guard.get_mut_cow(&address) else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        let AMM::CaliberPropPool(existing) = existing else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };
        existing.apply_snapshot_merged(snap, target_block);
        Ok(PendingExecutionOutcome::Applied)
    }

    async fn execute_pending_task(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        address: Address,
        task: &PendingSyncTask,
        target_block: u64,
    ) -> Result<PendingExecutionOutcome, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        match task.action {
            PendingSyncAction::AsyncUpdate => {
                let Some(mut local_amm) = ({ state.read().await.get(&address).cloned() }) else {
                    return Ok(PendingExecutionOutcome::MissingPool);
                };
                // Elfomo 特判：块级水位不能当新鲜度闸门（规则 2），必须
                // 「锁外 fetch → 写锁内对 current existing `merge_snapshot`」。
                if matches!(local_amm, AMM::ElfomoFiPropPool(_)) {
                    return Self::execute_elfomo_snapshot_reconcile(
                        provider,
                        state,
                        address,
                        target_block,
                    )
                    .await;
                }
                // BinaryFi 特判：必须「锁外 fetch → 写锁内对 current existing 合并」，
                // 不能走下面的通用路径（锁外克隆整池 → RPC → 整只覆盖）——RPC 窗口内
                // 实时流落在 live 池子上的 swap 增量不在这个克隆里，覆盖后会连同事件
                // 账本一起丢，且水位取 max 会把这段永久跳过。周期任务
                // （`sync_services::start_binaryfi_prop_sync_task`）已是同一形状，
                // 这里是该反模式的最后一个入口。
                if matches!(local_amm, AMM::BinaryFiPropPool(_)) {
                    return Self::execute_binaryfi_async_update(
                        provider, state, address, &local_amm,
                    )
                    .await;
                }
                // Caliber 特判：同 Elfomo——块级水位不能当新鲜度闸门（规则 2），
                // 且必须对 current existing 做 rebase 合并而非整只覆盖。
                if matches!(local_amm, AMM::CaliberPropPool(_)) {
                    return Self::execute_caliber_snapshot_reconcile(
                        provider,
                        state,
                        address,
                        target_block,
                    )
                    .await;
                }
                let snapshot_last_synced_block = local_amm.last_synced_block();
                // AsyncUpdate: no last_synced_block guard.
                // RPC availability is already guaranteed by claim_due_filtered's
                // required_block ≤ canonical_head check at pop time.
                // On Ethereum this is always safe (canonical == realtime).
                local_amm.update::<N, _>(provider.clone()).await?;
                // 刷新后水位 = max(update() 自己钉的读块, 目标 canonical)。
                // 多数池子的 update() 读 `latest` 而不钉读块，用 canonical 作近似；
                // caliber / binaryfi 会在 update() 内钉住真实读块（storage head /
                // get_block_number），此处取 max 不会把它压低。
                local_amm.set_last_synced_block(local_amm.last_synced_block().max(target_block));
                let mut guard = state.write().await;
                if let Some(existing) = guard.get(&address) {
                    if should_skip_async_apply_for(
                        &local_amm,
                        existing.last_synced_block(),
                        snapshot_last_synced_block,
                    ) {
                        return Ok(PendingExecutionOutcome::SkippedStale);
                    }
                    // 写回前把水位对齐到 existing：**在调用点自己取 max**，不依赖
                    // 各池 setter 的实现（契约见 `AutomatedMarketMaker::
                    // set_last_synced_block`）。防止快照写回把水位回退到旧块 → 后续
                    // 旧块日志被重新应用（重复消费容量/双计），或让 DeferredStale
                    // 新鲜度守卫失效（旧快照覆盖更新的实时状态）。
                    let keep = existing
                        .last_synced_block()
                        .max(local_amm.last_synced_block());
                    local_amm.set_last_synced_block(keep);
                }
                guard.insert_amm(local_amm);
                Ok(PendingExecutionOutcome::Applied)
            }
            PendingSyncAction::Resync => {
                let Some(local_amm) = ({ state.read().await.get(&address).cloned() }) else {
                    return Ok(PendingExecutionOutcome::MissingPool);
                };
                // Elfomo 特判：与 AsyncUpdate 同形（锁外 fetch → 锁内 merge）。
                // **必须放在块级水位闸门之前**：Elfomo 水位被 raw-tx 顶到乐观头，
                // 通用闸门会让它恒为 DeferredStale，纠错通道永远排不上（规则 2）。
                // raw-tx 驱动水位的池型（Caliber / Elfomo）必须**放在块级水位闸门
                // 之前**：它们的水位被 flashblock 乐观头顶高，通用闸门会让纠错任务
                // 恒为 `DeferredStale`、永远排不上（原则文档 §3 规则 2）。
                if resync_skips_block_watermark_gate(&local_amm) {
                    return match &local_amm {
                        AMM::ElfomoFiPropPool(_) => {
                            Self::execute_elfomo_snapshot_reconcile(
                                provider,
                                state,
                                address,
                                target_block,
                            )
                            .await
                        }
                        AMM::CaliberPropPool(_) => {
                            Self::execute_caliber_snapshot_reconcile(
                                provider,
                                state,
                                address,
                                target_block,
                            )
                            .await
                        }
                        _ => Ok(PendingExecutionOutcome::MissingPool),
                    };
                }
                // 新鲜度保护（前置检查，避免无谓的链上点读）：本地实时状态已新于
                // 目标 canonical 块时，禁止用旧块快照覆盖/回卷；把任务推迟到
                // canonical 追上本地状态后，再以更新的块做纠错。
                if local_amm.last_synced_block() > target_block {
                    return Ok(PendingExecutionOutcome::DeferredStale(
                        local_amm.last_synced_block(),
                    ));
                }
                let variant = local_amm.variant();
                let mut refreshed = match variant
                    .sync_all_pools::<N, _>(
                        vec![local_amm],
                        BlockId::from(target_block),
                        provider.clone(),
                    )
                    .await
                {
                    // 目标块超前于存储 RPC 头部（flashblocks 乐观头 vs HTTP 节点
                    // 落后）：不降级读取旧块数据，任务保持排队等待 RPC 追上后
                    // 以目标块重读，绝不把"旧块数据 + last_synced=目标块"当作成功。
                    Err(AMMError::BlockNotAvailable { .. }) => {
                        return Ok(PendingExecutionOutcome::RetryLater(target_block));
                    }
                    res => res?,
                };

                let Some(mut synced) = refreshed.pop() else {
                    return Err(AMMError::Msg(format!(
                        "Resync returned empty result for pool {address:?}"
                    )));
                };
                synced.set_last_synced_block(target_block);
                let mut guard = state.write().await;
                // 竞态兜底：RPC 读取期间实时流可能已推进，写锁内再校验一次。
                if let Some(existing) = guard.get(&address) {
                    if existing.last_synced_block() > target_block {
                        return Ok(PendingExecutionOutcome::DeferredStale(
                            existing.last_synced_block(),
                        ));
                    }
                }
                guard.insert_amm(synced);
                Ok(PendingExecutionOutcome::Applied)
            }
        }
    }

    fn is_recoverable_delay_error(err: &AMMError) -> bool {
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("block not found")
            || msg.contains("header not found")
            || msg.contains("requested to block")
            || msg.contains("invalid block range")
            // Some RPC backends surface transient getLogs failures as -32603 Internal error.
            || msg.contains("error code -32603")
            || msg.contains("internal error")
    }

    pub(super) async fn drain_pending_sync_queue(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Notify,
        canonical_head: &Arc<AtomicU64>,
        coverage_only: bool,
        max_items: usize,
    ) -> Result<(), StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let canonical = canonical_head.load(Ordering::Relaxed);
        if canonical == 0 {
            return Ok(());
        }

        let due_tasks = {
            let mut queue = pending_sync_queue.lock().await;
            if coverage_only {
                queue.claim_due_coverage(canonical, max_items)
            } else {
                queue.claim_due_non_coverage(canonical, max_items)
            }
        };
        Self::execute_due_tasks(
            provider,
            state,
            pending_sync_queue,
            pending_sync_notify,
            canonical,
            due_tasks,
        )
        .await;

        Ok(())
    }

    async fn execute_due_tasks(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: &Notify,
        canonical: u64,
        due_tasks: Vec<(Address, PendingSyncTask)>,
    ) where
        P: Provider<N> + Clone,
        N: Network,
    {
        for (address, task) in due_tasks {
            // Resync 是"按指定块精确纠错"：以任务携带的 required_block 为读取
            // 目标，不随每次执行时的 canonical（flashblocks 乐观头，可能领先
            // 存储 RPC 1 块）漂移——否则目标块永远追不上、任务永远失败。
            // AsyncUpdate 保持以当前 canonical 做普通刷新（update() 走 latest）。
            let target_block = if matches!(task.action, PendingSyncAction::Resync) {
                task.required_block
            } else {
                canonical
            };
            match Self::execute_pending_task(provider, state, address, &task, target_block).await {
                Ok(PendingExecutionOutcome::Applied) => {
                    if matches!(task.action, PendingSyncAction::AsyncUpdate)
                        && matches!(task.reason, PendingSyncReason::AsyncUpdate)
                    {
                        info!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                            target_block = target_block,
                            "Pending sync task applied"
                        );
                    } else {
                        warn!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                            target_block = target_block,
                            "Pending sync task applied"
                        );
                    }
                    pending_sync_queue
                        .lock()
                        .await
                        .complete_success(address, canonical);
                }
                Ok(PendingExecutionOutcome::SkippedStale) => {
                    // 实时流与异步快照的正常竞争（水位差），不是异常：
                    // Elfomo/BinaryFi 这类"每块都有事件"的池子尤其会常态出现。
                    debug!(
                        ?address,
                        action = ?task.action,
                        reason = ?task.reason,
                        first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                        target_block = target_block,
                        "Pending sync task skipped due to newer local state"
                    );
                    pending_sync_queue
                        .lock()
                        .await
                        .complete_success(address, canonical);
                }
                Ok(PendingExecutionOutcome::DeferredStale(deferred_to_block)) => {
                    warn!(
                        ?address,
                        action = ?task.action,
                        reason = ?task.reason,
                        first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                        target_block = target_block,
                        deferred_to_block = deferred_to_block,
                        "Pending sync task deferred: local state newer than target block"
                    );
                    pending_sync_queue.lock().await.postpone(
                        address,
                        task.action,
                        task.reason,
                        deferred_to_block,
                    );
                }
                Ok(PendingExecutionOutcome::RetryLater(required_block)) => {
                    // 首次 warn，后续退避重试期间降为 debug，避免 RPC 落后窗口
                    // 内（通常 ≤1 块、<2s 追平）连续刷屏。
                    if task.retry_count == 0 {
                        warn!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            required_block,
                            "Pending sync task deferred: storage RPC head behind required block; will retry"
                        );
                    } else {
                        debug!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            required_block,
                            retry_count = task.retry_count,
                            "Pending sync task still waiting for storage RPC head; retrying later"
                        );
                    }
                    pending_sync_queue
                        .lock()
                        .await
                        .retry_later(address, required_block);
                    pending_sync_notify.notify_one();
                }
                Ok(PendingExecutionOutcome::MissingPool) => {
                    pending_sync_queue.lock().await.drop_task(address);
                }
                Err(e) => {
                    let recoverable = Self::is_recoverable_delay_error(&e);
                    if !recoverable {
                        warn!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            "Pending sync task failed: {}",
                            e
                        );
                    }
                    pending_sync_queue.lock().await.on_failure(address, None);
                }
            }
        }
    }

    pub(super) async fn run_pending_sync_worker(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: Arc<Notify>,
        canonical_head: Arc<AtomicU64>,
        interval: Duration,
    ) where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        loop {
            let _ = Self::drain_pending_sync_queue(
                &provider,
                &state,
                &pending_sync_queue,
                &pending_sync_notify,
                &canonical_head,
                false,
                usize::MAX,
            )
            .await;
            tokio::select! {
                _ = pending_sync_notify.notified() => {},
                _ = sleep(interval) => {},
            }
        }
    }

    // 兜底强制更新同步本地池子最新链上数据
    pub(super) async fn run_maintenance_coverage_scheduler(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        canonical_head: Arc<AtomicU64>,
        interval: Duration,
    ) where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        loop {
            sleep(interval).await;
            let canonical = canonical_head.load(Ordering::Relaxed);
            if canonical == 0 {
                continue;
            }

            let mut pools: Vec<(Address, u64)> = {
                let guard = state.read().await;
                guard
                    .state
                    .iter()
                    .map(|(address, amm)| (*address, amm.last_synced_block()))
                    .collect()
            };
            if pools.is_empty() {
                continue;
            }

            // Oldest-first coverage: lower last_synced_block gets higher priority.
            pools.sort_by_key(|(address, last_synced)| (*last_synced, *address));
            let coverage_batch = MAINT_COVERAGE_BATCH_SIZE;

            let mut queue = pending_sync_queue.lock().await;
            let selected: Vec<(Address, u64)> = pools.into_iter().take(coverage_batch).collect();
            for (address, _) in selected {
                queue.enqueue(
                    address,
                    PendingSyncAction::Resync,
                    canonical,
                    PendingSyncReason::MaintenanceCoverage,
                );
            }
            drop(queue);

            let _ = Self::drain_maintenance_coverage_batch(
                &provider,
                &state,
                &pending_sync_queue,
                &canonical_head,
                MAINT_COVERAGE_BATCH_SIZE,
            )
            .await;
        }
    }

    fn local_cl_probe_snapshot(amm: &AMM) -> Option<ClProbeSnapshot> {
        match amm {
            AMM::UniswapV3Pool(pool) => Some(ClProbeSnapshot {
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
                fee: None,
            }),
            AMM::PancakeV3Pool(pool) => Some(ClProbeSnapshot {
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
                fee: None,
            }),
            AMM::AerodromeSlipstreamPool(pool) => Some(ClProbeSnapshot {
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
                fee: Some(pool.fee),
            }),
            _ => None,
        }
    }

    fn local_v2_like_probe_snapshot(amm: &AMM) -> Option<V2LikeProbeSnapshot> {
        match amm {
            AMM::UniswapV2Pool(pool) => Some(V2LikeProbeSnapshot {
                reserve_0: pool.reserve_0,
                reserve_1: pool.reserve_1,
            }),
            AMM::SushiV2Pool(pool) => Some(V2LikeProbeSnapshot {
                reserve_0: pool.reserve_0,
                reserve_1: pool.reserve_1,
            }),
            AMM::PancakeV2Pool(pool) => Some(V2LikeProbeSnapshot {
                reserve_0: pool.reserve_0,
                reserve_1: pool.reserve_1,
            }),
            AMM::AerodromeV2Pool(pool) => Some(V2LikeProbeSnapshot {
                reserve_0: pool.reserve_0,
                reserve_1: pool.reserve_1,
            }),
            _ => None,
        }
    }

    fn local_v4_lite_probe_snapshot(amm: &AMM) -> Option<V4LiteProbeSnapshot> {
        match amm {
            AMM::UniswapV4Pool(pool) => Some(V4LiteProbeSnapshot {
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
            }),
            AMM::PancakeInfinityPool(pool) => Some(V4LiteProbeSnapshot {
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
            }),
            _ => None,
        }
    }

    fn curve_ng_stable_probe_snapshot_from_pool(pool: &CurveNGPool) -> CurveNGStableProbeSnapshot {
        CurveNGStableProbeSnapshot {
            balances: pool.balances.clone(),
            admin_balances: pool.admin_balances.clone(),
            rates: if pool.supports_stored_rates {
                Some(pool.rates.clone())
            } else {
                None
            },
            rates_asset_types: if pool.supports_stored_rates && !pool.asset_types.is_empty() {
                Some(pool.asset_types.clone())
            } else {
                None
            },
            amp: pool.amp,
            fee: pool.fee,
            admin_fee: pool.admin_fee,
            offpeg_fee_multiplier: pool.offpeg_fee_multiplier,
        }
    }

    fn local_curve_ng_stable_probe_snapshot(amm: &AMM) -> Option<CurveNGStableProbeSnapshot> {
        match amm {
            AMM::CurveNGPool(pool) if pool.pool_type.is_stable() && pool.n_coins > 0 => {
                Some(Self::curve_ng_stable_probe_snapshot_from_pool(pool))
            }
            _ => None,
        }
    }

    fn curve_ng_crypto_probe_snapshot_from_pool(pool: &CurveNGPool) -> CurveNGCryptoProbeSnapshot {
        CurveNGCryptoProbeSnapshot {
            balances: pool.balances.clone(),
            price_scale: pool.price_scale.clone().unwrap_or_default(),
            d: pool.d,
        }
    }

    fn local_curve_ng_crypto_probe_snapshot(amm: &AMM) -> Option<CurveNGCryptoProbeSnapshot> {
        match amm {
            AMM::CurveNGPool(pool) if pool.pool_type.is_crypto() && pool.n_coins > 0 => {
                Some(Self::curve_ng_crypto_probe_snapshot_from_pool(pool))
            }
            _ => None,
        }
    }

    async fn fetch_curve_ng_stable_probe_snapshots(
        provider: &P,
        targets: &mut [CurveNGPool],
        block: u64,
    ) -> Result<HashMap<Address, CurveNGStableProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        CurveNGFactory::refresh_runtime_data_batch::<N, _>(
            targets,
            BlockId::from(block),
            provider.clone(),
        )
        .await?;

        let mut snapshots = HashMap::with_capacity(targets.len());
        for pool in targets.iter() {
            snapshots.insert(
                pool.address,
                Self::curve_ng_stable_probe_snapshot_from_pool(pool),
            );
        }
        Ok(snapshots)
    }

    // Fetch remote snapshots for CryptoSwap pools by splitting into TwoCrypto/TriCrypto
    // and calling their respective batch contracts. Each has individual fallback on failure.
    async fn fetch_curve_ng_crypto_probe_snapshots(
        provider: &P,
        targets: &[CurveNGPool],
        block: u64,
    ) -> Result<HashMap<Address, CurveNGCryptoProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let block = BlockId::from(block);
        let mut snapshots = HashMap::with_capacity(targets.len());

        let mut twocrypto_addrs: Vec<Address> = Vec::new();
        let mut tricrypto_addrs: Vec<Address> = Vec::new();
        for pool in targets.iter() {
            match pool.pool_type {
                crate::amms::curve_ng::CurveNGPoolType::TwoCrypto => {
                    twocrypto_addrs.push(pool.address);
                }
                crate::amms::curve_ng::CurveNGPoolType::TriCrypto => {
                    tricrypto_addrs.push(pool.address);
                }
                _ => {}
            }
        }

        // Fetch TwoCrypto pools
        if !twocrypto_addrs.is_empty() {
            Self::fetch_twocrypto_probe_snapshots(
                provider,
                block,
                &twocrypto_addrs,
                &mut snapshots,
            )
            .await;
        }

        // Fetch TriCrypto pools
        if !tricrypto_addrs.is_empty() {
            Self::fetch_tricrypto_probe_snapshots(
                provider,
                block,
                &tricrypto_addrs,
                &mut snapshots,
            )
            .await;
        }

        Ok(snapshots)
    }

    async fn fetch_twocrypto_probe_snapshots(
        provider: &P,
        block: BlockId,
        addresses: &[Address],
        snapshots: &mut HashMap<Address, CurveNGCryptoProbeSnapshot>,
    ) where
        P: Provider<N> + Clone,
        N: Network,
    {
        let return_data = match GetCurveNGTwoCryptoRuntimeDataBatchRequest::deploy_builder(
            provider.clone(),
            addresses.to_vec(),
        )
        .call_raw()
        .block(block)
        .await
        {
            Ok(data) => data,
            Err(e) => {
                warn!(
                    count = addresses.len(),
                    "drift_probe: TwoCrypto batch failed, retrying individually: {}", e
                );
                for &addr in addresses {
                    let Ok(single) = GetCurveNGTwoCryptoRuntimeDataBatchRequest::deploy_builder(
                        provider.clone(),
                        vec![addr],
                    )
                    .call_raw()
                    .block(block)
                    .await
                    else {
                        warn!(address = ?addr, "drift_probe: TwoCrypto individual fetch failed");
                        continue;
                    };
                    let Ok(decoded) = <Vec<TwoCryptoRuntimeData> as SolValue>::abi_decode(&single)
                    else {
                        continue;
                    };
                    if let Some(data) = decoded.into_iter().next() {
                        snapshots.insert(
                            data.poolAddress,
                            CurveNGCryptoProbeSnapshot {
                                balances: data.balances,
                                price_scale: vec![data.priceScale],
                                d: Some(data.d),
                            },
                        );
                    }
                }
                return;
            }
        };

        let Ok(decoded) = <Vec<TwoCryptoRuntimeData> as SolValue>::abi_decode(&return_data) else {
            warn!("drift_probe: TwoCrypto batch decode failed");
            return;
        };
        for data in decoded {
            snapshots.insert(
                data.poolAddress,
                CurveNGCryptoProbeSnapshot {
                    balances: data.balances,
                    price_scale: vec![data.priceScale],
                    d: Some(data.d),
                },
            );
        }
    }

    async fn fetch_tricrypto_probe_snapshots(
        provider: &P,
        block: BlockId,
        addresses: &[Address],
        snapshots: &mut HashMap<Address, CurveNGCryptoProbeSnapshot>,
    ) where
        P: Provider<N> + Clone,
        N: Network,
    {
        let return_data = match GetCurveNGTriCryptoRuntimeDataBatchRequest::deploy_builder(
            provider.clone(),
            addresses.to_vec(),
        )
        .call_raw()
        .block(block)
        .await
        {
            Ok(data) => data,
            Err(e) => {
                warn!(
                    count = addresses.len(),
                    "drift_probe: TriCrypto batch failed, retrying individually: {}", e
                );
                for &addr in addresses {
                    let Ok(single) = GetCurveNGTriCryptoRuntimeDataBatchRequest::deploy_builder(
                        provider.clone(),
                        vec![addr],
                    )
                    .call_raw()
                    .block(block)
                    .await
                    else {
                        warn!(address = ?addr, "drift_probe: TriCrypto individual fetch failed");
                        continue;
                    };
                    let Ok(decoded) = <Vec<TriCryptoRuntimeData> as SolValue>::abi_decode(&single)
                    else {
                        continue;
                    };
                    if let Some(data) = decoded.into_iter().next() {
                        snapshots.insert(
                            data.poolAddress,
                            CurveNGCryptoProbeSnapshot {
                                balances: data.balances,
                                price_scale: data.priceScale,
                                d: Some(data.d),
                            },
                        );
                    }
                }
                return;
            }
        };

        let Ok(decoded) = <Vec<TriCryptoRuntimeData> as SolValue>::abi_decode(&return_data) else {
            warn!("drift_probe: TriCrypto batch decode failed");
            return;
        };
        for data in decoded {
            snapshots.insert(
                data.poolAddress,
                CurveNGCryptoProbeSnapshot {
                    balances: data.balances,
                    price_scale: data.priceScale,
                    d: Some(data.d),
                },
            );
        }
    }

    async fn fetch_v2_like_probe_snapshots_batch(
        provider: &P,
        targets: &[(Address, Variant)],
        block: u64,
    ) -> Result<HashMap<Address, V2LikeProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let block = BlockId::from(block);
        let addresses: Vec<Address> = targets
            .iter()
            .filter_map(|(address, variant)| match variant {
                Variant::UniswapV2Pool
                | Variant::SushiV2Pool
                | Variant::PancakeV2Pool
                | Variant::AerodromeV2Pool => Some(*address),
                _ => None,
            })
            .collect();

        if addresses.is_empty() {
            return Ok(HashMap::new());
        }

        let return_data =
            GetV2LikeReservesProbeBatchRequest::deploy_builder(provider.clone(), addresses.clone())
                .call_raw()
                .block(block)
                .await?;

        let decoded = <Vec<(bool, u128, u128)> as SolValue>::abi_decode(&return_data)?;
        if decoded.len() != addresses.len() {
            warn!(
                expected = addresses.len(),
                decoded = decoded.len(),
                "V2Like drift probe batch decode length mismatch"
            );
        }

        let mut snapshots = HashMap::with_capacity(addresses.len());
        for (address, (ok, reserve_0, reserve_1)) in addresses.into_iter().zip(decoded) {
            if !ok {
                continue;
            }
            snapshots.insert(
                address,
                V2LikeProbeSnapshot {
                    reserve_0,
                    reserve_1,
                },
            );
        }

        Ok(snapshots)
    }

    async fn fetch_v3_probe_snapshots_batch(
        provider: &P,
        addresses: &[Address],
        block: u64,
    ) -> Result<HashMap<Address, ClProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut snapshots = HashMap::with_capacity(addresses.len());
        let block = BlockId::from(block);
        for chunk in addresses.chunks(DRIFT_V3_SLOT0_BATCH_STEP) {
            let mut pending_groups: Vec<Vec<Address>> = vec![chunk.to_vec()];

            while let Some(group) = pending_groups.pop() {
                let attempt = GetUniswapV3PoolSlot0BatchRequest::deploy_builder(
                    provider.clone(),
                    group.clone(),
                )
                .call_raw()
                .block(block)
                .await;

                match attempt {
                    Ok(return_data) => {
                        let decoded =
                            <Vec<(bool, i32, u128, U256)> as SolValue>::abi_decode(&return_data)?;
                        if decoded.len() != group.len() {
                            warn!(
                                expected = group.len(),
                                decoded = decoded.len(),
                                "V3 drift probe batch decode length mismatch"
                            );
                        }

                        let mut ok_false_addresses = Vec::new();
                        for (address, (ok, tick, liquidity, sqrt_price)) in
                            group.into_iter().zip(decoded)
                        {
                            if !ok {
                                ok_false_addresses.push(address);
                                continue;
                            }
                            snapshots.insert(
                                address,
                                ClProbeSnapshot {
                                    sqrt_price,
                                    tick,
                                    liquidity,
                                    fee: None,
                                },
                            );
                        }

                        // Batch succeeded: only retry addresses explicitly marked as unreadable (ok=false).
                        for address in ok_false_addresses {
                            let probe = super::IV3StateProbe::new(address, provider.clone());
                            let slot0 = probe.slot0().block(block).call().await;
                            let liquidity = probe.liquidity().block(block).call().await;
                            match (slot0, liquidity) {
                                (Ok(slot0), Ok(liquidity)) => {
                                    snapshots.insert(
                                        address,
                                        ClProbeSnapshot {
                                            sqrt_price: U256::from(slot0.sqrtPriceX96),
                                            tick: slot0.tick.as_i32(),
                                            liquidity,
                                            fee: None,
                                        },
                                    );
                                }
                                _ => {
                                    // Keep as unreadable this round; handled by higher-level retry/enqueue logic.
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if group.len() <= 1 {
                            let address = group[0];
                            let probe = super::IV3StateProbe::new(address, provider.clone());
                            let slot0 = probe.slot0().block(block).call().await;
                            let liquidity = probe.liquidity().block(block).call().await;
                            match (slot0, liquidity) {
                                (Ok(slot0), Ok(liquidity)) => {
                                    snapshots.insert(
                                        address,
                                        ClProbeSnapshot {
                                            sqrt_price: U256::from(slot0.sqrtPriceX96),
                                            tick: slot0.tick.as_i32(),
                                            liquidity,
                                            fee: None,
                                        },
                                    );
                                }
                                _ => warn!(
                                    address = ?address,
                                    "V3 probe single fallback failed: {}",
                                    err
                                ),
                            }
                            continue;
                        }

                        let split = group.len() / 2;
                        let left = group[..split].to_vec();
                        let right = group[split..].to_vec();
                        warn!(
                            size = group.len(),
                            left = left.len(),
                            right = right.len(),
                            "V3 probe batch failed, fallback split: {}",
                            err
                        );
                        pending_groups.push(right);
                        pending_groups.push(left);
                    }
                }
            }
        }

        Ok(snapshots)
    }

    async fn fetch_slipstream_probe_snapshots_batch(
        provider: &P,
        addresses: &[Address],
        block: u64,
    ) -> Result<HashMap<Address, ClProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut snapshots = HashMap::with_capacity(addresses.len());
        let block = BlockId::from(block);
        for chunk in addresses.chunks(DRIFT_SLIPSTREAM_PROBE_BATCH_STEP) {
            let chunk_addrs = chunk.to_vec();
            let return_data = GetAerodromeSlipstreamProbeBatchRequest::deploy_builder(
                provider.clone(),
                chunk_addrs.clone(),
            )
            .call_raw()
            .block(block)
            .await?;

            let decoded =
                <Vec<(bool, i32, u128, U256, u32)> as SolValue>::abi_decode(&return_data)?;
            if decoded.len() != chunk_addrs.len() {
                warn!(
                    expected = chunk_addrs.len(),
                    decoded = decoded.len(),
                    "Slipstream drift probe decode length mismatch"
                );
            }

            for (address, (ok, tick, liquidity, sqrt_price, fee)) in
                chunk_addrs.into_iter().zip(decoded)
            {
                if !ok {
                    continue;
                }
                snapshots.insert(
                    address,
                    ClProbeSnapshot {
                        sqrt_price,
                        tick,
                        liquidity,
                        fee: Some(fee),
                    },
                );
            }
        }

        Ok(snapshots)
    }

    async fn fetch_pancake_probe_snapshots_batch(
        provider: &P,
        addresses: &[Address],
        block: u64,
    ) -> Result<HashMap<Address, ClProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut snapshots = HashMap::with_capacity(addresses.len());
        let block = BlockId::from(block);
        for chunk in addresses.chunks(DRIFT_V3_SLOT0_BATCH_STEP) {
            let mut pending_groups: Vec<Vec<Address>> = vec![chunk.to_vec()];

            while let Some(group) = pending_groups.pop() {
                let attempt = GetPancakeV3PoolSlot0BatchRequest::deploy_builder(
                    provider.clone(),
                    group.clone(),
                )
                .call_raw()
                .block(block)
                .await;

                match attempt {
                    Ok(return_data) => {
                        let decoded =
                            <Vec<(i32, u128, U256)> as SolValue>::abi_decode(&return_data)?;
                        if decoded.len() != group.len() {
                            warn!(
                                expected = group.len(),
                                decoded = decoded.len(),
                                "Pancake V3 drift probe batch decode length mismatch"
                            );
                        }

                        for (address, (tick, liquidity, sqrt_price)) in
                            group.into_iter().zip(decoded)
                        {
                            snapshots.insert(
                                address,
                                ClProbeSnapshot {
                                    sqrt_price,
                                    tick,
                                    liquidity,
                                    fee: None,
                                },
                            );
                        }
                    }
                    Err(err) => {
                        if group.len() <= 1 {
                            let address = group[0];
                            let probe = super::IPancakeV3StateProbe::new(address, provider.clone());
                            let slot0 = probe.slot0().block(block).call().await;
                            let liquidity = probe.liquidity().block(block).call().await;
                            match (slot0, liquidity) {
                                (Ok(slot0), Ok(liquidity)) => {
                                    snapshots.insert(
                                        address,
                                        ClProbeSnapshot {
                                            sqrt_price: U256::from(slot0.sqrtPriceX96),
                                            tick: slot0.tick.as_i32(),
                                            liquidity,
                                            fee: None,
                                        },
                                    );
                                }
                                _ => warn!(
                                    address = ?address,
                                    "Pancake V3 probe single fallback failed: {}",
                                    err
                                ),
                            }
                            continue;
                        }

                        let split = group.len() / 2;
                        let left = group[..split].to_vec();
                        let right = group[split..].to_vec();
                        warn!(
                            size = group.len(),
                            left = left.len(),
                            right = right.len(),
                            "Pancake V3 probe batch failed, fallback split: {}",
                            err
                        );
                        pending_groups.push(right);
                        pending_groups.push(left);
                    }
                }
            }
        }

        Ok(snapshots)
    }

    async fn fetch_v4_lite_probe_snapshots_batch(
        provider: &P,
        targets: &[(Address, Variant, Address, B256)],
        block: u64,
    ) -> Result<HashMap<Address, V4LiteProbeSnapshot>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut addresses = Vec::new();
        let mut probes = Vec::new();
        for (address, variant, manager_address, pool_id) in targets {
            match variant {
                Variant::UniswapV4Pool | Variant::PancakeInfinityPool => {
                    addresses.push(*address);
                    probes.push(GetV4LitePoolStateBatchRequest::PoolProbe {
                        manager: *manager_address,
                        poolId: *pool_id,
                    });
                }
                _ => continue,
            }
        }

        if probes.is_empty() {
            return Ok(HashMap::new());
        }

        let return_data = GetV4LitePoolStateBatchRequest::deploy_builder(provider.clone(), probes)
            .call_raw()
            .block(BlockId::from(block))
            .await?;

        let decoded = <Vec<(bool, i32, u128, U256)> as SolValue>::abi_decode(&return_data)?;
        if decoded.len() != addresses.len() {
            warn!(
                expected = addresses.len(),
                decoded = decoded.len(),
                "V4Lite drift probe batch decode length mismatch"
            );
        }

        let mut snapshots = HashMap::with_capacity(addresses.len());
        for (address, (ok, tick, liquidity, sqrt_price)) in addresses.into_iter().zip(decoded) {
            if !ok {
                continue;
            }
            snapshots.insert(
                address,
                V4LiteProbeSnapshot {
                    sqrt_price,
                    tick,
                    liquidity,
                },
            );
        }

        Ok(snapshots)
    }

    pub(super) async fn run_silent_drift_probe_task(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        canonical_head: Arc<AtomicU64>,
        scan_tick: Duration,
    ) where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        let mut last_probe_at: HashMap<Address, Instant> = HashMap::new();
        let mut cached_v2_like_addresses: Vec<Address> = Vec::new();
        let mut cached_v3_addresses: Vec<Address> = Vec::new();
        let mut cached_v4_like_addresses: Vec<Address> = Vec::new();
        let mut cached_slipstream_addresses: Vec<Address> = Vec::new();
        let mut cached_curve_ng_stable_addresses: Vec<Address> = Vec::new();
        let mut cached_curve_ng_crypto_addresses: Vec<Address> = Vec::new();
        let mut v2_like_cursor: usize = 0;
        let mut v3_cursor: usize = 0;
        let mut v4_like_cursor: usize = 0;
        let mut slipstream_cursor: usize = 0;
        let mut curve_ng_stable_cursor: usize = 0;
        let mut curve_ng_crypto_cursor: usize = 0;
        let mut active_kind = DriftProbeKind::V2Like;
        let mut last_cache_refresh: Option<Instant> = None;

        enum DueProbe {
            V2Like {
                address: Address,
                local: V2LikeProbeSnapshot,
                variant: Variant,
            },
            Cl {
                address: Address,
                local: ClProbeSnapshot,
                is_pancake: bool,
                kind: DriftProbeKind,
            },
            V4Like {
                address: Address,
                local: V4LiteProbeSnapshot,
                variant: Variant,
                manager_address: Address,
                pool_id: B256,
            },
            CurveNGStable {
                pool: CurveNGPool,
                local: CurveNGStableProbeSnapshot,
            },
            CurveNGCrypto {
                pool: CurveNGPool,
                local: CurveNGCryptoProbeSnapshot,
            },
        }

        loop {
            sleep(scan_tick).await;

            let canonical = canonical_head.load(Ordering::Relaxed);
            if canonical == 0 {
                continue;
            }

            let now = Instant::now();
            let cache_stale = last_cache_refresh
                .map(|t| now.saturating_duration_since(t) >= DRIFT_CANDIDATE_CACHE_TTL)
                .unwrap_or(true);
            if cache_stale
                || (cached_v2_like_addresses.is_empty()
                    && cached_v3_addresses.is_empty()
                    && cached_v4_like_addresses.is_empty()
                    && cached_slipstream_addresses.is_empty()
                    && cached_curve_ng_stable_addresses.is_empty()
                    && cached_curve_ng_crypto_addresses.is_empty())
            {
                let guard = state.read().await;
                cached_v2_like_addresses.clear();
                cached_v3_addresses.clear();
                cached_v4_like_addresses.clear();
                cached_slipstream_addresses.clear();
                cached_curve_ng_stable_addresses.clear();
                cached_curve_ng_crypto_addresses.clear();
                for (addr, amm) in &guard.state {
                    match amm.as_ref() {
                        AMM::UniswapV2Pool(_)
                        | AMM::SushiV2Pool(_)
                        | AMM::PancakeV2Pool(_)
                        | AMM::AerodromeV2Pool(_) => cached_v2_like_addresses.push(*addr),
                        AMM::UniswapV3Pool(_) | AMM::PancakeV3Pool(_) => {
                            cached_v3_addresses.push(*addr)
                        }
                        AMM::UniswapV4Pool(_) | AMM::PancakeInfinityPool(_) => {
                            cached_v4_like_addresses.push(*addr)
                        }
                        AMM::AerodromeSlipstreamPool(_) => cached_slipstream_addresses.push(*addr),
                        AMM::CurveNGPool(pool)
                            if pool.pool_type.is_stable() && pool.n_coins > 0 =>
                        {
                            cached_curve_ng_stable_addresses.push(*addr)
                        }
                        AMM::CurveNGPool(pool)
                            if pool.pool_type.is_crypto() && pool.n_coins > 0 =>
                        {
                            cached_curve_ng_crypto_addresses.push(*addr)
                        }
                        _ => {}
                    }
                }
                cached_v2_like_addresses.sort_unstable();
                cached_v3_addresses.sort_unstable();
                cached_v4_like_addresses.sort_unstable();
                cached_slipstream_addresses.sort_unstable();
                cached_curve_ng_stable_addresses.sort_unstable();
                cached_curve_ng_crypto_addresses.sort_unstable();
                if v2_like_cursor >= cached_v2_like_addresses.len() {
                    v2_like_cursor = 0;
                }
                if v3_cursor >= cached_v3_addresses.len() {
                    v3_cursor = 0;
                }
                if v4_like_cursor >= cached_v4_like_addresses.len() {
                    v4_like_cursor = 0;
                }
                if slipstream_cursor >= cached_slipstream_addresses.len() {
                    slipstream_cursor = 0;
                }
                if curve_ng_stable_cursor >= cached_curve_ng_stable_addresses.len() {
                    curve_ng_stable_cursor = 0;
                }
                if curve_ng_crypto_cursor >= cached_curve_ng_crypto_addresses.len() {
                    curve_ng_crypto_cursor = 0;
                }
                last_cache_refresh = Some(now);
            }

            let mut selected_kind = active_kind;
            let mut selected_addresses = match selected_kind {
                DriftProbeKind::V2Like => &cached_v2_like_addresses,
                DriftProbeKind::V3Like => &cached_v3_addresses,
                DriftProbeKind::V4Like => &cached_v4_like_addresses,
                DriftProbeKind::Slipstream => &cached_slipstream_addresses,
                DriftProbeKind::CurveNGStable => &cached_curve_ng_stable_addresses,
                DriftProbeKind::CurveNGCrypto => &cached_curve_ng_crypto_addresses,
            };
            let mut attempts = 0;
            while selected_addresses.is_empty() && attempts < 5 {
                selected_kind = selected_kind.next();
                selected_addresses = match selected_kind {
                    DriftProbeKind::V2Like => &cached_v2_like_addresses,
                    DriftProbeKind::V3Like => &cached_v3_addresses,
                    DriftProbeKind::V4Like => &cached_v4_like_addresses,
                    DriftProbeKind::Slipstream => &cached_slipstream_addresses,
                    DriftProbeKind::CurveNGStable => &cached_curve_ng_stable_addresses,
                    DriftProbeKind::CurveNGCrypto => &cached_curve_ng_crypto_addresses,
                };
                attempts += 1;
            }
            if selected_addresses.is_empty() {
                continue;
            }
            active_kind = selected_kind.next();

            let cursor = match selected_kind {
                DriftProbeKind::V2Like => &mut v2_like_cursor,
                DriftProbeKind::V3Like => &mut v3_cursor,
                DriftProbeKind::V4Like => &mut v4_like_cursor,
                DriftProbeKind::Slipstream => &mut slipstream_cursor,
                DriftProbeKind::CurveNGStable => &mut curve_ng_stable_cursor,
                DriftProbeKind::CurveNGCrypto => &mut curve_ng_crypto_cursor,
            };
            if *cursor >= selected_addresses.len() {
                *cursor = 0;
            }

            let mut due: Vec<DueProbe> = Vec::new();
            let guard = state.read().await;
            for offset in 0..selected_addresses.len() {
                if due.len() >= DRIFT_MAX_POOLS_PER_TICK {
                    break;
                }
                let idx = (*cursor + offset) % selected_addresses.len();
                let address = selected_addresses[idx];
                let Some(amm) = guard.state.get(&address) else {
                    continue;
                };
                let amm_ref = amm.as_ref();
                let kind = match amm_ref {
                    AMM::UniswapV2Pool(_)
                    | AMM::SushiV2Pool(_)
                    | AMM::PancakeV2Pool(_)
                    | AMM::AerodromeV2Pool(_) => DriftProbeKind::V2Like,
                    AMM::UniswapV3Pool(_) | AMM::PancakeV3Pool(_) => DriftProbeKind::V3Like,
                    AMM::UniswapV4Pool(_) | AMM::PancakeInfinityPool(_) => DriftProbeKind::V4Like,
                    AMM::AerodromeSlipstreamPool(_) => DriftProbeKind::Slipstream,
                    AMM::CurveNGPool(pool) if pool.pool_type.is_stable() && pool.n_coins > 0 => {
                        DriftProbeKind::CurveNGStable
                    }
                    AMM::CurveNGPool(pool) if pool.pool_type.is_crypto() && pool.n_coins > 0 => {
                        DriftProbeKind::CurveNGCrypto
                    }
                    _ => continue,
                };
                if kind != selected_kind {
                    continue;
                }
                let local = match selected_kind {
                    DriftProbeKind::V2Like => {
                        let Some(snapshot) = Self::local_v2_like_probe_snapshot(amm_ref) else {
                            continue;
                        };
                        DueProbe::V2Like {
                            address,
                            local: snapshot,
                            variant: amm_ref.variant(),
                        }
                    }
                    DriftProbeKind::CurveNGCrypto => {
                        let Some(snapshot) = Self::local_curve_ng_crypto_probe_snapshot(amm_ref)
                        else {
                            continue;
                        };
                        let AMM::CurveNGPool(pool) = amm_ref else {
                            continue;
                        };
                        DueProbe::CurveNGCrypto {
                            pool: pool.clone(),
                            local: snapshot,
                        }
                    }
                    DriftProbeKind::CurveNGStable => {
                        let Some(snapshot) = Self::local_curve_ng_stable_probe_snapshot(amm_ref)
                        else {
                            continue;
                        };
                        let AMM::CurveNGPool(pool) = amm_ref else {
                            continue;
                        };
                        DueProbe::CurveNGStable {
                            pool: pool.clone(),
                            local: snapshot,
                        }
                    }
                    DriftProbeKind::V3Like | DriftProbeKind::Slipstream => {
                        let Some(snapshot) = Self::local_cl_probe_snapshot(amm_ref) else {
                            continue;
                        };
                        let is_pancake = matches!(amm_ref, AMM::PancakeV3Pool(_));
                        DueProbe::Cl {
                            address,
                            local: snapshot,
                            is_pancake,
                            kind: selected_kind,
                        }
                    }
                    DriftProbeKind::V4Like => {
                        let Some(snapshot) = Self::local_v4_lite_probe_snapshot(amm_ref) else {
                            continue;
                        };
                        let (variant, manager_address, pool_id) = match amm_ref {
                            AMM::UniswapV4Pool(pool) => {
                                (Variant::UniswapV4Pool, pool.manager_address, pool.pool_id)
                            }
                            AMM::PancakeInfinityPool(pool) => (
                                Variant::PancakeInfinityPool,
                                pool.manager_address,
                                pool.pool_id,
                            ),
                            _ => continue,
                        };
                        DueProbe::V4Like {
                            address,
                            local: snapshot,
                            variant,
                            manager_address,
                            pool_id,
                        }
                    }
                };
                let last_synced_block = amm_ref.last_synced_block();

                // If local state is already ahead of canonical head (common on Base flashblocks),
                // probing against canonical reads would create known transient mismatches.
                // Skip probing until canonical catches up to avoid repeated probe/enqueue churn.
                if last_synced_block > canonical {
                    continue;
                }

                let hot = canonical.saturating_sub(last_synced_block) <= DRIFT_HOT_WINDOW_BLOCKS;
                let interval = if hot {
                    DRIFT_HOT_POOL_INTERVAL
                } else {
                    DRIFT_COLD_POOL_INTERVAL
                };

                if let Some(last) = last_probe_at.get(&address) {
                    if now.saturating_duration_since(*last) < interval {
                        continue;
                    }
                }

                last_probe_at.insert(address, now);
                due.push(local);
            }
            if !selected_addresses.is_empty() {
                // Round-robin advance within the selected probe type.
                let advance = due.len().max(1);
                *cursor = (*cursor + advance) % selected_addresses.len();
            }
            drop(guard);

            if due.is_empty() {
                continue;
            }

            let mut v2_due = Vec::new();
            let mut cl_due = Vec::new();
            let mut v4_due = Vec::new();
            let mut curve_due = Vec::new();
            let mut curve_crypto_due = Vec::new();
            for item in due {
                match item {
                    DueProbe::V2Like {
                        address,
                        local,
                        variant,
                    } => v2_due.push((address, local, variant)),
                    DueProbe::Cl {
                        address,
                        local,
                        is_pancake,
                        kind,
                    } => cl_due.push((address, local, is_pancake, kind)),
                    DueProbe::V4Like {
                        address,
                        local,
                        variant,
                        manager_address,
                        pool_id,
                    } => v4_due.push((address, local, variant, manager_address, pool_id)),
                    DueProbe::CurveNGStable { pool, local } => curve_due.push((pool, local)),
                    DueProbe::CurveNGCrypto { pool, local } => curve_crypto_due.push((pool, local)),
                }
            }

            let mut enqueue_resync = Vec::new();
            let mut enqueue_async = Vec::new();

            if !v2_due.is_empty() {
                let v2_targets: Vec<(Address, Variant)> = v2_due
                    .iter()
                    .map(|(address, _, variant)| (*address, *variant))
                    .collect();
                match Self::fetch_v2_like_probe_snapshots_batch(&provider, &v2_targets, canonical)
                    .await
                {
                    Ok(remote_by_address) => {
                        for (address, local, _) in v2_due {
                            let Some(remote) = remote_by_address.get(&address) else {
                                continue;
                            };
                            if local != *remote {
                                warn!(
                                    ?address,
                                    local_reserve_0 = local.reserve_0,
                                    local_reserve_1 = local.reserve_1,
                                    remote_reserve_0 = remote.reserve_0,
                                    remote_reserve_1 = remote.reserve_1,
                                    "drift_probe: V2Like reserve drift detected; enqueueing resync"
                                );
                                enqueue_resync.push(address);
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            count = v2_targets.len(),
                            "drift_probe: V2Like batch failed: {}", e
                        );
                    }
                }
            }

            if !cl_due.is_empty() {
                let mut remote_by_address: HashMap<Address, ClProbeSnapshot> = HashMap::new();

                let v3_due: Vec<Address> = cl_due
                    .iter()
                    .filter_map(|(address, _, is_pancake, kind)| {
                        if *kind == DriftProbeKind::V3Like && !*is_pancake {
                            Some(*address)
                        } else {
                            None
                        }
                    })
                    .collect();
                if !v3_due.is_empty() {
                    match Self::fetch_v3_probe_snapshots_batch(&provider, &v3_due, canonical).await
                    {
                        Ok(map) => remote_by_address.extend(map),
                        Err(e) => {
                            let failed_addresses =
                                Self::diagnose_v3_probe_failures(&provider, &v3_due, canonical)
                                    .await;
                            let sample: Vec<_> = failed_addresses
                                .iter()
                                .take(8)
                                .map(|a| format!("{a:#x}"))
                                .collect();
                            warn!(
                                count = v3_due.len(),
                                failed_count = failed_addresses.len(),
                                failed_sample = ?sample,
                                "drift_probe: V3 batch failed: {}",
                                e
                            );
                        }
                    }
                }

                let pancake_due: Vec<Address> = cl_due
                    .iter()
                    .filter_map(|(address, _, is_pancake, kind)| {
                        if *kind == DriftProbeKind::V3Like && *is_pancake {
                            Some(*address)
                        } else {
                            None
                        }
                    })
                    .collect();
                if !pancake_due.is_empty() {
                    match Self::fetch_pancake_probe_snapshots_batch(
                        &provider,
                        &pancake_due,
                        canonical,
                    )
                    .await
                    {
                        Ok(map) => remote_by_address.extend(map),
                        Err(e) => {
                            warn!(
                                count = pancake_due.len(),
                                "drift_probe: PancakeV3 batch failed: {}", e
                            );
                        }
                    }
                }

                let slipstream_due: Vec<Address> = cl_due
                    .iter()
                    .filter_map(|(address, _, _, kind)| {
                        if *kind == DriftProbeKind::Slipstream {
                            Some(*address)
                        } else {
                            None
                        }
                    })
                    .collect();
                if !slipstream_due.is_empty() {
                    match Self::fetch_slipstream_probe_snapshots_batch(
                        &provider,
                        &slipstream_due,
                        canonical,
                    )
                    .await
                    {
                        Ok(map) => remote_by_address.extend(map),
                        Err(e) => {
                            let failed_addresses = Self::diagnose_slipstream_probe_failures(
                                &provider,
                                &slipstream_due,
                                canonical,
                            )
                            .await;
                            let sample: Vec<_> = failed_addresses
                                .iter()
                                .take(8)
                                .map(|a| format!("{a:#x}"))
                                .collect();
                            warn!(
                                count = slipstream_due.len(),
                                failed_count = failed_addresses.len(),
                                failed_sample = ?sample,
                                "drift_probe: Slipstream batch failed: {}",
                                e
                            );
                        }
                    }
                }

                for (address, local, _, _) in cl_due {
                    let Some(remote) = remote_by_address.get(&address).copied() else {
                        continue;
                    };

                    if local == remote {
                        continue;
                    }

                    let fee_only = local.sqrt_price == remote.sqrt_price
                        && local.tick == remote.tick
                        && local.liquidity == remote.liquidity
                        && local.fee != remote.fee;

                    if fee_only {
                        if let Some(remote_fee) = remote.fee {
                            let mut guard = state.write().await;
                            if let Some(amm) = guard.get_mut_cow(&address) {
                                if let AMM::AerodromeSlipstreamPool(p) = amm {
                                    p.fee = remote_fee;
                                }
                            }
                        }
                        continue;
                    }

                    warn!(
                        ?address,
                        local_sqrt_price = ?local.sqrt_price,
                        remote_sqrt_price = ?remote.sqrt_price,
                        local_tick = local.tick,
                        remote_tick = remote.tick,
                        local_liquidity = local.liquidity,
                        remote_liquidity = remote.liquidity,
                        "drift_probe: CL state drift detected; enqueueing resync"
                    );
                    enqueue_resync.push(address);
                }
            }

            if !v4_due.is_empty() {
                let v4_targets: Vec<(Address, Variant, Address, B256)> = v4_due
                    .iter()
                    .map(|(address, _, variant, manager_address, pool_id)| {
                        (*address, *variant, *manager_address, *pool_id)
                    })
                    .collect();
                match Self::fetch_v4_lite_probe_snapshots_batch(&provider, &v4_targets, canonical)
                    .await
                {
                    Ok(remote_by_address) => {
                        for (address, local, _, _, _) in v4_due {
                            let Some(remote) = remote_by_address.get(&address) else {
                                continue;
                            };
                            if local != *remote {
                                warn!(
                                    ?address,
                                    local_sqrt_price = ?local.sqrt_price,
                                    remote_sqrt_price = ?remote.sqrt_price,
                                    local_tick = local.tick,
                                    remote_tick = remote.tick,
                                    local_liquidity = local.liquidity,
                                    remote_liquidity = remote.liquidity,
                                    "drift_probe: V4Lite state drift detected; enqueueing resync"
                                );
                                enqueue_resync.push(address);
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            count = v4_targets.len(),
                            "drift_probe: V4Lite batch failed: {}", e
                        );
                    }
                }
            }

            if !curve_due.is_empty() {
                let mut remote_pools: Vec<CurveNGPool> =
                    curve_due.iter().map(|(pool, _)| pool.clone()).collect();

                match Self::fetch_curve_ng_stable_probe_snapshots(
                    &provider,
                    &mut remote_pools,
                    canonical,
                )
                .await
                {
                    Ok(remote_by_address) => {
                        for (pool, local) in curve_due {
                            let address = pool.address;
                            let Some(remote) = remote_by_address.get(&address) else {
                                continue;
                            };
                            match classify_curve_ng_stable_drift(&local, remote) {
                                Some(PendingSyncAction::Resync) => enqueue_resync.push(address),
                                Some(PendingSyncAction::AsyncUpdate) => enqueue_async.push(address),
                                None => {}
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            count = remote_pools.len(),
                            "drift_probe: CurveNG stable batch failed: {}", e
                        );
                    }
                }
            }

            if !curve_crypto_due.is_empty() {
                let crypto_pools: Vec<CurveNGPool> = curve_crypto_due
                    .iter()
                    .map(|(pool, _)| pool.clone())
                    .collect();

                match Self::fetch_curve_ng_crypto_probe_snapshots(
                    &provider,
                    &crypto_pools,
                    canonical,
                )
                .await
                {
                    Ok(remote_by_address) => {
                        for (pool, local) in curve_crypto_due {
                            let address = pool.address;
                            let Some(remote) = remote_by_address.get(&address) else {
                                continue;
                            };
                            match classify_curve_ng_crypto_drift(&local, remote) {
                                Some(PendingSyncAction::Resync) => enqueue_resync.push(address),
                                Some(PendingSyncAction::AsyncUpdate) => enqueue_async.push(address),
                                None => {}
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            count = crypto_pools.len(),
                            "drift_probe: CurveNG crypto batch failed: {}", e
                        );
                    }
                }
            }

            if !enqueue_resync.is_empty() || !enqueue_async.is_empty() {
                let mut queue = pending_sync_queue.lock().await;
                for address in enqueue_resync {
                    queue.enqueue(
                        address,
                        PendingSyncAction::Resync,
                        canonical,
                        PendingSyncReason::DriftProbe,
                    );
                }
                for address in enqueue_async {
                    queue.enqueue(
                        address,
                        PendingSyncAction::AsyncUpdate,
                        canonical,
                        PendingSyncReason::DriftProbe,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_curve_ng_stable_drift_classification() {
        let make = |balances, rates, amp| CurveNGStableProbeSnapshot {
            balances,
            admin_balances: vec![U256::from(1u64), U256::from(2u64)],
            rates,
            rates_asset_types: None,
            amp,
            fee: U256::from(10u64),
            admin_fee: U256::from(5u64),
            offpeg_fee_multiplier: U256::from(1_000_000u64),
        };

        let base = make(
            vec![U256::from(100u64), U256::from(200u64)],
            Some(vec![
                U256::from(1_000_000_000_000_000_000u128),
                U256::from(2_000_000_000_000_000_000u128),
            ]),
            Some(U256::from(100u64)),
        );

        let no_diff = base.clone();
        assert_eq!(classify_curve_ng_stable_drift(&base, &no_diff), None);

        let remote_without_rates = CurveNGStableProbeSnapshot {
            rates: None,
            ..base.clone()
        };
        // If one side does not expose rates, we intentionally skip rate drift classification.
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &remote_without_rates),
            None
        );

        let rate_diff = CurveNGStableProbeSnapshot {
            rates: Some(vec![
                U256::from(1_000_000_000_000_000_001u128),
                U256::from(2_000_000_000_000_000_000u128),
            ]),
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &rate_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        let both_without_rates = CurveNGStableProbeSnapshot {
            rates: None,
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&both_without_rates, &remote_without_rates),
            None
        );

        let balance_diff = CurveNGStableProbeSnapshot {
            balances: vec![U256::from(101u64), U256::from(200u64)],
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &balance_diff),
            Some(PendingSyncAction::Resync)
        );

        let admin_balance_diff = CurveNGStableProbeSnapshot {
            admin_balances: vec![U256::from(1u64), U256::from(3u64)],
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &admin_balance_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        let fee_diff = CurveNGStableProbeSnapshot {
            fee: U256::from(11u64),
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &fee_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        let admin_fee_diff = CurveNGStableProbeSnapshot {
            admin_fee: U256::from(6u64),
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &admin_fee_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        let offpeg_diff = CurveNGStableProbeSnapshot {
            offpeg_fee_multiplier: U256::from(2_000_000u64),
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &offpeg_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        let balance_and_admin_diff = CurveNGStableProbeSnapshot {
            balances: vec![U256::from(101u64), U256::from(200u64)],
            admin_balances: vec![U256::from(1u64), U256::from(3u64)],
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &balance_and_admin_diff),
            Some(PendingSyncAction::Resync)
        );

        // amp drift → AsyncUpdate
        let amp_diff = CurveNGStableProbeSnapshot {
            amp: Some(U256::from(200u64)),
            ..base.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&base, &amp_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        // ERC4626 token (type 3): rates drift should be SKIPPED (no false positive)
        let pool_with_4626 = CurveNGStableProbeSnapshot {
            balances: vec![U256::from(100u64), U256::from(200u64)],
            admin_balances: vec![U256::from(1u64), U256::from(2u64)],
            rates: Some(vec![
                U256::from(1_000_000_000_000_000_000u128),
                U256::from(2_000_000_000_000_000_000u128),
            ]),
            rates_asset_types: Some(vec![0, 3]), // coin 1 is ERC4626
            amp: Some(U256::from(100u64)),
            fee: U256::from(10u64),
            admin_fee: U256::from(5u64),
            offpeg_fee_multiplier: U256::from(1_000_000u64),
        };
        let remote_4626_drift = CurveNGStableProbeSnapshot {
            rates: Some(vec![
                U256::from(1_000_000_000_000_000_000u128), // Standard coin 0: same
                U256::from(2_000_000_000_000_000_001u128), // ERC4626 coin 1: drifted (should be skipped)
            ]),
            ..pool_with_4626.clone()
        };
        // Both rates differ at coin 0? No — coin 0 is standard and matches.
        // Only coin 1 (ERC4626) differs, which should be skipped → no drift detected.
        assert_eq!(
            classify_curve_ng_stable_drift(&pool_with_4626, &remote_4626_drift),
            None
        );

        // ERC4626 coin 0 also drifts → should still skip ERC4626 but detect standard drift
        let remote_standard_drift = CurveNGStableProbeSnapshot {
            rates: Some(vec![
                U256::from(1_000_000_000_000_000_001u128), // Standard coin 0: drifted
                U256::from(2_000_000_000_000_000_001u128), // ERC4626 coin 1: drifted (skipped)
            ]),
            ..pool_with_4626.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&pool_with_4626, &remote_standard_drift),
            Some(PendingSyncAction::AsyncUpdate)
        );

        // asset_types unknown (None) → compare all rates normally (legacy behavior)
        let unknown_at = CurveNGStableProbeSnapshot {
            rates_asset_types: None,
            ..pool_with_4626.clone()
        };
        let remote_all_drift = CurveNGStableProbeSnapshot {
            rates: Some(vec![
                U256::from(1_000_000_000_000_000_001u128),
                U256::from(2_000_000_000_000_000_001u128),
            ]),
            ..unknown_at.clone()
        };
        assert_eq!(
            classify_curve_ng_stable_drift(&unknown_at, &remote_all_drift),
            Some(PendingSyncAction::AsyncUpdate)
        );
    }

    #[test]
    fn test_curve_ng_crypto_drift_classification() {
        let base = CurveNGCryptoProbeSnapshot {
            balances: vec![U256::from(100u64), U256::from(200u64)],
            price_scale: vec![U256::from(1_000_000u64)],
            d: Some(U256::from(300u64)),
        };

        let no_diff = base.clone();
        assert_eq!(classify_curve_ng_crypto_drift(&base, &no_diff), None);

        // Balance drift → Resync
        let balance_diff = CurveNGCryptoProbeSnapshot {
            balances: vec![U256::from(101u64), U256::from(200u64)],
            price_scale: base.price_scale.clone(),
            d: base.d,
        };
        assert_eq!(
            classify_curve_ng_crypto_drift(&base, &balance_diff),
            Some(PendingSyncAction::Resync)
        );

        // price_scale drift → AsyncUpdate
        let ps_diff = CurveNGCryptoProbeSnapshot {
            balances: base.balances.clone(),
            price_scale: vec![U256::from(1_000_001u64)],
            d: base.d,
        };
        assert_eq!(
            classify_curve_ng_crypto_drift(&base, &ps_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        // D drift → AsyncUpdate
        let d_diff = CurveNGCryptoProbeSnapshot {
            balances: base.balances.clone(),
            price_scale: base.price_scale.clone(),
            d: Some(U256::from(301u64)),
        };
        assert_eq!(
            classify_curve_ng_crypto_drift(&base, &d_diff),
            Some(PendingSyncAction::AsyncUpdate)
        );

        // D None vs Some → differs → AsyncUpdate
        let d_none = CurveNGCryptoProbeSnapshot {
            balances: base.balances.clone(),
            price_scale: base.price_scale.clone(),
            d: None,
        };
        assert_eq!(
            classify_curve_ng_crypto_drift(&base, &d_none),
            Some(PendingSyncAction::AsyncUpdate)
        );
    }

    /// BinaryFi AsyncUpdate 的写回语义：对 current existing 合并（不整只替换）、
    /// 水位只前进不回退、只清快照实际覆盖的 stale pair。
    #[test]
    fn binaryfi_async_update_merge_keeps_watermark_and_untouched_stale() {
        use crate::amms::binaryfi_prop::{BinaryFiPropPool, Snapshot};

        let empty_snapshot = Snapshot {
            assets: vec![],
            decimals: vec![],
            scales: vec![],
            poolBalances: vec![],
            vaultReserves: vec![],
            vaultBalances: vec![],
            quotePairs: vec![],
            quotes: vec![],
            fee: U256::ZERO,
        };

        let mut pool = BinaryFiPropPool::default();
        pool.set_last_synced_block(100);
        pool.stale_pairs = vec![7, 9];

        // 陈旧快照（S=50 < 本地 100）：不得把水位拉回去，且只清它覆盖的 pair。
        merge_binaryfi_snapshot(&mut pool, &empty_snapshot, 50, &[7]);
        assert_eq!(pool.last_synced_block(), 100, "水位不得回退");
        assert_eq!(pool.stale_pairs, vec![9], "窗口内新标记的 stale 必须保留");

        // 更新的快照（S=200）：推进水位并清掉覆盖到的 pair。
        merge_binaryfi_snapshot(&mut pool, &empty_snapshot, 200, &[9]);
        assert_eq!(pool.last_synced_block(), 200);
        assert!(pool.stale_pairs.is_empty());
    }

    #[test]
    fn test_should_skip_async_apply_when_local_is_newer() {
        assert!(should_skip_async_apply(101, 100));
        assert!(!should_skip_async_apply(100, 100));
        assert!(!should_skip_async_apply(99, 100));
    }

    /// 回归（2026-09-11 事故）：Caliber 的 Resync 必须绕过块级水位闸门。
    ///
    /// Caliber 的 `batchUpdateParameters` 是 raw-tx 更新，`apply_batch_update`
    /// 把 `last_synced_block` 顶到 flashblock 乐观头；而 Resync 的
    /// `required_block` 取自请求时的 canonical head（落后乐观头 ≥1 块）→ 通用
    /// 闸门 `last_synced_block() > target_block` 恒真，任务被 `postpone` 无限后移
    /// （日志 `Pending sync task deferred: local state newer than target block`），
    /// 发单后立即纠错与 maintenance 覆盖对账两条通道全部饿死。
    #[test]
    fn resync_skips_block_watermark_gate_for_rawtx_driven_props() {
        use crate::amms::Token;

        let contract = address!("0x154586b2479b9a11e3d4db90024dc0e26f097312");
        let pair_id = B256::from([0x11u8; 32]);
        let caliber = AMM::CaliberPropPool(crate::amms::caliber_prop::CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address:
                crate::amms::caliber_prop::CaliberPropPool::virtual_address_from_pair_id(
                    pair_id, contract,
                ),
            token_x: Address::from([1u8; 20]),
            token_y: Address::from([2u8; 20]),
            token_a: Token::new_with_decimals(Address::from([1u8; 20]), 18),
            token_b: Token::new_with_decimals(Address::from([2u8; 20]), 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1_000u64),
            reserve_b: U256::from(1_000u64),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
            swap_ledger: Default::default(),
        });
        let elfomo = AMM::ElfomoFiPropPool(crate::amms::elfomo_prop::ElfomoFiPropPool::default());
        // BinaryFi 的水位只由快照 `apply_snapshot` 推进（raw L2 update 不动它），
        // 因此仍走通用闸门。
        let binaryfi =
            AMM::BinaryFiPropPool(crate::amms::binaryfi_prop::BinaryFiPropPool::default());
        let other = AMM::FermiPropPool(crate::amms::fermi_prop::FermiPropPool::default());

        assert!(resync_skips_block_watermark_gate(&caliber));
        assert!(resync_skips_block_watermark_gate(&elfomo));
        assert!(!resync_skips_block_watermark_gate(&binaryfi));
        assert!(!resync_skips_block_watermark_gate(&other));
    }

    /// 水位契约（`AutomatedMarketMaker::set_last_synced_block`）：只前进不回退。
    ///
    /// 2026-09 审查发现 `caliber_prop` / `curve_legacy` / `fermi_prop` 三处实现
    /// 是普通赋值，会让更旧的块号把水位拉回去；本测试覆盖当时偏差的池型，防止
    /// 再次复制粘贴回退。
    #[test]
    fn set_last_synced_block_is_monotonic_across_pool_types() {
        let mut fermi = AMM::FermiPropPool(crate::amms::fermi_prop::FermiPropPool::default());
        let mut curve = AMM::CurveLegacyPool(crate::amms::curve_legacy::CurveLegacyPool::new(
            address!("0x1111111111111111111111111111111111111111"),
            crate::amms::curve_legacy::CurveLegacyPoolType::StableSwap,
        ));
        let mut binaryfi =
            AMM::BinaryFiPropPool(crate::amms::binaryfi_prop::BinaryFiPropPool::default());

        for amm in [&mut fermi, &mut curve, &mut binaryfi] {
            amm.set_last_synced_block(500);
            assert_eq!(amm.last_synced_block(), 500);
            amm.set_last_synced_block(400);
            assert_eq!(amm.last_synced_block(), 500, "水位不得回退到更旧的块号");
            amm.set_last_synced_block(600);
            assert_eq!(amm.last_synced_block(), 600);
        }
    }

    #[test]
    fn test_binaryfi_async_update_not_skipped_by_newer_local_state() {
        // BinaryFi：即使 existing 已新于快照克隆块，也不得竞态丢弃快照——
        // 快照携带事件流无法提供的 quote/bid/容量/费率，丢弃会让恢复通道失效。
        let pool = crate::amms::binaryfi_prop::BinaryFiPropPool::default();
        let amm = AMM::BinaryFiPropPool(pool);
        assert!(!should_skip_async_apply_for(&amm, 101, 100));
        assert!(!should_skip_async_apply_for(&amm, 100, 100));
        assert!(!should_skip_async_apply_for(&amm, 99, 100));
    }

    #[test]
    fn test_non_binaryfi_async_update_keeps_stale_skip_semantics() {
        // 非 BinaryFi（此处用 Fermi prop 池代表）：existing 新于快照克隆块 → 仍跳过，
        // 与既有 should_skip_async_apply 语义一致，改动不影响其他 AMM。
        let pool = crate::amms::fermi_prop::FermiPropPool::default();
        let amm = AMM::FermiPropPool(pool);
        assert!(should_skip_async_apply_for(&amm, 101, 100));
        assert!(!should_skip_async_apply_for(&amm, 100, 100));
    }

    #[test]
    fn test_pending_queue_keeps_canonical_gate() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000aa");
        queue.enqueue(
            addr,
            PendingSyncAction::AsyncUpdate,
            120,
            PendingSyncReason::AsyncUpdate,
        );

        let none_due = queue.claim_due_non_coverage(119, usize::MAX);
        assert!(
            none_due.is_empty(),
            "task must remain blocked before canonical"
        );

        let due = queue.claim_due_non_coverage(120, usize::MAX);
        assert_eq!(
            due.len(),
            1,
            "task should be claimable at canonical boundary"
        );
    }

    #[test]
    fn test_pending_queue_postpone_defers_to_newer_block() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000ad");

        queue.enqueue(
            addr,
            PendingSyncAction::Resync,
            100,
            PendingSyncReason::Resync,
        );

        // 本地实时状态已到 120，目标块 100 → 推迟到 120
        queue.postpone(
            addr,
            PendingSyncAction::Resync,
            PendingSyncReason::Resync,
            120,
        );

        // canonical 100 时不应再被认领
        let none_due = queue.claim_due_non_coverage(100, usize::MAX);
        assert!(
            none_due.is_empty(),
            "deferred task must not be claimable at old block"
        );

        // canonical 120 时重新可认领，action/reason 保留
        let due = queue.claim_due_non_coverage(120, usize::MAX);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1.required_block, 120);
        assert_eq!(due[0].1.action, PendingSyncAction::Resync);
    }

    #[test]
    fn test_pending_queue_postpone_same_block_resets_retry() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000ae");

        queue.enqueue(
            addr,
            PendingSyncAction::Resync,
            120,
            PendingSyncReason::Resync,
        );
        let _due = queue.claim_due_non_coverage(120, usize::MAX);
        queue.on_failure(addr, None);

        queue.postpone(
            addr,
            PendingSyncAction::Resync,
            PendingSyncReason::Resync,
            120,
        );

        let queue_ref = queue.tasks.get(&addr).expect("task must remain queued");
        assert_eq!(
            queue_ref.tasks.len(),
            1,
            "postpone must not duplicate tasks"
        );
        let task = queue_ref.tasks.front().unwrap();
        assert_eq!(task.required_block, 120);
        assert_eq!(task.retry_count, 0, "postpone must reset retry backoff");
    }

    #[test]
    fn test_pending_queue_merges_same_block_but_preserves_later_blocks() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000ab");

        queue.enqueue(
            addr,
            PendingSyncAction::AsyncUpdate,
            120,
            PendingSyncReason::AsyncUpdate,
        );
        queue.enqueue(
            addr,
            PendingSyncAction::Resync,
            120,
            PendingSyncReason::Resync,
        );
        queue.enqueue(
            addr,
            PendingSyncAction::AsyncUpdate,
            121,
            PendingSyncReason::AsyncUpdate,
        );

        let due_120 = queue.claim_due_non_coverage(120, usize::MAX);
        assert_eq!(due_120.len(), 1, "same-block requests should coalesce");
        assert_eq!(due_120[0].1.required_block, 120);
        assert_eq!(
            due_120[0].1.action,
            PendingSyncAction::Resync,
            "same-block merge should preserve the stronger action"
        );

        queue.complete_success(addr, 120);

        let due_121 = queue.claim_due_non_coverage(121, usize::MAX);
        assert_eq!(
            due_121.len(),
            1,
            "later block should remain queued separately"
        );
        assert_eq!(due_121[0].1.required_block, 121);
    }

    #[test]
    fn test_inflight_task_does_not_absorb_later_block() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000ac");

        queue.enqueue(
            addr,
            PendingSyncAction::AsyncUpdate,
            200,
            PendingSyncReason::AsyncUpdate,
        );

        let due = queue.claim_due_non_coverage(200, usize::MAX);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1.required_block, 200);

        queue.enqueue(
            addr,
            PendingSyncAction::AsyncUpdate,
            201,
            PendingSyncReason::AsyncUpdate,
        );

        queue.complete_success(addr, 200);

        let due_next = queue.claim_due_non_coverage(201, usize::MAX);
        assert_eq!(due_next.len(), 1);
        assert_eq!(due_next[0].1.required_block, 201);
    }

    #[test]
    fn test_retry_later_keeps_block_and_throttles() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000ad");

        queue.enqueue(
            addr,
            PendingSyncAction::Resync,
            68872128,
            PendingSyncReason::Resync,
        );
        let claimed = queue.claim_due_non_coverage(68872128, usize::MAX);
        assert_eq!(claimed.len(), 1);

        queue.retry_later(addr, 68872128);

        let task = queue
            .tasks
            .get(&addr)
            .and_then(|q| q.tasks.front())
            .expect("task must stay queued after BlockNotAvailable retry");
        assert_eq!(
            task.required_block, 68872128,
            "must keep the exact target block"
        );
        assert_eq!(task.retry_count, 1);
        assert!(
            task.next_retry_at > Instant::now(),
            "must not retry immediately (busy-loop guard)"
        );

        // 退避未到期前不可再次出队
        let none_due = queue.claim_due_non_coverage(68872128, usize::MAX);
        assert!(
            none_due.is_empty(),
            "throttled task must not be claimed early"
        );

        // 第二次 retry_later：退避继续增长，不换块、不重复入队
        queue.retry_later(addr, 68872128);
        let task = queue
            .tasks
            .get(&addr)
            .and_then(|q| q.tasks.front())
            .expect("task must stay queued");
        assert_eq!(task.required_block, 68872128);
        assert_eq!(task.retry_count, 2);
        let queue_ref = queue.tasks.get(&addr).unwrap();
        assert_eq!(
            queue_ref.tasks.len(),
            1,
            "retry_later must not duplicate tasks"
        );
    }

    #[test]
    fn test_retry_later_re_enqueues_on_new_block() {
        let mut queue = PendingSyncQueue::default();
        let addr = address!("00000000000000000000000000000000000000ae");

        queue.enqueue(
            addr,
            PendingSyncAction::Resync,
            100,
            PendingSyncReason::Resync,
        );
        let _ = queue.claim_due_non_coverage(100, usize::MAX);
        queue.retry_later(addr, 110);

        let task = queue
            .tasks
            .get(&addr)
            .and_then(|q| q.tasks.front())
            .expect("task must be re-queued at new block");
        assert_eq!(task.required_block, 110);
        assert_eq!(task.retry_count, 0, "new-block requeue starts fresh");
    }
}
