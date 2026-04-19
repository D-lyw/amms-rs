use super::{ISlipstreamStateProbe, IV3StateProbe, StateSpace, StateSpaceManager};
use crate::amms::amm::{AutomatedMarketMaker, SyncAction, AMM};
use crate::amms::error::AMMError;
use crate::state_space::error::StateSpaceError;
use alloy::eips::BlockId;
use alloy::network::Network;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use futures::stream;
use futures::StreamExt;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

pub(super) const DRIFT_HOT_POOL_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const DRIFT_COLD_POOL_INTERVAL: Duration = Duration::from_secs(300);
pub(super) const DRIFT_MISMATCH_TRIGGER: u8 = 2;
pub(super) const DRIFT_MAX_POOLS_PER_TICK: usize = 32;
pub(super) const DRIFT_HOT_WINDOW_BLOCKS: u64 = 60;
pub(super) const MAINT_COVERAGE_BATCH_SIZE: usize = 80;

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
pub(super) struct PendingSyncQueue {
    tasks: HashMap<Address, PendingSyncTask>,
    in_flight: HashSet<Address>,
}

enum PendingExecutionOutcome {
    Applied,
    MissingPool,
    Deferred { required_block: u64 },
}

impl PendingSyncQueue {
    pub(super) fn enqueue(
        &mut self,
        address: Address,
        action: PendingSyncAction,
        required_block: u64,
        reason: PendingSyncReason,
    ) {
        let now = Instant::now();
        match self.tasks.get_mut(&address) {
            Some(existing) => {
                existing.required_block = existing.required_block.max(required_block);
                if action.priority() > existing.action.priority() {
                    existing.action = action;
                }
                if reason.priority() >= existing.reason.priority() {
                    existing.reason = reason;
                }
            }
            None => {
                self.tasks.insert(
                    address,
                    PendingSyncTask {
                        action,
                        required_block,
                        reason,
                        retry_count: 0,
                        next_retry_at: now,
                        first_seen_at: now,
                    },
                );
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
            .filter(|(addr, task)| {
                !self.in_flight.iter().any(|in_flight| in_flight == *addr)
                    && task.required_block <= canonical_head
                    && task.next_retry_at <= now
                    && filter(task)
            })
            .map(|(addr, task)| (*addr, task.clone()))
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
    ) -> Vec<(Address, PendingSyncTask)> {
        self.claim_due_filtered(canonical_head, usize::MAX, |task| {
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
        if let Some(task) = self.tasks.get_mut(&address) {
            if task.required_block > executed_block {
                task.retry_count = 0;
                task.next_retry_at = Instant::now();
                return;
            }
        }
        self.tasks.remove(&address);
    }

    pub(super) fn drop_task(&mut self, address: Address) {
        self.in_flight.remove(&address);
        self.tasks.remove(&address);
    }

    pub(super) fn on_failure(&mut self, address: Address, maybe_next_required_block: Option<u64>) {
        self.in_flight.remove(&address);
        if let Some(task) = self.tasks.get_mut(&address) {
            task.retry_count = task.retry_count.saturating_add(1);
            let exp = task.retry_count.min(5);
            let delay_ms = (200u64).saturating_mul(2u64.saturating_pow(exp));
            task.next_retry_at = Instant::now() + Duration::from_millis(delay_ms.min(10_000));
            if let Some(required) = maybe_next_required_block {
                task.required_block = task.required_block.max(required);
            }
        }
    }

    pub(super) fn defer_task(&mut self, address: Address, required_block: u64) {
        self.in_flight.remove(&address);
        if let Some(task) = self.tasks.get_mut(&address) {
            task.required_block = task.required_block.max(required_block);
            task.next_retry_at = Instant::now();
        }
    }
}

impl<N, P> StateSpaceManager<N, P> {
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
                        .push(amm.clone());
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
                guard.state.insert(address, pool);
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
        let Some(mut local_amm) = ({ state.read().await.state.get(&address).cloned() }) else {
            return Ok(PendingExecutionOutcome::MissingPool);
        };

        if local_amm.last_synced_block() > target_block {
            return Ok(PendingExecutionOutcome::Deferred {
                required_block: local_amm.last_synced_block(),
            });
        };

        match task.action {
            PendingSyncAction::AsyncUpdate => {
                local_amm.update::<N, _>(provider.clone()).await?;
                local_amm.set_last_synced_block(target_block);
                let mut guard = state.write().await;
                if let Some(existing) = guard.state.get(&address) {
                    if existing.last_synced_block() > target_block {
                        return Ok(PendingExecutionOutcome::Deferred {
                            required_block: existing.last_synced_block(),
                        });
                    }
                }
                guard.state.insert(address, local_amm);
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
                if let Some(existing) = guard.state.get(&address) {
                    if existing.last_synced_block() > target_block {
                        return Ok(PendingExecutionOutcome::Deferred {
                            required_block: existing.last_synced_block(),
                        });
                    }
                }
                guard.state.insert(address, synced);
                Ok(PendingExecutionOutcome::Applied)
            }
        }
    }

    fn is_recoverable_delay_error(err: &AMMError) -> bool {
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("block not found")
            || msg.contains("header not found")
            || msg.contains("requested to block")
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
                queue.claim_due_non_coverage(canonical)
            }
        };

        for (address, task) in due_tasks {
            match Self::execute_pending_task(provider, state, address, &task, canonical).await {
                Ok(PendingExecutionOutcome::Applied) => {
                    info!(
                        ?address,
                        action = ?task.action,
                        reason = ?task.reason,
                        first_seen_ms = task.first_seen_at.elapsed().as_millis(),
                        target_block = canonical,
                        "Pending sync task applied"
                    );
                    pending_sync_queue
                        .lock()
                        .await
                        .complete_success(address, canonical);
                }
                Ok(PendingExecutionOutcome::MissingPool) => {
                    pending_sync_queue.lock().await.drop_task(address);
                }
                Ok(PendingExecutionOutcome::Deferred { required_block }) => {
                    pending_sync_queue
                        .lock()
                        .await
                        .defer_task(address, required_block);
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

        Ok(())
    }

    pub(super) async fn run_pending_sync_worker(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        canonical_head: Arc<AtomicU64>,
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
            sleep(Duration::from_secs(2)).await;
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
            for (address, _) in pools.into_iter().take(coverage_batch) {
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

    fn local_cl_probe_snapshot(amm: &AMM) -> Option<(U256, i32, u128)> {
        match amm {
            AMM::UniswapV3Pool(pool) => Some((pool.sqrt_price, pool.tick, pool.liquidity)),
            AMM::PancakeV3Pool(pool) => Some((pool.sqrt_price, pool.tick, pool.liquidity)),
            AMM::AerodromeSlipstreamPool(pool) => {
                Some((pool.sqrt_price, pool.tick, pool.liquidity))
            }
            _ => None,
        }
    }

    async fn fetch_cl_probe_snapshot(
        provider: &P,
        amm: &AMM,
        block: u64,
    ) -> Result<Option<(U256, i32, u128)>, AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let block = BlockId::from(block);
        match amm {
            AMM::UniswapV3Pool(pool) => {
                let contract = IV3StateProbe::new(pool.address, provider.clone());
                let slot0 = contract.slot0().block(block).call().await?;
                let liquidity = contract.liquidity().block(block).call().await?;
                Ok(Some((
                    slot0.sqrtPriceX96.to(),
                    slot0.tick.as_i32(),
                    liquidity,
                )))
            }
            AMM::PancakeV3Pool(pool) => {
                let contract = IV3StateProbe::new(pool.address, provider.clone());
                let slot0 = contract.slot0().block(block).call().await?;
                let liquidity = contract.liquidity().block(block).call().await?;
                Ok(Some((
                    slot0.sqrtPriceX96.to(),
                    slot0.tick.as_i32(),
                    liquidity,
                )))
            }
            AMM::AerodromeSlipstreamPool(pool) => {
                let contract = ISlipstreamStateProbe::new(pool.address, provider.clone());
                let slot0 = contract.slot0().block(block).call().await?;
                let liquidity = contract.liquidity().block(block).call().await?;
                Ok(Some((
                    slot0.sqrtPriceX96.to(),
                    slot0.tick.as_i32(),
                    liquidity,
                )))
            }
            _ => Ok(None),
        }
    }

    pub(super) async fn run_silent_drift_probe_task(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        canonical_head: Arc<AtomicU64>,
    ) where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        let mut mismatch_streak: HashMap<Address, u8> = HashMap::new();
        let mut last_probe_at: HashMap<Address, Instant> = HashMap::new();
        let scan_tick = Duration::from_secs(10);
        let probe_concurrency = 4usize;

        loop {
            sleep(scan_tick).await;

            let canonical = canonical_head.load(Ordering::Relaxed);
            if canonical == 0 {
                continue;
            }

            let mut candidates: Vec<(Address, AMM)> = {
                let guard = state.read().await;
                guard
                    .state
                    .iter()
                    .filter_map(|(addr, amm)| {
                        if Self::local_cl_probe_snapshot(amm).is_some() {
                            Some((*addr, amm.clone()))
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            if candidates.is_empty() {
                continue;
            }

            candidates.sort_by_key(|(addr, _)| *addr);
            let now = Instant::now();
            let mut due = Vec::new();

            for (address, amm) in candidates {
                if due.len() >= DRIFT_MAX_POOLS_PER_TICK {
                    break;
                }

                // If local state is already ahead of canonical head (common on Base flashblocks),
                // probing against canonical reads would create known transient mismatches.
                // Skip probing until canonical catches up to avoid repeated probe/enqueue churn.
                if amm.last_synced_block() > canonical {
                    mismatch_streak.remove(&address);
                    continue;
                }

                let hot =
                    canonical.saturating_sub(amm.last_synced_block()) <= DRIFT_HOT_WINDOW_BLOCKS;
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

                let Some(local) = Self::local_cl_probe_snapshot(&amm) else {
                    continue;
                };

                last_probe_at.insert(address, now);
                due.push((address, amm, local));
            }

            if due.is_empty() {
                continue;
            }

            let results = stream::iter(due.into_iter().map(|(address, amm, local)| {
                let provider = provider.clone();
                async move {
                    (
                        address,
                        local,
                        Self::fetch_cl_probe_snapshot(&provider, &amm, canonical).await,
                    )
                }
            }))
            .buffer_unordered(probe_concurrency)
            .collect::<Vec<_>>()
            .await;

            let mut enqueue_resync = Vec::new();
            for (address, local, remote_res) in results {
                match remote_res {
                    Ok(Some(remote)) => {
                        if local == remote {
                            mismatch_streak.remove(&address);
                            continue;
                        }

                        let streak = mismatch_streak
                            .entry(address)
                            .and_modify(|v| *v = v.saturating_add(1))
                            .or_insert(1);

                        if *streak >= DRIFT_MISMATCH_TRIGGER {
                            enqueue_resync.push(address);
                            mismatch_streak.remove(&address);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(?address, "drift probe RPC failed: {}", e);
                    }
                }
            }

            if !enqueue_resync.is_empty() {
                let mut queue = pending_sync_queue.lock().await;
                for address in enqueue_resync {
                    queue.enqueue(
                        address,
                        PendingSyncAction::Resync,
                        canonical,
                        PendingSyncReason::DriftProbe,
                    );
                }
            }
        }
    }
}
