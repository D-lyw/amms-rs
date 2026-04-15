//! Curve NG Factory 发现机制
//!
//! 负责发现和初始化 Curve NG 协议的池子。

use super::{
    types::{CurveNGPool, CurveNGPoolType, CurveNGTwoCryptoVariant},
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
    }
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetCurveNGPoolDataBatchRequest,
    "src/amms/abi/GetCurveNGPoolDataBatchRequest.json"
);

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
        let step = 20;
        // 使用 pool_chunks 保存 (address, pool_type) 元组，而不是直接引用 AMM
        // 这样可以避免对 amms 的引用，允许后续将其 move 给 amms_map
        let pool_chunks = amms
            .iter()
            .map(|amm| {
                if let AMM::CurveNGPool(pool) = amm {
                    let pool_type_u8: u8 = match pool.pool_type {
                        CurveNGPoolType::StableSwap => 0,
                        CurveNGPoolType::TwoCrypto => 1,
                        CurveNGPoolType::TriCrypto => 2,
                    };
                    (pool.address, pool_type_u8)
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
                                    // 更新池子数据
                                    pool.n_coins = data.nCoins;
                                    pool.coins = data.coins.clone();
                                    pool.balances = data.balances.clone();
                                    pool.decimals = data.decimals.clone();

                                    // 设置 rates (从合约响应获取, 如果为空则默认 1e18)
                                    pool.rates = if !data.rates.is_empty() {
                                        data.rates.clone()
                                    } else {
                                        data.decimals
                                            .iter()
                                            .map(|d| {
                                                let precision = U256::from(10).pow(
                                                    U256::from(18).saturating_sub(U256::from(*d)),
                                                );
                                                U256::from(10).pow(U256::from(18)) * precision
                                            })
                                            .collect()
                                    };

                                    pool.amp = if data.amp > U256::ZERO {
                                        Some(data.amp)
                                    } else {
                                        None
                                    };
                                    pool.fee = data.fee;
                                    pool.admin_fee = data.adminFee;

                                    // CryptoSwap 特定参数
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

                                    // 批量初始化后更新价格缓存
                                    pool.update_spot_prices();
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

        // ---------------------------------------------------------
        // 补充步骤: 为 StableSwap NG 池获取 offpeg_fee_multiplier
        // ---------------------------------------------------------
        let mut stable_ng_pools = Vec::new();
        for amm in amms_map.values() {
            if let AMM::CurveNGPool(pool) = amm {
                if pool.pool_type.is_stable() {
                    stable_ng_pools.push(pool.address);
                }
            }
        }

        if !stable_ng_pools.is_empty() {
            tracing::info!(
                target: "curve_ng::init_batch",
                count = stable_ng_pools.len(),
                "Fetching offpeg_fee_multiplier for StableSwap NG pools"
            );

            let mut tasks = FuturesUnordered::new();
            for pool_addr in stable_ng_pools {
                let provider = provider.clone();
                tasks.push(async move {
                    let contract = crate::amms::curve_ng::ICurveNGPool::new(pool_addr, provider);
                    let multiplier = contract.offpeg_fee_multiplier().call().block(block).await;
                    (pool_addr, multiplier)
                });
            }

            while let Some((pool_addr, res)) = tasks.next().await {
                if let Ok(multiplier_val) = res {
                    if let Some(AMM::CurveNGPool(pool)) = amms_map.get_mut(&pool_addr) {
                        pool.offpeg_fee_multiplier = multiplier_val;
                        tracing::debug!(
                            target: "curve_ng::init_batch",
                            pool = ?pool_addr,
                            multiplier = ?multiplier_val,
                            "Updated offpeg_fee_multiplier"
                        );
                    }
                } else {
                    // 失败通常意味着该池子没有此方法 (可能是旧版本或伪装的 NG)
                    // 默认为 0 即可
                    tracing::debug!(
                        target: "curve_ng::init_batch",
                        pool = ?pool_addr,
                        "Failed to fetch offpeg_fee_multiplier (defaulting to 0)"
                    );
                }
            }
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
                    let pool_type_u8: u8 = match pool.pool_type {
                        CurveNGPoolType::StableSwap => 0,
                        CurveNGPoolType::TwoCrypto => 1,
                        CurveNGPoolType::TriCrypto => 2,
                    };

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
                                    pool.n_coins = data.nCoins;
                                    pool.coins = data.coins.clone();
                                    pool.balances = data.balances.clone();
                                    pool.decimals = data.decimals.clone();
                                    pool.rates = if !data.rates.is_empty() {
                                        data.rates.clone()
                                    } else {
                                        vec![
                                            U256::from(10).pow(U256::from(18));
                                            data.nCoins as usize
                                        ]
                                    };
                                    pool.amp = if data.amp > U256::ZERO {
                                        Some(data.amp)
                                    } else {
                                        None
                                    };
                                    pool.fee = data.fee;
                                    pool.admin_fee = data.adminFee;

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
}
