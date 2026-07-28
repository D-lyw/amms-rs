use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, OnceLock, RwLock},
};

use alloy::{
    dyn_abi::DynSolType,
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    sol,
};
use error::{AMMError, BatchContractError};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use uniswap_v3_math::{error::UniswapV3MathError, tick_math};

pub mod aerodrome_slipstream;
pub mod aerodrome_v2;
pub mod algebra_integral;
pub mod amm;
pub mod balancer_v2;
pub mod balancer_v3;
pub mod caliber_prop;
pub mod consts;
pub mod curve_legacy;
pub mod curve_ng;
pub mod ekubo;
pub mod erc_4626;
pub mod error;
pub mod factory;
pub mod float;
pub mod fluid_dex;
pub mod pancake_infinity;
pub mod pancake_v2;
pub mod pancake_v3;
pub mod pendle;
pub mod rocketpool;
pub mod sky;
pub mod sushi_v2;
pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod uniswap_v4;

fn tick_sqrt_ratio_cache() -> &'static RwLock<HashMap<i32, U256>> {
    static CACHE: OnceLock<RwLock<HashMap<i32, U256>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn sqrt_ratio_at_tick_cached(tick: i32) -> Result<U256, UniswapV3MathError> {
    {
        let cache = tick_sqrt_ratio_cache()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cache.get(&tick).copied() {
            return Ok(cached);
        }
    }

    let sqrt_ratio = tick_math::get_sqrt_ratio_at_tick(tick)?;
    let mut cache = tick_sqrt_ratio_cache()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(*cache.entry(tick).or_insert(sqrt_ratio))
}

#[cfg(test)]
mod tests {
    use super::sqrt_ratio_at_tick_cached;

    #[test]
    fn cached_sqrt_ratio_matches_tick_math() {
        for tick in [-100, -1, 0, 1, 100, 887_272, -887_272] {
            let expected = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(tick).unwrap();
            let cached_once = sqrt_ratio_at_tick_cached(tick).unwrap();
            let cached_twice = sqrt_ratio_at_tick_cached(tick).unwrap();

            assert_eq!(cached_once, expected);
            assert_eq!(cached_twice, expected);
        }
    }
}

sol! {
    #[sol(rpc)]
    GetTokenDecimalsBatchRequest,
    "src/amms/abi/GetTokenDecimalsBatchRequest.json",
}

sol!(
#[derive(Debug, PartialEq, Eq)]
#[sol(rpc)]
contract IERC20 {
    function decimals() external view returns (uint8);
    function symbol() external view returns (string memory);
});

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    pub address: Address,
    pub decimals: u8,
    pub symbol: String,
    pub chain_id: u64,
}

impl Token {
    pub async fn new<N, P>(address: Address, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let token = Arc::new(IERC20::new(address, provider.clone()));
        let decimals = token.decimals().call().await?;
        let symbol = token.symbol().call().await?;

        Ok(Self {
            address,
            decimals,
            chain_id: provider.get_chain_id().await?,
            symbol,
        })
    }

    pub const fn new_with_decimals(address: Address, decimals: u8) -> Self {
        Self {
            address,
            decimals,
            symbol: String::new(),
            chain_id: 0,
        }
    }

    pub const fn address(&self) -> &Address {
        &self.address
    }

    pub const fn decimals(&self) -> u8 {
        self.decimals
    }

    /// Checks if the provided reserve amount is considered sufficient liquidity for this token.
    /// This helps filter out "dust" or "zombie" pools.
    pub fn has_sufficient_liquidity(&self, reserve: u128) -> bool {
        let symbol = self.symbol.to_uppercase();

        // 1. Check for known high-value base assets
        if symbol == "WETH" || symbol == "WBNB" || symbol == "ETH" || symbol == "BNB" {
            // Require at least 0.1 ETH/BNB (~$300)
            // 0.1 * 10^18 = 100_000_000_000_000_000
            return reserve >= 100_000_000_000_000_000;
        }
        if symbol == "WBTC" || symbol == "BTC" || symbol == "CBTC" {
            // Require at least 0.005 BTC (~$300)
            // 0.005 * 10^8 = 500_000
            return reserve >= 500_000;
        }
        if symbol == "USDC" || symbol == "USDT" || symbol == "DAI" {
            // Require at least 300 USD
            // 300 * 10^decimals
            // For 6 decimals: 300 * 10^6 = 300_000_000
            // For 18 decimals: 300 * 10^18
            let threshold = 300u128.saturating_mul(10u128.pow(self.decimals as u32));
            return reserve >= threshold;
        }

        // 2. Generic check for other tokens based on decimals
        if self.decimals >= 18 {
            // 0.0001 unit (e.g. 10^14 wei)
            reserve >= 10u128.pow(self.decimals as u32 - 4)
        } else if self.decimals >= 6 {
            // 100 units (e.g. 100 * 10^6 = 10^8)
            let threshold = 100u128.saturating_mul(10u128.pow(self.decimals as u32));
            reserve >= threshold
        } else {
            // Fallback for very low decimals
            reserve >= 100_000
        }
    }
}

impl From<Address> for Token {
    fn from(address: Address) -> Self {
        Self {
            address,
            decimals: 0,
            ..Default::default()
        }
    }
}

impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

/// Fetches the decimal precision for a list of ERC-20 tokens.
///
/// # Returns
/// A map of token addresses to their decimal precision.
pub async fn get_token_decimals<N, P>(
    tokens: Vec<Address>,
    provider: P,
) -> Result<HashMap<Address, u8>, BatchContractError>
where
    N: Network,
    P: Provider<N> + Clone + Clone,
{
    let mut token_decimals = HashMap::new();

    // Filter out Address::ZERO (Native ETH) and set decimals to 18
    let eth_placeholder: Address = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"
        .parse()
        .unwrap();
    let tokens_to_fetch: Vec<Address> = tokens
        .into_iter()
        .filter(|&t| {
            if t.is_zero() || t == eth_placeholder {
                token_decimals.insert(t, 18);
                false
            } else {
                true
            }
        })
        .collect();

    let step = 765;

    let mut futures = FuturesUnordered::new();
    tokens_to_fetch.chunks(step).for_each(|group| {
        let provider = provider.clone();

        futures.push(async move {
            (
                group,
                GetTokenDecimalsBatchRequest::deploy_builder(provider, group.to_vec())
                    .call_raw()
                    .await,
            )
        });
    });

    let return_type = DynSolType::Array(Box::new(DynSolType::Uint(8)));

    while let Some(res) = futures.next().await {
        let (token_addresses, return_data) = res;

        let return_data = return_type.abi_decode_sequence(&return_data?)?;

        if let Some(tokens_arr) = return_data.as_array() {
            for (decimals, token_address) in tokens_arr.iter().zip(token_addresses.iter()) {
                token_decimals.insert(
                    *token_address,
                    decimals.as_uint().expect("Could not get uint").0.to::<u8>(),
                );
            }
        }
    }
    Ok(token_decimals)
}
