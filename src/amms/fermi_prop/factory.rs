//! Fermi propAMM Factory
//!
//! Fermi 是固定部署（engine 管理 pair 注册表，`PairRegistered` 事件驱动发现）。
//! - `discover()` 通过 engine `getPairs()` 枚举全部 pair 并返回骨架池
//! - `init_batch()` 按部署分组去重，每组只做一次链上 init，其余实例克隆

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

use super::types::{IFermiEngine, IFermiEngine::TokenPair};
use super::{
    fermi_lane_index, fermi_virtual_address, sorted_tokens, FermiPropPool, FERMI_ENGINE_ADDRESS,
    FERMI_REGISTRY_ADDRESS, FERMI_SWAPPER_ADDRESS, FERMI_VAULT_ADDRESS, FERMI_WRAPPER_ADDRESS,
};

// ============================================================================
// FermiPropFactory
// ============================================================================

/// Fermi propAMM 池子工厂
///
/// ## 发现机制
///
/// Fermi engine 持有全部 pair（`getPairs()`），pair 生命周期由
/// `PairRegistered`/`PairUnregistered`/`PairActiveSet` 事件驱动。
/// `discover()` 调用 `getPairs()` 返回全部 pair 骨架；单个 pair 的
/// 曲线参数/lane/余额在 `init` 时填充。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FermiPropFactory {
    /// engine 合约地址
    pub engine_address: Address,
    /// swapper 合约地址
    pub swapper_address: Address,
    /// IPropAMM wrapper 地址
    pub wrapper_address: Address,
    /// registry 地址
    pub registry_address: Address,
    /// trader vault 地址
    pub vault_address: Address,
    /// 链 ID
    pub chain_id: u64,
    /// 工厂合约的创建区块号（用作 StateSpace 扫描的起始点）
    pub creation_block: u64,
}

impl FermiPropFactory {
    /// 创建 Fermi 工厂实例（全部合约地址由部署配置传入，支持多部署）。
    pub fn new(
        engine_address: Address,
        swapper_address: Address,
        wrapper_address: Address,
        registry_address: Address,
        vault_address: Address,
        chain_id: u64,
        creation_block: u64,
    ) -> Self {
        Self {
            engine_address,
            swapper_address,
            wrapper_address,
            registry_address,
            vault_address,
            chain_id,
            creation_block,
        }
    }

    /// Ethereum 主网默认部署便捷构造（地址见 types.rs 常量）。
    pub fn new_default(chain_id: u64, creation_block: u64) -> Self {
        Self::new(
            FERMI_ENGINE_ADDRESS,
            FERMI_SWAPPER_ADDRESS,
            FERMI_WRAPPER_ADDRESS,
            FERMI_REGISTRY_ADDRESS,
            FERMI_VAULT_ADDRESS,
            chain_id,
            creation_block,
        )
    }

    /// 构建池子骨架（曲线参数/lane/余额在 init 时填充）。
    pub fn skeleton(&self, pair: TokenPair, created_block: u64) -> FermiPropPool {
        let (token_a, token_b) = sorted_tokens(pair.token0, pair.token1);
        FermiPropPool {
            engine_address: self.engine_address,
            swapper_address: self.swapper_address,
            wrapper_address: self.wrapper_address,
            registry_address: self.registry_address,
            vault_address: self.vault_address,
            token_a,
            token_b,
            decimals_a: 18,
            decimals_b: 18,
            virtual_address: fermi_virtual_address(self.engine_address, token_a, token_b),
            lane_index: fermi_lane_index(token_a, token_b),
            active: pair.active,
            chain_id: self.chain_id,
            created_block,
            last_synced_block: 0,
            ..Default::default()
        }
    }
}

// ============================================================================
// Discovery 实现
// ============================================================================

impl DiscoverySync for FermiPropFactory {
    fn discover<N, P>(
        &self,
        _to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let factory = self.clone();
        async move {
            let engine = IFermiEngine::new(factory.engine_address, provider);
            let pairs = engine
                .getPairs()
                .block(BlockId::latest())
                .call()
                .await
                .map_err(|e| AMMError::Msg(format!("fermi: getPairs failed: {e}")))?;
            let mut amms = Vec::with_capacity(pairs.len());
            for pair in pairs {
                amms.push(AMM::FermiPropPool(
                    factory.skeleton(pair, factory.creation_block),
                ));
            }
            Ok(amms)
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

impl AutomatedMarketMakerFactory for FermiPropFactory {
    type PoolVariant = FermiPropPool;

    fn address(&self) -> Address {
        self.engine_address
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        // Fermi 的 pair 由 engine getPairs 发现；create_pool 仅支持
        // PairRegistered 事件驱动的创建（骨架由事件参数组装）。
        let topics = log.topics();
        if log.address() == self.engine_address
            && topics.len() == 3
            && topics[0] == super::FERMI_PAIR_REGISTERED_EVENT
        {
            let base = Address::from_word(topics[1]);
            let quote = Address::from_word(topics[2]);
            let pair = TokenPair {
                token0: base.min(quote),
                token1: base.max(quote),
                active: true,
            };
            return Ok(AMM::FermiPropPool(
                self.skeleton(pair, log.block_number.unwrap_or_default()),
            ));
        }
        Err(AMMError::Msg(
            "fermi: unsupported pool creation event".to_string(),
        ))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        super::FERMI_PAIR_REGISTERED_EVENT
    }

    fn pool_events(&self) -> Vec<B256> {
        vec![
            super::FERMI_PAIR_REGISTERED_EVENT,
            super::FERMI_PAIR_UNREGISTERED_EVENT,
            super::FERMI_PAIR_ACTIVE_SET_EVENT,
            super::FERMI_SWAPPED_EVENT,
        ]
    }

    fn pool_variant(&self) -> Self::PoolVariant {
        Default::default()
    }
}

// ============================================================================
// init_batch：批量初始化池子
// ============================================================================

/// 批量初始化 Fermi propAMM 池子。
///
/// 按部署分组（engine/wrapper/registry/vault 相同 = 同一部署），每组只对
/// 第一个实例做一次链上 init，其余实例克隆其状态并恢复各自虚拟身份，
/// 避免多个 pair 实例各自触发重复链上读取。
pub async fn init_batch<N, P>(
    amms: Vec<AMM>,
    block_number: BlockId,
    provider: P,
) -> Result<Vec<AMM>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let mut groups: std::collections::HashMap<(Address, Address, Address, Address), Vec<AMM>> =
        std::collections::HashMap::new();
    for amm in amms {
        let key = match &amm {
            AMM::FermiPropPool(p) => p.deployment_key(),
            _ => (amm.address(), Address::ZERO, Address::ZERO, Address::ZERO),
        };
        groups.entry(key).or_default().push(amm);
    }

    let mut initialized = Vec::with_capacity(groups.values().map(|g| g.len()).sum());
    for (_, group) in groups {
        let Some(seed) = group.first().cloned() else {
            continue;
        };
        let address = seed.address();
        let seed_tokens = match &seed {
            AMM::FermiPropPool(p) => Some((p.token_a, p.token_b)),
            _ => None,
        };
        match seed
            .clone()
            .init::<N, P>(block_number, provider.clone())
            .await
        {
            Ok(mut p) => {
                for _other in group.iter().skip(1) {
                    let mut clone = p.clone();
                    if let (AMM::FermiPropPool(cp), Some((ta, tb))) = (&mut clone, seed_tokens) {
                        cp.token_a = ta;
                        cp.token_b = tb;
                    }
                    initialized.push(clone);
                }
                initialized.push(p);
            }
            Err(e) => {
                warn!(
                    target: "amms::fermi_prop",
                    pool = %address,
                    error = %e,
                    "fermi: failed to init pool"
                );
            }
        }
    }
    Ok(initialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_factory_default_addresses() {
        let factory = FermiPropFactory::new_default(1, 1);
        assert_eq!(factory.engine_address, FERMI_ENGINE_ADDRESS);
        assert_eq!(factory.swapper_address, FERMI_SWAPPER_ADDRESS);
        assert_eq!(factory.wrapper_address, FERMI_WRAPPER_ADDRESS);
        assert_eq!(factory.registry_address, FERMI_REGISTRY_ADDRESS);
        assert_eq!(factory.vault_address, FERMI_VAULT_ADDRESS);
        assert_eq!(factory.creation_block, 1);
        assert_eq!(factory.pool_events().len(), 4);
        assert_eq!(
            factory.pool_creation_event(),
            crate::amms::fermi_prop::FERMI_PAIR_REGISTERED_EVENT
        );
    }

    #[test]
    fn test_skeleton_sorts_and_keys() {
        let factory = FermiPropFactory::new_default(1, 42);
        let weth = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // getPairs 返回 token0 < token1 排序（USDC < WETH）
        let pair = TokenPair {
            token0: usdc,
            token1: weth,
            active: true,
        };
        let pool = factory.skeleton(pair, 42);
        assert_eq!(pool.token_a, usdc);
        assert_eq!(pool.token_b, weth);
        assert_eq!(
            pool.virtual_address,
            fermi_virtual_address(FERMI_ENGINE_ADDRESS, usdc, weth)
        );
        assert_eq!(pool.lane_index, fermi_lane_index(usdc, weth));
        assert!(pool.active);
        assert_eq!(pool.created_block, 42);
    }
}
