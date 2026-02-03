//! Curve Legacy Factory
//!
//! 通过 Curve AddressProvider 和 Registry 发现 Legacy 池子。
//! 使用批量获取合约高效初始化池子数据。

use super::types::{CurveLegacyPool, CurveLegacyPoolType};
use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::error::AMMError;
use crate::amms::factory::{AutomatedMarketMakerFactory, DiscoverySync};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::eth::Log,
    sol,
    sol_types::SolValue,
};
use futures::{stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

// 从 JSON 生成的模块导入 PoolInput 类型
use GetCurveLegacyPoolDataBatchRequest::PoolInput;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct CurveLegacyFactory {
    pub address: Address,
    pub pool_type: CurveLegacyPoolType,
    pub creation_block: u64,
}

impl CurveLegacyFactory {
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

        // 减小批次大小以避免 Contract Size 限制和 RPC 执行超时
        // Curve Legacy 池子初始化计算量大，特别是 CryptoSwap，将批次大小从 10 降低到 2
        // CryptoSwap 的初始化非常消耗 Gas，即使是 4 个池子也可能导致 Revert
        let step = 2;
        // 使用 pool_chunks 保存 (address, pool_type) 元组
        let pool_chunks = amms
            .iter()
            .map(|amm| {
                if let AMM::CurveLegacyPool(pool) = amm {
                    let pool_type_u8: u8 = match pool.pool_type {
                        CurveLegacyPoolType::StableSwap => 0,
                        CurveLegacyPoolType::CryptoSwap => 1,
                    };
                    (pool.address, pool_type_u8)
                } else {
                    (Address::ZERO, 0)
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

        for (_i, chunk) in pool_chunks.into_iter().enumerate() {
            let mut inputs: Vec<PoolInput> = Vec::new();

            for (addr, pool_type_u8) in &chunk {
                if *addr != Address::ZERO {
                    inputs.push(PoolInput {
                        pool: *addr,
                        poolType: *pool_type_u8,
                    });
                }
            }

            if inputs.is_empty() {
                continue;
            }

            let deployer =
                GetCurveLegacyPoolDataBatchRequest::deploy_builder(provider.clone(), inputs);

            // 执行批量调用，设置高 gas limit (5M，RPC节点最大限制) 以避免默认1M限制
            let _batch_success = match deployer.gas(5_000_000).call_raw().block(block).await {
                Ok(res) => {
                    match <Vec<PoolData> as SolValue>::abi_decode(&res) {
                        Ok(pool_data_list) => {
                            for data in pool_data_list {
                                let pool_addr = data.poolAddress;

                                if let Some(AMM::CurveLegacyPool(pool)) =
                                    amms_map.get_mut(&pool_addr)
                                {
                                    pool.n_coins = data.nCoins;
                                    pool.coins = data.coins.clone();
                                    pool.balances = data.balances.clone();
                                    pool.decimals = data.decimals.clone();

                                    pool.amp = if data.amp > U256::ZERO {
                                        Some(data.amp)
                                    } else {
                                        None
                                    };
                                    pool.fee = data.fee;
                                    pool.admin_fee = data.adminFee;

                                    // CryptoSwap 特定参数
                                    if pool.pool_type == CurveLegacyPoolType::CryptoSwap {
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
                                        pool.allowed_extra_profit =
                                            if data.allowedExtraProfit > U256::ZERO {
                                                Some(data.allowedExtraProfit)
                                            } else {
                                                None
                                            };
                                        pool.adjustment_step = if data.adjustmentStep > U256::ZERO {
                                            Some(data.adjustmentStep)
                                        } else {
                                            None
                                        };
                                        pool.ma_half_time = if data.maHalfTime > U256::ZERO {
                                            Some(data.maHalfTime)
                                        } else {
                                            None
                                        };

                                        if !data.priceScale.is_empty() {
                                            pool.price_scale = Some(data.priceScale.clone());
                                        }
                                    }

                                    // Populate rates if available (Solidity returns empty if no stored_rates)
                                    if !data.rates.is_empty() {
                                        pool.rates = data.rates.clone();
                                    }

                                    // 批量初始化后更新价格缓存
                                    // 使用catch_unwind防止单个池子的计算错误导致整个程序崩溃
                                    let pool_addr_for_log = pool.address;
                                    if let Err(_e) = std::panic::catch_unwind(
                                        std::panic::AssertUnwindSafe(|| {
                                            pool.update_spot_prices();
                                        }),
                                    ) {
                                        tracing::warn!(
                                            "Pool {:?} update_spot_prices panicked, skipping",
                                            pool_addr_for_log
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        "Pool {:?} not found in amms_map or not CurveLegacyPool",
                                        pool_addr
                                    );
                                }
                            }
                            true
                        }
                        Err(e) => {
                            tracing::warn!("Failed to decode legacy batch response: {}", e);
                            false
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to execute legacy batch request: {}, falling back to individual init", e);

                    // 降级: 逐个初始化该批次的池子
                    for (addr, pool_type_u8) in &chunk {
                        if *addr == Address::ZERO {
                            continue;
                        }

                        let pool_type = if *pool_type_u8 == 0 {
                            CurveLegacyPoolType::StableSwap
                        } else {
                            CurveLegacyPoolType::CryptoSwap
                        };

                        let pool = CurveLegacyPool::new(*addr, pool_type);
                        match pool.init(block, provider.clone()).await {
                            Ok(initialized_pool) => {
                                amms_map.insert(*addr, AMM::CurveLegacyPool(initialized_pool));
                                tracing::debug!("Individually initialized pool {:?}", addr);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to individually init pool {:?}: {}",
                                    addr,
                                    e
                                );
                            }
                        }

                        // 逐个请求间添加延迟
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }

                    false
                }
            };

            // 批次间延迟，避免触发 429
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }

        // 版本检测后处理：对 StableSwap 池检测是否是新版 (有 A_precise 方法)
        // 新版池子 uses_a_precision = true，旧版池子 uses_a_precision = false
        let stableswap_pools: Vec<Address> = amms_map
            .iter()
            .filter_map(|(addr, amm)| {
                if let AMM::CurveLegacyPool(pool) = amm {
                    if pool.pool_type == CurveLegacyPoolType::StableSwap {
                        return Some(*addr);
                    }
                }
                None
            })
            .collect();

        if !stableswap_pools.is_empty() {
            tracing::info!(
                "Detecting version for {} StableSwap pools...",
                stableswap_pools.len()
            );

            let mut version_tasks = FuturesUnordered::new();
            for pool_addr in stableswap_pools {
                let provider_clone = provider.clone();
                version_tasks.push(async move {
                    let detect = IVersionDetect::new(pool_addr, provider_clone);

                    // 尝试获取 A 和 A_precise
                    let a_result = detect.A().block(block).call().await;
                    let a_precise_result = detect.A_precise().block(block).call().await;

                    let uses_a_precision = match (a_result, a_precise_result) {
                        (Ok(a), Ok(a_precise)) => {
                            // 新版池子: A_precise() = A() * 100
                            a_precise == a * U256::from(100)
                        }
                        _ => false, // A_precise 调用失败表示旧版池子
                    };

                    (pool_addr, uses_a_precision)
                });
            }

            while let Some((pool_addr, uses_a_precision)) = version_tasks.next().await {
                if let Some(AMM::CurveLegacyPool(pool)) = amms_map.get_mut(&pool_addr) {
                    pool.uses_a_precision = uses_a_precision;
                    if uses_a_precision {
                        tracing::debug!(
                            "Pool {:?} detected as new version (uses A_PRECISION=100)",
                            pool_addr
                        );
                    }
                }
            }
        }

        // =========================================================================
        // 验证必要参数：CryptoSwap 池必须有 d 和 gamma，StableSwap 池必须有 amp
        // 无效池子将被移除，避免后续 simulate_swap 时 divide by zero
        // =========================================================================
        let invalid_pools: Vec<Address> = amms_map
            .iter()
            .filter_map(|(addr, amm)| {
                if let AMM::CurveLegacyPool(pool) = amm {
                    let is_invalid = match pool.pool_type {
                        CurveLegacyPoolType::CryptoSwap => {
                            // CryptoSwap 必须有 d, gamma, mid_fee, out_fee, fee_gamma
                            pool.d.is_none()
                                || pool.gamma.is_none()
                                || pool.mid_fee.is_none()
                                || pool.out_fee.is_none()
                                || pool.fee_gamma.is_none()
                        }
                        CurveLegacyPoolType::StableSwap => {
                            // StableSwap 必须有 amp
                            pool.amp.is_none()
                        }
                    };

                    if is_invalid {
                        return Some(*addr);
                    }
                }
                None
            })
            .collect();

        if !invalid_pools.is_empty() {
            tracing::warn!(
                "Removing {} Curve Legacy pools with missing required parameters",
                invalid_pools.len()
            );

            for addr in &invalid_pools {
                if let Some(AMM::CurveLegacyPool(pool)) = amms_map.get(addr) {
                    tracing::warn!(
                        pool = ?addr,
                        pool_type = ?pool.pool_type,
                        has_amp = pool.amp.is_some(),
                        has_d = pool.d.is_some(),
                        has_gamma = pool.gamma.is_some(),
                        has_mid_fee = pool.mid_fee.is_some(),
                        has_out_fee = pool.out_fee.is_some(),
                        has_fee_gamma = pool.fee_gamma.is_some(),
                        "Skipping pool due to missing required parameters"
                    );
                }
                amms_map.remove(addr);
            }
        }

        Ok(amms_map.into_values().collect())
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
