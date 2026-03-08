// Aerodrome Slipstream (CL AMM) Implementation
//
// This module implements Aerodrome's Slipstream concentrated liquidity protocol,
// which is a Uniswap V3 fork with optimizations for the Base chain.
//
// Key differences from Uniswap V3:
// - Uses tickSpacing instead of fee for pool identification
// - Pools created via EIP-1167 deterministic clones
// - Dynamic fee via swap fee module
// - ve(3,3) tokenomics integration
//
// Architecture:
// - pool.rs: CL pool implementation with AutomatedMarketMaker trait
// - factory.rs: CL factory with custom tick spacing

pub mod factory;
pub mod pool;

#[cfg(test)]
mod test_sync_drift;

// Re-export main types
pub use factory::{AerodromeSlipstreamFactoryConfig, get_tick_spacing};
pub use pool::{AerodromeSlipstreamFactory, AerodromeSlipstreamPool, TickInfo, CurrentState, StepComputations, ICLPool, ICLPoolEvents, ICLPoolFactory, ICustomFeeModule};
