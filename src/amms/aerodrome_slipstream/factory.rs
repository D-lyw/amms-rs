//! Aerodrome Slipstream Factory Implementation
//!
//! This module implements the factory contract for Aerodrome's CL pools,
//! which handles pool creation with custom tick spacing configurations.
//!
//! # Fee Mechanism
//!
//! Aerodrome Slipstream uses **tickSpacing** (not fee) to identify pools.
//! The fee can be dynamic and may differ from the default values.
//!
//! ## Default Fee Tiers (from Factory contract)
//!
//! | tickSpacing | Default Fee | Rate    |
//! |-------------|-------------|---------|
//! | 1           | 100         | 0.01%   |
//! | 10          | 500         | 0.05%   |
//! | 50          | 500         | 0.05%   |
//! | 100         | 500         | 0.05%   |
//! | 200         | 3000        | 0.3%    |
//! | 2000        | 10000       | 1%      |
//!
//! ## Getting Actual Fee
//!
//! Always use `pool.fee()` or `factory.getSwapFee(pool)` to get the actual
//! fee, as it may differ from the default due to dynamic fee mechanism.

use alloy::primitives::Address;

/// Aerodrome Slipstream Factory configuration
#[derive(Debug, Clone)]
pub struct AerodromeSlipstreamFactoryConfig {
    /// Factory contract address
    pub address: Address,
    /// Block number when factory was deployed
    pub creation_block: u64,
}

impl AerodromeSlipstreamFactoryConfig {
    /// Create a new factory configuration
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
        }
    }
}

/// Tick spacing constants for Aerodrome Slipstream
///
/// These are the enabled tick spacings on the Factory contract.
/// Note: The fee associated with each tickSpacing can be dynamic.
pub const TICK_SPACING_1: i32 = 1; // 0.01% default
pub const TICK_SPACING_10: i32 = 10; // 0.05% default
pub const TICK_SPACING_50: i32 = 50; // 0.05% default
pub const TICK_SPACING_100: i32 = 100; // 0.05% default
pub const TICK_SPACING_200: i32 = 200; // 0.3% default
pub const TICK_SPACING_2000: i32 = 2000; // 1% default

/// Legacy constants for backwards compatibility
#[deprecated(note = "Use TICK_SPACING_1 instead")]
pub const TICK_SPACING_0_01_PERCENT: i32 = TICK_SPACING_1;
#[deprecated(note = "Use TICK_SPACING_100 instead")]
pub const TICK_SPACING_0_05_PERCENT: i32 = TICK_SPACING_100;
#[deprecated(note = "Use TICK_SPACING_200 instead")]
pub const TICK_SPACING_0_3_PERCENT: i32 = TICK_SPACING_200;
#[deprecated(note = "Use TICK_SPACING_2000 instead")]
pub const TICK_SPACING_1_PERCENT: i32 = TICK_SPACING_2000;

/// Get the default fee for a given tick spacing.
///
/// # Important
///
/// This returns the **default** fee, not the actual fee.
/// The actual fee may differ due to dynamic fee mechanism.
/// Use `pool.fee()` or `factory.getSwapFee(pool)` for the actual fee.
///
/// # Arguments
///
/// * `tick_spacing` - The tick spacing of the pool
///
/// # Returns
///
/// The default fee in pips (1e-6), e.g., 500 = 0.05%
pub fn get_default_fee(tick_spacing: i32) -> u32 {
    match tick_spacing {
        1 => 100,      // 0.01%
        10 => 500,     // 0.05%
        50 => 500,     // 0.05%
        100 => 500,    // 0.05%
        200 => 3000,   // 0.3%
        2000 => 10000, // 1%
        _ => 500,      // Default to 0.05%
    }
}

/// Get tick spacing for a given default fee tier.
///
/// # Note
///
/// Multiple tick spacings may have the same default fee.
/// This returns the most common tick spacing for the given fee.
///
/// # Arguments
///
/// * `fee` - Fee in pips (1e-6), e.g., 3000 = 0.3%
///
/// # Returns
///
/// The tick spacing for the given fee tier
pub fn get_tick_spacing(fee: u32) -> i32 {
    match fee {
        100 => TICK_SPACING_1,
        500 => TICK_SPACING_100, // Most common 0.05% tier
        3000 => TICK_SPACING_200,
        10000 => TICK_SPACING_2000,
        _ => TICK_SPACING_100, // Default
    }
}
