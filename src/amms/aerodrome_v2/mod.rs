// Aerodrome V2 AMM Implementation
//
// This module implements Aerodrome's V2 AMM protocol, which supports two pool types:
// - Volatile (vAMM): Standard x*y=k constant product AMM (identical to Uniswap V2)
// - Stable (sAMM): Stable swap using x³y+y³x=k formula with Newton-Raphson iteration
//
// Both pool types use the same Pool.sol contract but with different swap calculations.
// The pool type is determined by the `stable` boolean flag in the PoolCreated event.
//
// Architecture:
// - pool.rs: Unified pool implementation supporting both volatile and stable types
// - factory.rs: Pool factory configuration

pub mod factory;
pub mod pool;

#[cfg(test)]
mod test_sync_drift;

// Re-export main types
pub use factory::AerodromeV2FactoryConfig;
pub use pool::{AerodromeV2Factory, AerodromeV2Pool, IAerodromeV2Pool, IAerodromeV2Factory};
