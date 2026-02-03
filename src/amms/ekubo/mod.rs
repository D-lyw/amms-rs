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
}

#[cfg(test)]
mod test_price;
