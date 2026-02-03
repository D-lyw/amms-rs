//! Ekubo V2 Factory Implementation
//!
//! This module contains the EkuboFactory for pool discovery and initialization.

use super::pool::EkuboPool;
use super::types::EkuboPoolKey;
use crate::amms::{
    amm::AMM,
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use serde::{Deserialize, Serialize};
use std::future::Future;

// ========== Batch Request Contracts ==========

sol! {
    #[sol(rpc)]
    GetEkuboTickBitmapBatchRequest,
    "src/amms/abi/GetEkuboTickBitmapBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetEkuboTickDataBatchRequest,
    "src/amms/abi/GetEkuboTickDataBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetEkuboPoolStateBatchRequest,
    "src/amms/abi/GetEkuboPoolStateBatchRequest.json",
}

sol! {
    struct BatchPoolState {
        uint160 sqrtRatio;
        int32 tick;
        uint128 liquidity;
        bool success;
    }
}

// ========== Pool Events ==========

sol! {
    // 池子初始化事件 - 用于发现新池子
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    interface EkuboPoolEvents {
        event PoolInitialized(
            bytes32 poolId,
            (address, address, bytes32) poolKey,
            int32 tick,
            uint96 sqrtRatio
        );
    }
}

// ========== EkuboFactory ==========

/// Ekubo Factory for pool discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EkuboFactory {
    pub address: Address,
    pub creation_block: u64,
}

impl EkuboFactory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        EkuboFactory {
            address,
            creation_block,
        }
    }
}

impl AutomatedMarketMakerFactory for EkuboFactory {
    type PoolVariant = EkuboPool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        EkuboPoolEvents::PoolInitialized::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let pool_init_event = EkuboPoolEvents::PoolInitialized::decode_log(&log.inner)?;

        // 直接使用事件中的 poolId，而不是重新计算
        // Ekubo 的 poolId 计算方式可能与 Rust 端的 keccak256(abi.encode(...)) 不同
        let pool_id = pool_init_event.poolId;

        // PoolKey 是 (token0, token1, config)
        let token0 = pool_init_event.poolKey.0;
        let token1 = pool_init_event.poolKey.1;
        let config_bytes = pool_init_event.poolKey.2; // FixedBytes<32>

        // 转换 FixedBytes<32> -> U256
        let config = U256::from_be_bytes::<32>(*config_bytes.as_ref());

        let pool_key = EkuboPoolKey::from_raw(token0, token1, config);
        let pool_config = pool_key.parse_config();

        let sqrt_price = EkuboPool::sqrt_ratio_from_tick(pool_init_event.tick)?;

        Ok(AMM::EkuboPool(EkuboPool {
            address: self.address,
            pool_key,
            pool_id,
            tick: pool_init_event.tick,
            sqrt_price,
            token_a: token0.into(),
            token_b: token1.into(),
            tick_spacing: pool_config.tick_spacing,
            fee: pool_config.fee as u128,
            ..Default::default()
        }))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for EkuboFactory {
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
        EkuboFactory::init_batch(amms, to_block, provider)
    }
}

impl EkuboFactory {
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
            .address(vec![self.address()])
            .from_block(self.creation_block)
            .to_block(block_number.as_u64().unwrap_or_default());

        let logs = provider.get_logs(&disc_filter).await?;

        let mut pools = vec![];
        for log in logs {
            pools.push(self.create_pool(log)?);
        }

        Ok(pools)
    }

    pub async fn init_batch<N, P>(
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use crate::amms::amm::AutomatedMarketMaker;
        use crate::amms::Token;
        use futures::stream::{self, StreamExt};

        // Removed GetEkuboPoolStateBatchRequest usage due to incorrect sqrtRatio format parsing (uint96 packed vs Q64.128)
        // Instead, we use parallel fetch_core_state calls which correctly use poolPrice() view function.

        let ekubo_indices: Vec<usize> = pools
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p, AMM::EkuboPool(_)))
            .map(|(i, _)| i)
            .collect();

        if ekubo_indices.is_empty() {
            return Ok(pools);
        }

        // Step 1: Parallel fetch core state and initialize tokens
        let mut tasks = Vec::new();
        
        for &idx in &ekubo_indices {
            if let AMM::EkuboPool(p) = &pools[idx] {
                let mut new_pool = EkuboPool::new(p.address, p.pool_key.clone());
                let provider = provider.clone();
                let block = block_number;

                tasks.push(async move {
                    // Fetch core state (sqrt_price, tick, liquidity) using poolPrice() correctly
                    new_pool = new_pool.fetch_core_state(block, provider.clone()).await?;
                    
                    // Initialize tokens (already done inside fetch_core_state? Yes, let's check pool.rs)
                    // pool.rs fetch_core_state calls Token::new(). So we don't need to do it here manually!
                    // This simplifies the logic significantly.
                    
                    Ok::<(usize, EkuboPool), AMMError>((idx, new_pool))
                });
            }
        }

        // Execute in parallel
        let results: Vec<Result<(usize, EkuboPool), AMMError>> =
            stream::iter(tasks).buffered(10).collect().await;

        for res in results {
            match res {
                Ok((pool_idx, new_pool)) => {
                    pools[pool_idx] = AMM::EkuboPool(new_pool);
                }
                Err(e) => {
                    tracing::warn!("Failed to init Ekubo pool: {}", e);
                    // We keep the original (uninitialized) pool, it will be filtered out in Step 4 if empty
                }
            }
        }


        // Step 3: Batch sync tick bitmaps and data (Chunked)
        const CHUNK_SIZE: usize = 2;

        for chunk in pools.chunks_mut(CHUNK_SIZE) {
            if chunk.iter().any(|p| matches!(p, AMM::EkuboPool(_))) {
                // Step 2: Batch sync tick bitmaps
                Self::sync_tick_bitmaps::<N, _>(chunk, block_number, provider.clone()).await?;

                // Step 3: Batch sync tick data
                Self::sync_tick_data::<N, _>(chunk, block_number, provider.clone()).await?;
            }
        }

        // Step 4: Final Validation - Remove incomplete pools
        // Check for pools missing tick data, which would cause false arbitrage
        let initial_count = pools.len();
        pools.retain(|pool| {
            if let AMM::EkuboPool(ekubo_pool) = pool {
                // If liquidity is 0 AND no tick data, then it's truly empty/uninitialized
                if ekubo_pool.liquidity == 0
                    && (ekubo_pool.tick_bitmap.is_empty() || ekubo_pool.ticks.is_empty())
                {
                    tracing::warn!(
                        target = "amms::ekubo::init_batch",
                        pool_id = ?ekubo_pool.pool_id,
                        "Pool removed due to no liquidity and missing tick data"
                    );
                    return false;
                }
                // If liquidity > 0 but no ticks, it means positions are wider than our sync range.
                // We keep it, assuming constant liquidity within the sync range.
            }
            true
        });

        if pools.len() < initial_count {
            tracing::info!(
                target = "amms::ekubo::init_batch",
                removed_count = initial_count - pools.len(),
                remaining_count = pools.len(),
                "Cleaned up incomplete Ekubo pools"
            );
        }

        Ok(pools)
    }

    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::init_batch(amms, to_block, provider).await
    }

    /// Sync tick bitmaps for Ekubo pools
    /// Uses GetEkuboTickBitmapBatchRequest to batch fetch tick bitmap words
    /// V2 uses an offset of 89421695 for tick bitmap indexing
    pub async fn sync_tick_bitmaps<N, P>(
        pools: &mut [AMM],
        _block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use alloy::sol_types::SolValue;
        use std::collections::HashMap;
        use GetEkuboTickBitmapBatchRequest::TickBitmapInfo;

        // Ekubo V2 bitmap offset
        const BITMAP_OFFSET: i64 = 89421695;

        // Ekubo tick range
        const MIN_TICK: i32 = -88722839;
        const MAX_TICK: i32 = 88722839;

        // Maximum tick range to sync (to limit response size)
        // +/- 500000 ticks should cover most practical ranges
        const MAX_TICK_RANGE: i32 = 500000;

        // Convert tick to V2 word position using the offset algorithm
        fn tick_to_word_v2(tick: i32, tick_spacing: i32) -> i64 {
            let mut compressed = (tick as i64) / (tick_spacing as i64);
            if tick < 0 && tick % tick_spacing != 0 {
                compressed -= 1; // Round towards negative infinity
            }
            let raw_index = compressed + BITMAP_OFFSET;
            raw_index / 256
        }

        // Collect pool info for batch request
        let mut batch_infos: Vec<(usize, TickBitmapInfo, i32, i32)> = Vec::new(); // (pool_idx, info, minTick, maxTick)

        for (idx, pool) in pools.iter().enumerate() {
            if let AMM::EkuboPool(ekubo_pool) = pool {
                if ekubo_pool.tick_spacing <= 0 {
                    tracing::warn!(
                        target = "amms::ekubo::sync_tick_bitmaps",
                        pool_id = ?ekubo_pool.pool_id,
                        tick_spacing = ekubo_pool.tick_spacing,
                        "Invalid tick_spacing - skipping bitmap sync for pool"
                    );
                    continue;
                }

                // Calculate tick range to sync
                // Center around current tick, but limit range
                let current_tick = ekubo_pool.tick;
                let half_range = MAX_TICK_RANGE / 2;
                let min_tick = (current_tick - half_range).max(MIN_TICK);
                let max_tick = (current_tick + half_range).min(MAX_TICK);

                batch_infos.push((
                    idx,
                    TickBitmapInfo {
                        poolId: ekubo_pool.pool_id,
                        tickSpacing: ekubo_pool.tick_spacing as u32,
                        minTick: min_tick,
                        maxTick: max_tick,
                    },
                    min_tick,
                    max_tick,
                ));
            }
        }

        if batch_infos.is_empty() {
            return Ok(());
        }

        // Process in chunks to avoid gas limits
        const CHUNK_SIZE: usize = 1;

        for chunk in batch_infos.chunks(CHUNK_SIZE) {
            let infos: Vec<TickBitmapInfo> =
                chunk.iter().map(|(_, info, _, _)| info.clone()).collect();

            let return_data =
                GetEkuboTickBitmapBatchRequest::deploy_builder(provider.clone(), infos)
                    .call_raw()
                    .await
                    .map_err(|e| {
                        AMMError::Msg(format!("Tick bitmap batch request failed: {}", e))
                    })?;

            let all_bitmaps: Vec<Vec<U256>> =
                <Vec<Vec<U256>> as SolValue>::abi_decode(&return_data)
                    .map_err(|e| AMMError::Msg(format!("Failed to decode tick bitmaps: {}", e)))?;

            // Process results
            for ((pool_idx, info, min_tick, max_tick), bitmaps) in
                chunk.iter().zip(all_bitmaps.iter())
            {
                if let AMM::EkuboPool(ref mut ekubo_pool) = pools[*pool_idx] {
                    let tick_spacing = info.tickSpacing as i32;
                    let min_word = tick_to_word_v2(*min_tick, tick_spacing);

                    for (i, bitmap) in bitmaps.iter().enumerate() {
                        if !bitmap.is_zero() {
                            let word = (min_word + i as i64) as i32;
                            ekubo_pool.tick_bitmap.insert(word, *bitmap);
                        }
                    }

                    tracing::trace!(
                        target = "amms::ekubo::sync_tick_bitmaps",
                        pool_id = ?ekubo_pool.pool_id,
                        tick_range = ?(*min_tick, *max_tick),
                        bitmap_count = bitmaps.len(),
                        non_zero_count = bitmaps.iter().filter(|b| !b.is_zero()).count(),
                        "Tick bitmaps synced"
                    );
                }
            }
        }

        Ok(())
    }

    /// Sync tick data for Ekubo pools
    /// Extracts initialized ticks from tick_bitmap and fetches their liquidity data
    /// Chunked to avoid exceeding EVM code size limits (similar to Uniswap V3)
    pub async fn sync_tick_data<N, P>(
        pools: &mut [AMM],
        _block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use super::types::TickInfo;
        use alloy::primitives::Bytes;
        use alloy::sol_types::SolValue;
        use futures::future::BoxFuture;
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::collections::HashMap;
        use GetEkuboTickDataBatchRequest::TickDataInfo;

        // Maximum ticks per batch request to avoid code size/gas limit
        const MAX_TICKS_PER_BATCH: usize = 20;

        // Build a map of pool_id -> pool_idx for result mapping
        let mut pool_id_to_idx: HashMap<[u8; 32], usize> = HashMap::new();
        for (idx, pool) in pools.iter().enumerate() {
            if let AMM::EkuboPool(ekubo_pool) = pool {
                // Ignore invalid pools
                if ekubo_pool.tick_spacing <= 0 {
                    continue;
                }
                pool_id_to_idx.insert(ekubo_pool.pool_id.0, idx);
            }
        }

        if pool_id_to_idx.is_empty() {
            return Ok(());
        }

        // Ekubo V2 bitmap offset
        const BITMAP_OFFSET: i64 = 89421695;

        // Collect all (pool_id, ticks) pairs first
        // We need to convert bitmap word/bit back to tick using V2's algorithm:
        // rawIndex = word * 256 + bit
        // tick = (rawIndex - BITMAP_OFFSET) * tickSpacing
        let pool_ticks_data: Vec<([u8; 32], Vec<i32>)> = pools
            .iter()
            .filter_map(|pool| {
                if let AMM::EkuboPool(ekubo_pool) = pool {
                    let mut ticks: Vec<i32> = Vec::new();
                    for (&word_pos, &bitmap) in ekubo_pool.tick_bitmap.iter() {
                        if bitmap.is_zero() {
                            continue;
                        }
                        for bit in 0..256u32 {
                            if !bitmap.bit(bit as usize) {
                                continue;
                            }
                            // V2 algorithm: rawIndex = word * 256 + bit
                            // tick = (rawIndex - BITMAP_OFFSET) * tickSpacing
                            let raw_index = (word_pos as i64) * 256 + (bit as i64);
                            let compressed = raw_index - BITMAP_OFFSET;
                            let tick = (compressed * (ekubo_pool.tick_spacing as i64)) as i32;
                            ticks.push(tick);
                        }
                    }
                    if !ticks.is_empty() {
                        Some((ekubo_pool.pool_id.0, ticks))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        if pool_ticks_data.is_empty() {
            return Ok(());
        }

        // Build chunked requests
        let mut futures: FuturesUnordered<
            BoxFuture<'_, Result<(Vec<([u8; 32], Vec<i32>)>, Bytes), AMMError>>,
        > = FuturesUnordered::new();

        let mut current_batch: Vec<TickDataInfo> = Vec::new();
        // Track (pool_id, ticks) for each entry in current batch
        let mut current_batch_meta: Vec<([u8; 32], Vec<i32>)> = Vec::new();
        let mut current_batch_ticks = 0usize;

        for (pool_id_bytes, mut ticks) in pool_ticks_data {
            while !ticks.is_empty() {
                let remaining_space = MAX_TICKS_PER_BATCH.saturating_sub(current_batch_ticks);

                if remaining_space == 0 {
                    // Batch is full, flush it
                    let provider_clone = provider.clone();
                    let batch = std::mem::take(&mut current_batch);
                    let meta = std::mem::take(&mut current_batch_meta);
                    current_batch_ticks = 0;

                    futures.push(Box::pin(async move {
                        let return_data =
                            GetEkuboTickDataBatchRequest::deploy_builder(provider_clone, batch)
                                .call_raw()
                                .await?;
                        Ok::<_, AMMError>((meta, return_data))
                    }));
                    continue; // Re-process remaining ticks
                }

                // Take as many ticks as we can fit
                let take_count = remaining_space.min(ticks.len());
                let selected_ticks: Vec<i32> = ticks.drain(0..take_count).collect();

                // Convert to B256 pool_id
                let pool_id = alloy::primitives::B256::from(pool_id_bytes);

                current_batch.push(TickDataInfo {
                    poolId: pool_id,
                    ticks: selected_ticks.clone(),
                });
                current_batch_meta.push((pool_id_bytes, selected_ticks));
                current_batch_ticks += take_count;

                // Flush if batch is full
                if current_batch_ticks >= MAX_TICKS_PER_BATCH {
                    let provider_clone = provider.clone();
                    let batch = std::mem::take(&mut current_batch);
                    let meta = std::mem::take(&mut current_batch_meta);
                    current_batch_ticks = 0;

                    futures.push(Box::pin(async move {
                        let return_data =
                            GetEkuboTickDataBatchRequest::deploy_builder(provider_clone, batch)
                                .call_raw()
                                .await?;
                        Ok::<_, AMMError>((meta, return_data))
                    }));
                }
            }
        }

        // Flush remaining batch
        if !current_batch.is_empty() {
            let provider_clone = provider.clone();
            let batch = std::mem::take(&mut current_batch);
            let meta = std::mem::take(&mut current_batch_meta);

            futures.push(Box::pin(async move {
                let return_data =
                    GetEkuboTickDataBatchRequest::deploy_builder(provider_clone, batch)
                        .call_raw()
                        .await?;
                Ok::<_, AMMError>((meta, return_data))
            }));
        }

        // Process results
        while let Some(res) = futures.next().await {
            let (meta, return_data) = res?;
            let tick_infos: Vec<Vec<(bool, u128, i128)>> =
                <Vec<Vec<(bool, u128, i128)>> as SolValue>::abi_decode(&return_data)?;

            for ((pool_id_bytes, ticks), tick_info_vec) in meta.iter().zip(tick_infos.iter()) {
                if let Some(&pool_idx) = pool_id_to_idx.get(pool_id_bytes) {
                    if let AMM::EkuboPool(ref mut ekubo_pool) = pools[pool_idx] {
                        for (tick_data, &tick) in tick_info_vec.iter().zip(ticks.iter()) {
                            if tick_data.0 {
                                // initialized
                                ekubo_pool.ticks.insert(
                                    tick,
                                    TickInfo {
                                        liquidity_gross: tick_data.1,
                                        liquidity_net: tick_data.2,
                                        initialized: true,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
