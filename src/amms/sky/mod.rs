//! SKY Protocol Converter AMM Implementation
//!
//! This module implements support for SKY protocol converters:
//! - DaiUsds: DAI ↔ USDS (1:1, no fees)
//! - LitePsm: DAI ↔ USDC (with tin/tout fees)
//! - LitePsmWrapper: USDS ↔ USDC (routes through LitePSM + DaiUsds)
//!
//! These converters use fixed exchange rates rather than AMM curves (x*y=k),
//! providing zero slippage for DaiUsds and predictable fees for PSM variants.

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::U256_10000,
    error::AMMError,
    float::q64_to_float,
    uniswap_v2::div_uu,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use thiserror::Error;
use tracing::info;

sol! {
    /// Interface for DaiUsds contract (DAI ↔ USDS)
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IDaiUsds {
        function dai() external view returns (address);
        function usds() external view returns (address);
    }

    /// Interface for LitePSM contract (DAI ↔ USDC)
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract ILitePsm {
        function dai() external view returns (address);
        function gem() external view returns (address);
        function tin() external view returns (uint256);
        function tout() external view returns (uint256);
        function to18ConversionFactor() external view returns (uint256);
    }

    /// Interface for LitePsmWrapper contract (USDS ↔ USDC)
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract ILitePsmWrapper {
        function usds() external view returns (address);
        function gem() external view returns (address);
        function psm() external view returns (address);
        function tin() external view returns (uint256);
        function tout() external view returns (uint256);
        function to18ConversionFactor() external view returns (uint256);
    }

    /// ERC20 interface for decimals
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IERC20 {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
    }
}

/// SKY converter type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SkyConverterType {
    /// DAI ↔ USDS (1:1, no fees)
    #[default]
    DaiUsds,
    /// DAI ↔ USDC (with tin/tout fees)
    LitePsm,
    /// USDS ↔ USDC (routes through LitePSM + DaiUsds)
    LitePsmWrapper,
}

#[derive(Error, Debug)]
pub enum SkyConverterError {
    #[error("Invalid converter type")]
    InvalidConverterType,
    #[error("Fee configuration error")]
    FeeConfigurationError,
    #[error("Token not in converter")]
    TokenNotInConverter,
    #[error("Initialization error: {0}")]
    InitializationError(String),
    #[error("Division by zero")]
    DivisionByZero,
}

/// SKY Protocol Converter
///
/// Represents a SKY protocol converter that provides fixed-rate token exchanges.
/// Unlike traditional AMMs, these converters use predetermined exchange rates
/// rather than bonding curves.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkyConverter {
    pub last_synced_block: u64,
    /// Converter contract address
    pub address: Address,
    /// Type of converter
    pub converter_type: SkyConverterType,

    // Token pair (token_0 is typically the "base" token)
    pub token_0: Address,
    pub token_1: Address,
    pub token_0_decimals: u8,
    pub token_1_decimals: u8,

    // Fees in basis points (0 for DaiUsds)
    // tin: fee for token_1 → token_0 (buying token_0)
    // tout: fee for token_0 → token_1 (selling token_0)
    pub tin: u32,
    pub tout: u32,

    // Cached prices
    pub token_0_price: f64,
    pub token_1_price: f64,
}

impl AutomatedMarketMaker for SkyConverter {
    fn address(&self) -> Address {
        self.address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        // SKY protocol is only on Ethereum mainnet
        Some(vec![1])
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        // SKY converters are stateless for DaiUsds
        // LitePSM may have fee update events, but we ignore them for simplicity
        vec![]
    }

    fn sync(&mut self, _log: &Log) -> Result<SyncAction, AMMError> {
        // SKY converters are stateless, no sync needed
        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_0, self.token_1]
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // SKY converters have unlimited liquidity by design
        true
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.token_0 {
            self.token_0_decimals
        } else if token == self.token_1 {
            self.token_1_decimals
        } else {
            0
        }
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        q64_to_float(self.calculate_price_64_x_64(base_token)?)
    }

    fn spot_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let price = if base_token == self.token_0 {
            self.token_0_price
        } else if base_token == self.token_1 {
            self.token_1_price
        } else {
            return Err(AMMError::TokenNotFound(base_token));
        };

        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        match self.converter_type {
            SkyConverterType::DaiUsds => {
                // Fixed 1:1 conversion, DAI and USDS both have 18 decimals
                Ok(amount_in)
            }

            SkyConverterType::LitePsm => {
                // token_0 = DAI (18 decimals), token_1 = USDC (6 decimals)
                if base_token == self.token_0 {
                    // DAI → USDC: apply tout (sell DAI fee) + decimal conversion (18 → 6)
                    let amount_after_fee = amount_in
                        .checked_mul(U256::from(10000 - self.tout))
                        .ok_or(AMMError::ArithmeticError)?
                        / U256_10000;
                    let amount_out = amount_after_fee
                        .checked_div(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_out)
                } else {
                    // USDC → DAI: apply tin (buy DAI fee) + decimal conversion (6 → 18)
                    let amount_after_fee = amount_in
                        .checked_mul(U256::from(10000 - self.tin))
                        .ok_or(AMMError::ArithmeticError)?
                        / U256_10000;
                    let amount_out = amount_after_fee
                        .checked_mul(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_out)
                }
            }

            SkyConverterType::LitePsmWrapper => {
                // token_0 = USDS (18 decimals), token_1 = USDC (6 decimals)
                // Internally routes through DaiUsds + LitePSM
                if base_token == self.token_0 {
                    // USDS → USDC: 1:1 to DAI, then DAI → USDC with tout
                    let amount_after_fee = amount_in
                        .checked_mul(U256::from(10000 - self.tout))
                        .ok_or(AMMError::ArithmeticError)?
                        / U256_10000;
                    let amount_out = amount_after_fee
                        .checked_div(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_out)
                } else {
                    // USDC → USDS: USDC → DAI with tin, then 1:1 to USDS
                    let amount_after_fee = amount_in
                        .checked_mul(U256::from(10000 - self.tin))
                        .ok_or(AMMError::ArithmeticError)?
                        / U256_10000;
                    let amount_out = amount_after_fee
                        .checked_mul(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_out)
                }
            }
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        // SKY converters are stateless, so simulate_swap_mut is same as simulate_swap
        self.simulate_swap(base_token, quote_token, amount_in)
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }

        match self.converter_type {
            SkyConverterType::DaiUsds => {
                // Fixed 1:1 conversion, reverse is also 1:1
                Ok(amount_out)
            }

            SkyConverterType::LitePsm => {
                // Reverse the fee calculation
                if base_token == self.token_0 {
                    // DAI → USDC: need to reverse the tout fee
                    // amount_out = amount_in * (10000 - tout) / 10000 / 10^12
                    // amount_in = amount_out * 10^12 * 10000 / (10000 - tout)
                    if self.tout >= 10000 {
                        return Err(AMMError::Msg("Fee too high".into()));
                    }
                    let fee_factor = 10000 - self.tout;
                    let amount_in = amount_out
                        .checked_mul(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_mul(U256_10000)
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_div(U256::from(fee_factor))
                        .ok_or(AMMError::ArithmeticError)?;
                    // Round up to ensure we get at least amount_out
                    Ok(amount_in + U256::from(1u64))
                } else {
                    // USDC → DAI: need to reverse the tin fee
                    if self.tin >= 10000 {
                        return Err(AMMError::Msg("Fee too high".into()));
                    }
                    let fee_factor = 10000 - self.tin;
                    let amount_in = amount_out
                        .checked_div(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_mul(U256_10000)
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_div(U256::from(fee_factor))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_in + U256::from(1u64))
                }
            }

            SkyConverterType::LitePsmWrapper => {
                // Same logic as LitePsm (just different token pair)
                if base_token == self.token_0 {
                    // USDS → USDC
                    if self.tout >= 10000 {
                        return Err(AMMError::Msg("Fee too high".into()));
                    }
                    let fee_factor = 10000 - self.tout;
                    let amount_in = amount_out
                        .checked_mul(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_mul(U256_10000)
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_div(U256::from(fee_factor))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_in + U256::from(1u64))
                } else {
                    // USDC → USDS
                    if self.tin >= 10000 {
                        return Err(AMMError::Msg("Fee too high".into()));
                    }
                    let fee_factor = 10000 - self.tin;
                    let amount_in = amount_out
                        .checked_div(U256::from(10u64.pow(12)))
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_mul(U256_10000)
                        .ok_or(AMMError::ArithmeticError)?
                        .checked_div(U256::from(fee_factor))
                        .ok_or(AMMError::ArithmeticError)?;
                    Ok(amount_in + U256::from(1u64))
                }
            }
        }
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        match self.converter_type {
            SkyConverterType::DaiUsds => {
                let contract = IDaiUsds::new(self.address, provider);
                let dai = contract.dai().block(block_number).call().await?;
                let usds = contract.usds().block(block_number).call().await?;

                let dai_addr: Address = dai;
                let usds_addr: Address = usds;

                // Get decimals
                let dai_contract = IERC20::new(dai_addr, contract.provider());
                let usds_contract = IERC20::new(usds_addr, contract.provider());

                let dai_decimals = dai_contract.decimals().block(block_number).call().await?;
                let usds_decimals = usds_contract.decimals().block(block_number).call().await?;

                self.token_0 = dai_addr;
                self.token_1 = usds_addr;
                self.token_0_decimals = dai_decimals;
                self.token_1_decimals = usds_decimals;
                self.tin = 0;
                self.tout = 0;
            }

            SkyConverterType::LitePsm => {
                let contract = ILitePsm::new(self.address, provider);
                let dai = contract.dai().block(block_number).call().await?;
                let gem = contract.gem().block(block_number).call().await?;
                let tin = contract.tin().block(block_number).call().await?;
                let tout = contract.tout().block(block_number).call().await?;

                let dai_addr: Address = dai;
                let gem_addr: Address = gem;
                let tin_val: U256 = tin;
                let tout_val: U256 = tout;

                // Get decimals
                let dai_contract = IERC20::new(dai_addr, contract.provider());
                let gem_contract = IERC20::new(gem_addr, contract.provider());

                let dai_decimals = dai_contract.decimals().block(block_number).call().await?;
                let gem_decimals = gem_contract.decimals().block(block_number).call().await?;

                self.token_0 = dai_addr;
                self.token_1 = gem_addr;
                self.token_0_decimals = dai_decimals;
                self.token_1_decimals = gem_decimals;
                self.tin = tin_val.to();
                self.tout = tout_val.to();
            }

            SkyConverterType::LitePsmWrapper => {
                let contract = ILitePsmWrapper::new(self.address, provider);
                let usds = contract.usds().block(block_number).call().await?;
                let gem = contract.gem().block(block_number).call().await?;
                let tin = contract.tin().block(block_number).call().await?;
                let tout = contract.tout().block(block_number).call().await?;

                let usds_addr: Address = usds;
                let gem_addr: Address = gem;
                let tin_val: U256 = tin;
                let tout_val: U256 = tout;

                // Get decimals
                let usds_contract = IERC20::new(usds_addr, contract.provider());
                let gem_contract = IERC20::new(gem_addr, contract.provider());

                let usds_decimals = usds_contract.decimals().block(block_number).call().await?;
                let gem_decimals = gem_contract.decimals().block(block_number).call().await?;

                self.token_0 = usds_addr;
                self.token_1 = gem_addr;
                self.token_0_decimals = usds_decimals;
                self.token_1_decimals = gem_decimals;
                self.tin = tin_val.to();
                self.tout = tout_val.to();
            }
        }

        // Calculate initial prices
        self.token_0_price = self.calculate_price(self.token_0, self.token_1)?;
        self.token_1_price = self.calculate_price(self.token_1, self.token_0)?;

        info!(
            target: "amms::sky::init",
            address = ?self.address,
            converter_type = ?self.converter_type,
            token_0 = ?self.token_0,
            token_1 = ?self.token_1,
            tin = self.tin,
            tout = self.tout,
            "SKY converter initialized"
        );

        Ok(self)
    }
}

impl SkyConverter {
    /// Creates a new SKY converter with the given address and type
    pub fn new(address: Address, converter_type: SkyConverterType) -> Self {
        Self {
            address,
            converter_type,
            ..Default::default()
        }
    }

    /// Creates a DaiUsds converter (DAI ↔ USDS)
    pub fn new_dai_usds(address: Address) -> Self {
        Self::new(address, SkyConverterType::DaiUsds)
    }

    /// Creates a LitePSM converter (DAI ↔ USDC)
    pub fn new_lite_psm(address: Address) -> Self {
        Self::new(address, SkyConverterType::LitePsm)
    }

    /// Creates a LitePsmWrapper converter (USDS ↔ USDC)
    pub fn new_lite_psm_wrapper(address: Address) -> Self {
        Self::new(address, SkyConverterType::LitePsmWrapper)
    }

    /// Calculates price in Q64.64 fixed-point format
    pub fn calculate_price_64_x_64(&self, base_token: Address) -> Result<u128, AMMError> {
        let decimal_shift = self.token_0_decimals as i8 - self.token_1_decimals as i8;

        // For fixed-rate converters, price is essentially 1 (adjusted for decimals)
        // But we need to account for fees

        // Calculate effective exchange rate
        let (effective_rate, decimal_adjustment) = if base_token == self.token_0 {
            // Selling token_0 for token_1: apply tout fee
            // Rate = (10000 - tout) / 10000, adjusted for decimals
            let rate: U256 = if self.tout == 0 {
                U256::from(1u64) << 64 // 1.0 in Q64
            } else {
                (U256::from(10000 - self.tout) << 64) / U256_10000
            };
            (rate, decimal_shift)
        } else if base_token == self.token_1 {
            // Selling token_1 for token_0: apply tin fee
            let rate = if self.tin == 0 {
                U256::from(1u64) << 64 // 1.0 in Q64
            } else {
                (U256::from(10000 - self.tin) << 64) / U256_10000
            };
            (rate, -decimal_shift)
        } else {
            return Err(AMMError::TokenNotFound(base_token));
        };

        // Apply decimal adjustment
        let adjusted_rate = match decimal_adjustment.cmp(&0) {
            Ordering::Less => {
                let shift = decimal_adjustment.unsigned_abs() as u32;
                if shift > 64 {
                    effective_rate / U256::from(10u64.pow(shift))
                } else {
                    effective_rate >> shift
                }
            }
            Ordering::Greater => {
                let shift = decimal_adjustment as u32;
                if shift > 64 {
                    effective_rate * U256::from(10u64.pow(shift))
                } else {
                    effective_rate << shift
                }
            }
            Ordering::Equal => effective_rate,
        };

        Ok(adjusted_rate.to())
    }

    /// Batch initialization for multiple SKY converters
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let total = amms.len();
        let mut initialized = Vec::with_capacity(total);

        for amm in amms {
            match amm {
                AMM::SkyConverter(converter) => {
                    let addr = converter.address;
                    match converter.init(block_number, provider.clone()).await {
                        Ok(init_converter) => {
                            initialized.push(AMM::SkyConverter(init_converter));
                        }
                        Err(e) => {
                            info!(
                                target: "amms::sky::init_batch",
                                address = ?addr,
                                error = ?e,
                                "Failed to initialize SKY converter"
                            );
                        }
                    }
                }
                _ => {
                    info!(
                        target: "amms::sky::init_batch",
                        "Non-SKY converter in batch, skipping"
                    );
                }
            }
        }

        let valid = initialized.len();
        let invalid = total - valid;
        info!(
            target: "amms::sky::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(initialized)
    }

    /// Returns true if this converter involves the given token
    pub fn has_token(&self, token: Address) -> bool {
        self.token_0 == token || self.token_1 == token
    }

    /// Returns the other token in the pair
    pub fn get_other_token(&self, token: Address) -> Option<Address> {
        if token == self.token_0 {
            Some(self.token_1)
        } else if token == self.token_1 {
            Some(self.token_0)
        } else {
            None
        }
    }
}

/// Known SKY converter addresses on Ethereum mainnet
pub mod addresses {
    use alloy::primitives::address;

    /// DaiUsds: DAI ↔ USDS converter
    pub const DAI_USDS: alloy::primitives::Address =
        address!("3225737a9Bbb6473CB4a45b7244ACa2BeFdB276A");

    /// LitePSM: DAI ↔ USDC converter
    pub const LITE_PSM: alloy::primitives::Address =
        address!("f6e72db5454dd049d0788e411b06cfaf16853042");

    /// LitePsmWrapper: USDS ↔ USDC converter
    pub const LITE_PSM_WRAPPER: alloy::primitives::Address =
        address!("70254BD530684CF4a6323F51098FA39AAE6130b6");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dai_usds_simulation() {
        let mut converter = SkyConverter::new_dai_usds(addresses::DAI_USDS);
        converter.token_0 = "0x6B175474E89094C44Da98b954EedeAC495271d0F".parse().unwrap(); // DAI
        converter.token_1 = "0xdC035Df69075f5b12d8F7Bbd66d0Df2C27eaB2CE".parse().unwrap(); // USDS
        converter.token_0_decimals = 18;
        converter.token_1_decimals = 18;

        // Test 1:1 conversion
        let amount_in = U256::from(1_000_000_000_000_000_000u64); // 1 token
        let amount_out = converter
            .simulate_swap(converter.token_0, converter.token_1, amount_in)
            .unwrap();
        assert_eq!(amount_out, amount_in);
    }

    #[test]
    fn test_lite_psm_simulation() {
        let mut converter = SkyConverter::new_lite_psm(addresses::LITE_PSM);
        converter.token_0 = "0x6B175474E89094C44Da98b954EedeAC495271d0F".parse().unwrap(); // DAI
        converter.token_1 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap(); // USDC
        converter.token_0_decimals = 18;
        converter.token_1_decimals = 6;
        converter.tin = 10; // 0.1% fee
        converter.tout = 10; // 0.1% fee

        // Test DAI → USDC
        let amount_in = U256::from(1_000_000_000_000_000_000u64); // 1 DAI (18 decimals)
        let amount_out = converter
            .simulate_swap(converter.token_0, converter.token_1, amount_in)
            .unwrap();
        // Expected: 1 DAI * (10000 - 10) / 10000 / 10^12 = 999900 USDC (6 decimals)
        let expected = U256::from(999900u64);
        assert_eq!(amount_out, expected);

        // Test USDC → DAI
        let amount_in = U256::from(1_000_000u64); // 1 USDC (6 decimals)
        let amount_out = converter
            .simulate_swap(converter.token_1, converter.token_0, amount_in)
            .unwrap();
        // Expected: 1 USDC * (10000 - 10) / 10000 * 10^12 = 999900000000000000 DAI (18 decimals)
        let expected = U256::from(999900000000000000u64);
        assert_eq!(amount_out, expected);
    }

    #[test]
    fn test_zero_amount() {
        let converter = SkyConverter::new_dai_usds(addresses::DAI_USDS);
        let amount_out = converter
            .simulate_swap(Address::ZERO, Address::ZERO, U256::ZERO)
            .unwrap();
        assert!(amount_out.is_zero());
    }
}
