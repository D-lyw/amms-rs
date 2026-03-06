//! Curve Legacy Factory
//!
//! 通过 Curve AddressProvider 和 Registry 发现 Legacy 池子。
//! 使用并发 init() 调用初始化池子数据 (不使用 Solidity 批量合约，原因见 init_batch 文档注释)。

use super::types::{CurveLegacyPool, CurveLegacyPoolType};
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
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};

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

    /// 批量初始化池子
    ///
    /// # 设计说明：为什么不使用 Solidity 批量合约
    ///
    /// 与 UniswapV2/V3 等标准化协议不同，Curve Legacy 池存在大量非标行为，
    /// 使得 Solidity 批量合约不可靠：
    ///
    /// 1. **调用隔离问题**：Solidity 构造函数中所有外部调用共享 gas 上下文，
    ///    部分老 Vyper 池的 `coins()` 使用 `assert` 做边界检查，失败时消耗全部
    ///    forwarded gas，导致 try/catch 无法捕获 out-of-gas 错误，整个构造函数 revert。
    ///
    /// 2. **Subtype 检测缺失**：Solidity 合约无法检测 Meta/Lending/Plain 子类型，
    ///    无法获取 `base_pool`, `virtual_price`, `lp_token`, `underlying_coins` 等
    ///    Metapool/Lending 池必需的字段，导致这些池无法正确模拟 swap。
    ///
    /// 3. **int128 接口 fallback 不完整**：Solidity 合约仅对 StableSwap 尝试 int128
    ///    fallback，CryptoSwap 池缺失此路径。
    ///
    /// 因此，对于 CurveLegacy（通常只有 ~12 个池），直接并发调用 Rust `init()` 方法，
    /// 每个池使用独立的 RPC `eth_call`（各自有完整的 30M gas 额度），
    /// 既能处理所有非标边界情况，性能开销也完全可接受。
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
            "Initializing {} Curve Legacy pools via individual init...",
            total
        );

        // 并发初始化所有池子，每个池使用独立的 RPC eth_call
        // init() 内部会完整处理：
        //   - uint256/int128 双 ABI 兼容
        //   - Meta/Lending/Plain subtype 检测
        //   - A_precision 版本检测
        //   - stored_rates 获取
        //   - CryptoSwap 参数 (D, gamma, price_scale 等)

        let mut chunks = Vec::new();
        let chunk_size = 10;
        let mut current_chunk = Vec::with_capacity(chunk_size);
        for amm in amms {
            current_chunk.push(amm);
            if current_chunk.len() == chunk_size {
                chunks.push(std::mem::take(&mut current_chunk));
            }
        }
        if !current_chunk.is_empty() {
            chunks.push(current_chunk);
        }

        let mut results: Vec<AMM> = Vec::with_capacity(total);
        let mut success_count = 0u32;
        let mut fail_count = 0u32;

        let num_chunks = chunks.len();

        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut tasks = FuturesUnordered::new();
            for amm in chunk {
                let provider = provider.clone();
                tasks.push(async move {
                    let addr = amm.address();
                    match amm.init(block, provider).await {
                        Ok(initialized) => {
                            tracing::debug!(pool = ?addr, "Successfully initialized Curve Legacy pool");
                            Some(initialized)
                        }
                        Err(e) => {
                            tracing::warn!(pool = ?addr, error = %e, "Failed to initialize Curve Legacy pool, skipping");
                            None
                        }
                    }
                });
            }

            while let Some(result) = tasks.next().await {
                if let Some(amm) = result {
                    success_count += 1;
                    results.push(amm);
                } else {
                    fail_count += 1;
                }
            }

            // 批次间延迟，避免触发 RPC 限制
            if i < num_chunks - 1 {
                tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
            }
        }

        // =========================================================================
        // 验证必要参数：CryptoSwap 池必须有 d 和 gamma，StableSwap 池必须有 amp
        // 无效池子将被移除，避免后续 simulate_swap 时 divide by zero
        // =========================================================================
        let pre_filter = results.len();
        results.retain(|amm| {
            if let AMM::CurveLegacyPool(pool) = amm {
                let is_valid = match pool.pool_type {
                    CurveLegacyPoolType::CryptoSwap => {
                        // CryptoSwap 必须有 d, gamma, mid_fee, out_fee, fee_gamma
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
                        // StableSwap 必须有 amp
                        let valid = pool.amp.is_some();
                        if !valid {
                            tracing::warn!(
                                pool = ?pool.address,
                                "Removing StableSwap pool: missing amp parameter"
                            );
                        }
                        valid
                    }
                };
                is_valid
            } else {
                true
            }
        });

        let filtered = pre_filter - results.len();

        tracing::info!(
            "Curve Legacy init complete: {}/{} succeeded, {} failed, {} filtered (invalid params)",
            success_count,
            total,
            fail_count,
            filtered
        );

        Ok(results)
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
