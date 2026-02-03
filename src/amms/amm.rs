use crate::amms::balancer_v2::BalancerV2Pool;
use crate::amms::balancer_v3::BalancerV3Pool;
use crate::amms::curve_legacy::CurveLegacyPool;
use crate::amms::curve_ng::CurveNGPool;
use crate::amms::ekubo::EkuboPool;
use crate::amms::fluid_dex::FluidDexPool;
use crate::amms::pancake_infinity::PancakeInfinityPool;
use crate::amms::pancake_v2::PancakeV2Pool;
use crate::amms::pancake_v3::PancakeV3Pool;
use crate::amms::sushi_v2::SushiV2Pool;
use crate::amms::uniswap_v4::UniswapV4Pool;

use super::{
    erc_4626::ERC4626Vault, error::AMMError, uniswap_v2::UniswapV2Pool, uniswap_v3::UniswapV3Pool,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// Action required after syncing a log
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// No further action needed
    None,
    /// Pool requires asynchronous update (e.g. fetching external price)
    AsyncUpdate,
    /// Pool state is invalid/incomplete, requires full re-sync
    Resync,
}

#[allow(async_fn_in_trait)]
pub trait AutomatedMarketMaker: Send + Sync + 'static {
    /// Address of the AMM
    fn address(&self) -> Address;

    /// Returns the list of supported chain IDs for this AMM.
    /// If None, it supports all chains (chain-agnostic).
    /// If Some(chains), it only supports the chains in the list.
    fn supported_chains(&self) -> Option<Vec<u64>> {
        None
    }

    fn last_synced_block(&self) -> u64;

    fn set_last_synced_block(&mut self, block_number: u64);

    /// Event signatures that indicate when the AMM should be synced
    fn sync_events(&self) -> Vec<B256>;

    /// Syncs the AMM state
    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError>;

    /// Returns a list of token addresses used in the AMM
    fn tokens(&self) -> Vec<Address>;

    /// Calculates the price of `base_token` in terms of `quote_token`
    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError>;

    /// Returns the cached spot price of `base_token` in terms of `quote_token`
    /// This is an O(1) operation using pre-calculated values.
    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        // Default implementation falls back to calculate_price for unoptimized AMMs
        self.calculate_price(base_token, quote_token)
    }

    /// Checks if the pool has sufficient liquidity to be considered for arbitrage.
    /// Used to filter out "zombie" or dust pools that distort price calculations.
    fn has_sufficient_liquidity(&self) -> bool {
        true
    }

    /// Simulate a swap
    /// Returns the amount_out in `quote token` for a given `amount_in` of `base_token`
    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError>;

    /// Simulate a swap, mutating the AMM state
    /// Returns the amount_out in `quote token` for a given `amount_in` of `base_token`
    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError>;

    /// Initializes an empty pool and syncs state up to `block_number`
    async fn init<N, P>(self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone;

    /// Updates the AMM state asynchronously (e.g. fetching external parameters like Oracle prices)
    /// This is called after `sync` (log application) to allow for expensive/async updates.
    async fn update<N, P>(&mut self, _provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Ok(())
    }
}

macro_rules! amm {
    ($($pool_type:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type")]
        pub enum AMM {
            $($pool_type($pool_type),)+
        }

        impl AutomatedMarketMaker for AMM {
            fn address(&self) -> Address {
                match self {
                    $(AMM::$pool_type(pool) => pool.address(),)+
                }
            }

            fn last_synced_block(&self) -> u64 {
                match self {
                    $(AMM::$pool_type(pool) => pool.last_synced_block(),)+
                }
            }

            fn set_last_synced_block(&mut self, block_number: u64) {
                match self {
                    $(AMM::$pool_type(pool) => pool.set_last_synced_block(block_number),)+
                }
            }

            fn sync_events(&self) -> Vec<B256> {
                match self {
                    $(AMM::$pool_type(pool) => pool.sync_events(),)+
                }
            }

            fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
                match self {
                    $(AMM::$pool_type(pool) => pool.sync(log),)+
                }
            }


            fn simulate_swap(&self, base_token: Address, quote_token: Address, amount_in: U256) -> Result<U256, AMMError> {
                match self {
                    $(AMM::$pool_type(pool) => pool.simulate_swap(base_token, quote_token, amount_in),)+
                }
            }

            fn simulate_swap_mut(&mut self, base_token: Address, quote_token: Address, amount_in: U256) -> Result<U256, AMMError> {
                match self {
                    $(AMM::$pool_type(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),)+
                }
            }

            async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
            where
                N: Network,
                P: Provider<N> + Clone,
            {
                match self {
                    $(AMM::$pool_type(pool) => pool.update(provider).await,)+
                }
            }

            fn tokens(&self) -> Vec<Address> {
                match self {
                    $(AMM::$pool_type(pool) => pool.tokens(),)+
                }
            }

            fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
                match self {
                    $(AMM::$pool_type(pool) => pool.calculate_price(base_token, quote_token),)+
                }
            }

            fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
                match self {
                    $(AMM::$pool_type(pool) => pool.spot_price(base_token, quote_token),)+
                }
            }

            fn has_sufficient_liquidity(&self) -> bool {
                match self {
                    $(AMM::$pool_type(pool) => pool.has_sufficient_liquidity(),)+
                }
            }

            async fn init<N, P>(self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
            where
                Self: Sized,
                N: Network,
                P: Provider<N> + Clone,
            {
                let block_u64 = block_number.as_u64();
                match self {
                    $(
                        AMM::$pool_type(pool) => {
                            let mut pool = pool.init(block_number, provider).await?;
                            if let Some(b) = block_u64 {
                                pool.set_last_synced_block(b);
                            }
                            Ok(AMM::$pool_type(pool))
                        }
                    )+
                }
            }
        }


        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum Variant {
            $($pool_type,)+
        }

        impl AMM {
            pub fn variant(&self) -> Variant {
                match self {
                    $(AMM::$pool_type(_) => Variant::$pool_type,)+
                }
            }
        }

        impl Hash for AMM {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.address().hash(state);
            }
        }

        impl PartialEq for AMM {
            fn eq(&self, other: &Self) -> bool {
                self.address() == other.address()
            }
        }

        impl Eq for AMM {}

        $(
            impl From<$pool_type> for AMM {
                fn from(amm: $pool_type) -> Self {
                    AMM::$pool_type(amm)
                }
            }
        )+
    };
}

amm!(
    UniswapV2Pool,
    UniswapV3Pool,
    UniswapV4Pool,
    PancakeInfinityPool,
    ERC4626Vault,
    BalancerV2Pool,
    BalancerV3Pool,
    SushiV2Pool,
    PancakeV3Pool,
    PancakeV2Pool,
    FluidDexPool,
    CurveNGPool,
    CurveLegacyPool,
    EkuboPool
);
