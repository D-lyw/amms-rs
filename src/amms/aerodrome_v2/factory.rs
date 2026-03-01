//! Aerodrome V2 Factory Implementation
//!
//! This module implements the factory contract interface for Aerodrome V2 AMM,
//! which handles pool creation and fee configuration.

use alloy::primitives::Address;

/// Aerodrome V2 Factory configuration
///
/// # Example
///
/// ```rust,no_run
/// use amms::aerodrome_v2::factory::AerodromeV2FactoryConfig;
///
/// let config = AerodromeV2FactoryConfig {
///     address: "0x4200...".parse().unwrap(),
///     creation_block: 123456,
/// };
/// ```
#[derive(Debug, Clone)]
pub struct AerodromeV2FactoryConfig {
    /// Factory contract address
    pub address: Address,
    /// Block number when factory was deployed
    pub creation_block: u64,
}

impl AerodromeV2FactoryConfig {
    /// Create a new factory configuration
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
        }
    }
}
