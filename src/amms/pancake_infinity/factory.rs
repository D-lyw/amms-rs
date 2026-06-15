use std::collections::{HashMap, HashSet};
use std::future::Future;

use alloy::eips::BlockId;
use alloy::primitives::{aliases::I24, Address, B256, U160, U256};
use alloy::providers::{Network, Provider};
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::sol_types::SolEvent;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::error::{AMMError, BatchContractError};
use crate::amms::factory::{AutomatedMarketMakerFactory, DiscoverySync};
use crate::amms::get_token_decimals;
use crate::amms::pancake_infinity::{ICLPoolManager, PancakeInfinityPool};
use crate::amms::uniswap_v3::tick_to_word;
use crate::amms::uniswap_v4::lense::{
    decode_liquidity_gross_and_net, get_liquidity_slot, get_pool_state_slot, get_tick_bitmap_slot,
    get_tick_info_slot,
};
use crate::amms::Token;
use uniswap_v3_math::tick_math::{MAX_TICK, MIN_TICK};
use ICLPoolManager::ICLPoolManagerInstance;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct PancakeInfinityFactory {
    pub address: Address,
    pub creation_block: u64,
}

impl PancakeInfinityFactory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        PancakeInfinityFactory {
            address,
            creation_block,
        }
    }

    pub async fn get_all_pools<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let disc_filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()]);
        let sync_provider = provider.clone();
        let mut futures = FuturesUnordered::new();
        let sync_step = 100_000;
        let mut latest_block = self.creation_block;
        while latest_block < block_number.as_u64().unwrap_or_default() {
            let mut block_filter = disc_filter.clone();
            let from_block = latest_block;
            let to_block = (from_block + sync_step).min(block_number.as_u64().unwrap_or_default());
            block_filter = block_filter.from_block(from_block);
            block_filter = block_filter.to_block(to_block);
            let sync_provider = sync_provider.clone();
            futures.push(async move { sync_provider.get_logs(&block_filter).await });
            latest_block = to_block + 1;
        }
        let mut pools = vec![];
        while let Some(res) = futures.next().await {
            let logs = res?;
            for log in logs {
                match self.create_pool(log)? {
                    AMM::PancakeInfinityPool(pool) => {
                        if pool.pool_key.hooks == Address::ZERO {
                            pools.push(AMM::PancakeInfinityPool(pool));
                        }
                    }
                    amm => pools.push(amm),
                }
            }
        }
        Ok(pools)
    }

    pub async fn sync_slot_0<N, P>(
        pools: &mut [PancakeInfinityPool],
        block_number: BlockId,
        provider: P,
    ) -> Result<HashSet<B256>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        #[derive(Clone, Copy)]
        enum SlotRequestKind {
            Slot0,
            Liquidity,
        }

        let mut failed_pool_ids = HashSet::new();
        let mut pools_by_manager: HashMap<Address, Vec<&mut PancakeInfinityPool>> = HashMap::new();
        for pool in pools.iter_mut() {
            pools_by_manager
                .entry(pool.manager_address)
                .or_default()
                .push(pool);
        }
        for (manager_address, mut manager_pools) in pools_by_manager {
            let ipool_manager = ICLPoolManagerInstance::new(manager_address, provider.clone());
            let mut slots = Vec::with_capacity(manager_pools.len() * 2);
            let mut slot_requests = Vec::with_capacity(manager_pools.len() * 2);
            for (pool_idx, pool) in manager_pools.iter().enumerate() {
                slots.push(B256::from(get_pool_state_slot(pool.pool_id)));
                slot_requests.push((pool_idx, pool.pool_id, SlotRequestKind::Slot0));
                slots.push(B256::from(get_liquidity_slot(pool.pool_id)));
                slot_requests.push((pool_idx, pool.pool_id, SlotRequestKind::Liquidity));
            }

            let chunks: Vec<_> = slots.chunks(200).collect();
            let mut slot0_by_pool = HashMap::new();
            let mut liquidity_by_pool = HashMap::new();
            let mut chunk_cursor = 0usize;

            for batch in chunks.chunks(10) {
                let mut futures = Vec::new();
                let mut batch_ranges = Vec::with_capacity(batch.len());
                for chunk in batch {
                    let start = chunk_cursor;
                    let end = start + chunk.len();
                    batch_ranges.push((start, end));
                    chunk_cursor = end;

                    let chunk_vec = chunk.to_vec();
                    let ipool_manager = ipool_manager.clone();
                    futures.push(async move {
                        ipool_manager
                            .extsload_2(chunk_vec)
                            .block(block_number)
                            .call()
                            .await
                    });
                }

                let batch_results = futures::future::join_all(futures).await;
                for ((start, end), result) in
                    batch_ranges.into_iter().zip(batch_results.into_iter())
                {
                    match result {
                        Ok(chunk_results) => {
                            if chunk_results.len() != end - start {
                                tracing::warn!(
                                    target: "amms::pancake_infinity::sync_slot_0",
                                    manager = ?manager_address,
                                    expected = end - start,
                                    actual = chunk_results.len(),
                                    "extsload_2 returned mismatched slot count; failing affected pools"
                                );
                                for (_, pool_id, _) in &slot_requests[start..end] {
                                    failed_pool_ids.insert(*pool_id);
                                }
                                continue;
                            }

                            for ((pool_idx, pool_id, kind), data) in slot_requests[start..end]
                                .iter()
                                .copied()
                                .zip(chunk_results.into_iter())
                            {
                                if failed_pool_ids.contains(&pool_id) {
                                    continue;
                                }
                                match kind {
                                    SlotRequestKind::Slot0 => {
                                        slot0_by_pool.insert(pool_idx, data);
                                    }
                                    SlotRequestKind::Liquidity => {
                                        liquidity_by_pool.insert(pool_idx, data);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "amms::pancake_infinity::sync_slot_0",
                                manager = ?manager_address,
                                error = ?err,
                                "extsload_2 batch failed; failing affected pools"
                            );
                            for (_, pool_id, _) in &slot_requests[start..end] {
                                failed_pool_ids.insert(*pool_id);
                            }
                        }
                    }
                }
            }

            for (pool_idx, pool) in manager_pools.iter_mut().enumerate() {
                if failed_pool_ids.contains(&pool.pool_id) {
                    continue;
                }

                let Some(slot0_data) = slot0_by_pool.get(&pool_idx).copied() else {
                    tracing::warn!(
                        target: "amms::pancake_infinity::sync_slot_0",
                        pool_id = ?pool.pool_id,
                        manager = ?manager_address,
                        "Missing slot0 data for pool; failing refresh"
                    );
                    failed_pool_ids.insert(pool.pool_id);
                    continue;
                };
                let Some(liquidity_data) = liquidity_by_pool.get(&pool_idx).copied() else {
                    tracing::warn!(
                        target: "amms::pancake_infinity::sync_slot_0",
                        pool_id = ?pool.pool_id,
                        manager = ?manager_address,
                        "Missing liquidity data for pool; failing refresh"
                    );
                    failed_pool_ids.insert(pool.pool_id);
                    continue;
                };

                if slot0_data.is_zero() {
                    tracing::warn!(
                        target: "amms::pancake_infinity::sync_slot_0",
                        pool_id = ?pool.pool_id,
                        manager = ?manager_address,
                        "Pool has zero slot0 data; failing refresh"
                    );
                    failed_pool_ids.insert(pool.pool_id);
                    continue;
                }

                let sqrt_price_x96 = U160::from_be_slice(&slot0_data[12..32]);
                let tick_bytes =
                    unsafe { (slot0_data.as_ptr().add(9) as *const [u8; 3]).read_unaligned() };
                let tick = I24::from_be_bytes::<3>(tick_bytes);
                let protocol_fee_bytes =
                    unsafe { (slot0_data.as_ptr().add(6) as *const [u8; 3]).read_unaligned() };
                let protocol_fee =
                    alloy::primitives::aliases::U24::from_be_bytes(protocol_fee_bytes);
                let lp_fee_bytes =
                    unsafe { (slot0_data.as_ptr().add(3) as *const [u8; 3]).read_unaligned() };
                let lp_fee = alloy::primitives::aliases::U24::from_be_bytes(lp_fee_bytes);

                if liquidity_data.is_zero() {
                    tracing::info!(
                        target: "amms::pancake_infinity::sync_slot_0",
                        pool_id = ?pool.pool_id,
                        manager = ?manager_address,
                        "Pool active liquidity is zero at current tick; keeping pool tracked"
                    );
                }

                let liquidity = u128::from_be_bytes(liquidity_data[16..32].try_into().unwrap());
                pool.sqrt_price = U256::from(sqrt_price_x96);
                pool.tick = tick.as_i32();
                pool.protocol_fee = protocol_fee.to::<u32>();
                pool.lp_fee = lp_fee.to::<u32>();
                pool.liquidity = liquidity;
            }
        }
        Ok(failed_pool_ids)
    }

    pub async fn sync_token_decimals<N, P>(
        pools: &mut [PancakeInfinityPool],
        provider: P,
    ) -> Result<(), BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut tokens = HashSet::new();
        for pool in pools.iter() {
            for token in pool.tokens() {
                tokens.insert(token);
            }
        }
        let token_decimals = get_token_decimals(tokens.into_iter().collect(), provider).await?;
        for pool in pools.iter_mut() {
            if let Some(decimals) = token_decimals.get(&pool.token_a.address) {
                pool.token_a.decimals = *decimals;
            }
            if let Some(decimals) = token_decimals.get(&pool.token_b.address) {
                pool.token_b.decimals = *decimals;
            }
        }
        Ok(())
    }

    pub async fn sync_tick_data<N, P>(
        pools: &mut [PancakeInfinityPool],
        block_id: BlockId,
        provider: P,
    ) -> Result<HashSet<B256>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut failed_pool_ids = HashSet::new();
        let pool_ticks = pools
            .into_par_iter()
            .filter_map(|pool| {
                let tick_spacing = pool.tick_spacing;
                let min_word = tick_to_word(MIN_TICK, tick_spacing);
                let max_word = tick_to_word(MAX_TICK, tick_spacing);
                let tick_bitmap = &pool.tick_bitmap;
                let initialized_ticks: Vec<i32> = (min_word..=max_word)
                    .filter_map(|word_pos| {
                        tick_bitmap
                            .get(&(word_pos as i16))
                            .filter(|&bitmap| *bitmap != U256::ZERO)
                            .map(|&bitmap| (word_pos, bitmap))
                    })
                    .flat_map(|(word_pos, bitmap)| {
                        (0..256)
                            .filter(move |i| {
                                (bitmap & (U256::from(1) << U256::from(*i))) != U256::ZERO
                            })
                            .map(move |i| (word_pos * 256 + i) * tick_spacing)
                    })
                    .collect();
                if initialized_ticks.is_empty() {
                    None
                } else {
                    Some((pool.pool_id, initialized_ticks))
                }
            })
            .collect::<HashMap<_, _>>();
        if pool_ticks.is_empty() {
            return Ok(failed_pool_ids);
        }
        let mut pools_by_manager: HashMap<Address, Vec<&mut PancakeInfinityPool>> = HashMap::new();
        for pool in pools.iter_mut() {
            if pool_ticks.contains_key(&pool.pool_id) {
                pools_by_manager
                    .entry(pool.manager_address)
                    .or_default()
                    .push(pool);
            }
        }
        for (manager_address, mut manager_pools) in pools_by_manager {
            let mut all_slots = Vec::new();
            let mut pool_slot_indices = Vec::new();
            for (pool_idx, pool) in manager_pools.iter().enumerate() {
                if let Some(ticks) = pool_ticks.get(&pool.pool_id) {
                    for &tick in ticks {
                        let slot = B256::from(get_tick_info_slot(pool.pool_id, tick));
                        all_slots.push(slot);
                        pool_slot_indices.push((pool_idx, pool.pool_id, tick));
                    }
                }
            }
            let chunks: Vec<_> = all_slots.chunks(100).collect();
            let ipool_manager = ICLPoolManagerInstance::new(manager_address, provider.clone());
            let mut chunk_cursor = 0usize;

            for batch in chunks.chunks(10) {
                let mut futures = Vec::new();
                let mut batch_ranges = Vec::with_capacity(batch.len());
                for chunk in batch {
                    let start = chunk_cursor;
                    let end = start + chunk.len();
                    batch_ranges.push((start, end));
                    chunk_cursor = end;

                    let chunk_vec = chunk.to_vec();
                    let ipool_manager = ipool_manager.clone();
                    futures.push(async move {
                        ipool_manager
                            .extsload_2(chunk_vec)
                            .block(block_id)
                            .call()
                            .await
                    });
                }

                let batch_results = futures::future::join_all(futures).await;
                for ((start, end), result) in
                    batch_ranges.into_iter().zip(batch_results.into_iter())
                {
                    match result {
                        Ok(chunk_results) => {
                            if chunk_results.len() != end - start {
                                tracing::warn!(
                                    target: "amms::pancake_infinity::sync_tick_data",
                                    manager = ?manager_address,
                                    expected = end - start,
                                    actual = chunk_results.len(),
                                    "extsload_2 returned mismatched tick count; failing affected pools"
                                );
                                for (_, pool_id, _) in &pool_slot_indices[start..end] {
                                    failed_pool_ids.insert(*pool_id);
                                }
                                continue;
                            }

                            for ((pool_idx, pool_id, tick), word) in pool_slot_indices[start..end]
                                .iter()
                                .copied()
                                .zip(chunk_results.into_iter())
                            {
                                if failed_pool_ids.contains(&pool_id) {
                                    continue;
                                }
                                let (liquidity_gross, liquidity_net) =
                                    decode_liquidity_gross_and_net(B256::from(word));
                                manager_pools[pool_idx].ticks.insert(
                                    tick,
                                    crate::amms::uniswap_v3::Info {
                                        liquidity_gross,
                                        liquidity_net,
                                        initialized: true,
                                    },
                                );
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "amms::pancake_infinity::sync_tick_data",
                                manager = ?manager_address,
                                error = ?err,
                                "extsload_2 batch failed; failing affected pools"
                            );
                            for (_, pool_id, _) in &pool_slot_indices[start..end] {
                                failed_pool_ids.insert(*pool_id);
                            }
                        }
                    }
                }
            }
        }
        Ok(failed_pool_ids)
    }

    pub async fn sync_tick_bitmap<N, P>(
        pools: &mut [PancakeInfinityPool],
        block_id: BlockId,
        provider: P,
    ) -> Result<HashSet<B256>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut failed_pool_ids = HashSet::new();
        let mut pools_by_manager: HashMap<Address, Vec<&mut PancakeInfinityPool>> = HashMap::new();
        for pool in pools.iter_mut() {
            pools_by_manager
                .entry(pool.manager_address)
                .or_default()
                .push(pool);
        }
        for (manager_address, mut manager_pools) in pools_by_manager {
            let mut all_slots = Vec::new();
            let mut pool_slot_indices = Vec::new();
            for (pool_idx, pool) in manager_pools.iter().enumerate() {
                let min_word = tick_to_word(MIN_TICK, pool.tick_spacing);
                let max_word = tick_to_word(MAX_TICK, pool.tick_spacing);
                for word in min_word + 1..max_word {
                    let slot = get_tick_bitmap_slot(pool.pool_id, word as i16);
                    if slot != U256::ZERO {
                        all_slots.push(B256::from(slot));
                        pool_slot_indices.push((pool_idx, pool.pool_id, word as i16));
                    }
                }
            }
            let chunks: Vec<_> = all_slots.chunks(500).collect();
            let ipool_manager = ICLPoolManagerInstance::new(manager_address, provider.clone());
            let mut chunk_cursor = 0usize;

            for batch in chunks.chunks(10) {
                let mut futures = Vec::new();
                let mut batch_ranges = Vec::with_capacity(batch.len());
                for chunk in batch {
                    let start = chunk_cursor;
                    let end = start + chunk.len();
                    batch_ranges.push((start, end));
                    chunk_cursor = end;

                    let chunk_vec = chunk.to_vec();
                    let ipool_manager = ipool_manager.clone();
                    futures.push(async move {
                        ipool_manager
                            .extsload_2(chunk_vec)
                            .block(block_id)
                            .call()
                            .await
                    });
                }

                let batch_results = futures::future::join_all(futures).await;
                for ((start, end), result) in
                    batch_ranges.into_iter().zip(batch_results.into_iter())
                {
                    match result {
                        Ok(chunk_results) => {
                            if chunk_results.len() != end - start {
                                tracing::warn!(
                                    target: "amms::pancake_infinity::sync_tick_bitmap",
                                    manager = ?manager_address,
                                    expected = end - start,
                                    actual = chunk_results.len(),
                                    "extsload_2 returned mismatched bitmap count; failing affected pools"
                                );
                                for (_, pool_id, _) in &pool_slot_indices[start..end] {
                                    failed_pool_ids.insert(*pool_id);
                                }
                                continue;
                            }

                            for ((pool_idx, pool_id, word), bitmap_word) in pool_slot_indices
                                [start..end]
                                .iter()
                                .copied()
                                .zip(chunk_results.into_iter())
                            {
                                if failed_pool_ids.contains(&pool_id) {
                                    continue;
                                }
                                let bitmap = U256::from_be_bytes(bitmap_word.0);
                                manager_pools[pool_idx].tick_bitmap.insert(word, bitmap);
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "amms::pancake_infinity::sync_tick_bitmap",
                                manager = ?manager_address,
                                error = ?err,
                                "extsload_2 batch failed; failing affected pools"
                            );
                            for (_, pool_id, _) in &pool_slot_indices[start..end] {
                                failed_pool_ids.insert(*pool_id);
                            }
                        }
                    }
                }
            }
        }
        Ok(failed_pool_ids)
    }

    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut pools: Vec<PancakeInfinityPool> = amms
            .into_iter()
            .filter_map(|amm| {
                if let AMM::PancakeInfinityPool(p) = amm {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        let mut failed_pool_ids =
            Self::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }

        // Clear previous tick data to prevent stale data buildup
        for pool in pools.iter_mut() {
            pool.tick_bitmap.clear();
            pool.ticks.clear();
        }

        failed_pool_ids
            .extend(Self::sync_tick_bitmap(&mut pools, block_number, provider.clone()).await?);
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }

        failed_pool_ids
            .extend(Self::sync_tick_data(&mut pools, block_number, provider.clone()).await?);
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }

        let mut price_failed_pool_ids = HashSet::new();
        for pool in pools.iter_mut() {
            match pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                Ok(price) => {
                    pool.token_a_price = price;
                    pool.token_b_price = if price != 0.0 { 1.0 / price } else { 0.0 };
                }
                Err(e) => {
                    tracing::warn!(
                        target: "amms::pancake_infinity::sync",
                        pool_id = ?pool.pool_id,
                        error = ?e,
                        "Failed to refresh PancakeInfinity spot prices; failing pool refresh"
                    );
                    price_failed_pool_ids.insert(pool.pool_id);
                }
            }
        }

        failed_pool_ids.extend(price_failed_pool_ids);
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }

        Ok(pools.into_iter().map(AMM::PancakeInfinityPool).collect())
    }

    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let total = amms.len();

        let mut pools: Vec<PancakeInfinityPool> = amms
            .into_iter()
            .filter_map(|amm| {
                if let AMM::PancakeInfinityPool(p) = amm {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();

        let mut failed_pool_ids =
            Self::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }
        Self::sync_token_decimals(&mut pools, provider.clone()).await?;

        let (structurally_valid, structurally_invalid): (Vec<_>, Vec<_>) =
            pools.into_par_iter().partition(|pool| {
                !pool.token_a.address.is_zero()
                    && !pool.token_b.address.is_zero()
                    && pool.token_a.decimals > 0
                    && pool.token_b.decimals > 0
            });

        if !structurally_invalid.is_empty() {
            for pool in &structurally_invalid {
                tracing::info!(
                    target: "amms::pancake_infinity::init_batch",
                    pool_id = ?pool.pool_id,
                    token_a = ?pool.token_a.address,
                    token_b = ?pool.token_b.address,
                    token_a_decimals = ?pool.token_a.decimals,
                    token_b_decimals = ?pool.token_b.decimals,
                    "Filtering out structurally invalid PancakeInfinity pool"
                );
            }
        }

        let mut pools = structurally_valid;

        // Clear previous tick data to prevent stale data buildup
        for pool in pools.iter_mut() {
            pool.tick_bitmap.clear();
            pool.ticks.clear();
        }

        failed_pool_ids
            .extend(Self::sync_tick_bitmap(&mut pools, block_number, provider.clone()).await?);
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }

        failed_pool_ids
            .extend(Self::sync_tick_data(&mut pools, block_number, provider.clone()).await?);
        if !failed_pool_ids.is_empty() {
            pools.retain(|pool| !failed_pool_ids.contains(&pool.pool_id));
        }

        let (liquid_pools, dust_pools): (Vec<_>, Vec<_>) = pools
            .into_par_iter()
            .partition(|pool| pool.has_sufficient_liquidity());

        if !dust_pools.is_empty() {
            for pool in &dust_pools {
                tracing::warn!(
                    target: "amms::pancake_infinity::init_batch",
                    pool_id = ?pool.pool_id,
                    liquidity = pool.liquidity,
                    ticks = pool.ticks.len(),
                    "Filtering out dust PancakeInfinity pool by has_sufficient_liquidity"
                );
            }
        }

        let mut pools = liquid_pools;
        for pool in pools.iter_mut() {
            pool.token_a_price =
                pool.calculate_price(pool.token_a.address, pool.token_b.address)?;
            pool.token_b_price =
                pool.calculate_price(pool.token_b.address, pool.token_a.address)?;
        }

        let result: Vec<AMM> = pools.into_iter().map(AMM::PancakeInfinityPool).collect();
        let valid = result.len();
        let invalid = structurally_invalid.len() + dust_pools.len();
        tracing::info!(
            target: "amms::pancake_infinity::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(result)
    }
}

impl AutomatedMarketMakerFactory for PancakeInfinityFactory {
    type PoolVariant = PancakeInfinityPool;
    fn address(&self) -> Address {
        self.address
    }
    fn pool_creation_event(&self) -> B256 {
        ICLPoolManager::Initialize::SIGNATURE_HASH
    }
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let event = ICLPoolManager::Initialize::decode_log(&log.inner)?;
        let key = ICLPoolManager::PoolKey {
            currency0: event.currency0,
            currency1: event.currency1,
            hooks: event.hooks,
            poolManager: self.address,
            fee: event.fee,
            parameters: event.parameters,
        };
        Ok(AMM::PancakeInfinityPool(PancakeInfinityPool {
            pool_key: key,
            pool_id: event.id,
            token_a: Token::new_with_decimals(event.currency0, 0),
            token_b: Token::new_with_decimals(event.currency1, 0),
            tick_spacing: I24::from_be_bytes::<3>(
                (&event.parameters.0[29..32]).try_into().unwrap(),
            )
            .as_i32(),
            lp_fee: event.fee.to::<u32>(),
            sqrt_price: U256::from(event.sqrtPriceX96),
            tick: event.tick.as_i32(),
            liquidity: 0,
            protocol_fee: 0,
            manager_address: self.address,
            tick_bitmap: HashMap::new(),
            ticks: HashMap::new(),
            last_synced_block: 0,
            token_a_price: 0.0,
            token_b_price: 0.0,
        }))
    }
    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for PancakeInfinityFactory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        self.get_all_pools(to_block, provider.clone())
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        PancakeInfinityFactory::init_batch(amms, to_block, provider)
    }
}
