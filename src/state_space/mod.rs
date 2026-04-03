pub mod cache;
pub mod discovery;
pub mod error;
pub mod filters;
pub mod hooks;
pub mod sync_services;

use crate::amms::amm::AutomatedMarketMaker;
use crate::amms::amm::{SyncAction, AMM};
use crate::amms::error::AMMError;
use crate::amms::factory::Factory;
use crate::amms::fluid_dex::get_liquidity_layer;
use crate::amms::{
    aerodrome_slipstream::{ICustomFeeModule, BASE_SLIPSTREAM_FACTORY},
    balancer_v2, balancer_v3, ekubo,
};
use crate::state_space::hooks::HookHandle;
use crate::state_space::hooks::HookRegistry;
use crate::state_space::hooks::SnapshotConfig;
use crate::state_space::hooks::StateHook;

use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::network::primitives::HeaderResponse;
use alloy::network::Network;

use alloy::primitives::{Address, Bloom, BloomInput, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::{eth::Log, Block, Filter, FilterSet};
use alloy::sol;
use alloy::sol_types::SolEvent;
use async_stream::stream;
use cache::StateChange;
use cache::StateChangeCache;

use error::StateSpaceError;
use filters::AMMFilter;
use filters::PoolFilter;
use futures::stream::FuturesUnordered;
use futures::Stream;
use futures::StreamExt;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::{collections::HashMap, future::Future, marker::PhantomData, sync::Arc};
use tokio::sync::RwLock;

use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub const CACHE_SIZE: usize = 100;

#[derive(Clone)]
pub struct StateSpaceManager<N, P> {
    pub state: Arc<RwLock<StateSpace>>,
    pub block_filter: Filter,
    pub provider: P,
    pub latest_block: Arc<AtomicU64>,
    hooks: HookRegistry<Vec<Address>>,
    phantom: PhantomData<N>,
}

const LOG_ADDRESS_CHUNK_SIZE: usize = 200;
const BASE_CHAIN_ID: u64 = 8453;
const ARBITRUM_CHAIN_ID: u64 = 42161;
const ETHEREUM_MAINNET_CHAIN_ID: u64 = 1;

sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    interface ICLFactoryReader {
        function swapFeeModule() external view returns (address);
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
    fn filter(&self, from_block: u64, to_block: u64) -> Filter {
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

impl<N, P> StateSpaceManager<N, P> {
    /// Registers a hook to be called on every state change.
    pub async fn register_hook(&self, hook: StateHook<Vec<Address>>) -> HookHandle<Vec<Address>> {
        self.hooks.register(hook).await
    }

    /// Subscribes to AMM state changes via newHeads + eth_getLogs pull mode.
    ///
    /// Flow: newHeads(N) -> logsBloom prefilter -> eth_getLogs(block=N) -> sync
    pub async fn subscribe(
        &self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
        StateSpaceError,
    >
    where
        P: Provider<N> + Clone + 'static,
        N: Network<BlockResponse = Block>,
    {
        let provider = self.provider.clone();
        let latest_block = self.latest_block.clone();
        let state = self.state.clone();
        let hooks = self.hooks.clone();

        let chain_id = { state.read().await.chain_id };
        let query_chunks = Self::build_query_chunks(&provider, &state, chain_id).await;

        info!(
            "Starting newHeads + logsBloom + getLogs sync ({} query chunks)",
            query_chunks.len()
        );

        Ok(Box::pin(stream! {
            let mut last_hash: Option<FixedBytes<32>> = None;

            loop {
                let mut heads_stream = match provider.subscribe_blocks().await {
                    Ok(sub) => sub.into_stream(),
                    Err(e) => {
                        error!("Failed to subscribe to newHeads: {}", e);
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                info!("Subscribed to newHeads");

                let current_synced = latest_block.load(Ordering::Relaxed);
                if let Ok(chain_head) = provider.get_block_number().await {
                    if chain_head > current_synced && current_synced > 0 {
                        match Self::backfill_range(
                            &provider,
                            &state,
                            &hooks,
                            &query_chunks,
                            current_synced + 1,
                            chain_head,
                            &latest_block,
                            chain_id,
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
                                error!("Initial backfill failed: {}", e);
                            }
                        }
                    }
                }

                loop {
                    match tokio::time::timeout(Duration::from_secs(60), heads_stream.next()).await {
                        Ok(Some(new_head)) => {
                            let block_num = new_head.number();
                            let block_hash = new_head.hash();
                            let parent_hash = new_head.parent_hash();
                            let logs_bloom = new_head.logs_bloom();

                            let last_processed = latest_block.load(Ordering::Relaxed);

                            if block_num < last_processed {
                                continue;
                            }

                            if block_num == last_processed {
                                if let Some(last) = last_hash {
                                    if last == block_hash {
                                        continue;
                                    }
                                }
                            } else {
                                if block_num > last_processed + 1 {
                                    match Self::backfill_range(
                                        &provider,
                                        &state,
                                        &hooks,
                                        &query_chunks,
                                        last_processed + 1,
                                        block_num - 1,
                                        &latest_block,
                                        chain_id,
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
                                            error!(
                                                "Gap backfill {}..{} failed: {}",
                                                last_processed + 1,
                                                block_num - 1,
                                                e
                                            );
                                            continue;
                                        }
                                    }
                                } else if let Some(last) = last_hash {
                                    if parent_hash != last {
                                        warn!(
                                            "Parent hash mismatch at block {} (expected parent {}, got {})",
                                            block_num,
                                            last,
                                            parent_hash
                                        );

                                        match provider.get_block(block_num.into()).await {
                                            Ok(Some(block)) => {
                                                if block.header.parent_hash != last {
                                                    warn!(
                                                        "Canonical parent mismatch confirmed at block {}",
                                                        block_num
                                                    );
                                                }
                                            }
                                            Ok(None) => {
                                                warn!("Fallback get_block returned None for block {}", block_num);
                                            }
                                            Err(e) => {
                                                warn!("Fallback get_block failed for block {}: {}", block_num, e);
                                            }
                                        }
                                    }
                                }
                            }

                            let all_logs = match Self::collect_logs_for_chunks(
                                &provider,
                                &query_chunks,
                                block_num,
                                block_num,
                                Some(&logs_bloom),
                            )
                            .await
                            {
                                Ok(logs) => logs,
                                Err(e) => {
                                    error!("get_logs failed for block {}: {}", block_num, e);
                                    continue;
                                }
                            };

                            match Self::apply_logs_for_block(
                                &provider,
                                &state,
                                &hooks,
                                block_num,
                                all_logs,
                                &latest_block,
                            )
                            .await
                            {
                                Ok(affected) => {
                                    last_hash = Some(block_hash);
                                    if !affected.is_empty() {
                                        yield Ok(affected);
                                    }
                                }
                                Err(e) => {
                                    error!("Process block {} failed: {}", block_num, e);
                                }
                            }
                        }
                        Ok(None) => {
                            warn!("newHeads stream ended");
                            break;
                        }
                        Err(_) => {
                            warn!("newHeads timeout (60s), reconnecting...");
                            break;
                        }
                    }
                }

                warn!("Reconnecting in 2s...");
                sleep(Duration::from_secs(2)).await;
            }
        }))
    }

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
                    state.write().await.state.insert(addr, new_amm);
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
        mut logs: Vec<Log>,
        latest_block: &Arc<AtomicU64>,
    ) -> Result<Vec<Address>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        latest_block.store(block_num, Ordering::Relaxed);

        if logs.is_empty() {
            return Ok(vec![]);
        }

        logs.sort_by_key(|log| {
            (
                log.block_number.unwrap_or_default(),
                log.transaction_index.unwrap_or_default(),
                log.log_index.unwrap_or_default(),
            )
        });

        let (affected, needs_resync, needs_async_update) = state.write().await.sync(&logs)?;

        let amms_to_resync: Vec<AMM> = {
            let guard = state.read().await;
            needs_resync
                .iter()
                .filter_map(|addr| guard.state.get(addr).cloned())
                .collect()
        };

        if !amms_to_resync.is_empty() {
            let _ = Self::execute_batch_tasks(
                state,
                amms_to_resync,
                provider.clone(),
                "auto-resync",
                |amm, provider| async move {
                    amm.init(BlockId::Number(block_num.into()), provider).await
                },
            )
            .await;
        }

        let amms_to_update: Vec<AMM> = {
            let guard = state.read().await;
            needs_async_update
                .iter()
                .filter_map(|addr| guard.state.get(addr).cloned())
                .collect()
        };

        if !amms_to_update.is_empty() {
            let _ = Self::execute_batch_tasks(
                state,
                amms_to_update,
                provider.clone(),
                "async-update",
                |mut amm, provider| async move {
                    amm.update(provider).await?;
                    Ok(amm)
                },
            )
            .await;
        }

        if !affected.is_empty() {
            hooks.notify(&affected).await;
        }

        Ok(affected)
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

            let filter = chunk.filter(from_block, to_block);
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
        latest_block: &Arc<AtomicU64>,
        chain_id: u64,
    ) -> Result<Vec<Vec<Address>>, StateSpaceError>
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
                                latest_block,
                            )
                            .await?;

                            if !affected.is_empty() {
                                results.push(affected);
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
    ) -> Vec<LogQueryChunk>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let guard = state.read().await;

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

            match amm {
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
                _ => {
                    if has_events {
                        topic_addresses.insert(amm.address());
                    }
                }
            }
        }

        drop(guard);

        if has_slipstream_pool && chain_id == BASE_CHAIN_ID {
            if let Some(fee_module) = Self::resolve_slipstream_fee_module(provider).await {
                topic_addresses.insert(fee_module);
                topic_signatures.insert(ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH);
            }
        }

        let mut chunks = Vec::new();

        if !topic_addresses.is_empty() && !topic_signatures.is_empty() {
            let mut topic_addresses: Vec<Address> = topic_addresses.into_iter().collect();
            topic_addresses.sort();

            let mut topic_signatures: Vec<FixedBytes<32>> = topic_signatures.into_iter().collect();
            topic_signatures.sort();

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

        chunks
    }

    async fn resolve_slipstream_fee_module(provider: &P) -> Option<Address>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let factory = ICLFactoryReader::new(BASE_SLIPSTREAM_FACTORY, provider.clone());
        match factory.swapFeeModule().call().await {
            Ok(addr) if addr != Address::ZERO => Some(addr),
            Ok(_) => None,
            Err(e) => {
                warn!("Failed to fetch Slipstream FeeModule address: {}", e);
                None
            }
        }
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
            ETHEREUM_MAINNET_CHAIN_ID => 50,
            _ => 50,
        }
    }
}

#[derive(Clone)]
pub struct StateSpaceBuilder<N, P> {
    pub provider: P,
    pub latest_block: u64,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
    pub filters: Vec<PoolFilter>,
    pub hooks: Vec<StateHook<Vec<Address>>>,
    pub snapshot_path: Option<PathBuf>,
    pub snapshot_config: Option<SnapshotConfig>,
    pub rate_sync_interval: Option<Duration>,
    pub curve_sync_interval: Option<Duration>,
    pub maintenance_interval: Option<Duration>,
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
            latest_block: 0,
            factories: vec![],
            amms: vec![],
            filters: vec![],
            phantom: PhantomData,
            snapshot_path: None,
            snapshot_config: None,
            rate_sync_interval: None,
            curve_sync_interval: None,
            maintenance_interval: None,
            hooks: vec![],
        }
    }

    pub fn block(self, latest_block: u64) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            latest_block,
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

    pub fn with_rate_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            rate_sync_interval: Some(interval),
            ..self
        }
    }

    pub fn with_maintenance_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            maintenance_interval: Some(interval),
            ..self
        }
    }

    pub fn with_curve_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            curve_sync_interval: Some(interval),
            ..self
        }
    }

    pub async fn sync(mut self) -> Result<StateSpaceManager<N, P>, AMMError> {
        let mut state_space = StateSpace::default();

        let chain_id = self.provider.get_chain_id().await?;
        info!(target: "state_space::sync", "Syncing AMMs for chain {}", chain_id);

        let chain_tip_u64 = if self.latest_block > 0 {
            self.latest_block
        } else {
            self.provider.get_block_number().await?
        };

        // If latest_block was not set (0), update it with the fetched chain tip
        if self.latest_block == 0 {
            self.latest_block = chain_tip_u64;
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
                state_space.state.insert(amm.address(), amm);
            }
        }

        // Sync remaining AMM variants in batches by variant
        for (variant, remaining_amms) in amm_variants.drain() {
            info!(target: "state_space::sync", variant = ?variant, count = remaining_amms.len(), "Syncing batch");
            let provider = self.provider.clone();
            if variant == crate::amms::amm::Variant::UniswapV3Pool {
                let chunk_size = 25;
                for chunk in remaining_amms.chunks(chunk_size) {
                    let synced = variant
                        .init_batch::<N, _>(chunk.to_vec(), chain_tip, provider.clone())
                        .await?;

                    // 在每次循环结束时短暂 sleep，避免超出 RPC 调用频率
                    sleep(Duration::from_millis(1500)).await;

                    for amm in synced {
                        let mut amm = amm;
                        amm.set_last_synced_block(chain_tip_u64);
                        state_space.state.insert(amm.address(), amm);
                    }
                }
            } else {
                let synced = variant
                    .init_batch::<N, _>(remaining_amms, chain_tip, provider.clone())
                    .await?;

                // 在每次循环结束时短暂 sleep，避免超出 RPC 调用频率
                sleep(Duration::from_millis(1500)).await;

                for amm in synced {
                    let mut amm = amm;
                    amm.set_last_synced_block(chain_tip_u64);
                    state_space.state.insert(amm.address(), amm);
                }
            }
        }

        let latest_block = Arc::new(AtomicU64::new(self.latest_block));
        state_space.latest_block = latest_block.clone();
        state_space.chain_id = chain_id;

        let state_space = Arc::new(RwLock::new(state_space));

        if let Some(snapshot_config) = self.snapshot_config {
            let hook = snapshot_config.into_state_hook(state_space.clone()).await;
            self.hooks.push(hook);
        }

        if let Some(interval) = self.rate_sync_interval {
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
        }

        // Curve NG StableSwap pools: stored_rates for rebasing tokens & D value sync
        if let Some(interval) = self.curve_sync_interval.or(self.rate_sync_interval) {
            tokio::spawn(sync_services::start_curve_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        if let Some(interval) = self.maintenance_interval {
            tokio::spawn(sync_services::start_state_maintenance_task(
                state_space.clone(),
                self.factories.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        Ok(StateSpaceManager {
            latest_block,
            state: state_space,
            block_filter,
            provider: self.provider,
            phantom: PhantomData,
            hooks: HookRegistry::new(self.hooks),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct StateSpace {
    pub state: HashMap<Address, AMM>,
    pub latest_block: Arc<AtomicU64>,
    pub chain_id: u64,
    cache: StateChangeCache<CACHE_SIZE>,
}

impl StateSpace {
    pub fn get(&self, address: &Address) -> Option<&AMM> {
        self.state.get(address)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AMM> {
        self.state.get_mut(address)
    }

    fn resolve_slipstream_fee_event_pool(&self, topics: &[FixedBytes<32>]) -> Option<Address> {
        if topics.len() < 2 {
            return None;
        }

        if topics[0] != ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH {
            return None;
        }

        let pool_address = Address::from_word(topics[1]);
        match self.state.get(&pool_address) {
            Some(AMM::AerodromeSlipstreamPool(_)) => Some(pool_address),
            _ => None,
        }
    }

    pub fn sync(
        &mut self,
        logs: &[Log],
    ) -> Result<(Vec<Address>, Vec<Address>, Vec<Address>), StateSpaceError> {
        // 处理流程：
        // 1) 先按 (block_number, transaction_index, log_index) 排序，避免 WS/回补乱序破坏缓存与回滚语义
        // 2) 逐条应用 log 到对应池子的本地状态，并缓存“该区块开始前”的 AMM 快照用于 reorg unwind
        if logs.is_empty() {
            return Ok((vec![], vec![], vec![]));
        }

        let mut logs_sorted = logs.to_vec();
        logs_sorted.sort_by_key(|log| {
            (
                log.block_number.unwrap_or_default(),
                log.transaction_index.unwrap_or_default(),
                log.log_index.unwrap_or_default(),
            )
        });

        let latest = self.latest_block.load(Ordering::Relaxed);

        // We do not check for reorgs here using block numbers because partitioned log subscriptions
        // (chunking) can cause logs to arrive out of order or in interleaved batches.
        // A "late" batch from one chunk is not a reorg of the chain.
        // We rely on:
        // 1. Per-AMM `last_synced_block` checks to prevent rewinding individual pools.
        // 2. The periodic `start_state_maintenance_task` to handle actual chain reorgs/discrepancies.
        // 3. The `needs_resync` set to handle syncs that fail due to insufficient data (e.g. Curve V1 RemoveLiquidityOne)

        let mut affected_amms = HashSet::new();
        let mut needs_resync = HashSet::new();
        let mut needs_async_update = HashSet::new();
        let mut max_processed_block = latest;

        for log in &logs_sorted {
            let log_block_number = log
                .block_number
                .ok_or(StateSpaceError::MissingBlockNumber)?;

            // Track the latest block info seen in this batch
            if log_block_number > max_processed_block {
                max_processed_block = log_block_number;
            }

            let address = log.address();
            let direct_hit = self.state.contains_key(&address);

            let target_address = if direct_hit {
                Some(address)
            } else if log.topics().len() >= 2 {
                if Some(address) == get_liquidity_layer(self.chain_id) {
                    let pool_address = Address::from_word(log.topics()[1]);
                    match self.state.get(&pool_address) {
                        Some(AMM::FluidDexPool(_)) => Some(pool_address),
                        _ => None,
                    }
                } else if Some(address) == balancer_v2::get_vault_address(self.chain_id) {
                    // Balancer V2: poolId is in topics[1]
                    // The first 20 bytes of poolId is the pool address, which is used as the key in StateSpace
                    let pool_id = log.topics()[1];
                    let pool_address = Address::from_slice(&pool_id.as_slice()[0..20]);

                    match self.state.get(&pool_address) {
                        Some(AMM::BalancerV2Pool(p)) if p.pool_id == pool_id => Some(pool_address),
                        _ => None,
                    }
                } else if Some(address) == balancer_v3::get_vault_address(self.chain_id) {
                    // Balancer V3: pool address is in topics[1]
                    let pool_address = Address::from_word(log.topics()[1]);
                    if self.state.contains_key(&pool_address) {
                        Some(pool_address)
                    } else {
                        None
                    }
                } else if let Some(pool_address) =
                    self.resolve_slipstream_fee_event_pool(log.topics())
                {
                    Some(pool_address)
                } else {
                    let pool_id = log.topics()[1];
                    let virtual_address = Address::from_slice(&pool_id.as_slice()[0..20]);
                    match self.state.get(&virtual_address) {
                        Some(AMM::UniswapV4Pool(p)) if p.manager_address == address => {
                            Some(virtual_address)
                        }
                        Some(AMM::PancakeInfinityPool(p)) if p.manager_address == address => {
                            Some(virtual_address)
                        }
                        _ => None,
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

                    match self.state.get(&virtual_address) {
                        Some(AMM::EkuboPool(p)) if p.pool_id == pool_id => Some(virtual_address),
                        _ => None,
                    }
                } else {
                    None
                }
            } else {
                None
            };

            let Some(target_address) = target_address else {
                continue;
            };

            let Some(amm) = self.state.get_mut(&target_address) else {
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
                }
            }
        }

        // Update latest_block internally to ensure consistency with state lock
        if max_processed_block > latest {
            self.latest_block
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
    pub latest_block: u64,
    pub cache: (Vec<StateChange>, u64),
}

impl From<StateSpace> for SerializableStateSpace {
    fn from(ss: StateSpace) -> Self {
        Self {
            state: ss.state,
            latest_block: ss.latest_block.load(Ordering::Relaxed),
            cache: (ss.cache.cache.into_iter().collect(), ss.cache.oldest_block),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, FixedBytes};

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
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(1), 50);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(10), 50);
    }

    #[test]
    fn slipstream_custom_fee_event_routes_to_pool_topic1() {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        let mut state = StateSpace::default();
        state.state.insert(
            pool_address,
            AMM::AerodromeSlipstreamPool(
                crate::amms::aerodrome_slipstream::AerodromeSlipstreamPool::new(pool_address),
            ),
        );

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
}

impl From<SerializableStateSpace> for StateSpace {
    fn from(val: SerializableStateSpace) -> Self {
        let (cache, oldest_block) = val.cache;
        StateSpace {
            state: val.state,
            latest_block: Arc::new(AtomicU64::new(val.latest_block)),
            cache: StateChangeCache {
                cache: cache.into_iter().collect(),
                oldest_block,
            },
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
