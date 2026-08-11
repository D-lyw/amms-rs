//! BinaryFi propAMM Factory
//!
//! BinaryFi 是固定地址的单池 propAMM（无池子创建事件），因此：
//! - `discover()` 直接返回固定池子骨架
//! - `init_batch()` 通过 `GetBinaryFiPropStateBatchRequest` 批量读取合约
//!   一次性完成 12 资产 decimals + 余额 + 132 对 quote 的初始化

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
    BinaryFiPropPool, BINARYFI_DEFAULT_FEE_PPM, BINARYFI_ENGINE_ADDRESS, BINARYFI_POOL_ADDRESS,
    BINARYFI_ROUTER_ADDRESS, BINARYFI_SWAP_EVENT, BINARYFI_UPDATE_EVENT, BINARYFI_VAULT_ADDRESS,
};

// ============================================================================
// BinaryFiPropFactory
// ============================================================================

/// BinaryFi propAMM 池子工厂
///
/// ## 发现机制
///
/// BinaryFi 是固定地址的单池实例，不 emit 池子创建事件：
/// `discover()` 直接返回固定池子骨架，初始化由批量读取合约完成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryFiPropFactory {
    /// 池子合约地址
    pub contract_address: Address,
    /// 引擎合约地址（update 日志来源）
    pub engine_address: Address,
    /// 金库合约地址
    pub vault_address: Address,
    /// quote 批量读取合约的 recipient（Router）
    pub router_address: Address,
    /// 链 ID
    pub chain_id: u64,
    /// 工厂合约的创建区块号（用作 StateSpace 扫描的起始点）
    pub creation_block: u64,
}

impl BinaryFiPropFactory {
    /// 创建 BinaryFi 工厂实例（全部合约地址由部署配置传入，支持跨链/多部署）
    pub fn new(
        pool_address: Address,
        engine_address: Address,
        vault_address: Address,
        router_address: Address,
        chain_id: u64,
        creation_block: u64,
    ) -> Self {
        Self {
            contract_address: pool_address,
            engine_address,
            vault_address,
            router_address,
            chain_id,
            creation_block,
        }
    }

    /// XLayer 默认部署便捷构造（地址见模块常量）
    pub fn new_default(chain_id: u64, creation_block: u64) -> Self {
        Self::new(
            BINARYFI_POOL_ADDRESS,
            BINARYFI_ENGINE_ADDRESS,
            BINARYFI_VAULT_ADDRESS,
            BINARYFI_ROUTER_ADDRESS,
            chain_id,
            creation_block,
        )
    }

    /// 构建池子骨架（资产在 init 时由批量读取合约填充）
    fn skeleton(&self, created_block: u64) -> BinaryFiPropPool {
        BinaryFiPropPool {
            pool_address: self.contract_address,
            virtual_address: Address::ZERO,
            exposed_pair: None,
            engine_address: self.engine_address,
            vault_address: self.vault_address,
            router_address: self.router_address,
            chain_id: self.chain_id,
            created_block,
            last_synced_block: 0,
            assets: Vec::new(),
            prices: Vec::new(),
            spreads: Vec::new(),
            bid_offsets: Vec::new(),
            ask_offsets: Vec::new(),
            q0j: Vec::new(),
            sell_raw: Vec::new(),
            price_scales: Vec::new(),
            buy_disabled: Vec::new(),
            buy_zero_over_vault: Vec::new(),
            max_outputs: Vec::new(),
            max_inputs: Vec::new(),
            reserves: Vec::new(),
            rates: Vec::new(),
            stale_pairs: Vec::new(),
            price_updated_block: Vec::new(),
            sell_ladders: Vec::new(),
            buy_ladders: Vec::new(),
            buy_ladder_remaining: Vec::new(),
            ladder_reserves: Vec::new(),
            price0_calibrated: false,
            fee_ppm: BINARYFI_DEFAULT_FEE_PPM,
        }
    }
}

// ============================================================================
// Discovery 实现
// ============================================================================

impl DiscoverySync for BinaryFiPropFactory {
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
        async move { Ok(vec![AMM::BinaryFiPropPool(factory.skeleton(created_block))]) }
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

impl AutomatedMarketMakerFactory for BinaryFiPropFactory {
    type PoolVariant = BinaryFiPropPool;

    fn address(&self) -> Address {
        self.contract_address
    }

    fn create_pool(&self, _log: Log) -> Result<AMM, AMMError> {
        // BinaryFi 不 emit 池子创建事件，此方法不可用
        Err(AMMError::Msg(
            "binaryfi: pool creation is event-less, use discover()".to_string(),
        ))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        // BinaryFi 无池子创建事件
        B256::ZERO
    }

    fn pool_events(&self) -> Vec<B256> {
        vec![BINARYFI_SWAP_EVENT, BINARYFI_UPDATE_EVENT]
    }

    fn pool_variant(&self) -> Self::PoolVariant {
        Default::default()
    }
}

// ============================================================================
// init_batch: 批量初始化池子
// ============================================================================

/// 批量初始化 BinaryFi propAMM 池子
///
/// 每个池子通过 `GetBinaryFiPropStateBatchRequest` 静态调用一次完成：
/// 12 资产 decimals + 池子余额 + 132 对 quote 费率。
pub async fn init_batch<N, P>(
    amms: Vec<AMM>,
    block_number: BlockId,
    provider: P,
) -> Result<Vec<AMM>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    // 虚拟子池：同一部署（pool/engine/router/vault 相同）的多个实例共享一份
    // 链上状态。按部署分组，每组只对第一个实例做一次全量批量快照初始化，
    // 其余实例克隆其状态并恢复各自的虚拟身份（virtual_address/exposed_pair），
    // 避免 65 个实例各自触发一次链上批量读取。
    let mut groups: std::collections::HashMap<(Address, Address, Address, Address), Vec<AMM>> =
        std::collections::HashMap::new();
    for amm in amms {
        let key = match &amm {
            AMM::BinaryFiPropPool(p) => p.deployment_key(),
            // 非 BinaryFi 池子（理论不会出现，防御）：以自身地址为组 key，
            // 保证每组仅一个实例，行为与逐个 init 一致。
            _ => (amm.address(), Address::ZERO, Address::ZERO, Address::ZERO),
        };
        groups.entry(key).or_default().push(amm);
    }

    let mut initialized = Vec::with_capacity(groups.values().map(|g| g.len()).sum());
    for (_, group) in groups {
        let Some(seed) = group.first().cloned() else {
            continue;
        };
        let (seed_virtual, seed_exposed) = match &seed {
            AMM::BinaryFiPropPool(p) => (p.virtual_address, p.exposed_pair),
            _ => (Address::ZERO, None),
        };
        let address = seed.address();
        match seed.init::<N, P>(block_number, provider.clone()).await {
            Ok(mut p) => {
                // init 不会改动虚拟身份，显式恢复以防未来变更
                if let AMM::BinaryFiPropPool(pp) = &mut p {
                    pp.virtual_address = seed_virtual;
                    pp.exposed_pair = seed_exposed;
                }
                for other in group.iter().skip(1) {
                    let (ov, oe) = match other {
                        AMM::BinaryFiPropPool(pp) => (pp.virtual_address, pp.exposed_pair),
                        _ => (Address::ZERO, None),
                    };
                    let mut clone = p.clone();
                    if let AMM::BinaryFiPropPool(cp) = &mut clone {
                        cp.virtual_address = ov;
                        cp.exposed_pair = oe;
                    }
                    initialized.push(clone);
                }
                initialized.push(p);
            }
            Err(e) => {
                // 初始化失败会静默丢失池子（上层拓扑仍引用它），必须显式告警
                warn!(
                    target: "amms::binaryfi_prop",
                    pool = %address,
                    error = %e,
                    "binaryfi: failed to init pool"
                );
            }
        }
    }
    Ok(initialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::binaryfi_prop::BINARYFI_CHAIN_ID;

    #[test]
    fn test_factory_address() {
        let factory = BinaryFiPropFactory::new_default(BINARYFI_CHAIN_ID, 1);
        assert_eq!(factory.address(), BINARYFI_POOL_ADDRESS);
        assert_eq!(factory.engine_address, BINARYFI_ENGINE_ADDRESS);
        assert_eq!(factory.vault_address, BINARYFI_VAULT_ADDRESS);
        assert_eq!(factory.router_address, BINARYFI_ROUTER_ADDRESS);
        assert_eq!(factory.pool_events().len(), 2);
        assert!(factory.pool_creation_event().is_zero());
    }

    #[test]
    fn test_skeleton_defaults() {
        let factory = BinaryFiPropFactory::new_default(BINARYFI_CHAIN_ID, 1);
        let pool = factory.skeleton(42);
        assert_eq!(pool.address(), BINARYFI_POOL_ADDRESS);
        assert_eq!(pool.engine_address, BINARYFI_ENGINE_ADDRESS);
        assert_eq!(pool.vault_address, BINARYFI_VAULT_ADDRESS);
        assert_eq!(pool.router_address, BINARYFI_ROUTER_ADDRESS);
        assert_eq!(pool.created_block, 42);
        assert!(pool.assets.is_empty());
        assert_eq!(pool.sync_events().len(), 3);
    }
}
