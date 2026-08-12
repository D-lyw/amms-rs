//! Ekubo V2 AMM Module
//!
//! This module provides support for Ekubo V2 pools on EVM.
//!
//! ## Architecture
//!
//! Ekubo is a singleton CLMM (Concentrated Liquidity Market Maker) similar to Uniswap V4.
//! - Core contract: `0xe0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444`
//! - CoreDataFetcher (V2): `0x208bb00c6b142351e4a431f6dd323691ebb7c285`
//!
//! ## File Structure
//!
//! - `types.rs` - Type definitions (PoolConfig, EkuboPoolKey, EkuboSwapEvent, TickInfo)
//! - `pool.rs` - EkuboPool struct and AMM trait implementation
//! - `factory.rs` - EkuboFactory for pool discovery
//! - `math.rs` - Ekubo-specific swap math calculations
//!
//! ## References
//!
//! - [Ekubo V2 Contracts](https://github.com/EkuboProtocol/evm-contracts/tree/v2.0.0)
//! - [Ekubo Documentation](https://docs.ekubo.org/integration-guides/reference/evm-contracts-v2)

pub mod factory;
pub mod math;
pub mod pool;
pub mod types;

use alloy::primitives::{address, Address};

pub fn get_core_address(chain_id: u64) -> Option<Address> {
    match chain_id {
        // Ekubo V2 Core on Ethereum Mainnet
        1 => Some(address!("e0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444")),
        _ => None,
    }
}

// Re-export main types for convenience
pub use factory::EkuboFactory;
pub use pool::EkuboPool;
pub use types::{parse_swap_event_log0, EkuboPoolKey, EkuboSwapEvent, PoolConfig, TickInfo};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, U256};

    #[test]
    fn test_pool_id_computation() {
        let pool_key =
            EkuboPoolKey::new_concentrated(Address::ZERO, Address::ZERO, 3000, 60, Address::ZERO);

        let pool_id = pool_key.pool_id();
        assert_ne!(pool_id, B256::ZERO);
    }

    #[test]
    fn test_pool_config_v2_roundtrip() {
        let fee = 3689348814741910u64; // ~0.02%
        let tick_spacing = 5000;
        let extension = Address::ZERO;

        let config = PoolConfig::create_v2(fee, tick_spacing, extension);
        let parsed = PoolConfig::from_bytes32(config);

        assert_eq!(parsed.fee, fee);
        assert_eq!(parsed.tick_spacing, tick_spacing);
        assert_eq!(parsed.extension, extension);
    }

    #[test]
    fn test_pool_key_from_raw() {
        // ETH/USDC pool from user
        let token0 = Address::ZERO; // ETH (native)
        let token1 = alloy::primitives::address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let config = U256::from_str_radix(
            "0000000000000000000000000000000000000000000d1b71758e21960000137e",
            16,
        )
        .unwrap();

        let pool_key = EkuboPoolKey::from_raw(token0, token1, config);
        let pool_config = pool_key.parse_config();

        println!("Fee: {}", pool_config.fee);
        println!("Tick Spacing: {}", pool_config.tick_spacing);
        println!("Extension: {:?}", pool_config.extension);

        // Verify pool_id can be computed
        let pool_id = pool_key.pool_id();
        assert_ne!(pool_id, B256::ZERO);
    }

    #[test]
    fn test_simulate_swap_mut_advances_local_pool_state() {
        use crate::amms::amm::AutomatedMarketMaker;
        use crate::amms::Token;

        let token0 = alloy::primitives::address!("0000000000000000000000000000000000000001");
        let token1 = alloy::primitives::address!("0000000000000000000000000000000000000002");

        let pool_key = EkuboPoolKey::new_concentrated(token0, token1, 3000, 60, Address::ZERO);
        let mut pool = EkuboPool::new(Address::ZERO, pool_key);

        pool.token_a = Token::new_with_decimals(token0, 18);
        pool.token_b = Token::new_with_decimals(token1, 18);
        // tick 0 -> sqrt ratio = 2^128 (Q64.128 fixed point)
        pool.sqrt_price = U256::from(1u128) << 128;
        pool.tick = 0;
        pool.tick_spacing = 60;
        pool.fee = 3689348814741910u128; // ~0.02%

        // Wide position so a small swap does not cross any tick
        let liquidity = 1_000_000_000_000_000_000u128;
        pool.modify_position(-240, 240, liquidity as i128).unwrap();

        pool.token_a_price = pool.calculate_price(token0, token1).unwrap();
        pool.token_b_price = pool.calculate_price(token1, token0).unwrap();

        let amount_in = U256::from(1_000_000_000_000u128);
        let before_sqrt = pool.sqrt_price;
        let before_tick = pool.tick;
        let before_liquidity = pool.liquidity;
        let before_price_a = pool.token_a_price;
        let before_price_b = pool.token_b_price;

        // simulate_swap must be read-only
        let out_read = pool.simulate_swap(token0, token1, amount_in).unwrap();
        assert_eq!(pool.sqrt_price, before_sqrt);
        assert_eq!(pool.tick, before_tick);
        assert_eq!(pool.liquidity, before_liquidity);
        assert_eq!(pool.token_a_price, before_price_a);
        assert_eq!(pool.token_b_price, before_price_b);

        // simulate_swap_mut must advance the local pool state
        let out_mut = pool.simulate_swap_mut(token0, token1, amount_in).unwrap();
        assert_eq!(out_mut, out_read);
        assert_ne!(
            pool.sqrt_price, before_sqrt,
            "simulate_swap_mut should advance local pool state"
        );
        assert!(pool.token_a_price.is_finite() && pool.token_a_price > 0.0);
        assert!(pool.token_b_price.is_finite() && pool.token_b_price > 0.0);

        // The next simulation on the same input must observe the advanced state
        let out_after = pool.simulate_swap(token0, token1, amount_in).unwrap();
        assert_ne!(
            out_after, out_read,
            "same input after simulate_swap_mut should observe advanced local state"
        );
    }
}

#[cfg(test)]
mod test_price;
