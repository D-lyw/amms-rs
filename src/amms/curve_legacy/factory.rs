//! Curve Legacy Factory
//!
//! 通过 Curve AddressProvider 和 Registry 发现 Legacy 池子。
//! 批量预取可安全复用的公共字段，再保留逐池 `init()` 完成完整的类型识别与 Meta 拓扑初始化。

use super::types::{
    CurveLegacyBatchInitContext, CurveLegacyBatchInitHints, CurveLegacyBatchPrefetch,
    CurveLegacyPool, CurveLegacyPoolType,
};
use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::error::AMMError;
use crate::amms::factory::{AutomatedMarketMakerFactory, DiscoverySync};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::eth::Log,
    sol,
    sol_types::SolValue,
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const CURVE_LEGACY_INIT_CHUNK_SIZE: usize = 6;
const CURVE_LEGACY_INTER_CHUNK_SLEEP_MS: u64 = 600;
const CURVE_LEGACY_PREFETCH_STEP: usize = 6;
const CURVE_LEGACY_PREFETCH_SLEEP_MS: u64 = 200;
const ETHEREUM_CHAIN_ID: u64 = 1;

sol! {
    #[sol(rpc)]
    interface IAddressProvider {
        function get_address(uint256 id) external view returns (address);
        function get_registry() external view returns (address);
    }

    #[sol(rpc)]
    interface IRegistry {
        function pool_count() external view returns (uint256);
        function pool_list(uint256 i) external view returns (address);
        function get_coins(address pool) external view returns (address[8]);
        function get_n_coins(address pool) external view returns (uint256[2]);
        function get_decimals(address pool) external view returns (uint256[8]);
    }

    // 手动定义 PoolData 用于返回值解码 (与 Solidity 合约匹配)
    struct PoolData {
        address poolAddress;
        uint8 poolType;
        uint8 nCoins;
        address[] coins;
        uint256[] balances;
        uint8[] decimals;
        uint256 amp;
        uint256 fee;
        uint256 adminFee;
        uint256 d;
        uint256 gamma;
        uint256 midFee;
        uint256 outFee;
        uint256 feeGamma;
        uint256 allowedExtraProfit;
        uint256 adjustmentStep;
        uint256 maHalfTime;
        uint256[] priceScale;
        uint256[] rates;
    }

    // 用于版本检测的接口
    #[sol(rpc)]
    interface IVersionDetect {
        function A() external view returns (uint256);
        function A_precise() external view returns (uint256);
    }
}

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetCurveLegacyPoolDataBatchRequest,
    "src/amms/abi/GetCurveLegacyPoolDataBatchRequest.json"
);

use GetCurveLegacyPoolDataBatchRequest::PoolInput;

// 从 JSON 生成的模块导入 PoolInput 类型

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct CurveLegacyFactory {
    pub address: Address,
    pub pool_type: CurveLegacyPoolType,
    pub creation_block: u64,
}

impl CurveLegacyFactory {
    fn pool_type_to_u8(pool_type: CurveLegacyPoolType) -> u8 {
        match pool_type {
            CurveLegacyPoolType::StableSwap => 0,
            CurveLegacyPoolType::CryptoSwap => 1,
        }
    }

    async fn resolve_batch_init_context<N, P>(provider: P) -> Result<CurveLegacyBatchInitContext, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| AMMError::Msg(format!("Curve Legacy batch init failed to read chain id: {}", e)))?;
        let context = CurveLegacyBatchInitContext::new(Some(chain_id));

        if chain_id != ETHEREUM_CHAIN_ID {
            return Ok(context);
        }

        Ok(context)
    }

    fn pool_data_to_prefetch(data: &PoolData) -> CurveLegacyBatchPrefetch {
        let price_scale = if data.priceScale.is_empty() {
            None
        } else {
            Some(data.priceScale.clone())
        };

        CurveLegacyBatchPrefetch {
            n_coins: data.nCoins,
            coins: data.coins.clone(),
            balances: data.balances.clone(),
            decimals: data.decimals.clone(),
            amp: (data.amp != U256::ZERO).then_some(data.amp),
            fee: Some(data.fee),
            admin_fee: Some(data.adminFee),
            rates: data
                .rates
                .iter()
                .copied()
                .filter(|rate| *rate != U256::ZERO)
                .collect(),
            d: (data.d != U256::ZERO).then_some(data.d),
            gamma: (data.gamma != U256::ZERO).then_some(data.gamma),
            mid_fee: (data.midFee != U256::ZERO).then_some(data.midFee),
            out_fee: (data.outFee != U256::ZERO).then_some(data.outFee),
            fee_gamma: (data.feeGamma != U256::ZERO).then_some(data.feeGamma),
            allowed_extra_profit: (data.allowedExtraProfit != U256::ZERO)
                .then_some(data.allowedExtraProfit),
            adjustment_step: (data.adjustmentStep != U256::ZERO).then_some(data.adjustmentStep),
            ma_half_time: (data.maHalfTime != U256::ZERO).then_some(data.maHalfTime),
            price_scale,
        }
    }

    fn attach_batch_context(amms: &mut [AMM], context: &CurveLegacyBatchInitContext) {
        for amm in amms {
            if let AMM::CurveLegacyPool(pool) = amm {
                pool.batch_init_hints = Some(CurveLegacyBatchInitHints {
                    context: context.clone(),
                    prefetch: context.get_prefetch(pool.address),
                });
            }
        }
    }

    fn apply_prefetch_to_pool(
        pool: &mut CurveLegacyPool,
        context: &CurveLegacyBatchInitContext,
        prefetch: CurveLegacyBatchPrefetch,
    ) {
        context.insert_prefetch(pool.address, prefetch.clone());
        pool.batch_init_hints = Some(CurveLegacyBatchInitHints {
            context: context.clone(),
            prefetch: Some(prefetch),
        });
    }

    async fn prefetch_curve_legacy_pool_data<N, P>(
        amms: &mut [AMM],
        block: BlockId,
        provider: P,
        context: &CurveLegacyBatchInitContext,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::attach_batch_context(amms, context);

        let indexed_inputs = amms
            .iter()
            .enumerate()
            .filter_map(|(idx, amm)| match amm {
                AMM::CurveLegacyPool(pool) => Some((
                    idx,
                    PoolInput {
                        pool: pool.address,
                        poolType: Self::pool_type_to_u8(pool.pool_type),
                    },
                )),
                _ => None,
            })
            .collect::<Vec<_>>();

        for chunk in indexed_inputs.chunks(CURVE_LEGACY_PREFETCH_STEP) {
            let mut inputs = Vec::with_capacity(chunk.len());
            for (_, input) in chunk.iter() {
                inputs.push(input.clone());
            }
            let deployer = GetCurveLegacyPoolDataBatchRequest::deploy_builder(provider.clone(), inputs);

            let chunk_failed = match deployer.call_raw().block(block).await {
                Ok(res) => match <Vec<PoolData> as SolValue>::abi_decode(&res) {
                    Ok(pool_data_list) if pool_data_list.len() == chunk.len() => {
                        for ((idx, _), data) in chunk.iter().zip(pool_data_list.iter()) {
                            if let Some(AMM::CurveLegacyPool(pool)) = amms.get_mut(*idx) {
                                Self::apply_prefetch_to_pool(
                                    pool,
                                    context,
                                    Self::pool_data_to_prefetch(data),
                                );
                            }
                        }
                        false
                    }
                    Ok(pool_data_list) => {
                        tracing::warn!(
                            target: "amms::curve_legacy::init_batch",
                            expected = chunk.len(),
                            actual = pool_data_list.len(),
                            "Curve Legacy batch prefetch returned mismatched pool count; retrying individually"
                        );
                        true
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "amms::curve_legacy::init_batch",
                            error = %e,
                            "Curve Legacy batch prefetch decode failed; retrying individually"
                        );
                        true
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        target: "amms::curve_legacy::init_batch",
                        error = %e,
                        "Curve Legacy batch prefetch RPC failed; retrying individually"
                    );
                    true
                }
            };

            if chunk_failed {
                for (idx, input) in chunk {
                    let deployer = GetCurveLegacyPoolDataBatchRequest::deploy_builder(
                        provider.clone(),
                        vec![input.clone()],
                    );
                    match deployer.call_raw().block(block).await {
                        Ok(res) => {
                            if let Ok(pool_data_list) = <Vec<PoolData> as SolValue>::abi_decode(&res) {
                                if let Some(data) = pool_data_list.first() {
                                    if let Some(AMM::CurveLegacyPool(pool)) = amms.get_mut(*idx) {
                                        Self::apply_prefetch_to_pool(
                                            pool,
                                            context,
                                            Self::pool_data_to_prefetch(data),
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "amms::curve_legacy::init_batch",
                                pool = ?input.pool,
                                error = %e,
                                "Curve Legacy individual prefetch failed; pool will fall back to full init() RPC path"
                            );
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(
                CURVE_LEGACY_PREFETCH_SLEEP_MS,
            ))
            .await;
        }

        Ok(())
    }

    async fn init_curve_legacy_amms<N, P>(
        mut amms: Vec<AMM>,
        block: BlockId,
        provider: P,
        context: &CurveLegacyBatchInitContext,
    ) -> Result<(Vec<AMM>, u32, u32), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::prefetch_curve_legacy_pool_data::<N, _>(&mut amms, block, provider.clone(), context)
            .await?;

        let mut chunks = Vec::new();
        let mut current_chunk = Vec::with_capacity(CURVE_LEGACY_INIT_CHUNK_SIZE);
        for amm in amms {
            current_chunk.push(amm);
            if current_chunk.len() == CURVE_LEGACY_INIT_CHUNK_SIZE {
                chunks.push(std::mem::take(&mut current_chunk));
            }
        }
        if !current_chunk.is_empty() {
            chunks.push(current_chunk);
        }

        let total = chunks.iter().map(|chunk| chunk.len()).sum();
        let mut results: Vec<AMM> = Vec::with_capacity(total);
        let mut success_count = 0u32;
        let mut fail_count = 0u32;
        let num_chunks = chunks.len();

        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut tasks = FuturesUnordered::new();
            for amm in chunk {
                let provider = provider.clone();
                let is_curve_legacy = matches!(&amm, AMM::CurveLegacyPool(_));
                tasks.push(async move {
                    let addr = amm.address();
                    match amm.init(block, provider).await {
                        Ok(initialized) => {
                            tracing::debug!(pool = ?addr, "Successfully initialized Curve Legacy pool");
                            Ok(Some(initialized))
                        }
                        Err(e) => {
                            if is_curve_legacy && CurveLegacyPool::is_fatal_init_error(&e) {
                                tracing::error!(
                                    pool = ?addr,
                                    error = %e,
                                    "Fatal Curve Legacy initialization error"
                                );
                                Err(e)
                            } else {
                                tracing::warn!(pool = ?addr, error = %e, "Failed to initialize Curve Legacy pool, skipping");
                                Ok(None)
                            }
                        }
                    }
                });
            }

            while let Some(result) = tasks.next().await {
                let result = result?;
                if let Some(amm) = result {
                    success_count += 1;
                    results.push(amm);
                } else {
                    fail_count += 1;
                }
            }

            if i < num_chunks.saturating_sub(1) {
                // Curve Legacy per-pool init can fan out into many RPC eth_call probes
                // (ABI fallbacks, subtype detection, Meta/base-pool resolution). Keep
                // batch concurrency conservative and sleep between chunks to avoid
                // provider-side RPS throttling in production.
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    CURVE_LEGACY_INTER_CHUNK_SLEEP_MS,
                ))
                .await;
            }
        }

        Ok((results, success_count, fail_count))
    }

    fn collect_missing_base_pool_dependencies(
        initialized: &[AMM],
        known_addresses: &HashSet<Address>,
    ) -> (Vec<AMM>, Vec<Address>) {
        let mut synthesized = Vec::new();
        let mut fallback_init_addresses = Vec::new();
        let mut seen = known_addresses.clone();

        for amm in initialized {
            let AMM::CurveLegacyPool(pool) = amm else {
                continue;
            };

            let Some(base_addr) = pool.base_pool_address else {
                continue;
            };

            if !seen.insert(base_addr) {
                continue;
            }

            if let Some(base_view) = pool.base_pool_view.as_ref() {
                synthesized.push(AMM::CurveLegacyPool(base_view.to_curve_legacy_pool()));
            } else {
                tracing::warn!(
                    meta_pool = ?pool.address,
                    base_pool = ?base_addr,
                    "Meta pool is missing base_pool_view during batch dependency expansion; falling back to direct base-pool init"
                );
                fallback_init_addresses.push(base_addr);
            }
        }

        (synthesized, fallback_init_addresses)
    }

    fn filter_invalid_initialized_amms(results: &mut Vec<AMM>) -> usize {
        let pre_filter = results.len();
        results.retain(|amm| {
            if let AMM::CurveLegacyPool(pool) = amm {
                match pool.pool_type {
                    CurveLegacyPoolType::CryptoSwap => {
                        let valid = pool.d.is_some()
                            && pool.gamma.is_some()
                            && pool.mid_fee.is_some()
                            && pool.out_fee.is_some()
                            && pool.fee_gamma.is_some();
                        if !valid {
                            tracing::warn!(
                                pool = ?pool.address,
                                pool_type = ?pool.pool_type,
                                has_d = pool.d.is_some(),
                                has_gamma = pool.gamma.is_some(),
                                has_mid_fee = pool.mid_fee.is_some(),
                                has_out_fee = pool.out_fee.is_some(),
                                has_fee_gamma = pool.fee_gamma.is_some(),
                                "Removing CryptoSwap pool: missing required parameters"
                            );
                        }
                        valid
                    }
                    CurveLegacyPoolType::StableSwap => {
                        let valid = pool.amp.is_some();
                        if !valid {
                            tracing::warn!(
                                pool = ?pool.address,
                                "Removing StableSwap pool: missing amp parameter"
                            );
                        }
                        valid
                    }
                }
            } else {
                true
            }
        });
        pre_filter - results.len()
    }

    pub fn new(address: Address, pool_type: CurveLegacyPoolType, creation_block: u64) -> Self {
        Self {
            address,
            pool_type,
            creation_block,
        }
    }

    /// 获取池子地址列表
    pub async fn get_pool_addresses<N, P>(&self, provider: P) -> Result<Vec<Address>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let registry = IRegistry::new(self.address, provider.clone());
        let count = registry
            .pool_count()
            .call()
            .await
            .map_err(|e| AMMError::Msg(e.to_string()))?;
        let count_u64 = count.to::<u64>();

        tracing::info!(
            "Found {} pools in Registry {:?} ({:?})",
            count_u64,
            self.address,
            self.pool_type
        );

        // 1. 获取池子总数
        let count = registry
            .pool_count()
            .call()
            .await
            .map_err(|e| AMMError::Msg(e.to_string()))?;
        let count_u64 = count.to::<u64>();

        tracing::info!(
            "Found {} pools in Registry {:?} ({:?})",
            count_u64,
            self.address,
            self.pool_type
        );

        if count_u64 == 0 {
            return Ok(vec![]);
        }

        // 2. 分批次获取池子地址，严格控制 RPS
        let mut addresses = Vec::new();
        // 降低批量大小以减少并发压力
        let batch_size = 20;
        let mut start_index = 0;

        while start_index < count_u64 {
            let end_index = std::cmp::min(start_index + batch_size, count_u64);
            let current_batch_size = end_index - start_index;

            tracing::info!(
                "Fetching pools {} to {} of {}...",
                start_index,
                end_index,
                count_u64
            );

            let mut tasks = FuturesUnordered::new();
            for i in start_index..end_index {
                let reg = registry.clone();
                tasks.push(async move { reg.pool_list(U256::from(i)).call().await });
            }

            while let Some(res) = tasks.next().await {
                if let Ok(addr) = res {
                    addresses.push(addr);
                }
            }

            start_index += current_batch_size;

            // 批次间添加延迟，避免触发 RPS 限制
            if start_index < count_u64 {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        Ok(addresses)
    }

    /// 获取池子列表 (使用批量获取)
    pub async fn get_pools<N, P>(
        &self,
        block: BlockId,
        provider: P,
    ) -> Result<Vec<CurveLegacyPool>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let addresses = self.get_pool_addresses::<N, _>(provider.clone()).await?;

        if addresses.is_empty() {
            return Ok(vec![]);
        }

        let amms: Vec<AMM> = addresses
            .into_iter()
            .map(|addr| AMM::CurveLegacyPool(CurveLegacyPool::new(addr, self.pool_type)))
            .collect();

        let initialized = Self::init_batch(amms, block, provider).await?;

        Ok(initialized
            .into_iter()
            .filter_map(|amm| {
                if let AMM::CurveLegacyPool(pool) = amm {
                    Some(pool)
                } else {
                    None
                }
            })
            .collect())
    }

    /// 批量初始化池子
    ///
    /// # 设计说明：为什么不是“纯 Solidity 全量批量初始化”
    ///
    /// 与 UniswapV2/V3 等标准化协议不同，Curve Legacy 池存在大量非标行为，
    /// 使得“把完整初始化逻辑全部塞进一个 Solidity 批量合约”并不可靠：
    ///
    /// 1. **调用隔离问题**：Solidity 构造函数中所有外部调用共享 gas 上下文，
    ///    部分老 Vyper 池的 `coins()` 使用 `assert` 做边界检查，失败时消耗全部
    ///    forwarded gas，导致 try/catch 无法捕获 out-of-gas 错误，整个构造函数 revert。
    ///
    /// 2. **Subtype 检测缺失**：Solidity 合约无法检测 Meta/Lending/Plain 子类型，
    ///    无法获取 `base_pool`, `virtual_price`, `lp_token`, `underlying_coins` 等
    ///    Metapool/Lending 池必需的字段，导致这些池无法正确模拟 swap。
    ///
    /// 3. **int128 接口 fallback 不完整**：完整初始化需要按池子能力做 uint256/int128、
    ///    Stable/Crypto、Meta/base-pool 递归探测，纯批量合约难以优雅覆盖这套分支。
    ///
    /// 因此当前实现采用混合策略：
    ///
    /// 1. 先用 Solidity batch 合约预取 `coins/balances/decimals/A/fee` 以及可直接拿到的
    ///    Crypto 参数，尽量压缩重复 RPC。
    /// 2. 再保留 Rust 逐池 `init()` 作为唯一完整初始化路径，负责 family 分类、
    ///    Meta 检测、StableMeta registry fallback、base-pool 递归初始化等复杂逻辑。
    ///
    /// 这样既能减少生产环境的 RPC 压力，又不牺牲 Curve Legacy 各类老池子的兼容性。
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(vec![]);
        }

        let total = amms.len();
        tracing::info!(
            "Initializing {} Curve Legacy pools via batch-prefetch + per-pool init...",
            total
        );

        // 并发初始化所有池子，每个池使用独立的 RPC eth_call
        // NOTE: 当前 Curve Legacy batch init 仍走逐池 init。后续如果恢复 dedicated batch
        // path，Meta/Base 相关初始化字段也必须在 batch path 中完整覆盖，不能丢字段。
        // init() 内部会完整处理：
        //   - family 分类 (StableSwap / CryptoSwap)
        //   - uint256/int128 双 ABI 兼容
        //   - Meta topology 检测与 base_pool_view 物化
        //   - Stable subtype 分类 (Meta / Lending / Plain)
        //   - A_precision 版本检测
        //   - stored_rates 获取
        //   - CryptoSwap 参数 (D, gamma, price_scale 等)
        //
        // 另外，batch init 还要负责把 MetaPool 依赖的 base pool 作为顶级池补回结果集中，
        // 这样上层 state-space / graph / execution 才能把 base pool 当作独立一等池同步维护。

        let batch_context = Self::resolve_batch_init_context::<N, _>(provider.clone()).await?;
        let mut known_addresses: HashSet<Address> = amms.iter().map(AMM::address).collect();
        let (mut results, mut success_count, mut fail_count) =
            Self::init_curve_legacy_amms::<N, _>(
                amms,
                block,
                provider.clone(),
                &batch_context,
            )
            .await?;

        let (mut synthesized_deps, fallback_dep_addrs) =
            Self::collect_missing_base_pool_dependencies(&results, &known_addresses);
        for amm in &synthesized_deps {
            known_addresses.insert(amm.address());
        }

        if !fallback_dep_addrs.is_empty() {
            let fallback_amms: Vec<AMM> = fallback_dep_addrs
                .into_iter()
                .filter(|addr| known_addresses.insert(*addr))
                .map(|addr| {
                    AMM::CurveLegacyPool(CurveLegacyPool::new(
                        addr,
                        CurveLegacyPoolType::StableSwap,
                    ))
                })
                .collect();

            if !fallback_amms.is_empty() {
                let (mut fallback_results, dep_success, dep_fail) =
                    Self::init_curve_legacy_amms::<N, _>(
                        fallback_amms,
                        block,
                        provider,
                        &batch_context,
                    )
                    .await?;
                success_count += dep_success;
                fail_count += dep_fail;
                results.append(&mut fallback_results);
            }
        }

        if !synthesized_deps.is_empty() {
            tracing::info!(
                synthesized = synthesized_deps.len(),
                "Added synthesized Curve Legacy base-pool dependencies for MetaPools"
            );
            results.append(&mut synthesized_deps);
        }

        let filtered = Self::filter_invalid_initialized_amms(&mut results);

        let mut deduped = Vec::with_capacity(results.len());
        let mut emitted = HashSet::with_capacity(results.len());
        for amm in results {
            if emitted.insert(amm.address()) {
                deduped.push(amm);
            } else {
                tracing::debug!(pool = ?amm.address(), "Dropping duplicate Curve Legacy pool from batch init");
            }
        }

        tracing::info!(
            "Curve Legacy init complete: {}/{} requested succeeded, {} failed, {} filtered (invalid params), {} returned after dependency expansion",
            success_count,
            total,
            fail_count,
            filtered,
            deduped.len()
        );

        Ok(deduped)
    }

    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::init_batch(amms, block, provider).await
    }
}

impl DiscoverySync for CurveLegacyFactory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let factory = *self;
        async move {
            let pools = factory.get_pools(to_block, provider).await?;
            Ok(pools.into_iter().map(AMM::CurveLegacyPool).collect())
        }
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        async move { Self::sync_all_pools(amms, to_block, provider).await }
    }
}

impl AutomatedMarketMakerFactory for CurveLegacyFactory {
    type PoolVariant = CurveLegacyPool;

    fn address(&self) -> Address {
        self.address
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        B256::ZERO
    }

    fn create_pool(&self, _log: Log) -> Result<AMM, AMMError> {
        Err(AMMError::Msg(
            "Log-based pool creation not implemented for Curve Legacy".to_string(),
        ))
    }
}
