//! Curve NG Factory 发现机制
//!
//! 负责发现和初始化 Curve NG 协议的池子。

use super::{
    types::{CurveIndexSignature, CurveNGPool, CurveNGPoolType, CurveNGTwoCryptoVariant},
    AMMError,
};
use crate::amms::amm::{AutomatedMarketMaker, AMM};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    sol,
    sol_types::SolValue,
};
use futures::{stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use std::collections::HashMap;

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface ICurveNGFactory {
        function pool_count() external view returns (uint256);
        function pool_list(uint256 i) external view returns (address);
    }

    #[derive(Debug)]
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
        uint256[] priceScale;
        uint256[] rates;
        uint8[] assetTypes;
        bool supportsStoredRates;
        bool supportsOffpegFeeMultiplier;
        uint8 coinsIndexSignature;
        uint8 balancesIndexSignature;
        uint8 getDyIndexSignature;
        uint8 capabilityVersion;
        uint256 offpegFeeMultiplier;
    }
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetCurveNGPoolDataBatchRequest,
    "src/amms/abi/GetCurveNGPoolDataBatchRequest.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetCurveNGStableSwapRuntimeDataBatchRequest,
    "src/amms/abi/GetCurveNGStableSwapRuntimeDataBatchRequest.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetCurveNGTwoCryptoRuntimeDataBatchRequest,
    "src/amms/abi/GetCurveNGTwoCryptoRuntimeDataBatchRequest.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetCurveNGTriCryptoRuntimeDataBatchRequest,
    "src/amms/abi/GetCurveNGTriCryptoRuntimeDataBatchRequest.json"
);

sol! {
    #[derive(Debug)]
    struct StableSwapRuntimeData {
        address poolAddress;
        uint256[] balances;
        uint256[] adminBalances;
        uint256 amp;
        uint256 fee;
        uint256 adminFee;
        uint256[] rates;
        bool supportsStoredRates;
        bool supportsOffpegFeeMultiplier;
        uint256 offpegFeeMultiplier;
    }
}

sol! {
    #[derive(Debug)]
    struct TwoCryptoRuntimeData {
        address poolAddress;
        uint256[] balances;
        uint256 priceScale;
        uint256 d;
    }
}

sol! {
    #[derive(Debug)]
    struct TriCryptoRuntimeData {
        address poolAddress;
        uint256[] balances;
        uint256[] priceScale;
        uint256 d;
    }
}

// 从 JSON 生成的模块导入 PoolInput 类型
pub use GetCurveNGPoolDataBatchRequest::PoolInput;

/// Curve NG Factory
#[derive(Debug, Clone)]
pub struct CurveNGFactory {
    /// Factory 地址
    pub address: Address,
    /// 对应的池子类型
    pub pool_type: CurveNGPoolType,
    /// 创建区块 (用于过滤历史事件)
    pub creation_block: u64,
}

impl CurveNGFactory {
    fn pool_type_to_u8(pool_type: CurveNGPoolType) -> u8 {
        match pool_type {
            CurveNGPoolType::StableSwap => 0,
            CurveNGPoolType::TwoCrypto => 1,
            CurveNGPoolType::TriCrypto => 2,
        }
    }

    fn decode_index_signature(sig: u8) -> CurveIndexSignature {
        match sig {
            1 => CurveIndexSignature::Uint256,
            2 => CurveIndexSignature::Int128,
            _ => CurveIndexSignature::Unknown,
        }
    }

    fn default_rates_from_decimals(decimals: &[u8]) -> Vec<U256> {
        decimals
            .iter()
            .map(|d| {
                let precision = U256::from(10).pow(U256::from(18u8.saturating_sub(*d)));
                U256::from(10).pow(U256::from(18)) * precision
            })
            .collect()
    }

    fn apply_pool_data(pool: &mut CurveNGPool, data: &PoolData) {
        pool.n_coins = data.nCoins;
        pool.coins = data.coins.clone();
        pool.balances = data.balances.clone();
        pool.admin_balances = if pool.pool_type.is_stable() {
            vec![U256::ZERO; data.balances.len()]
        } else {
            Vec::new()
        };
        pool.decimals = data.decimals.clone();
        pool.rates = if !data.rates.is_empty() {
            data.rates.clone()
        } else {
            Self::default_rates_from_decimals(&data.decimals)
        };
        pool.asset_types = if !data.assetTypes.is_empty() {
            data.assetTypes.clone()
        } else {
            Vec::new()
        };

        pool.amp = if data.amp > U256::ZERO {
            Some(data.amp)
        } else {
            None
        };
        pool.fee = data.fee;
        pool.admin_fee = data.adminFee;
        pool.supports_stored_rates = data.supportsStoredRates;
        pool.supports_offpeg_fee_multiplier = data.supportsOffpegFeeMultiplier;
        pool.coins_index_signature = Self::decode_index_signature(data.coinsIndexSignature);
        pool.balances_index_signature = Self::decode_index_signature(data.balancesIndexSignature);
        pool.get_dy_index_signature = Self::decode_index_signature(data.getDyIndexSignature);
        pool.capability_version = data.capabilityVersion;

        if pool.pool_type.is_stable() && pool.supports_offpeg_fee_multiplier {
            pool.offpeg_fee_multiplier = data.offpegFeeMultiplier;
        }

        if pool.pool_type.is_crypto() {
            pool.d = if data.d > U256::ZERO {
                Some(data.d)
            } else {
                None
            };
            pool.gamma = if data.gamma > U256::ZERO {
                Some(data.gamma)
            } else {
                None
            };
            pool.mid_fee = if data.midFee > U256::ZERO {
                Some(data.midFee)
            } else {
                None
            };
            pool.out_fee = if data.outFee > U256::ZERO {
                Some(data.outFee)
            } else {
                None
            };
            pool.fee_gamma = if data.feeGamma > U256::ZERO {
                Some(data.feeGamma)
            } else {
                None
            };

            if !data.priceScale.is_empty() {
                pool.price_scale = Some(data.priceScale.clone());
            }
        }

        pool.update_spot_prices();
    }

    fn apply_stableswap_runtime_data(pool: &mut CurveNGPool, data: &StableSwapRuntimeData) {
        if pool.pool_type != CurveNGPoolType::StableSwap {
            return;
        }

        if !data.balances.is_empty() && data.balances.len() == pool.n_coins as usize {
            pool.balances = data.balances.clone();
        }
        if !data.adminBalances.is_empty() && data.adminBalances.len() == pool.n_coins as usize {
            pool.admin_balances = data.adminBalances.clone();
        } else if pool.admin_balances.len() != pool.n_coins as usize {
            pool.admin_balances = vec![U256::ZERO; pool.n_coins as usize];
        }

        pool.amp = if data.amp > U256::ZERO {
            Some(data.amp)
        } else {
            None
        };
        pool.fee = data.fee;
        pool.admin_fee = data.adminFee;
        pool.supports_stored_rates = data.supportsStoredRates;
        pool.supports_offpeg_fee_multiplier = data.supportsOffpegFeeMultiplier;

        if !data.rates.is_empty() && data.rates.len() == pool.n_coins as usize {
            pool.rates = data.rates.clone();
        }

        if data.supportsOffpegFeeMultiplier {
            pool.offpeg_fee_multiplier = data.offpegFeeMultiplier;
        }

        pool.update_spot_prices();
    }

    /// 创建新的 CurveNG Factory 实例
    pub fn new(address: Address, pool_type: CurveNGPoolType, creation_block: u64) -> Self {
        Self {
            address,
            pool_type,
            creation_block,
        }
    }

    /// 获取特定 Factory 下的所有池子地址
    pub async fn get_pool_addresses<N, P>(&self, provider: P) -> Result<Vec<Address>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone + 'static,
    {
        let factory = ICurveNGFactory::new(self.address, provider.clone());

        // 1. 获取池子总数
        let count = factory
            .pool_count()
            .call()
            .await
            .map_err(|e| AMMError::Msg(format!("Failed to get pool count: {}", e)))?;

        let count_u64 = count.to::<u64>();
        if count_u64 == 0 {
            return Ok(vec![]);
        }

        tracing::info!(
            "Found {} pools for Factory {:?} ({})",
            count_u64,
            self.pool_type,
            self.address
        );

        // 2. 分批次获取池子地址，严格控制 RPS
        let mut pools = Vec::new();
        // 降低批量大小以减少并发压力
        let batch_size = 5;
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
                let factory_ref = factory.clone();
                tasks.push(async move { factory_ref.pool_list(U256::from(i)).call().await });
            }

            while let Some(res) = tasks.next().await {
                match res {
                    Ok(addr) => pools.push(addr),
                    Err(e) => tracing::warn!("Failed to fetch pool address: {}", e),
                }
            }

            start_index += current_batch_size;

            // 批次间添加延迟，避免触发 RPS 限制
            if start_index < count_u64 {
                // 100ms 延迟
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        Ok(pools)
    }

    /// 获取特定 Factory 下的所有池子 (使用批量获取)
    pub async fn get_pools<N, P>(
        &self,
        block: BlockId,
        provider: P,
    ) -> Result<Vec<CurveNGPool>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone + 'static,
    {
        let pool_addresses = self.get_pool_addresses::<N, _>(provider.clone()).await?;

        if pool_addresses.is_empty() {
            return Ok(vec![]);
        }

        // 构建 AMM 列表
        let amms: Vec<AMM> = pool_addresses
            .into_iter()
            .map(|addr| AMM::CurveNGPool(CurveNGPool::new(addr, self.pool_type)))
            .collect();

        // 使用批量初始化
        let initialized = Self::init_batch(amms, block, provider).await?;

        // 提取 CurveNGPool
        Ok(initialized
            .into_iter()
            .filter_map(|amm| {
                if let AMM::CurveNGPool(pool) = amm {
                    Some(pool)
                } else {
                    None
                }
            })
            .collect())
    }
    /// 批量初始化池子 (使用批量获取合约)
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

        // 减小批次大小以避免 Gas Limit 或 Contract Size 限制
        // 调试模式：设置为 1 以验证逻辑正确性
        let step = 5;
        // 使用 pool_chunks 保存 (address, pool_type) 元组，而不是直接引用 AMM
        // 这样可以避免对 amms 的引用，允许后续将其 move 给 amms_map
        let pool_chunks = amms
            .iter()
            .map(|amm| {
                if let AMM::CurveNGPool(pool) = amm {
                    (pool.address, Self::pool_type_to_u8(pool.pool_type))
                } else {
                    (Address::ZERO, 0) // Should not happen given logic, but needed for type safety
                }
            })
            .chunks(step)
            .into_iter()
            .map(|chunk| chunk.collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let mut amms_map = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        // 记录批量初始化失败的池子地址，用于后续单独重试
        let mut failed_pools: Vec<Address> = Vec::new();

        for (i, chunk) in pool_chunks.into_iter().enumerate() {
            let mut inputs: Vec<PoolInput> = Vec::new();
            let mut chunk_addresses: Vec<Address> = Vec::new();

            for (addr, pool_type_u8) in &chunk {
                if *addr != Address::ZERO {
                    inputs.push(PoolInput {
                        pool: *addr,
                        poolType: *pool_type_u8,
                    });
                    chunk_addresses.push(*addr);
                }
            }

            if inputs.is_empty() {
                continue;
            }

            let deployer = GetCurveNGPoolDataBatchRequest::deploy_builder(provider.clone(), inputs);

            // 执行批量调用
            match deployer.call_raw().block(block).await {
                Ok(res) => {
                    match <Vec<PoolData> as SolValue>::abi_decode(&res) {
                        Ok(pool_data_list) => {
                            for data in pool_data_list {
                                let pool_addr = data.poolAddress;

                                if let Some(AMM::CurveNGPool(pool)) = amms_map.get_mut(&pool_addr) {
                                    Self::apply_pool_data(pool, &data);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "curve_ng::init_batch",
                                chunk_idx = i,
                                pools = ?chunk_addresses,
                                error = %e,
                                "Batch decode failed, will retry individually"
                            );
                            // 记录需要重试的池子
                            failed_pools.extend(chunk_addresses.clone());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "curve_ng::init_batch",
                        chunk_idx = i,
                        pools = ?chunk_addresses,
                        error = %e,
                        "Batch RPC call failed, will retry individually"
                    );
                    // 记录需要重试的池子
                    failed_pools.extend(chunk_addresses.clone());
                }
            };

            // 批次间延迟 间隔避免 RPS 限制
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }

        // 对失败的池子进行单独重试
        if !failed_pools.is_empty() {
            tracing::info!(
                target: "curve_ng::init_batch",
                count = failed_pools.len(),
                "Retrying failed pools individually"
            );

            for pool_addr in failed_pools {
                if let Some(AMM::CurveNGPool(pool)) = amms_map.get_mut(&pool_addr) {
                    let pool_type_u8 = Self::pool_type_to_u8(pool.pool_type);

                    let single_input = vec![PoolInput {
                        pool: pool_addr,
                        poolType: pool_type_u8,
                    }];

                    let deployer = GetCurveNGPoolDataBatchRequest::deploy_builder(
                        provider.clone(),
                        single_input,
                    );

                    match deployer.call_raw().block(block).await {
                        Ok(res) => {
                            if let Ok(pool_data_list) =
                                <Vec<PoolData> as SolValue>::abi_decode(&res)
                            {
                                if let Some(data) = pool_data_list.first() {
                                    Self::apply_pool_data(pool, data);

                                    tracing::info!(
                                        target: "curve_ng::init_batch",
                                        pool = ?pool_addr,
                                        "Individual retry succeeded"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "curve_ng::init_batch",
                                pool = ?pool_addr,
                                error = %e,
                                "Individual retry also failed, pool will be filtered out"
                            );
                        }
                    }

                    // 单独重试间隔
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }

        // ---------------------------------------------------------
        // TwoCrypto 变体识别: 标准 v2.1.0 vs periphery v2.1.0d
        // 判定规则（与单池 detect_twocrypto_variant 保持一致）：
        // - version() == "v2.1.0d" => PeripheryV210d
        // - VIEW() != 0x0         => PeripheryV210d
        // 详细背景见 `math/twocrypto_v210d.rs` 顶部文档。
        // ---------------------------------------------------------
        let mut twocrypto_pools = Vec::new();
        for amm in amms_map.values() {
            if let AMM::CurveNGPool(pool) = amm {
                if pool.pool_type == CurveNGPoolType::TwoCrypto {
                    twocrypto_pools.push(pool.address);
                }
            }
        }

        if !twocrypto_pools.is_empty() {
            let mut tasks = FuturesUnordered::new();
            for pool_addr in twocrypto_pools {
                let provider = provider.clone();
                tasks.push(async move {
                    let c = crate::amms::curve_ng::ICurveTwoCryptoMeta::new(pool_addr, provider);

                    let version = c.version().block(block).call().await.ok();
                    let view = c.VIEW().block(block).call().await.ok();
                    let math = c.MATH().block(block).call().await.ok();
                    let precisions = c.precisions().block(block).call().await.ok();
                    let future_a_gamma_time =
                        c.future_A_gamma_time().block(block).call().await.ok();
                    let last_timestamp = c.last_timestamp().block(block).call().await.ok();

                    (
                        pool_addr,
                        version,
                        view,
                        math,
                        precisions,
                        future_a_gamma_time,
                        last_timestamp,
                    )
                });
            }

            while let Some((pool_addr, version, view, math, precisions, future_t, last_t)) =
                tasks.next().await
            {
                if let Some(AMM::CurveNGPool(pool)) = amms_map.get_mut(&pool_addr) {
                    if let Some(v) = version {
                        pool.twocrypto_version = Some(v.clone());
                        if v.trim() == "v2.1.0d" {
                            pool.twocrypto_variant = CurveNGTwoCryptoVariant::PeripheryV210d;
                        }
                    }
                    if let Some(view_addr) = view {
                        if view_addr != Address::ZERO {
                            pool.twocrypto_view = Some(view_addr);
                            pool.twocrypto_variant = CurveNGTwoCryptoVariant::PeripheryV210d;
                        }
                    }
                    if let Some(math_addr) = math {
                        if math_addr != Address::ZERO {
                            pool.twocrypto_math = Some(math_addr);
                        }
                    }
                    if let Some(p) = precisions {
                        pool.twocrypto_precisions = Some(vec![p[0], p[1]]);
                    }
                    pool.twocrypto_future_a_gamma_time = future_t;
                    pool.twocrypto_last_timestamp = last_t;
                }
            }
        }

        let mut stableswap_pools = amms_map
            .values()
            .filter_map(|amm| match amm {
                AMM::CurveNGPool(pool)
                    if pool.pool_type == CurveNGPoolType::StableSwap && pool.n_coins > 0 =>
                {
                    Some(pool.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        if !stableswap_pools.is_empty() {
            match Self::refresh_runtime_data_batch(&mut stableswap_pools, block, provider.clone())
                .await
            {
                Ok(()) => {
                    for pool in stableswap_pools {
                        if let Some(AMM::CurveNGPool(existing)) = amms_map.get_mut(&pool.address) {
                            *existing = pool;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "curve_ng::init_batch",
                        error = %e,
                        "StableSwap runtime refresh failed after initialization; admin_balances will start from zero"
                    );
                }
            }
        }

        // 过滤掉未正确初始化的池子 (rates 为空)
        let mut filtered_count = 0;
        let initialized_amms: Vec<AMM> = amms_map
            .into_values()
            .filter(|amm| {
                if let AMM::CurveNGPool(pool) = amm {
                    if pool.rates.is_empty() {
                        tracing::warn!(
                            target: "curve_ng::init_batch",
                            pool = ?pool.address,
                            "Filtering out uninitialized pool: rates is empty"
                        );
                        filtered_count += 1;
                        return false;
                    }
                    if pool.rates.iter().any(|r| r.is_zero()) {
                        tracing::warn!(
                            target: "curve_ng::init_batch",
                            pool = ?pool.address,
                            rates = ?pool.rates,
                            "Filtering out pool: rates contains zero"
                        );
                        filtered_count += 1;
                        return false;
                    }
                }
                true
            })
            .collect();

        if filtered_count > 0 {
            tracing::warn!(
                target: "curve_ng::init_batch",
                filtered = filtered_count,
                remaining = initialized_amms.len(),
                "Some CurveNG pools were filtered due to initialization failure"
            );
        }

        let valid = initialized_amms.len();
        let invalid = total.saturating_sub(valid);
        tracing::info!(
            target: "curve_ng::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(initialized_amms)
    }

    /// 批量同步池子 (复用 init_batch)
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

    pub async fn refresh_runtime_data_batch<N, P>(
        pools: &mut [CurveNGPool],
        block: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        if pools.is_empty() {
            return Ok(());
        }

        let step = 10;
        let index_by_address = pools
            .iter()
            .enumerate()
            .map(|(idx, pool)| (pool.address, idx))
            .collect::<HashMap<_, _>>();

        let pool_chunks = pools
            .iter()
            .filter(|pool| pool.pool_type == CurveNGPoolType::StableSwap)
            .map(|pool| pool.address)
            .chunks(step)
            .into_iter()
            .map(|chunk| chunk.collect::<Vec<_>>())
            .collect::<Vec<_>>();

        for chunk in pool_chunks {
            let deployer = GetCurveNGStableSwapRuntimeDataBatchRequest::deploy_builder(
                provider.clone(),
                chunk.clone(),
            );

            match deployer.call_raw().block(block).await {
                Ok(res) => {
                    let pool_data_list =
                        <Vec<StableSwapRuntimeData> as SolValue>::abi_decode(&res)?;
                    for data in pool_data_list {
                        if let Some(&idx) = index_by_address.get(&data.poolAddress) {
                            Self::apply_stableswap_runtime_data(&mut pools[idx], &data);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "curve_ng::refresh_runtime_data_batch",
                        pools = ?chunk,
                        error = %e,
                        "Batch runtime refresh failed, retrying pools individually"
                    );

                    for addr in chunk {
                        let deployer = GetCurveNGStableSwapRuntimeDataBatchRequest::deploy_builder(
                            provider.clone(),
                            vec![addr],
                        );
                        let res = deployer.call_raw().block(block).await?;
                        let pool_data_list =
                            <Vec<StableSwapRuntimeData> as SolValue>::abi_decode(&res)?;
                        if let Some(data) = pool_data_list.first() {
                            if let Some(&idx) = index_by_address.get(&addr) {
                                Self::apply_stableswap_runtime_data(&mut pools[idx], data);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_index_signature() {
        assert_eq!(
            CurveNGFactory::decode_index_signature(0),
            CurveIndexSignature::Unknown
        );
        assert_eq!(
            CurveNGFactory::decode_index_signature(1),
            CurveIndexSignature::Uint256
        );
        assert_eq!(
            CurveNGFactory::decode_index_signature(2),
            CurveIndexSignature::Int128
        );
        assert_eq!(
            CurveNGFactory::decode_index_signature(255),
            CurveIndexSignature::Unknown
        );
    }
}
