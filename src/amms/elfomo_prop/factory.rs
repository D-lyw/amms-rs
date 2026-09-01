//! ElfomoFi propAMM Factory
//!
//! ElfomoFi 不 emit 池子创建事件；pair→(pool, vault) 映射由部署配置
//! （`ElfomoPairConfig`）在构建时传入，每 pair 独立 pool（参照 caliber_prop）。
//! `discover()` 为每个配置 pair 返回独立池子骨架，初始化由链上读取
//! （`getSupportedPairs()` + `getOrderbook()` + vault `balanceOf` + slot1）完成。

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256},
    providers::Provider,
    rpc::types::Log,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
};

use super::{
    ElfomoFiPropPool, ELFOMO_FACTORY_ADDRESS, ELFOMO_POOL_ADDRESS, ELFOMO_ROUTER_ADDRESS,
    ELFOMO_USDT0_ADDRESS, ELFOMO_VAULT_ADDRESS, ELFOMO_XETH_ADDRESS,
};

// ============================================================================
// ElfomoFiPropFactory
// ============================================================================

/// 单个 ElfomoFi pair 的部署配置（pair 定义 + 独立 pool/金库地址）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElfomoPairConfig {
    /// pair 的 token 0（from→to 输入侧）
    pub token_x: Address,
    /// pair 的 token 1（from→to 输出侧）
    pub token_y: Address,
    /// 该 pair 的 pool 合约（orderbook/种子所在）
    pub pool_address: Address,
    /// 该 pair 的金库合约（持币背书）
    pub vault_address: Address,
}

impl ElfomoPairConfig {
    /// XLayer 默认 pair（xETH/USDT0）便捷构造
    pub fn new_default() -> Self {
        Self {
            token_x: ELFOMO_XETH_ADDRESS,
            token_y: ELFOMO_USDT0_ADDRESS,
            pool_address: ELFOMO_POOL_ADDRESS,
            vault_address: ELFOMO_VAULT_ADDRESS,
        }
    }
}

/// ElfomoFi propAMM 池子工厂（每 pair 独立 pool，参照 caliber_prop）
///
/// ## 发现机制
///
/// ElfomoFi 不 emit 池子创建事件；pair→(pool, vault) 映射由部署配置
/// （`pairs`）在构建时传入，`discover()` 为每个配置 pair 返回独立池子骨架，
/// 初始化由链上读取（`getOrderbook` + `balanceOf` + slot1）完成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElfomoFiPropFactory {
    /// Factory 代理地址
    pub factory_address: Address,
    /// Router 地址（swap 事件来源）
    pub router_address: Address,
    /// 链 ID
    pub chain_id: u64,
    /// 工厂合约的创建区块号（StateSpace 扫描起点）
    pub creation_block: u64,
    /// pair 部署配置；空 = 默认单 pair（xETH/USDT0 常量）
    #[serde(default)]
    pub pairs: Vec<ElfomoPairConfig>,
}

impl ElfomoFiPropFactory {
    /// 创建 ElfomoFi 工厂实例（pair/pool/vault 全部由部署配置传入）
    pub fn new(
        pairs: Vec<ElfomoPairConfig>,
        factory_address: Address,
        router_address: Address,
        chain_id: u64,
        creation_block: u64,
    ) -> Self {
        Self {
            factory_address,
            router_address,
            chain_id,
            creation_block,
            pairs,
        }
    }

    /// XLayer 默认部署便捷构造（地址见模块常量）
    pub fn new_default(chain_id: u64, creation_block: u64) -> Self {
        Self::new(vec![ElfomoPairConfig::new_default()], ELFOMO_FACTORY_ADDRESS, ELFOMO_ROUTER_ADDRESS, chain_id, creation_block)
    }

    /// 为所有配置 pair 构建池子骨架（资产/档位在 init 时填充）。
    /// `pairs` 为空时回退默认单 pair（xETH/USDT0）。
    pub fn skeletons(&self, created_block: u64) -> Vec<ElfomoFiPropPool> {
        let cfgs: Vec<ElfomoPairConfig> = if self.pairs.is_empty() {
            vec![ElfomoPairConfig::new_default()]
        } else {
            self.pairs.clone()
        };
        cfgs.into_iter()
            .map(|cfg| {
                ElfomoFiPropPool::skeleton(
                    cfg.pool_address,
                    cfg.token_x,
                    cfg.token_y,
                    self.factory_address,
                    self.router_address,
                    cfg.vault_address,
                    self.chain_id,
                    created_block,
                )
            })
            .collect()
    }
}

// ============================================================================
// Discovery 实现
// ============================================================================

impl DiscoverySync for ElfomoFiPropFactory {
    fn discover<N, P>(
        &self,
        _to_block: BlockId,
        _provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let created_block = self.creation_block;
        let factory = self.clone();
        async move {
            Ok(factory
                .skeletons(created_block)
                .into_iter()
                .map(AMM::ElfomoFiPropPool)
                .collect())
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
        async move { init_batch::<N, P>(amms, to_block, provider).await }
    }
}

impl AutomatedMarketMakerFactory for ElfomoFiPropFactory {
    type PoolVariant = ElfomoFiPropPool;

    fn address(&self) -> Address {
        self.factory_address
    }

    fn create_pool(&self, _log: Log) -> Result<AMM, AMMError> {
        // ElfomoFi 不 emit 池子创建事件，此方法不可用
        Err(AMMError::Msg(
            "elfomofi: pool creation is event-less, use discover()".to_string(),
        ))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        // ElfomoFi 无池子创建事件
        B256::ZERO
    }

    fn pool_events(&self) -> Vec<B256> {
        super::ElfomoFiPropPool::default().sync_events()
    }

    fn pool_variant(&self) -> Self::PoolVariant {
        Default::default()
    }
}

// ============================================================================
// init_batch: 批量初始化池子
// ============================================================================

/// 批量初始化 ElfomoFi propAMM 池子
///
/// 每个池子通过一次链上读取完成：`getSupportedPairs()`（资产）+ `getOrderbook()`
/// （档位）+ vault `balanceOf`（金库余额）。
pub async fn init_batch<N, P>(
    amms: Vec<AMM>,
    block_number: BlockId,
    provider: P,
) -> Result<Vec<AMM>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let mut initialized = Vec::with_capacity(amms.len());
    for amm in amms {
        let address = amm.address();
        match amm.init::<N, P>(block_number, provider.clone()).await {
            Ok(pool) => initialized.push(pool),
            Err(e) => {
                // 初始化失败会静默丢失池子（上层拓扑仍引用它），必须显式告警
                warn!(
                    target: "amms::elfomo_prop",
                    pool = %address,
                    error = %e,
                    "elfomofi: failed to init pool"
                );
            }
        }
    }
    Ok(initialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::elfomo_prop::ELFOMO_CHAIN_ID;

    #[test]
    fn test_factory_address() {
        let factory = ElfomoFiPropFactory::new_default(ELFOMO_CHAIN_ID, 1);
        assert_eq!(factory.factory_address, ELFOMO_FACTORY_ADDRESS);
        assert_eq!(factory.router_address, ELFOMO_ROUTER_ADDRESS);
        assert_eq!(factory.pairs.len(), 1);
        assert_eq!(factory.pairs[0].pool_address, ELFOMO_POOL_ADDRESS);
        assert_eq!(factory.pairs[0].vault_address, ELFOMO_VAULT_ADDRESS);
        assert_eq!(factory.pairs[0].token_x, crate::amms::elfomo_prop::ELFOMO_XETH_ADDRESS);
        assert_eq!(factory.pairs[0].token_y, crate::amms::elfomo_prop::ELFOMO_USDT0_ADDRESS);
        assert!(factory.pool_creation_event().is_zero());
        assert_eq!(factory.pool_events().len(), 2);
    }

    #[test]
    fn test_skeleton_defaults() {
        let factory = ElfomoFiPropFactory::new_default(ELFOMO_CHAIN_ID, 42);
        let pools = factory.skeletons(42);
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(pool.address(), ELFOMO_POOL_ADDRESS);
        assert_eq!(pool.token_x, crate::amms::elfomo_prop::ELFOMO_XETH_ADDRESS);
        assert_eq!(pool.token_y, crate::amms::elfomo_prop::ELFOMO_USDT0_ADDRESS);
        assert_eq!(pool.created_block, 42);
        assert!(pool.tokens.is_empty());
        assert!(pool.levels.from_to_levels.is_empty());
        assert_eq!(pool.sync_events().len(), 2);
    }
}
