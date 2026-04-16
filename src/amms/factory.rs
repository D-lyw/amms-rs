use crate::amms::aerodrome_slipstream::AerodromeSlipstreamFactory;
use crate::amms::aerodrome_v2::AerodromeV2Factory;
use crate::amms::curve_legacy::factory::CurveLegacyFactory;
use crate::amms::curve_ng::factory::CurveNGFactory;
use crate::amms::ekubo::EkuboFactory;
use crate::amms::erc_4626::ERC4626Vault;
use crate::amms::fluid_dex::FluidDexFactory;
use crate::amms::pancake_infinity::factory::PancakeInfinityFactory;
use crate::amms::pancake_v2::PancakeV2Factory;
use crate::amms::pancake_v3::PancakeV3Factory;
use crate::amms::sky::SkyConverter;
use crate::amms::sushi_v2::SushiV2Factory;
use crate::amms::uniswap_v4::UniswapV4Factory;
use crate::amms::{balancer_v2::BalancerV2Factory, balancer_v3::BalancerV3Factory};

use super::{amm::Variant, uniswap_v2::UniswapV2Factory, uniswap_v3::UniswapV3Factory};
use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::eth::Log,
};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    hash::{Hash, Hasher},
};

pub trait DiscoverySync {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone;

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone;
}

pub trait AutomatedMarketMakerFactory: DiscoverySync {
    type PoolVariant: AutomatedMarketMaker + Default;

    /// Address of the factory contract
    fn address(&self) -> Address;

    /// Creates an unsynced pool from a creation log.
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError>;

    /// Returns the block number at which the factory was created.
    fn creation_block(&self) -> u64;

    /// Event signature that indicates when a new pool was created
    fn pool_creation_event(&self) -> B256;

    /// Event signatures signifying when a pool created by the factory should be synced
    fn pool_events(&self) -> Vec<B256> {
        Self::PoolVariant::default().sync_events()
    }

    fn pool_variant(&self) -> Self::PoolVariant {
        Self::PoolVariant::default()
    }
}

impl Variant {
    pub async fn init_batch<N, P>(
        self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        match self {
            Variant::UniswapV3Pool => {
                UniswapV3Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::UniswapV2Pool => {
                UniswapV2Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::UniswapV4Pool => {
                UniswapV4Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::PancakeInfinityPool => {
                PancakeInfinityFactory::init_batch::<N, _>(amms, to_block, provider).await
            }

            Variant::BalancerV2Pool => {
                BalancerV2Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::BalancerV3Pool => {
                BalancerV3Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::ERC4626Vault => {
                ERC4626Vault::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::SushiV2Pool => {
                SushiV2Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::PancakeV2Pool => {
                PancakeV2Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::PancakeV3Pool => {
                PancakeV3Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::FluidDexPool => {
                FluidDexFactory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::CurveNGPool => {
                CurveNGFactory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::CurveLegacyPool => {
                CurveLegacyFactory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::EkuboPool => EkuboFactory::init_batch::<N, _>(amms, to_block, provider).await,
            Variant::AerodromeV2Pool => {
                AerodromeV2Factory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::AerodromeSlipstreamPool => {
                AerodromeSlipstreamFactory::init_batch::<N, _>(amms, to_block, provider).await
            }
            Variant::SkyConverter => {
                SkyConverter::init_batch::<N, _>(amms, to_block, provider).await
            }
        }
    }

    pub async fn sync_all_pools<N, P>(
        self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        match self {
            Variant::UniswapV3Pool => {
                UniswapV3Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::UniswapV2Pool => {
                UniswapV2Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::UniswapV4Pool => {
                UniswapV4Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::PancakeInfinityPool => {
                PancakeInfinityFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::BalancerV2Pool => {
                BalancerV2Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::BalancerV3Pool => {
                BalancerV3Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::SushiV2Pool => {
                SushiV2Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::PancakeV2Pool => {
                PancakeV2Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::PancakeV3Pool => {
                PancakeV3Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::FluidDexPool => {
                FluidDexFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::ERC4626Vault => {
                let mut out = Vec::with_capacity(amms.len());
                for amm in amms {
                    if let Ok(synced) = amm.init(to_block, provider.clone()).await {
                        out.push(synced);
                    }
                }
                Ok(out)
            }
            Variant::CurveNGPool => {
                CurveNGFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::CurveLegacyPool => {
                CurveLegacyFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::EkuboPool => {
                EkuboFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::AerodromeV2Pool => {
                AerodromeV2Factory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::AerodromeSlipstreamPool => {
                AerodromeSlipstreamFactory::sync_all_pools::<N, _>(amms, to_block, provider).await
            }
            Variant::SkyConverter => {
                SkyConverter::init_batch::<N, _>(amms, to_block, provider).await
            }
        }
    }
}

macro_rules! factory {
    ($($factory_type:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum Factory {
            $($factory_type($factory_type),)+
        }

        impl Factory {
             pub fn address(&self) -> Address {
                match self {
                    $(Factory::$factory_type(factory) => factory.address(),)+
                }
            }

             pub fn discovery_event(&self) -> B256 {
                match self {
                    $(Factory::$factory_type(factory) => factory.pool_creation_event(),)+
                }
            }

             pub fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
                match self {
                    $(Factory::$factory_type(factory) => factory.create_pool(log),)+
                }
            }

             pub fn creation_block(&self) -> u64 {
                match self {
                    $(Factory::$factory_type(factory) => factory.creation_block(),)+
                }
            }

             pub fn pool_events(&self) -> Vec<B256> {
                match self {
                    $(Factory::$factory_type(factory) => factory.pool_events(),)+
                }
            }

            pub fn variant(&self) -> Variant {
                match self {
                    $(Factory::$factory_type(factory) => AMM::from(factory.pool_variant()).variant(),)+
                }
            }
        }

        impl Hash for Factory {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.address().hash(state);
            }
        }

        impl PartialEq for Factory {
            fn eq(&self, other: &Self) -> bool {
                self.address() == other.address()
            }
        }

        impl Eq for Factory {}


        impl Factory {
            pub async fn discover< N, P>(&self, to_block: BlockId, provider: P) -> Result<Vec<AMM>, AMMError>
            where
                                N: Network,
                P: Provider<N> + Clone,
            {
                match self {
                    $(Factory::$factory_type(factory) => factory.discover(to_block, provider).await,)+
                }
            }

            pub async fn sync< N, P>(&self, amms: Vec<AMM>, to_block: BlockId, provider: P) -> Result<Vec<AMM>, AMMError>
            where
                                N: Network,
                P: Provider<N> + Clone,
            {
                match self {
                    $(Factory::$factory_type(factory) => factory.sync(amms, to_block, provider).await,)+
                }
            }
        }

        $(
            impl From<$factory_type> for Factory {
                fn from(factory: $factory_type) -> Self {
                    Factory::$factory_type(factory)
                }
            }
        )+
    };
}

factory!(
    UniswapV2Factory,
    UniswapV3Factory,
    UniswapV4Factory,
    PancakeV2Factory,
    PancakeV3Factory,
    PancakeInfinityFactory,
    SushiV2Factory,
    FluidDexFactory,
    CurveLegacyFactory,
    AerodromeV2Factory,
    AerodromeSlipstreamFactory
);

#[derive(Default)]
pub struct NoopAMM;
impl AutomatedMarketMaker for NoopAMM {
    fn address(&self) -> Address {
        unreachable!()
    }

    fn last_synced_block(&self) -> u64 {
        unreachable!()
    }

    fn set_last_synced_block(&mut self, _block_number: u64) {
        unreachable!()
    }

    fn sync_events(&self) -> Vec<B256> {
        unreachable!()
    }

    fn sync(&mut self, _log: &Log) -> Result<SyncAction, AMMError> {
        unreachable!()
    }

    fn simulate_swap(
        &self,
        _base_token: Address,
        _quote_token: Address,
        _amount_in: U256,
    ) -> Result<U256, AMMError> {
        unreachable!()
    }

    fn simulate_swap_mut(
        &mut self,
        _base_token: Address,
        _quote_token: Address,
        _amount_in: U256,
    ) -> Result<U256, AMMError> {
        unreachable!()
    }
    fn calculate_price(
        &self,
        _base_token: Address,
        _quote_token: Address,
    ) -> Result<f64, AMMError> {
        unreachable!()
    }

    fn tokens(&self) -> Vec<Address> {
        unreachable!()
    }

    async fn init<N, P>(self, _block_number: BlockId, _provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        unreachable!()
    }

    fn decimals(&self, _token: Address) -> u8 {
        unreachable!()
    }
}
