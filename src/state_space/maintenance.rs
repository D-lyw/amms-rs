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
use tracing::{info, warn};

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
    rates: Option<Vec<U256>>,
    /// Per-rate asset type (0=Standard, 1=Oracle, 2=Rebasing, 3=ERC4626).
    /// Only populated when rates is Some. None means asset type unknown → compare all rates.
    rates_asset_types: Option<Vec<u8>>,
    amp: Option<U256>,
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
// Rates/amp drift → AsyncUpdate (non-event-driven slow variables, lightweight refresh).
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

    pub(super) fn claim_due_non_coverage_for_addresses(
        &mut self,
        canonical_head: u64,
        max_items: usize,
        addresses: &HashSet<Address>,
    ) -> Vec<(Address, PendingSyncTask)> {
        let now = Instant::now();
        let mut due: Vec<(Address, PendingSyncTask)> = self
            .tasks
            .iter()
            .filter_map(|(addr, queue)| {
                let task = queue.tasks.front()?;
                (addresses.contains(addr)
                    && !self.in_flight.iter().any(|in_flight| *in_flight == *addr)
                    && task.required_block <= canonical_head
                    && task.next_retry_at <= now
                    && task.reason != PendingSyncReason::MaintenanceCoverage)
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
        let Some(mut local_amm) = ({ state.read().await.get(&address).cloned() }) else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };

        match task.action {
            PendingSyncAction::AsyncUpdate => {
                let snapshot_last_synced_block = local_amm.last_synced_block();
                // AsyncUpdate: no last_synced_block guard.
                // RPC availability is already guaranteed by claim_due_filtered's
                // required_block ≤ canonical_head check at pop time.
                // On Ethereum this is always safe (canonical == realtime).
                local_amm.update::<N, _>(provider.clone()).await?;
                local_amm.set_last_synced_block(target_block);
                let mut guard = state.write().await;
                if let Some(existing) = guard.get(&address) {
                    if should_skip_async_apply(
                        existing.last_synced_block(),
                        snapshot_last_synced_block,
                    ) {
                        return Ok(PendingExecutionOutcome::SkippedStale);
                    }
                }
                guard.insert_amm(local_amm);
                Ok(PendingExecutionOutcome::Applied)
            }
            PendingSyncAction::Resync => {
                let variant = local_amm.variant();
                let mut refreshed = variant
                    .sync_all_pools::<N, _>(
                        vec![local_amm],
                        BlockId::from(target_block),
                        provider.clone(),
                    )
                    .await?;

                let Some(mut synced) = refreshed.pop() else {
                    return Err(AMMError::Msg(format!(
                        "Resync returned empty result for pool {address:?}"
                    )));
                };
                synced.set_last_synced_block(target_block);
                let mut guard = state.write().await;
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
        Self::execute_due_tasks(provider, state, pending_sync_queue, canonical, due_tasks).await;

        Ok(())
    }

    pub(super) async fn drain_pending_sync_queue_for_addresses(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        canonical_head: &Arc<AtomicU64>,
        addresses: &[Address],
        max_items: usize,
    ) -> Result<(), StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        if addresses.is_empty() || max_items == 0 {
            return Ok(());
        }

        let canonical = canonical_head.load(Ordering::Relaxed);
        if canonical == 0 {
            return Ok(());
        }

        let addresses: HashSet<Address> = addresses.iter().copied().collect();
        let due_tasks = {
            let mut queue = pending_sync_queue.lock().await;
            queue.claim_due_non_coverage_for_addresses(canonical, max_items, &addresses)
        };

        Self::execute_due_tasks(provider, state, pending_sync_queue, canonical, due_tasks).await;

        Ok(())
    }

    async fn execute_due_tasks(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        pending_sync_queue: &Arc<Mutex<PendingSyncQueue>>,
        canonical: u64,
        due_tasks: Vec<(Address, PendingSyncTask)>,
    ) where
        P: Provider<N> + Clone,
        N: Network,
    {
        for (address, task) in due_tasks {
            match Self::execute_pending_task(provider, state, address, &task, canonical).await {
                Ok(PendingExecutionOutcome::Applied) => {
                    if matches!(task.action, PendingSyncAction::AsyncUpdate)
                        && matches!(task.reason, PendingSyncReason::AsyncUpdate)
                    {
                        info!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                            target_block = canonical,
                            "Pending sync task applied"
                        );
                    } else {
                        warn!(
                            ?address,
                            action = ?task.action,
                            reason = ?task.reason,
                            first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                            target_block = canonical,
                            "Pending sync task applied"
                        );
                    }
                    pending_sync_queue
                        .lock()
                        .await
                        .complete_success(address, canonical);
                }
                Ok(PendingExecutionOutcome::SkippedStale) => {
                    warn!(
                        ?address,
                        action = ?task.action,
                        reason = ?task.reason,
                        first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                        target_block = canonical,
                        "Pending sync task skipped due to newer local state"
                    );
                    pending_sync_queue
                        .lock()
                        .await
                        .complete_success(address, canonical);
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
            rates,
            rates_asset_types: None,
            amp,
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
            rates: Some(vec![
                U256::from(1_000_000_000_000_000_000u128),
                U256::from(2_000_000_000_000_000_000u128),
            ]),
            rates_asset_types: Some(vec![0, 3]), // coin 1 is ERC4626
            amp: Some(U256::from(100u64)),
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

    #[test]
    fn test_should_skip_async_apply_when_local_is_newer() {
        assert!(should_skip_async_apply(101, 100));
        assert!(!should_skip_async_apply(100, 100));
        assert!(!should_skip_async_apply(99, 100));
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
}
