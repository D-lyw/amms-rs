use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::{AMMError, BatchContractError},
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    get_token_decimals,
    uniswap_v3::{Info, UniswapV3Pool},
    Token,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{aliases::I24, Address, Bytes, Signed, B256, I256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
    transports::BoxFuture,
};
use futures::{stream::FuturesUnordered, StreamExt};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};
use uniswap_v3_math::tick_math::{MAX_TICK, MIN_TICK};

pub mod adaptive_fee;
pub mod timepoint;

const ALGEBRA_SYNC_STEP: u64 = 50_000;
const ALGEBRA_MAX_TICK_DATA_PER_POOL: usize = 1280;
const ALGEBRA_TICK_TREE_SHIFT: i32 = 3466;
const ALGEBRA_TICK_TREE_LEAF_WORDS: i32 = 6932;
const ALGEBRA_TICK_TREE_ROOT_BITS: u8 = 32;
const ALGEBRA_PLUGIN_BEFORE_SWAP_FLAG: u8 = 1 << 0;
const ALGEBRA_PLUGIN_DYNAMIC_FEE_FLAG: u8 = 1 << 7;
const ALGEBRA_BATCH_MAX_IN_FLIGHT: usize = 3;
const ALGEBRA_BATCH_META_STEP: usize = 12;
const ALGEBRA_BATCH_STATE_STEP: usize = 12;
const ALGEBRA_BATCH_TICK_TABLE_STEP: usize = 12;
const ALGEBRA_BATCH_MAX_TICKS: usize = 48;
const ALGEBRA_INTER_BATCH_SLEEP_MS: u64 = 500;

sol! {
    #[allow(missing_docs)]
    #[derive(Debug)]
    #[sol(rpc)]
    contract IAlgebraFactory {
        event Pool(address indexed token0, address indexed token1, address pool);
        event CustomPool(address indexed deployer, address indexed token0, address indexed token1, address pool);
    }

    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAlgebraPool {
        event Initialize(uint160 price, int24 tick);
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed bottomTick,
            int24 indexed topTick,
            uint128 liquidityAmount,
            uint256 amount0,
            uint256 amount1
        );
        event Burn(
            address indexed owner,
            int24 indexed bottomTick,
            int24 indexed topTick,
            uint128 liquidityAmount,
            uint256 amount0,
            uint256 amount1
        );
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 price,
            uint128 liquidity,
            int24 tick
        );
        event SwapFee(address indexed sender, uint24 overrideFee, uint24 pluginFee);
        event BurnFee(address indexed owner, uint24 pluginFee);
        event CommunityFee(uint16 communityFeeNew);
        event Fee(uint16 fee);
        event Plugin(address newPluginAddress);
        event PluginConfig(uint8 newPluginConfig);
        event TickSpacing(int24 newTickSpacing);

        function token0() external view returns (address);
        function token1() external view returns (address);
        function tickSpacing() external view returns (int24);
        function fee() external view returns (uint16);
        function plugin() external view returns (address);
        function isUnlocked() external view returns (bool);

        function safelyGetStateOfAMM()
            external
            view
            returns (
                uint160 sqrtPrice,
                int24 tick,
                uint16 lastFee,
                uint8 pluginConfig_,
                uint128 activeLiquidity,
                int24 nextTick,
                int24 previousTick
            );
        function globalState()
            external
            view
            returns (
                uint160 price,
                int24 tick,
                uint16 lastFee,
                uint8 pluginConfig,
                uint16 communityFee,
                bool unlocked
            );

        function tickTable(int16 wordPosition) external view returns (uint256);
        function tickTreeRoot() external view returns (uint32);
        function tickTreeSecondLayer(int16) external view returns (uint256);
        function ticks(int24 tick)
            external
            view
            returns (
                uint256 liquidityTotal,
                int128 liquidityDelta,
                int24 prevTick,
                int24 nextTick,
                uint256 outerFeeGrowth0Token,
                uint256 outerFeeGrowth1Token
            );
        function liquidity() external view returns (uint128);
    }

    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    contract IAlgebraPoolExtendedSwapEvents {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 price,
            uint128 liquidity,
            int24 tick,
            uint24 overrideFee,
            uint24 pluginFee
        );
    }

    // Some Algebra forks (e.g. QuickSwap V4 / Hydrex) emit a Burn event
    // with appended pluginFee: Burn(..., uint24 pluginFee).
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    contract IAlgebraPoolExtendedBurnEvents {
        event Burn(
            address indexed owner,
            int24 indexed bottomTick,
            int24 indexed topTick,
            uint128 liquidityAmount,
            uint256 amount0,
            uint256 amount1,
            uint24 pluginFee
        );
    }
}

sol! {
    #[sol(rpc)]
    GetAlgebraPoolStaticMetaBatchRequest,
    "src/amms/abi/GetAlgebraPoolStaticMetaBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetAlgebraPoolStateBatchRequest,
    "src/amms/abi/GetAlgebraPoolStateBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetAlgebraPoolTickTableBatchRequest,
    "src/amms/abi/GetAlgebraPoolTickTableBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetAlgebraPoolTickDataBatchRequest,
    "src/amms/abi/GetAlgebraPoolTickDataBatchRequest.json",
}

// Multicall3 interface for batch-reads (observations/timepoints).
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }

        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

// Volatility oracle & dynamic fee view functions exposed by the plugin contract.
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IAlgebraPluginVolatility {
        /// Mirror of VolatilityOracle.Timepoint return type.
        function timepoints(uint256 index) external view returns (
            bool initialized,
            uint32 blockTimestamp,
            int56 tickCumulative,
            uint88 volatilityCumulative,
            int24 tick,
            int24 averageTick,
            uint16 windowStartIndex
        );
        function timepointIndex() external view returns (uint16 index);
        function lastTimepointTimestamp() external view returns (uint32);
        /// Dynamic fee config (exposed by DynamicFeeConnector).
        function feeConfig() external view returns (
            uint16 alpha1,
            uint16 alpha2,
            uint32 beta1,
            uint32 beta2,
            uint16 gamma1,
            uint16 gamma2,
            uint16 baseFee
        );
    }
}

// Dynamic fee manager interface — used to decode FeeConfiguration events
// emitted by the plugin contract when the fee parameters change on-chain.
sol! {
    #[allow(missing_docs)]
    interface IDynamicFeeManager {
        struct AlgebraFeeConfiguration {
            uint16 alpha1;
            uint16 alpha2;
            uint32 beta1;
            uint32 beta2;
            uint16 gamma1;
            uint16 gamma2;
            uint16 baseFee;
        }
        event FeeConfiguration(AlgebraFeeConfiguration feeConfiguration);
    }
}

const MULTICALL3_ADDRESS: alloy::primitives::Address =
    alloy::primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");

#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct AlgebraIntegralFactory {
    pub address: Address,
    pub creation_block: u64,
    #[serde(default)]
    pub include_custom_pools: bool,
    #[serde(default)]
    pub custom_deployers: Vec<Address>,
}

impl AlgebraIntegralFactory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
            include_custom_pools: true,
            custom_deployers: vec![],
        }
    }

    pub fn with_custom_deployers(mut self, deployers: Vec<Address>) -> Self {
        self.custom_deployers = deployers;
        self
    }

    fn accepts_custom_deployer(&self, deployer: Address) -> bool {
        if self.custom_deployers.is_empty() {
            return true;
        }
        self.custom_deployers.contains(&deployer)
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
        let mut signatures = vec![IAlgebraFactory::Pool::SIGNATURE_HASH];
        if self.include_custom_pools {
            signatures.push(IAlgebraFactory::CustomPool::SIGNATURE_HASH);
        }

        let disc_filter = Filter::new()
            .event_signature(FilterSet::from(signatures))
            .address(vec![self.address]);

        let mut futures = FuturesUnordered::new();
        let tip = block_number.as_u64().unwrap_or_default();
        let mut start = self.creation_block;

        while start <= tip {
            let end = (start + ALGEBRA_SYNC_STEP).min(tip);
            let mut block_filter = disc_filter.clone();
            block_filter = block_filter.from_block(start).to_block(end);

            let provider = provider.clone();
            futures.push(async move { provider.get_logs(&block_filter).await });
            start = end.saturating_add(1);
        }

        let mut pools = vec![];
        while let Some(res) = futures.next().await {
            let logs = res?;
            for log in logs {
                if let Ok(pool) = self.create_pool(log.clone()) {
                    pools.push(pool);
                }
            }
        }

        Ok(pools)
    }

    async fn sync_pool_state<N, P>(
        pool: &mut AlgebraIntegralPool,
        block: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let contract = IAlgebraPool::new(pool.inner.address, provider);

        let state = contract.safelyGetStateOfAMM().block(block).call().await?;
        pool.inner.sqrt_price = U256::from(state.sqrtPrice);
        pool.inner.tick = state.tick.as_i32();
        pool.inner.liquidity = state.activeLiquidity;
        pool.last_fee = u32::from(state.lastFee);
        pool.plugin_config = state.pluginConfig_;
        pool.next_tick_global = state.nextTick.as_i32();
        pool.prev_tick_global = state.previousTick.as_i32();

        if let Ok(global_state) = contract.globalState().block(block).call().await {
            pool.community_fee = global_state.communityFee;
            pool.unlocked = global_state.unlocked;
        }
        if let Ok(unlocked) = contract.isUnlocked().block(block).call().await {
            pool.unlocked = unlocked;
        }

        if let Ok(plugin) = contract.plugin().block(block).call().await {
            pool.plugin = plugin;
        }

        let spacing = contract.tickSpacing().block(block).call().await?;
        pool.tick_spacing = spacing.as_i32();
        pool.inner.tick_spacing = pool.tick_spacing;
        if pool.inner.tick_spacing <= 0 {
            return Err(AMMError::Msg("invalid algebra tick spacing".to_string()));
        }

        if let Ok(fee) = contract.fee().block(block).call().await {
            pool.inner.fee = u32::from(fee);
        } else if pool.last_fee > 0 {
            pool.inner.fee = pool.last_fee;
        }

        if pool.inner.fee == 0 {
            pool.inner.fee = 100;
        }

        pool.refresh_fee_mode();

        Ok(())
    }

    async fn sync_tick_table_for_pool<N, P>(
        pool: &mut AlgebraIntegralPool,
        block: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let contract = IAlgebraPool::new(pool.inner.address, provider);
        let root = contract.tickTreeRoot().block(block).call().await?;
        Arc::make_mut(&mut pool.inner.tick_bitmap).clear();

        for node_idx in 0..ALGEBRA_TICK_TREE_ROOT_BITS {
            if (root & (1u32 << node_idx)) == 0 {
                continue;
            }

            let second_layer = contract
                .tickTreeSecondLayer(i16::from(node_idx))
                .block(block)
                .call()
                .await?;

            for bit_idx in 0..256usize {
                if (second_layer & (U256::from(1u8) << U256::from(bit_idx))) == U256::ZERO {
                    continue;
                }

                let leaf_idx = i32::from(node_idx) * 256 + bit_idx as i32;
                if leaf_idx < 0 || leaf_idx >= ALGEBRA_TICK_TREE_LEAF_WORDS {
                    continue;
                }

                let word_pos = leaf_idx - ALGEBRA_TICK_TREE_SHIFT;
                let word_i16 = match i16::try_from(word_pos) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let bitmap = contract.tickTable(word_i16).block(block).call().await?;
                if bitmap != U256::ZERO {
                    pool.load_algebra_tick_table_word(word_i16, bitmap);
                }
            }
        }

        Ok(())
    }

    async fn sync_tick_data_for_pool<N, P>(
        pool: &mut AlgebraIntegralPool,
        block: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let ticks = pool.initialized_ticks_from_bitmap();

        if ticks.is_empty() {
            return Ok(());
        }

        let contract = IAlgebraPool::new(pool.inner.address, provider);
        for chunk in ticks.chunks(ALGEBRA_MAX_TICK_DATA_PER_POOL) {
            for tick in chunk {
                let i24 = I24::try_from(*tick)
                    .map_err(|_| AMMError::Msg(format!("tick out of range: {}", tick)))?;
                let data = contract.ticks(i24).block(block).call().await?;

                let liquidity_gross = u128::try_from(data.liquidityTotal).unwrap_or(u128::MAX);
                let info = Info {
                    liquidity_gross,
                    liquidity_net: data.liquidityDelta,
                    initialized: liquidity_gross > 0,
                };
                Arc::make_mut(&mut pool.inner.ticks).insert(*tick, info);
            }
        }

        Ok(())
    }

    async fn sync_static_meta_batch<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut requests: Vec<(usize, Vec<Address>)> = Vec::new();
        let mut start = 0usize;
        while start < pools.len() {
            let end = (start + ALGEBRA_BATCH_META_STEP).min(pools.len());
            let pool_addresses = pools[start..end]
                .iter()
                .map(|pool| pool.address())
                .collect();
            requests.push((start, pool_addresses));
            start = end;
        }

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let mut iter = requests.into_iter();

        while let Some((start, addresses)) = iter.next() {
            let provider = provider.clone();
            futures.push(Box::pin(async move {
                Ok::<(usize, Vec<(bool, Address, Address, i32, u16)>), AMMError>((
                    start,
                    Self::fetch_static_meta_batch_with_fallback::<N, _>(
                        provider,
                        block_number,
                        addresses,
                    )
                    .await?,
                ))
            }));

            if futures.len() >= ALGEBRA_BATCH_MAX_IN_FLIGHT {
                if let Some(res) = futures.next().await {
                    let (start, decoded) = res?;
                    let end = (start + decoded.len()).min(pools.len());
                    for (meta, pool) in decoded.into_iter().zip(pools[start..end].iter_mut()) {
                        let AMM::AlgebraIntegralPool(pool) = pool else {
                            continue;
                        };
                        if !meta.0 {
                            continue;
                        }
                        pool.inner.token_a.address = meta.1;
                        pool.inner.token_b.address = meta.2;
                        pool.tick_spacing = meta.3;
                        pool.inner.tick_spacing = meta.3;
                        pool.inner.fee = u32::from(meta.4);
                        if pool.inner.fee == 0 {
                            pool.inner.fee = 100;
                        }
                        pool.refresh_fee_mode();
                    }
                    sleep(Duration::from_millis(2)).await;
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (start, decoded) = res?;
            let end = (start + decoded.len()).min(pools.len());
            for (meta, pool) in decoded.into_iter().zip(pools[start..end].iter_mut()) {
                let AMM::AlgebraIntegralPool(pool) = pool else {
                    continue;
                };
                if !meta.0 {
                    continue;
                }
                pool.inner.token_a.address = meta.1;
                pool.inner.token_b.address = meta.2;
                pool.tick_spacing = meta.3;
                pool.inner.tick_spacing = meta.3;
                pool.inner.fee = u32::from(meta.4);
                if pool.inner.fee == 0 {
                    pool.inner.fee = 100;
                }
                pool.refresh_fee_mode();
            }
            sleep(Duration::from_millis(2)).await;
        }

        Ok(())
    }

    async fn sync_state_batch<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut requests: Vec<(usize, Vec<Address>)> = Vec::new();
        let mut start = 0usize;
        while start < pools.len() {
            let end = (start + ALGEBRA_BATCH_STATE_STEP).min(pools.len());
            let pool_addresses = pools[start..end]
                .iter()
                .map(|pool| pool.address())
                .collect();
            requests.push((start, pool_addresses));
            start = end;
        }

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let mut iter = requests.into_iter();

        while let Some((start, addresses)) = iter.next() {
            let provider = provider.clone();
            futures.push(Box::pin(async move {
                Ok::<
                    (
                        usize,
                        Vec<(
                            bool,
                            U256,
                            i32,
                            u128,
                            u16,
                            u32,
                            i32,
                            i32,
                            u16,
                            bool,
                            Address,
                        )>,
                    ),
                    AMMError,
                >((
                    start,
                    Self::fetch_state_batch_with_fallback::<N, _>(
                        provider,
                        block_number,
                        addresses,
                    )
                    .await?,
                ))
            }));

            if futures.len() >= ALGEBRA_BATCH_MAX_IN_FLIGHT {
                if let Some(res) = futures.next().await {
                    let (start, decoded) = res?;
                    let end = (start + decoded.len()).min(pools.len());
                    for (state, pool) in decoded.into_iter().zip(pools[start..end].iter_mut()) {
                        let AMM::AlgebraIntegralPool(pool) = pool else {
                            continue;
                        };
                        if !state.0 {
                            continue;
                        }

                        pool.inner.sqrt_price = state.1;
                        pool.inner.tick = state.2;
                        pool.inner.liquidity = state.3;
                        pool.last_fee = u32::from(state.4);
                        pool.plugin_config = state.5 as u8;
                        pool.next_tick_global = state.6;
                        pool.prev_tick_global = state.7;
                        pool.community_fee = state.8;
                        pool.unlocked = state.9;
                        pool.plugin = state.10;

                        if pool.last_fee > 0 {
                            pool.inner.fee = pool.last_fee;
                        }
                        if pool.inner.fee == 0 {
                            pool.inner.fee = 100;
                        }
                    }
                    sleep(Duration::from_millis(2)).await;
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (start, decoded) = res?;
            let end = (start + decoded.len()).min(pools.len());
            for (state, pool) in decoded.into_iter().zip(pools[start..end].iter_mut()) {
                let AMM::AlgebraIntegralPool(pool) = pool else {
                    continue;
                };
                if !state.0 {
                    continue;
                }

                pool.inner.sqrt_price = state.1;
                pool.inner.tick = state.2;
                pool.inner.liquidity = state.3;
                pool.last_fee = u32::from(state.4);
                pool.plugin_config = state.5 as u8;
                pool.next_tick_global = state.6;
                pool.prev_tick_global = state.7;
                pool.community_fee = state.8;
                pool.unlocked = state.9;
                pool.plugin = state.10;

                if pool.last_fee > 0 {
                    pool.inner.fee = pool.last_fee;
                }
                if pool.inner.fee == 0 {
                    pool.inner.fee = 100;
                }
            }
            sleep(Duration::from_millis(2)).await;
        }

        Ok(())
    }

    async fn sync_tick_table_batch<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut requests: Vec<(usize, Vec<Address>)> = Vec::new();
        let mut start = 0usize;
        while start < pools.len() {
            let end = (start + ALGEBRA_BATCH_TICK_TABLE_STEP).min(pools.len());
            let pool_addresses = pools[start..end]
                .iter()
                .map(|pool| pool.address())
                .collect();
            requests.push((start, pool_addresses));
            start = end;
        }

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let mut iter = requests.into_iter();

        while let Some((start, addresses)) = iter.next() {
            let provider = provider.clone();
            futures.push(Box::pin(async move {
                Ok::<(usize, Vec<Vec<U256>>), AMMError>((
                    start,
                    Self::fetch_tick_table_batch_with_fallback::<N, _>(
                        provider,
                        block_number,
                        addresses,
                    )
                    .await?,
                ))
            }));

            if futures.len() >= ALGEBRA_BATCH_MAX_IN_FLIGHT {
                if let Some(res) = futures.next().await {
                    let (start, decoded) = res?;
                    let end = (start + decoded.len()).min(pools.len());
                    for (tables, pool) in decoded.into_iter().zip(pools[start..end].iter_mut()) {
                        let AMM::AlgebraIntegralPool(pool) = pool else {
                            continue;
                        };
                        Arc::make_mut(&mut pool.inner.tick_bitmap).clear();
                        for chunk in tables.chunks_exact(2) {
                            let word_pos = I256::from_raw(chunk[0]).as_i16();
                            let bitmap = chunk[1];
                            if bitmap != U256::ZERO {
                                pool.load_algebra_tick_table_word(word_pos, bitmap);
                            }
                        }
                    }
                    sleep(Duration::from_millis(2)).await;
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (start, decoded) = res?;
            let end = (start + decoded.len()).min(pools.len());
            for (tables, pool) in decoded.into_iter().zip(pools[start..end].iter_mut()) {
                let AMM::AlgebraIntegralPool(pool) = pool else {
                    continue;
                };
                Arc::make_mut(&mut pool.inner.tick_bitmap).clear();
                for chunk in tables.chunks_exact(2) {
                    let word_pos = I256::from_raw(chunk[0]).as_i16();
                    let bitmap = chunk[1];
                    if bitmap != U256::ZERO {
                        pool.load_algebra_tick_table_word(word_pos, bitmap);
                    }
                }
            }
            sleep(Duration::from_millis(2)).await;
        }

        Ok(())
    }

    async fn sync_tick_data_batch<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use GetAlgebraPoolTickDataBatchRequest::TickDataInfo;

        let pool_ticks = pools
            .par_iter()
            .filter_map(|pool| {
                if let AMM::AlgebraIntegralPool(pool) = pool {
                    let initialized_ticks: Vec<Signed<24, 1>> = pool
                        .initialized_ticks_from_bitmap()
                        .into_iter()
                        .filter_map(|tick| Signed::<24, 1>::try_from(tick).ok())
                        .collect();

                    if initialized_ticks.is_empty() {
                        None
                    } else {
                        Some((pool.inner.address, initialized_ticks))
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(Address, Vec<Signed<24, 1>>)>>();

        let mut group_ticks = 0usize;
        let mut group = vec![];
        let mut requests: Vec<Vec<TickDataInfo>> = Vec::new();

        for (pool_address, mut ticks) in pool_ticks {
            while !ticks.is_empty() {
                let remaining_ticks = ALGEBRA_BATCH_MAX_TICKS - group_ticks;
                let selected_ticks = ticks.drain(0..remaining_ticks.min(ticks.len()));
                group_ticks += selected_ticks.len();

                group.push(TickDataInfo {
                    pool: pool_address,
                    ticks: selected_ticks.collect(),
                });

                if group_ticks >= ALGEBRA_BATCH_MAX_TICKS {
                    requests.push(std::mem::take(&mut group));
                    group_ticks = 0;
                }
            }
        }

        if !group.is_empty() {
            requests.push(std::mem::take(&mut group));
        }

        let mut pool_set = pools
            .iter_mut()
            .map(|pool| (pool.address(), pool))
            .collect::<HashMap<Address, &mut AMM>>();

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let mut iter = requests.into_iter();

        while let Some(calldata) = iter.next() {
            let provider = provider.clone();
            futures.push(Box::pin(async move {
                Ok::<(Vec<TickDataInfo>, Vec<Vec<(bool, u128, i128)>>), AMMError>((
                    calldata.clone(),
                    Self::fetch_tick_data_batch_with_fallback::<N, _>(
                        provider,
                        block_number,
                        calldata,
                    )
                    .await?,
                ))
            }));

            if futures.len() >= ALGEBRA_BATCH_MAX_IN_FLIGHT {
                if let Some(res) = futures.next().await {
                    let (tick_info, decoded) = res?;

                    for (tick_results, tick_info_item) in decoded.iter().zip(tick_info.iter()) {
                        let Some(pool) = pool_set.get_mut(&tick_info_item.pool) else {
                            continue;
                        };
                        let AMM::AlgebraIntegralPool(pool) = pool else {
                            continue;
                        };

                        for (tick_data, tick_idx) in
                            tick_results.iter().zip(tick_info_item.ticks.iter())
                        {
                            let info = Info {
                                liquidity_gross: tick_data.1,
                                liquidity_net: tick_data.2,
                                initialized: tick_data.0,
                            };
                            Arc::make_mut(&mut pool.inner.ticks).insert(tick_idx.as_i32(), info);
                        }
                    }
                    sleep(Duration::from_millis(2)).await;
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (tick_info, decoded) = res?;

            for (tick_results, tick_info_item) in decoded.iter().zip(tick_info.iter()) {
                let Some(pool) = pool_set.get_mut(&tick_info_item.pool) else {
                    continue;
                };
                let AMM::AlgebraIntegralPool(pool) = pool else {
                    continue;
                };

                for (tick_data, tick_idx) in tick_results.iter().zip(tick_info_item.ticks.iter()) {
                    let info = Info {
                        liquidity_gross: tick_data.1,
                        liquidity_net: tick_data.2,
                        initialized: tick_data.0,
                    };
                    Arc::make_mut(&mut pool.inner.ticks).insert(tick_idx.as_i32(), info);
                }
            }
            sleep(Duration::from_millis(2)).await;
        }

        Ok(())
    }

    async fn fetch_static_meta_batch_with_fallback<N, P>(
        provider: P,
        block_number: BlockId,
        addresses: Vec<Address>,
    ) -> Result<Vec<(bool, Address, Address, i32, u16)>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut pending = vec![(0usize, addresses)];
        let mut segments = Vec::new();

        while let Some((offset, batch)) = pending.pop() {
            let batch_len = batch.len();
            let result: Result<Vec<(bool, Address, Address, i32, u16)>, AMMError> = async {
                let return_data = GetAlgebraPoolStaticMetaBatchRequest::deploy_builder(
                    provider.clone(),
                    batch.clone(),
                )
                .call_raw()
                .block(block_number)
                .await
                .map_err(AMMError::from)?;
                <Vec<(bool, Address, Address, i32, u16)> as SolValue>::abi_decode(&return_data)
                    .map_err(AMMError::from)
            }
            .await;

            match result {
                Ok(decoded) => segments.push((offset, decoded)),
                Err(err) if batch_len > 1 => {
                    let mid = batch_len / 2;
                    warn!(
                        batch_size = batch_len,
                        split_left = mid,
                        split_right = batch_len - mid,
                        error = ?err,
                        "Algebra static meta batch failed, splitting"
                    );
                    pending.push((offset + mid, batch[mid..].to_vec()));
                    pending.push((offset, batch[..mid].to_vec()));
                }
                Err(err) => return Err(err),
            }
        }

        segments.sort_by_key(|(offset, _)| *offset);
        Ok(segments
            .into_iter()
            .flat_map(|(_, decoded)| decoded)
            .collect())
    }

    async fn fetch_state_batch_with_fallback<N, P>(
        provider: P,
        block_number: BlockId,
        addresses: Vec<Address>,
    ) -> Result<
        Vec<(
            bool,
            U256,
            i32,
            u128,
            u16,
            u32,
            i32,
            i32,
            u16,
            bool,
            Address,
        )>,
        AMMError,
    >
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut pending = vec![(0usize, addresses)];
        let mut segments = Vec::new();

        while let Some((offset, batch)) = pending.pop() {
            let batch_len = batch.len();
            let result: Result<
                Vec<(
                    bool,
                    U256,
                    i32,
                    u128,
                    u16,
                    u32,
                    i32,
                    i32,
                    u16,
                    bool,
                    Address,
                )>,
                AMMError,
            > = async {
                let return_data = GetAlgebraPoolStateBatchRequest::deploy_builder(
                    provider.clone(),
                    batch.clone(),
                )
                .call_raw()
                .block(block_number)
                .await
                .map_err(AMMError::from)?;
                <Vec<(
                    bool,
                    U256,
                    i32,
                    u128,
                    u16,
                    u32,
                    i32,
                    i32,
                    u16,
                    bool,
                    Address,
                )> as SolValue>::abi_decode(&return_data)
                .map_err(AMMError::from)
            }
            .await;

            match result {
                Ok(decoded) => segments.push((offset, decoded)),
                Err(err) if batch_len > 1 => {
                    let mid = batch_len / 2;
                    warn!(
                        batch_size = batch_len,
                        split_left = mid,
                        split_right = batch_len - mid,
                        error = ?err,
                        "Algebra state batch failed, splitting"
                    );
                    pending.push((offset + mid, batch[mid..].to_vec()));
                    pending.push((offset, batch[..mid].to_vec()));
                }
                Err(err) => return Err(err),
            }
        }

        segments.sort_by_key(|(offset, _)| *offset);
        Ok(segments
            .into_iter()
            .flat_map(|(_, decoded)| decoded)
            .collect())
    }

    async fn fetch_tick_table_batch_with_fallback<N, P>(
        provider: P,
        block_number: BlockId,
        addresses: Vec<Address>,
    ) -> Result<Vec<Vec<U256>>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut pending = vec![(0usize, addresses)];
        let mut segments = Vec::new();

        while let Some((offset, batch)) = pending.pop() {
            let batch_len = batch.len();
            let result: Result<Vec<Vec<U256>>, AMMError> = async {
                let return_data = GetAlgebraPoolTickTableBatchRequest::deploy_builder(
                    provider.clone(),
                    batch.clone(),
                )
                .call_raw()
                .block(block_number)
                .await
                .map_err(AMMError::from)?;
                <Vec<Vec<U256>> as SolValue>::abi_decode(&return_data).map_err(AMMError::from)
            }
            .await;

            match result {
                Ok(decoded) => segments.push((offset, decoded)),
                Err(err) if batch_len > 1 => {
                    let mid = batch_len / 2;
                    warn!(
                        batch_size = batch_len,
                        split_left = mid,
                        split_right = batch_len - mid,
                        error = ?err,
                        "Algebra tick table batch failed, splitting"
                    );
                    pending.push((offset + mid, batch[mid..].to_vec()));
                    pending.push((offset, batch[..mid].to_vec()));
                }
                Err(err) => return Err(err),
            }
        }

        segments.sort_by_key(|(offset, _)| *offset);
        Ok(segments
            .into_iter()
            .flat_map(|(_, decoded)| decoded)
            .collect())
    }

    async fn fetch_tick_data_batch_with_fallback<N, P>(
        provider: P,
        block_number: BlockId,
        calldata: Vec<GetAlgebraPoolTickDataBatchRequest::TickDataInfo>,
    ) -> Result<Vec<Vec<(bool, u128, i128)>>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut pending = vec![(0usize, calldata)];
        let mut segments = Vec::new();

        while let Some((offset, batch)) = pending.pop() {
            let batch_len = batch.len();
            let result: Result<Vec<Vec<(bool, u128, i128)>>, AMMError> = async {
                let return_data = GetAlgebraPoolTickDataBatchRequest::deploy_builder(
                    provider.clone(),
                    batch.clone(),
                )
                .call_raw()
                .block(block_number)
                .await
                .map_err(AMMError::from)?;
                <Vec<Vec<(bool, u128, i128)>> as SolValue>::abi_decode(&return_data)
                    .map_err(AMMError::from)
            }
            .await;

            match result {
                Ok(decoded) => segments.push((offset, decoded)),
                Err(err) if batch_len > 1 => {
                    let mid = batch_len / 2;
                    warn!(
                        batch_size = batch_len,
                        split_left = mid,
                        split_right = batch_len - mid,
                        error = ?err,
                        "Algebra tick data batch failed, splitting"
                    );
                    pending.push((offset + mid, batch[mid..].to_vec()));
                    pending.push((offset, batch[..mid].to_vec()));
                }
                Err(err) => return Err(err),
            }
        }

        segments.sort_by_key(|(offset, _)| *offset);
        Ok(segments
            .into_iter()
            .flat_map(|(_, decoded)| decoded)
            .collect())
    }

    pub async fn init_batch<N, P>(
        pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut out: Vec<AMM> = pools
            .into_iter()
            .filter(|amm| matches!(amm, AMM::AlgebraIntegralPool(_)))
            .collect();

        Self::sync_static_meta_batch::<N, _>(&mut out, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(ALGEBRA_INTER_BATCH_SLEEP_MS)).await;
        Self::sync_state_batch::<N, _>(&mut out, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(ALGEBRA_INTER_BATCH_SLEEP_MS)).await;
        Self::sync_tick_table_batch::<N, _>(&mut out, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(ALGEBRA_INTER_BATCH_SLEEP_MS)).await;
        Self::sync_tick_data_batch::<N, _>(&mut out, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(ALGEBRA_INTER_BATCH_SLEEP_MS)).await;

        Self::sync_token_decimals::<N, _>(&mut out, provider.clone()).await?;

        out.retain(|amm| {
            let AMM::AlgebraIntegralPool(pool) = amm else {
                return false;
            };
            !pool.inner.token_a.address.is_zero()
                && !pool.inner.token_b.address.is_zero()
                && pool.inner.tick_spacing > 0
                && pool.inner.token_a.decimals > 0
                && pool.inner.token_b.decimals > 0
        });

        // Refresh fee mode (plugin was set by sync_state_batch above) and
        // seed fee config + timepoints for dynamic-fee pools.
        for amm in &mut out {
            let AMM::AlgebraIntegralPool(pool) = amm else {
                continue;
            };
            pool.refresh_fee_mode();
            if pool.is_dynamic_fee_enabled() && !pool.plugin.is_zero() {
                pool.seed_fee_config::<N, _>(block_number, provider.clone())
                    .await;
                pool.seed_timepoints::<N, _>(block_number, provider.clone())
                    .await;
            }
        }

        for amm in &mut out {
            if let AMM::AlgebraIntegralPool(pool) = amm {
                if let Ok(price) =
                    pool.calculate_price(pool.inner.token_a.address, pool.inner.token_b.address)
                {
                    pool.inner.token_a_price = price;
                    pool.inner.token_b_price = if price == 0.0 { 0.0 } else { 1.0 / price };
                }
            }
        }

        Ok(out)
    }

    pub async fn sync_all_pools<N, P>(
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::sync_state_batch::<N, _>(&mut pools, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(ALGEBRA_INTER_BATCH_SLEEP_MS)).await;

        for amm in &mut pools {
            if let AMM::AlgebraIntegralPool(pool) = amm {
                Arc::make_mut(&mut pool.inner.tick_bitmap).clear();
                Arc::make_mut(&mut pool.inner.ticks).clear();
                // Clear stale timepoint cache — will be re-seeded below.
                pool.timepoints = None;
                pool.fee_config = None;
            }
        }

        Self::sync_tick_table_batch::<N, _>(&mut pools, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(ALGEBRA_INTER_BATCH_SLEEP_MS)).await;
        Self::sync_tick_data_batch::<N, _>(&mut pools, block_number, provider.clone()).await?;

        // Re-seed fee config + timepoints for dynamic-fee pools.
        for amm in &mut pools {
            let AMM::AlgebraIntegralPool(pool) = amm else {
                continue;
            };
            if pool.is_dynamic_fee_enabled() && !pool.plugin.is_zero() {
                pool.seed_fee_config::<N, _>(block_number, provider.clone())
                    .await;
                pool.seed_timepoints::<N, _>(block_number, provider.clone())
                    .await;
            }
        }

        for amm in &mut pools {
            let AMM::AlgebraIntegralPool(pool) = amm else {
                continue;
            };

            if let Ok(price) =
                pool.calculate_price(pool.inner.token_a.address, pool.inner.token_b.address)
            {
                pool.inner.token_a_price = price;
                pool.inner.token_b_price = if price == 0.0 { 0.0 } else { 1.0 / price };
            }
        }

        Ok(pools)
    }

    pub async fn sync_token_decimals<N, P>(
        pools: &mut [AMM],
        provider: P,
    ) -> Result<(), BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut tokens = HashSet::new();
        for amm in pools.iter() {
            if let AMM::AlgebraIntegralPool(pool) = amm {
                tokens.insert(pool.inner.token_a.address);
                tokens.insert(pool.inner.token_b.address);
            }
        }

        let token_decimals = get_token_decimals(tokens.into_iter().collect(), provider).await?;

        for amm in pools.iter_mut() {
            if let AMM::AlgebraIntegralPool(pool) = amm {
                if let Some(decimals) = token_decimals.get(&pool.inner.token_a.address) {
                    pool.inner.token_a.decimals = *decimals;
                }
                if let Some(decimals) = token_decimals.get(&pool.inner.token_b.address) {
                    pool.inner.token_b.decimals = *decimals;
                }
            }
        }

        Ok(())
    }
}

/// Configuration for Algebra's dynamic fee plugin.
///
/// Mirrors `AlgebraFeeConfiguration` from the Algebra plugins monorepo:
/// packages/dynamic-fee/contracts/types/AlgebraFeeConfiguration.sol
///
/// These parameters define the two sigmoids used in the AdaptiveFee formula:
///   fee = baseFee + sigmoid1(volatility, gamma1, alpha1, beta1)
///                  + sigmoid2(volatility, gamma2, alpha2, beta2)
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AlgebraFeeConfig {
    /// Max value of the first sigmoid (hundredths of a bip, i.e. 1e-6)
    pub alpha1: u16,
    /// Max value of the second sigmoid
    pub alpha2: u16,
    /// Shift along the x-axis (volatility) for the first sigmoid
    pub beta1: u32,
    /// Shift along the x-axis (volatility) for the second sigmoid
    pub beta2: u32,
    /// Horizontal stretch factor for the first sigmoid
    pub gamma1: u16,
    /// Horizontal stretch factor for the second sigmoid
    pub gamma2: u16,
    /// Minimum possible fee (hundredths of a bip, i.e. 1e-6)
    pub base_fee: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlgebraIntegralPool {
    #[serde(flatten)]
    pub inner: UniswapV3Pool,
    #[serde(default)]
    pub plugin: Address,
    #[serde(default)]
    pub plugin_config: u8,
    #[serde(default)]
    pub community_fee: u16,
    #[serde(default)]
    pub last_fee: u32,
    #[serde(default)]
    pub last_plugin_fee: u32,
    #[serde(default)]
    pub last_override_fee: u32,
    #[serde(default)]
    pub tick_spacing: i32,
    #[serde(default)]
    pub next_tick_global: i32,
    #[serde(default)]
    pub prev_tick_global: i32,
    #[serde(default)]
    pub unlocked: bool,
    #[serde(default)]
    pub custom_pool_deployer: Address,
    #[serde(default)]
    pub fee_mode: AlgebraFeeMode,
    /// Dynamic fee configuration (None if pool uses static fee)
    #[serde(default)]
    pub fee_config: Option<AlgebraFeeConfig>,
    /// Locally-maintained volatility oracle timepoints (None if not seeded)
    #[serde(default)]
    pub timepoints: Option<timepoint::TimepointCache>,
    /// Set to true when a FeeConfiguration event is received from the plugin,
    /// signalling that `fee_config` should be re-read on the next opportunity.
    #[serde(default)]
    pub stale_fee_config: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlgebraFeeMode {
    /// pluginConfig has no DYNAMIC_FEE flag: swap fee is effectively static from pool state.
    #[default]
    Static,
    /// pluginConfig has DYNAMIC_FEE but no BEFORE_SWAP hook:
    /// fee() can be dynamic via plugin.getCurrentFee(), but not per-swap override params.
    DynamicGlobal,
    /// pluginConfig has DYNAMIC_FEE + BEFORE_SWAP and plugin connected:
    /// beforeSwap may provide (overrideFee, pluginFee) per swap.
    DynamicHooked,
}

impl AlgebraIntegralPool {
    pub fn new(address: Address) -> Self {
        Self {
            inner: UniswapV3Pool::new(address),
            ..Default::default()
        }
    }

    fn refresh_prices(&mut self) {
        if let Ok(price) =
            self.calculate_price(self.inner.token_a.address, self.inner.token_b.address)
        {
            self.inner.token_a_price = price;
            self.inner.token_b_price = if price == 0.0 { 0.0 } else { 1.0 / price };
        }
    }

    fn load_algebra_tick_table_word(&mut self, raw_word_pos: i16, raw_bitmap: U256) {
        if raw_bitmap == U256::ZERO {
            return;
        }
        let spacing = self.inner.tick_spacing;
        if spacing <= 0 {
            return;
        }

        for bit in 0..256usize {
            let bit_mask = U256::from(1u8) << U256::from(bit as u32);
            if (raw_bitmap & bit_mask) == U256::ZERO {
                continue;
            }

            let raw_tick = raw_word_pos as i32 * 256 + bit as i32;
            if raw_tick % spacing != 0 {
                continue;
            }

            let compressed = raw_tick / spacing;
            let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(compressed);
            let compressed_mask = U256::from(1u8) << U256::from(bit_pos as u32);
            Arc::make_mut(&mut self.inner.tick_bitmap)
                .entry(word_pos)
                .and_modify(|word| *word |= compressed_mask)
                .or_insert(compressed_mask);
        }
    }

    fn initialized_ticks_from_bitmap(&self) -> Vec<i32> {
        let mut ticks = Vec::new();
        let spacing = self.inner.tick_spacing;
        if spacing <= 0 {
            return ticks;
        }

        for (&word_pos, &bitmap) in self.inner.tick_bitmap.iter() {
            if bitmap == U256::ZERO {
                continue;
            }

            for i in 0..256usize {
                if (bitmap & (U256::from(1u8) << U256::from(i as u32))) == U256::ZERO {
                    continue;
                }

                let compressed_tick = word_pos as i32 * 256 + i as i32;
                let Some(tick) = compressed_tick.checked_mul(spacing) else {
                    continue;
                };
                if (MIN_TICK..=MAX_TICK).contains(&tick) {
                    ticks.push(tick);
                }
            }
        }
        ticks.sort_unstable();
        ticks.dedup();
        ticks
    }

    /// Apply mint/burn delta to both tick structures and in-range active liquidity.
    fn apply_liquidity_delta(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        self.inner
            .modify_position(tick_lower, tick_upper, liquidity_delta)
    }

    fn reconcile_dynamic_fee(&mut self) {
        if self.last_override_fee > 0 {
            self.inner.fee = self
                .last_override_fee
                .saturating_add(self.last_plugin_fee)
                .min(999_999);
        } else if self.last_fee > 0 {
            self.inner.fee = self
                .last_fee
                .saturating_add(self.last_plugin_fee)
                .min(999_999);
        } else if self.last_plugin_fee > 0 {
            self.inner.fee = self.last_plugin_fee.min(999_999);
        }
        if self.inner.fee == 0 {
            self.inner.fee = 100;
        }
    }

    pub fn refresh_fee_mode(&mut self) {
        let dynamic_enabled = (self.plugin_config & ALGEBRA_PLUGIN_DYNAMIC_FEE_FLAG) != 0;
        let before_swap_enabled = (self.plugin_config & ALGEBRA_PLUGIN_BEFORE_SWAP_FLAG) != 0;
        let plugin_connected = !self.plugin.is_zero();

        self.fee_mode = if dynamic_enabled && before_swap_enabled && plugin_connected {
            AlgebraFeeMode::DynamicHooked
        } else if dynamic_enabled && plugin_connected {
            AlgebraFeeMode::DynamicGlobal
        } else {
            AlgebraFeeMode::Static
        };
    }

    pub fn fee_mode(&self) -> AlgebraFeeMode {
        self.fee_mode
    }

    pub fn is_dynamic_fee_enabled(&self) -> bool {
        !matches!(self.fee_mode, AlgebraFeeMode::Static)
    }

    pub fn is_before_swap_fee_hook_enabled(&self) -> bool {
        matches!(self.fee_mode, AlgebraFeeMode::DynamicHooked)
    }

    /// Compute the current dynamic fee from local timepoint data.
    ///
    /// Uses the locally-maintained timepoint cache + fee config to compute
    /// the same fee that the on-chain plugin's `getCurrentFee()` would return.
    ///
    /// Returns `None` if the timepoint cache is not seeded (falls back to
    /// a static fee value).
    pub fn compute_fee(&self, block_timestamp: u32) -> Option<u32> {
        if self.stale_fee_config {
            return None;
        }
        let config = self.fee_config.as_ref()?;
        let timepoints = self.timepoints.as_ref()?;

        // Get average volatility from the oracle cache.
        let volatility = timepoints
            .get_average_volatility(block_timestamp, self.inner.tick)
            .map(|v| v as u64)?;

        let fee = adaptive_fee::get_fee(volatility, config);
        Some(u32::from(fee))
    }

    /// Read fee configuration from the plugin contract.
    pub async fn seed_fee_config<N, P>(&mut self, block_number: BlockId, provider: P)
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let plugin_addr = self.plugin;
        if plugin_addr.is_zero() {
            return;
        }

        let plugin = IAlgebraPluginVolatility::new(plugin_addr, provider);
        match plugin.feeConfig().block(block_number).call().await {
            Ok(cfg) => {
                self.fee_config = Some(AlgebraFeeConfig {
                    alpha1: cfg.alpha1,
                    alpha2: cfg.alpha2,
                    beta1: cfg.beta1,
                    beta2: cfg.beta2,
                    gamma1: cfg.gamma1,
                    gamma2: cfg.gamma2,
                    base_fee: cfg.baseFee,
                });
                self.stale_fee_config = false;
            }
            Err(_) => {}
        }
    }

    /// Read timepoint data from the plugin contract via Multicall3 and seed the local cache.
    pub async fn seed_timepoints<N, P>(&mut self, block_number: BlockId, provider: P)
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let plugin_addr = match self.plugin.is_zero() {
            true => return,
            false => self.plugin,
        };

        let plugin = IAlgebraPluginVolatility::new(plugin_addr, provider.clone());

        // 1. Read the current timepoint index.
        let tp_idx = match plugin.timepointIndex().block(block_number).call().await {
            Ok(v) => v,
            Err(_) => return,
        };

        // 2. Read the last timepoint to get windowStartIndex.
        let last_tp = match plugin
            .timepoints(U256::from(tp_idx))
            .block(block_number)
            .call()
            .await
        {
            Ok(v) => v,
            Err(_) => return,
        };

        let cardinality = tp_idx.wrapping_add(1);

        // Read ALL timepoints from windowStartIndex → tp_idx.
        // This provides the full 24h window needed for volatility calculation.
        // If the range is large, we chunk into multiple Multicall3 calls
        // (1500 indices per batch).
        let start_idx = last_tp.windowStartIndex;
        let total_count = range_len_16(start_idx, tp_idx);
        let all_indices: Vec<u16> = (0..total_count)
            .map(|i| start_idx.wrapping_add(i))
            .collect();
        const MC_CHUNK: usize = 1500;

        let mc3 = IMulticall3::new(MULTICALL3_ADDRESS, provider.clone());
        let mut timepoints: Vec<(u16, timepoint::Timepoint)> = Vec::new();

        for chunk in all_indices.chunks(MC_CHUNK) {
            let mut mc_calls: Vec<IMulticall3::Call3> = Vec::with_capacity(chunk.len());
            for &idx in chunk {
                let cd = IAlgebraPluginVolatility::timepointsCall {
                    index: U256::from(idx),
                }
                .abi_encode();
                mc_calls.push(IMulticall3::Call3 {
                    target: plugin_addr,
                    allowFailure: true,
                    callData: cd.into(),
                });
            }

            let results = match mc3.aggregate3(mc_calls).block(block_number).call().await {
                Ok(r) => r,
                Err(_) => return,
            };

            for (res, &idx) in results.iter().zip(chunk.iter()) {
                if res.returnData.is_empty() || !res.success {
                    continue;
                }
                let dec =
                    IAlgebraPluginVolatility::timepointsCall::abi_decode_returns(&res.returnData)
                        .ok();
                let dec = match dec {
                    Some(d) => d,
                    None => continue,
                };
                if !dec.initialized || dec.blockTimestamp == 0 {
                    continue;
                }
                timepoints.push((
                    idx,
                    timepoint::Timepoint {
                        initialized: true,
                        block_timestamp: dec.blockTimestamp,
                        tick_cumulative: dec.tickCumulative.unchecked_into::<i64>(),
                        volatility_cumulative: u128::try_from(dec.volatilityCumulative)
                            .unwrap_or(0),
                        tick: dec.tick.as_i32(),
                        average_tick: dec.averageTick.as_i32(),
                        window_start_index: dec.windowStartIndex,
                    },
                ));
            }
        }

        if timepoints.is_empty() {
            return;
        }

        let seeded = timepoint::TimepointCache::seed(&timepoints, tp_idx, cardinality);
        self.timepoints = Some(seeded);
    }

    async fn refresh_dynamic_fee_snapshot_from_chain<N, P>(&mut self, provider: P)
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        if !self.is_dynamic_fee_enabled() || self.plugin.is_zero() {
            self.stale_fee_config = false;
            self.fee_config = None;
            self.timepoints = None;
            self.inner.fee = self.last_fee.max(1);
            return;
        }

        let latest = BlockId::latest();
        if self.stale_fee_config || self.fee_config.is_none() {
            self.seed_fee_config::<N, _>(latest, provider.clone()).await;
        }
        if self.timepoints.is_none() {
            self.seed_timepoints::<N, _>(latest, provider.clone()).await;
        }

        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(ts) = u32::try_from(now_ts) {
            if let Some(fee) = self.compute_fee(ts) {
                self.inner.fee = fee;
                return;
            }
        }

        self.inner.fee = self.last_fee.max(1);
    }
}

impl AutomatedMarketMaker for AlgebraIntegralPool {
    fn address(&self) -> Address {
        self.inner.address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        None
    }

    fn last_synced_block(&self) -> u64 {
        self.inner.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.inner.last_synced_block = self.inner.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IAlgebraPool::Initialize::SIGNATURE_HASH,
            IAlgebraPool::Mint::SIGNATURE_HASH,
            IAlgebraPool::Burn::SIGNATURE_HASH,
            IAlgebraPoolExtendedBurnEvents::Burn::SIGNATURE_HASH,
            IAlgebraPool::Swap::SIGNATURE_HASH,
            IAlgebraPoolExtendedSwapEvents::Swap::SIGNATURE_HASH,
            IAlgebraPool::SwapFee::SIGNATURE_HASH,
            IAlgebraPool::BurnFee::SIGNATURE_HASH,
            IAlgebraPool::CommunityFee::SIGNATURE_HASH,
            IAlgebraPool::Fee::SIGNATURE_HASH,
            IAlgebraPool::Plugin::SIGNATURE_HASH,
            IAlgebraPool::PluginConfig::SIGNATURE_HASH,
            IAlgebraPool::TickSpacing::SIGNATURE_HASH,
            IDynamicFeeManager::FeeConfiguration::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.topics()[0];
        let mut sync_action = SyncAction::None;

        match event_signature {
            IAlgebraPool::Initialize::SIGNATURE_HASH => {
                let event = IAlgebraPool::Initialize::decode_log(log.as_ref())?;
                self.inner.sqrt_price = U256::from(event.price);
                self.inner.tick = event.tick.as_i32();
                self.refresh_prices();
            }
            IAlgebraPool::Swap::SIGNATURE_HASH => {
                let event = IAlgebraPool::Swap::decode_log(log.as_ref())?;
                // Write timepoint with PRE-swap tick before updating state.
                if let Some(ref mut tps) = self.timepoints {
                    if let Some(ts) = log.block_timestamp.and_then(|v| u32::try_from(v).ok()) {
                        tps.write(ts, self.inner.tick);
                    }
                }
                self.inner.sqrt_price = U256::from(event.price);
                self.inner.liquidity = event.liquidity;
                self.inner.tick = event.tick.as_i32();
                self.refresh_prices();
            }
            IAlgebraPoolExtendedSwapEvents::Swap::SIGNATURE_HASH => {
                let event = IAlgebraPoolExtendedSwapEvents::Swap::decode_log(log.as_ref())?;
                // Write timepoint with PRE-swap tick (same as basic Swap above).
                if let Some(ref mut tps) = self.timepoints {
                    if let Some(ts) = log.block_timestamp.and_then(|v| u32::try_from(v).ok()) {
                        tps.write(ts, self.inner.tick);
                    }
                }
                self.inner.sqrt_price = U256::from(event.price);
                self.inner.liquidity = event.liquidity;
                self.inner.tick = event.tick.as_i32();
                self.last_override_fee = event.overrideFee.to::<u32>();
                self.last_plugin_fee = event.pluginFee.to::<u32>();
                if self.last_override_fee > 0 {
                    self.inner.fee = self
                        .last_override_fee
                        .saturating_add(self.last_plugin_fee)
                        .min(999_999);
                } else if self.last_plugin_fee > 0 {
                    self.inner.fee = self
                        .last_fee
                        .saturating_add(self.last_plugin_fee)
                        .min(999_999);
                }
                self.refresh_prices();
            }
            IAlgebraPool::Mint::SIGNATURE_HASH => {
                let event = IAlgebraPool::Mint::decode_log(log.as_ref())?;
                let delta = i128::try_from(event.liquidityAmount).unwrap_or(i128::MAX);
                if let Err(e) = self.apply_liquidity_delta(
                    event.bottomTick.as_i32(),
                    event.topTick.as_i32(),
                    delta,
                ) {
                    warn!(address = ?self.inner.address, err = ?e, "mint sync failed; request resync");
                    return Ok(SyncAction::Resync);
                }
            }
            IAlgebraPool::Burn::SIGNATURE_HASH => {
                let event = IAlgebraPool::Burn::decode_log(log.as_ref())?;
                let delta = i128::try_from(event.liquidityAmount)
                    .map(|v| -v)
                    .unwrap_or(i128::MIN + 1);
                if let Err(e) = self.apply_liquidity_delta(
                    event.bottomTick.as_i32(),
                    event.topTick.as_i32(),
                    delta,
                ) {
                    warn!(address = ?self.inner.address, err = ?e, "burn sync failed; request resync");
                    return Ok(SyncAction::Resync);
                }
            }
            IAlgebraPoolExtendedBurnEvents::Burn::SIGNATURE_HASH => {
                let event = IAlgebraPoolExtendedBurnEvents::Burn::decode_log(log.as_ref())?;
                let delta = i128::try_from(event.liquidityAmount)
                    .map(|v| -v)
                    .unwrap_or(i128::MIN + 1);
                if let Err(e) = self.apply_liquidity_delta(
                    event.bottomTick.as_i32(),
                    event.topTick.as_i32(),
                    delta,
                ) {
                    warn!(address = ?self.inner.address, err = ?e, "extended burn sync failed; request resync");
                    return Ok(SyncAction::Resync);
                }
            }
            IAlgebraPool::SwapFee::SIGNATURE_HASH => {
                let event = IAlgebraPool::SwapFee::decode_log(log.as_ref())?;
                self.last_override_fee = event.overrideFee.to::<u32>();
                self.last_plugin_fee = event.pluginFee.to::<u32>();
                if self.last_override_fee > 0 {
                    self.inner.fee = self
                        .last_override_fee
                        .saturating_add(self.last_plugin_fee)
                        .min(999_999);
                } else if self.last_plugin_fee > 0 {
                    self.inner.fee = self
                        .last_fee
                        .saturating_add(self.last_plugin_fee)
                        .min(999_999);
                }
            }
            IAlgebraPool::BurnFee::SIGNATURE_HASH => {
                let event = IAlgebraPool::BurnFee::decode_log(log.as_ref())?;
                self.last_plugin_fee = event.pluginFee.to::<u32>();
            }
            IAlgebraPool::CommunityFee::SIGNATURE_HASH => {
                let event = IAlgebraPool::CommunityFee::decode_log(log.as_ref())?;
                self.community_fee = event.communityFeeNew;
            }
            IAlgebraPool::Fee::SIGNATURE_HASH => {
                let event = IAlgebraPool::Fee::decode_log(log.as_ref())?;
                self.inner.fee = u32::from(event.fee);
            }
            // This event is emitted by the PLUGIN contract, not the pool.
            // The StateSpace layer resolves plugin addresses and routes the
            // event here.  When received, we mark the local fee config as
            // stale so the next seed_fee_config() call re-reads it.
            IDynamicFeeManager::FeeConfiguration::SIGNATURE_HASH => {
                // Decode only for validation; the struct fields are ignored
                // because we will re-read the full config from the plugin.
                let _event = IDynamicFeeManager::FeeConfiguration::decode_log(log.as_ref())?;
                self.stale_fee_config = true;
                // Reset fee_config so compute_fee() falls back until re-seed.
                self.fee_config = None;
                self.inner.fee = self.last_fee.max(1);
                // Fee config is plugin-side storage; refresh it asynchronously.
                sync_action = SyncAction::AsyncUpdate;
            }
            IAlgebraPool::Plugin::SIGNATURE_HASH => {
                let event = IAlgebraPool::Plugin::decode_log(log.as_ref())?;
                self.plugin = event.newPluginAddress;
                // Plugin switch invalidates plugin-scoped dynamic-fee context.
                self.fee_config = None;
                self.timepoints = None;
                self.stale_fee_config = !self.plugin.is_zero();
                self.inner.fee = self.last_fee.max(1);
                self.refresh_fee_mode();
                sync_action = SyncAction::AsyncUpdate;
            }
            IAlgebraPool::PluginConfig::SIGNATURE_HASH => {
                let event = IAlgebraPool::PluginConfig::decode_log(log.as_ref())?;
                self.plugin_config = event.newPluginConfig;
                self.refresh_fee_mode();
                if self.is_dynamic_fee_enabled() && !self.plugin.is_zero() {
                    self.fee_config = None;
                    self.stale_fee_config = true;
                } else {
                    self.fee_config = None;
                    self.timepoints = None;
                    self.stale_fee_config = false;
                    self.inner.fee = self.last_fee.max(1);
                }
                sync_action = SyncAction::AsyncUpdate;
            }
            IAlgebraPool::TickSpacing::SIGNATURE_HASH => {
                let event = IAlgebraPool::TickSpacing::decode_log(log.as_ref())?;
                self.tick_spacing = event.newTickSpacing.as_i32();
                self.inner.tick_spacing = self.tick_spacing;
                return Ok(SyncAction::Resync);
            }
            _ => return Ok(SyncAction::None),
        }

        self.reconcile_dynamic_fee();
        self.refresh_fee_mode();
        // Per-swap fee overrides are consumed by reconcile_dynamic_fee above.
        // Clear them so they don't contaminate the next event's fee calculation.
        self.last_override_fee = 0;
        self.last_plugin_fee = 0;

        // When dynamic fee is enabled and local timepoints are seeded, use
        // the locally-computed fee (this mirrors what the plugin's contract
        // `getCurrentFee()` would return).
        if self.is_dynamic_fee_enabled() && self.timepoints.is_some() {
            if let Some(ts) = log.block_timestamp.and_then(|v| u32::try_from(v).ok()) {
                if let Some(fee) = self.compute_fee(ts) {
                    self.inner.fee = fee;
                }
            }
        }

        Ok(sync_action)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.inner.token_a.address, self.inner.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        self.inner.calculate_price(base_token, quote_token)
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        self.inner.spot_price(base_token, quote_token)
    }

    fn has_sufficient_liquidity(&self) -> bool {
        self.inner.has_sufficient_liquidity()
    }

    fn decimals(&self, token: Address) -> u8 {
        self.inner.decimals(token)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        self.inner.simulate_swap(base_token, quote_token, amount_in)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        // The fee is kept up-to-date by sync() (event-driven) or init() (RPC).
        // Dynamic fee computation requires a block timestamp, which is not
        // available in this context — the caller must ensure the fee has
        // been refreshed via sync() or an explicit call to compute_fee().
        self.inner
            .simulate_swap_mut(base_token, quote_token, amount_in)
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        self.inner
            .simulate_swap_exact_out(base_token, quote_token, amount_out)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let contract = IAlgebraPool::new(self.inner.address, provider.clone());

        self.inner.token_a = Token::new(contract.token0().call().await?, provider.clone()).await?;
        self.inner.token_b = Token::new(contract.token1().call().await?, provider.clone()).await?;

        AlgebraIntegralFactory::sync_pool_state::<N, _>(&mut self, block_number, provider.clone())
            .await?;
        Arc::make_mut(&mut self.inner.tick_bitmap).clear();
        Arc::make_mut(&mut self.inner.ticks).clear();

        AlgebraIntegralFactory::sync_tick_table_for_pool::<N, _>(
            &mut self,
            block_number,
            provider.clone(),
        )
        .await?;

        AlgebraIntegralFactory::sync_tick_data_for_pool::<N, _>(
            &mut self,
            block_number,
            provider.clone(),
        )
        .await?;

        self.refresh_prices();
        self.refresh_fee_mode();

        // Seed fee config + timepoints from plugin if dynamic fee is enabled.
        if self.is_dynamic_fee_enabled() && !self.plugin.is_zero() {
            self.seed_fee_config::<N, _>(block_number, provider.clone())
                .await;
            self.seed_timepoints::<N, _>(block_number, provider.clone())
                .await;
        }

        Ok(self)
    }

    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let contract = IAlgebraPool::new(self.inner.address, provider.clone());

        if let Ok(plugin) = contract.plugin().call().await {
            self.plugin = plugin;
        }
        if let Ok(state) = contract.safelyGetStateOfAMM().call().await {
            self.last_fee = u32::from(state.lastFee);
            self.plugin_config = state.pluginConfig_;
            self.inner.liquidity = state.activeLiquidity;
            self.next_tick_global = state.nextTick.as_i32();
            self.prev_tick_global = state.previousTick.as_i32();
            self.inner.sqrt_price = U256::from(state.sqrtPrice);
            self.inner.tick = state.tick.as_i32();
            self.inner.fee = self.last_fee.max(1);
        }
        if let Ok(global_state) = contract.globalState().call().await {
            self.community_fee = global_state.communityFee;
            self.unlocked = global_state.unlocked;
        }
        if let Ok(unlocked) = contract.isUnlocked().call().await {
            self.unlocked = unlocked;
        }
        self.refresh_prices();
        self.refresh_fee_mode();
        self.refresh_dynamic_fee_snapshot_from_chain::<N, _>(provider)
            .await;

        Ok(())
    }
}

impl AutomatedMarketMakerFactory for AlgebraIntegralFactory {
    type PoolVariant = AlgebraIntegralPool;

    fn address(&self) -> Address {
        self.address
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        if log.topics()[0] == IAlgebraFactory::Pool::SIGNATURE_HASH {
            let evt = IAlgebraFactory::Pool::decode_log(&log.inner)?;
            return Ok(AMM::AlgebraIntegralPool(AlgebraIntegralPool {
                inner: UniswapV3Pool {
                    address: evt.pool,
                    token_a: evt.token0.into(),
                    token_b: evt.token1.into(),
                    ..Default::default()
                },
                ..Default::default()
            }));
        }

        if log.topics()[0] == IAlgebraFactory::CustomPool::SIGNATURE_HASH {
            let evt = IAlgebraFactory::CustomPool::decode_log(&log.inner)?;
            if !self.accepts_custom_deployer(evt.deployer) {
                return Err(AMMError::Msg("custom deployer filtered".to_string()));
            }

            return Ok(AMM::AlgebraIntegralPool(AlgebraIntegralPool {
                inner: UniswapV3Pool {
                    address: evt.pool,
                    token_a: evt.token0.into(),
                    token_b: evt.token1.into(),
                    ..Default::default()
                },
                custom_pool_deployer: evt.deployer,
                ..Default::default()
            }));
        }

        Err(AMMError::UnrecognizedEventSignature(log.topics()[0]))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        IAlgebraFactory::Pool::SIGNATURE_HASH
    }
}

impl DiscoverySync for AlgebraIntegralFactory {
    async fn discover<N, P>(&self, to_block: BlockId, provider: P) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        self.get_all_pools::<N, _>(to_block, provider).await
    }

    async fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let need_init = amms.iter().any(|amm| match amm {
            AMM::AlgebraIntegralPool(p) => {
                p.inner.token_a.address.is_zero() || p.inner.token_b.address.is_zero()
            }
            _ => false,
        });

        if need_init {
            info!("initializing algebra pools in batch");
            AlgebraIntegralFactory::init_batch::<N, _>(amms, to_block, provider).await
        } else {
            AlgebraIntegralFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
        }
    }
}

/// Number of steps from `from` to `to` in a circular buffer of size 2^16
/// (wrapping arithmetic is desired, matching Solidity's unchecked behaviour).
fn range_len_16(from: u16, to: u16) -> u16 {
    to.wrapping_sub(from).wrapping_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, LogData};

    fn mk_event_log(address: Address, topics: Vec<B256>, data: Vec<u8>) -> Log {
        let Some(log_data) = LogData::new(topics, Bytes::from(data)) else {
            panic!("failed to build test log data");
        };

        Log {
            inner: alloy::primitives::Log {
                address,
                data: log_data,
            },
            block_hash: None,
            block_number: Some(1),
            block_timestamp: Some(1),
            transaction_hash: None,
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    fn i24_topic(v: i32) -> B256 {
        let mut out = [if v < 0 { 0xff } else { 0x00 }; 32];
        out[28..32].copy_from_slice(&v.to_be_bytes());
        B256::from(out)
    }

    #[test]
    fn sync_handles_extended_burn_event() {
        let pool_addr = address!("00000000000000000000000000000000000000aa");
        let owner = address!("00000000000000000000000000000000000000bb");
        let sender = address!("00000000000000000000000000000000000000cc");

        let mut pool = AlgebraIntegralPool::new(pool_addr);
        pool.inner.tick_spacing = 1;
        pool.tick_spacing = 1;
        pool.inner.tick = 0;
        pool.inner.liquidity = 1_000;

        // 1) Seed the range with a canonical Mint event.
        let mint_liquidity = 100u128;
        let mint_log = mk_event_log(
            pool_addr,
            vec![
                IAlgebraPool::Mint::SIGNATURE_HASH,
                owner.into_word(),
                i24_topic(-10),
                i24_topic(10),
            ],
            (sender, mint_liquidity, U256::ZERO, U256::ZERO).abi_encode(),
        );

        let mint_action = pool.sync(&mint_log).expect("mint sync");
        assert_eq!(mint_action, SyncAction::None);
        assert_eq!(pool.inner.liquidity, 1_100);

        // 2) Apply fork-specific extended Burn event (0x932214d4...).
        let burn_liquidity = 40u128;
        let burn_log = mk_event_log(
            pool_addr,
            vec![
                IAlgebraPoolExtendedBurnEvents::Burn::SIGNATURE_HASH,
                owner.into_word(),
                i24_topic(-10),
                i24_topic(10),
            ],
            (burn_liquidity, U256::ZERO, U256::ZERO, 0u32).abi_encode(),
        );

        let burn_action = pool.sync(&burn_log).expect("extended burn sync");
        assert_eq!(burn_action, SyncAction::None);
        assert_eq!(pool.inner.liquidity, 1_060);
    }

    #[test]
    fn sync_events_include_extended_burn_signature() {
        let pool = AlgebraIntegralPool::new(address!("00000000000000000000000000000000000000aa"));
        let sigs = pool.sync_events();
        assert!(sigs.contains(&IAlgebraPoolExtendedBurnEvents::Burn::SIGNATURE_HASH));
    }

    #[test]
    fn sync_fee_configuration_requests_async_update() {
        let pool_addr = address!("00000000000000000000000000000000000000aa");
        let plugin_addr = address!("00000000000000000000000000000000000000dd");

        let mut pool = AlgebraIntegralPool::new(pool_addr);
        pool.plugin = plugin_addr;
        pool.last_fee = 321;
        pool.fee_config = Some(AlgebraFeeConfig::default());
        pool.stale_fee_config = false;

        let cfg = IDynamicFeeManager::AlgebraFeeConfiguration {
            alpha1: 10,
            alpha2: 20,
            beta1: 30,
            beta2: 40,
            gamma1: 50,
            gamma2: 60,
            baseFee: 70,
        };
        let log = mk_event_log(
            plugin_addr,
            vec![IDynamicFeeManager::FeeConfiguration::SIGNATURE_HASH],
            (cfg,).abi_encode(),
        );

        let action = pool.sync(&log).expect("fee config sync");
        assert_eq!(action, SyncAction::AsyncUpdate);
        assert!(pool.stale_fee_config);
        assert!(pool.fee_config.is_none());
        assert_eq!(pool.inner.fee, 321);
    }

    #[test]
    fn sync_plugin_config_event_marks_stale_and_requests_async_update() {
        let pool_addr = address!("00000000000000000000000000000000000000aa");
        let plugin_addr = address!("00000000000000000000000000000000000000dd");

        let mut pool = AlgebraIntegralPool::new(pool_addr);
        pool.plugin = plugin_addr;
        pool.last_fee = 400;
        pool.plugin_config = 0;
        pool.refresh_fee_mode();
        pool.fee_config = Some(AlgebraFeeConfig::default());
        pool.stale_fee_config = false;

        let log = mk_event_log(
            pool_addr,
            vec![IAlgebraPool::PluginConfig::SIGNATURE_HASH],
            {
                let mut data = vec![0u8; 32];
                data[31] = ALGEBRA_PLUGIN_DYNAMIC_FEE_FLAG;
                data
            },
        );

        let action = pool.sync(&log).expect("plugin config sync");
        assert_eq!(action, SyncAction::AsyncUpdate);
        assert!(pool.is_dynamic_fee_enabled());
        assert!(pool.stale_fee_config);
        assert!(pool.fee_config.is_none());
    }

    #[test]
    fn sync_plugin_event_clears_dynamic_context_and_requests_async_update() {
        let pool_addr = address!("00000000000000000000000000000000000000aa");
        let plugin_addr = address!("00000000000000000000000000000000000000dd");

        let mut pool = AlgebraIntegralPool::new(pool_addr);
        pool.last_fee = 250;
        pool.plugin = plugin_addr;
        pool.plugin_config = ALGEBRA_PLUGIN_DYNAMIC_FEE_FLAG;
        pool.refresh_fee_mode();
        pool.fee_config = Some(AlgebraFeeConfig::default());
        pool.timepoints = Some(timepoint::TimepointCache::empty());
        pool.stale_fee_config = false;

        let log = mk_event_log(
            pool_addr,
            vec![IAlgebraPool::Plugin::SIGNATURE_HASH],
            (plugin_addr,).abi_encode(),
        );

        let action = pool.sync(&log).expect("plugin sync");
        assert_eq!(action, SyncAction::AsyncUpdate);
        assert!(pool.fee_config.is_none());
        assert!(pool.timepoints.is_none());
        assert!(pool.stale_fee_config);
        assert_eq!(pool.inner.fee, 250);
    }
}
