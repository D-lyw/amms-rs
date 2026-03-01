//! Aerodrome Slipstream Factory Implementation
//!
//! This module implements the factory contract for Aerodrome's CL pools,
//! which handles pool creation with custom tick spacing configurations.

use alloy::primitives::Address;

/// Aerodrome Slipstream Factory configuration
///
/// # Tick Spacing
///
/// Aerodrome uses 2x larger tick spacing than Uniswap V3:
/// - 0.01% fee: ~20 ticks (vs 10 in Uni V3)
/// - 0.3% fee: ~120 ticks (vs 60 in Uni V3)
/// - 1% fee: ~400 ticks (vs 200 in Uni V3)
///
/// # Example
///
/// ```rust,no_run
/// use amms::aerodrome_slipstream::factory::AerodromeSlipstreamFactoryConfig;
///
/// let config = AerodromeSlipstreamFactoryConfig {
///     address: "0x4200...".parse().unwrap(),
///     creation_block: 234567,
/// };
/// ```
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

/// Tick spacing configuration for different fee tiers
///
/// Aerodrome uses 2x larger tick spacing compared to Uniswap V3
pub const TICK_SPACING_0_01_PERCENT: i32 = 20;  // vs 10 in Uni V3
pub const TICK_SPACING_0_05_PERCENT: i32 = 100; // Aerodrome-specific
pub const TICK_SPACING_0_3_PERCENT: i32 = 120;  // vs 60 in Uni V3
pub const TICK_SPACING_1_PERCENT: i32 = 400;    // vs 200 in Uni V3

/// Get tick spacing for a given fee tier
///
/// # Arguments
///
/// * `fee` - Fee in hundredths of a bip (e.g., 3000 = 0.3%)
///
/// # Returns
///
/// The tick spacing for the given fee tier
pub fn get_tick_spacing(fee: u32) -> i32 {
    match fee {
        100 => TICK_SPACING_0_01_PERCENT,
        500 => TICK_SPACING_0_05_PERCENT,
        3000 => TICK_SPACING_0_3_PERCENT,
        10000 => TICK_SPACING_1_PERCENT,
        _ => TICK_SPACING_0_3_PERCENT, // Default
    }
}
