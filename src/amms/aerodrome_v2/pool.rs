//! Aerodrome V2 Pool Implementation
//!
//! This module provides the core pool implementation for Aerodrome V2 AMM,
//! supporting both volatile and stable pool types through the same struct.
//!
//! # Pool Types
//!
//! - **Volatile** (`stable = false`): Standard `x * y = k` constant product AMM
//! - **Stable** (`stable = true`): Stable swap using `x³y + y³x = k` with Newton-Raphson iteration
//!
//! Both pool types use the same contract (`Pool.sol`) but different swap calculations.

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::{Filter, Log},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MPFR_T_PRECISION, MIN_POOL_RESERVE},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    Token,
};
use rug::Float;
use rug::ops::Pow;

// Import batch contract ABI
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetAerodromeV2PoolDataBatchRequest,
    "src/amms/abi/GetAerodromeV2PoolDataBatchRequest.json"
);

pub use IGetAerodromeV2PoolDataBatchRequest::IGetAerodromeV2PoolDataBatchRequestInstance;

sol! {
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAerodromeV2Pool {
        event Sync(uint256 reserve0, uint256 reserve1);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function metadata() external view returns (uint256 dec0, uint256 dec1, uint256 r0, uint256 r1, bool st, address t0, address t1);
        function stable() external view returns (bool);
        function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external returns (uint256, uint256);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAerodromeV2Factory {
        event PoolCreated(address indexed token0, address indexed token1, address pool, bool stable);
        function getFee(address pool, bool stable) external view returns (uint24);
    }
}

/// Aerodrome V2 Pool
///
/// This pool type supports both volatile and stable pools through the `stable` flag.
///
/// # Example
///
/// ```rust,no_run
/// use amms::aerodrome_v2::AerodromeV2Pool;
/// use alloy::primitives::address;
///
/// // Create a volatile pool
/// let volatile_pool = AerodromeV2Pool::new(address!("0x..."));
///
/// // Create a stable pool
/// let mut stable_pool = AerodromeV2Pool::new(address!("0x..."));
/// stable_pool.stable = true;
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AerodromeV2Pool {
    /// Pool address
    pub address: Address,
    /// Last synced block number
    #[serde(default)]
    pub last_synced_block: u64,
    /// Token A (token0)
    pub token_a: Token,
    /// Token B (token1)
    pub token_b: Token,
    /// Reserve of token0
    pub reserve_0: u128,
    /// Reserve of token1
    pub reserve_1: u128,
    /// Pool fee (fetch from factory dynamically)
    pub fee: u32,
    /// Stable flag - determines swap calculation method
    /// - false: Volatile pool (x * y = k)
    /// - true: Stable pool (x³y + y³x = k)
    pub stable: bool,
    /// Cached price of token A in terms of token B
    #[serde(default)]
    pub token_a_price: f64,
    /// Cached price of token B in terms of token A
    #[serde(default)]
    pub token_b_price: f64,
}

impl AerodromeV2Pool {
    /// Create a new Aerodrome V2 pool
    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    /// Create a new volatile pool
    pub fn new_volatile(address: Address) -> Self {
        Self {
            address,
            stable: false,
            ..Default::default()
        }
    }

    /// Create a new stable pool
    pub fn new_stable(address: Address) -> Self {
        Self {
            address,
            stable: true,
            ..Default::default()
        }
    }

    /// Calculates the amount received for a given `amount_in` `reserve_in` and `reserve_out`
    /// for volatile pools using the standard constant product formula.
    ///
    /// Matches Aerodrome Pool.sol implementation:
    /// ```solidity
    /// amountIn -= (amountIn * fee) / 10000;
    /// return (amountIn * reserveB) / (reserveA + amountIn);
    /// ```
    pub fn get_amount_out_volatile(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return U256::ZERO;
        }

        // Fee is in hundredths of a percent (base 10000)
        // Stable: 5 (0.05%), Volatile: 30 (0.3%)
        let fee_amount = (amount_in * U256::from(self.fee)) / U256::from(10000u64);

        // Deduct fee BEFORE swap calculation (matching Solidity)
        let amount_in_after_fee = amount_in - fee_amount;

        // Standard constant product formula: output = (input * reserveOut) / (reserveIn + input)
        (amount_in_after_fee * reserve_out) / (reserve_in + amount_in_after_fee)
    }

    /// Calculates the amount received for a given `amount_in` for stable pools
    /// using the Aerodrome/Solidly formula: `x³y + y³x = k`
    ///
    /// This matches the implementation in Aerodrome's Pool.sol contract.
    /// Uses f64 for calculations to avoid U256 overflow issues.
    ///
    /// Fee is deducted BEFORE calculation, matching Solidity:
    /// ```solidity
    /// amountIn -= (amountIn * fee) / 10000;
    /// ```
    pub fn get_amount_out_stable(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return U256::ZERO;
        }

        // Deduct fee BEFORE swap calculation (matching Solidity)
        // Fee is in hundredths of a percent (base 10000)
        let fee_amount = (amount_in * U256::from(self.fee)) / U256::from(10000u64);
        let amount_in_after_fee = amount_in - fee_amount;

        // Get decimals for both tokens
        let decimals0 = self.token_a.decimals as f64;
        let decimals1 = self.token_b.decimals as f64;

        // Convert to f64 for calculation
        let reserve_in_f = reserve_in.to::<u128>() as f64;
        let reserve_out_f = reserve_out.to::<u128>() as f64;
        let amount_in_f = amount_in_after_fee.to::<u128>() as f64;

        // Normalize to 18 decimals (same as Solidity)
        let precision = 1e18;
        let x = reserve_in_f * precision / decimals0;
        let y = reserve_out_f * precision / decimals1;

        // Calculate K = (x³y + y³x) / 10³⁶
        let xy = self.k_stable_f64(x, y);

        // Add amount_in and normalize
        let dx = amount_in_f * precision / decimals0;
        let x0 = x + dx;

        // Use Newton-Raphson iteration to find y_new such that: f(x0, y_new) = xy
        let y_new = self.get_y_stable_f64(x0, xy, y);

        // Calculate output amount
        let dy = y - y_new;
        if dy <= 0.0 {
            return U256::ZERO;
        }

        // Denormalize to output token decimals
        let y_out = dy * decimals1 / precision;

        // Convert back to U256
        if y_out < 0.0 || y_out >= (u128::MAX as f64) {
            return U256::ZERO;
        }

        U256::from(y_out as u128)
    }

    /// Calculate K = (x³y + y³x) / 10³⁶ for stable pools (f64 version)
    fn k_stable_f64(&self, x: f64, y: f64) -> f64 {
        let precision = 1e18;

        // _a = (x * y) / 1e18
        let a = (x * y) / precision;

        // _b = (x² / 1e18) + (y² / 1e18)
        let x_squared = (x * x) / precision;
        let y_squared = (y * y) / precision;
        let b = x_squared + y_squared;

        // K = (_a * _b) / 1e18 = (x³y + y³x) / 10³⁶
        (a * b) / precision
    }

    /// Calculate K = (x³y + y³x) / 10³⁶ for stable pools
    ///
    /// From Aerodrome Pool.sol _k function:
    /// ```solidity
    /// uint256 _x = (x * 1e18) / decimals0;
    /// uint256 _y = (y * 1e18) / decimals1;
    /// uint256 _a = (_x * _y) / 1e18;
    /// uint256 _b = ((_x * _x) / 1e18 + (_y * _y) / 1e18);
    /// return (_a * _b) / 1e18; // x3y+y3x >= k
    /// ```
    fn k_stable(&self, x: U256, y: U256, decimals0: U256, decimals1: U256) -> U256 {
        let precision = U256::from(1_000_000_000_000_000_000u64); // 1e18

        // Normalize to 18 decimals
        let x_norm = x * precision / decimals0;
        let y_norm = y * precision / decimals1;

        tracing::trace!("k_stable: x_norm={}, y_norm={}", x_norm, y_norm);

        // _a = (x * y) / 1e18
        let product = x_norm * y_norm;
        let a = product / precision;

        tracing::trace!("k_stable: product={}, a={}", product, a);

        // _b = (x² / 1e18) + (y² / 1e18)
        let x_squared = x_norm * x_norm / precision;
        let y_squared = y_norm * y_norm / precision;
        let b = x_squared + y_squared;

        tracing::trace!("k_stable: x_squared={}, y_squared={}, b={}", x_squared, y_squared, b);

        // K = (_a * _b) / 1e18 = (x³y + y³x) / 10³⁶
        let ab = a * b;
        let k = ab / precision;

        tracing::trace!("k_stable: ab={}, k={}", ab, k);

        k
    }

    /// Calculate y using Newton-Raphson iteration for stable swap (f64 version)
    fn get_y_stable_f64(&self, x0: f64, xy: f64, mut y: f64) -> f64 {
        let precision = 1e18;

        for _ in 0..255 {
            let k = self.f_stable_f64(x0, y);

            if k < xy {
                // Need to increase y
                let d = self.d_stable_f64(x0, y);
                if d == 0.0 {
                    return y;
                }
                let dy = ((xy - k) * precision) / d;
                if dy == 0.0 {
                    if (k - xy).abs() < 1.0 {
                        return y;
                    }
                    if self.f_stable_f64(x0, y + 1.0) > xy {
                        return y + 1.0;
                    }
                    y += 1.0;
                } else {
                    y += dy;
                }
            } else {
                // Need to decrease y
                let d = self.d_stable_f64(x0, y);
                if d == 0.0 {
                    return y;
                }
                let dy = ((k - xy) * precision) / d;
                if dy == 0.0 {
                    if (k - xy).abs() < 1.0 || self.f_stable_f64(x0, y - 1.0) < xy {
                        return y;
                    }
                    if y > 1.0 {
                        y -= 1.0;
                    }
                } else {
                    if y > dy {
                        y -= dy;
                    } else {
                        return y;
                    }
                }
            }
        }

        y
    }

    /// f(x, y) = (x³y + y³x) / 10³⁶ for Newton-Raphson iteration (f64 version)
    fn f_stable_f64(&self, x0: f64, y: f64) -> f64 {
        let precision = 1e18;

        let a = (x0 * y) / precision;
        let x0_squared = (x0 * x0) / precision;
        let y_squared = (y * y) / precision;
        let b = x0_squared + y_squared;

        (a * b) / precision
    }

    /// Derivative of f for Newton-Raphson: f'(x, y) = 3xy² + x³ (f64 version)
    fn d_stable_f64(&self, x0: f64, y: f64) -> f64 {
        let precision = 1e18;

        let y_squared = (y * y) / precision;
        let term1 = (3.0 * x0 * y_squared) / precision;

        let x0_squared = (x0 * x0) / precision;
        let term2 = (x0_squared * x0) / precision;

        term1 + term2
    }

    /// Calculate y using Newton-Raphson iteration for stable swap
    ///
    /// From Aerodrome Pool.sol _get_y function
    fn get_y_stable(&self, x0: U256, xy: U256, mut y: U256) -> U256 {
        let precision = U256::from(1_000_000_000_000_000_000u64); // 1e18

        for _ in 0..255 {
            let k = self.f_stable(x0, y);

            // Safety check: if k overflowed (is very large), return early
            if k > xy && k > U256::from(10).pow(U256::from(36).into()) {
                // Something went wrong, return a reasonable approximation
                return xy * y / (x0 + y);
            }

            if k < xy {
                // Need to increase y
                let d = self.d_stable(x0, y);
                if d.is_zero() {
                    return y;
                }
                let dy = ((xy - k) * precision) / d;
                let dy = if dy.is_zero() {
                    if k == xy {
                        return y;
                    }
                    if self.f_stable(x0, y + U256::from(1)) > xy {
                        return y + U256::from(1);
                    }
                    U256::from(1)
                } else {
                    dy
                };
                y = y.saturating_add(dy);
            } else {
                // Need to decrease y
                let d = self.d_stable(x0, y);
                if d.is_zero() {
                    return y;
                }
                let dy = ((k - xy) * precision) / d;
                let dy = if dy.is_zero() {
                    if k == xy || self.f_stable(x0, y.saturating_sub(U256::from(1))) < xy {
                        return y;
                    }
                    U256::from(1)
                } else {
                    dy
                };
                if y > dy {
                    y = y - dy;
                } else {
                    return y;
                }
            }
        }

        y
    }

    /// f(x, y) = (x³y + y³x) / 10³⁶ for Newton-Raphson iteration
    ///
    /// From Aerodrome Pool.sol _f function:
    /// ```solidity
    /// uint256 _a = (x0 * y) / 1e18;
    /// uint256 _b = ((x0 * x0) / 1e18 + (y * y) / 1e18);
    /// return (_a * _b) / 1e18;
    /// ```
    fn f_stable(&self, x0: U256, y: U256) -> U256 {
        let precision = U256::from(1_000_000_000_000_000_000u64); // 1e18

        // Check for overflow before multiplication
        // x0 * y can overflow if both are large
        let product = match x0.checked_mul(y) {
            Some(p) => p,
            None => return U256::MAX, // Signal overflow
        };
        let a = product / precision;

        // x0² can overflow
        let x0_squared = match x0.checked_mul(x0) {
            Some(s) => s / precision,
            None => return U256::MAX,
        };

        // y² can overflow
        let y_squared = match y.checked_mul(y) {
            Some(s) => s / precision,
            None => return U256::MAX,
        };

        let b = x0_squared + y_squared;

        // a * b can overflow
        match a.checked_mul(b) {
            Some(p) => p / precision,
            None => U256::MAX,
        }
    }

    /// Derivative of f for Newton-Raphson: f'(x, y) = 3xy² + x³
    ///
    /// From Aerodrome Pool.sol _d function:
    /// ```solidity
    /// return (3 * x0 * ((y * y) / 1e18)) / 1e18 + ((((x0 * x0) / 1e18) * x0) / 1e18);
    /// ```
    fn d_stable(&self, x0: U256, y: U256) -> U256 {
        let precision = U256::from(1_000_000_000_000_000_000u64); // 1e18

        let y_squared = y.checked_mul(y).map_or(U256::MAX, |s| s / precision);
        let term1 = U256::from(3u64) * x0;
        let term1 = term1.checked_mul(y_squared).map_or(U256::MAX, |t| t / precision);

        let x0_squared = x0.checked_mul(x0).map_or(U256::MAX, |s| s / precision);
        let term2 = x0_squared.checked_mul(x0).map_or(U256::MAX, |t| t / precision);

        term1.saturating_add(term2)
    }

    /// Get amount out based on pool type (volatile or stable)
    pub fn get_amount_out(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if self.stable {
            self.get_amount_out_stable(amount_in, reserve_in, reserve_out)
        } else {
            self.get_amount_out_volatile(amount_in, reserve_in, reserve_out)
        }
    }

    /// Generate calldata for a swap operation on this pool.
    ///
    /// # Arguments
    ///
    /// * `amount_0_out` - Amount of token0 to receive
    /// * `amount_1_out` - Amount of token1 to receive
    /// * `to` - Recipient address
    /// * `calldata` - Additional data for callback (e.g., flash loan)
    ///
    /// # Returns
    ///
    /// Encoded calldata for the swap function call
    pub fn swap_calldata(
        &self,
        amount_0_out: U256,
        amount_1_out: U256,
        to: Address,
        calldata: Vec<u8>,
    ) -> alloy::primitives::Bytes {
        // Use alloy's SolType to encode the swap call
        // function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data)
        let call = IAerodromeV2Pool::swapCall {
            amount0Out: amount_0_out,
            amount1Out: amount_1_out,
            to,
            data: calldata.into(),
        };
        call.abi_encode().into()
    }
}

// ============================================================================
// AutomatedMarketMaker Trait Implementation
// ============================================================================

impl AutomatedMarketMaker for AerodromeV2Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<alloy::primitives::FixedBytes<32>> {
        vec![IAerodromeV2Pool::Sync::SIGNATURE_HASH]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let signature = log.topics()[0];
        if signature == IAerodromeV2Pool::Sync::SIGNATURE_HASH {
            let sync_event = IAerodromeV2Pool::Sync::decode_log(&log.inner)?;

            let (reserve_0, reserve_1) = (
                sync_event.reserve0.to::<u128>(),
                sync_event.reserve1.to::<u128>(),
            );

            tracing::info!(
                target = "amms::aerodrome_v2::sync",
                block_number = ?log.block_number,
                address = ?self.address,
                stable = self.stable,
                reserve_0, reserve_1,
                "Sync"
            );

            self.reserve_0 = reserve_0;
            self.reserve_1 = reserve_1;

            // Update cached prices
            if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
                self.token_a_price = p;
                if p != 0.0 {
                    self.token_b_price = 1.0 / p;
                } else {
                    self.token_b_price = 0.0;
                }
            }
        }
        Ok(SyncAction::None)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.token_a.address == base_token {
            Ok(self.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            ))
        } else {
            Ok(self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            ))
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.token_a.address == base_token {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            );

            let amount_in_u128 = amount_in.try_into().map_err(|_| {
                AMMError::Msg("simulate_swap_mut: amount_in overflow to u128".to_string())
            })?;
            let amount_out_u128 = amount_out.try_into().map_err(|_| {
                AMMError::Msg("simulate_swap_mut: amount_out overflow to u128".to_string())
            })?;

            self.reserve_0 = self
                .reserve_0
                .checked_add(amount_in_u128)
                .ok_or(AMMError::Msg(
                    "simulate_swap_mut: reserve_0 overflow".to_string(),
                ))?;
            self.reserve_1 = self
                .reserve_1
                .checked_sub(amount_out_u128)
                .ok_or(AMMError::Msg(
                    "simulate_swap_mut: reserve_1 underflow".to_string(),
                ))?;

            Ok(amount_out)
        } else {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            );

            let amount_in_u128 = amount_in.try_into().map_err(|_| {
                AMMError::Msg("simulate_swap_mut: amount_in overflow to u128".to_string())
            })?;
            let amount_out_u128 = amount_out.try_into().map_err(|_| {
                AMMError::Msg("simulate_swap_mut: amount_out overflow to u128".to_string())
            })?;

            self.reserve_0 = self
                .reserve_0
                .checked_sub(amount_out_u128)
                .ok_or(AMMError::Msg(
                    "simulate_swap_mut: reserve_0 underflow".to_string(),
                ))?;
            self.reserve_1 = self
                .reserve_1
                .checked_add(amount_in_u128)
                .ok_or(AMMError::Msg(
                    "simulate_swap_mut: reserve_1 overflow".to_string(),
                ))?;

            Ok(amount_out)
        }
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.reserve_0 < MIN_POOL_RESERVE || self.reserve_1 < MIN_POOL_RESERVE {
            return Ok(0.0);
        }

        let r0_str = self.reserve_0.to_string();
        let r0_val = Float::parse_radix(&r0_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let r0 = Float::with_val(MPFR_T_PRECISION, r0_val);

        let r1_str = self.reserve_1.to_string();
        let r1_val = Float::parse_radix(&r1_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let r1 = Float::with_val(MPFR_T_PRECISION, r1_val);

        let shift = self.token_a.decimals as i32 - self.token_b.decimals as i32;
        let scale_factor = Float::with_val(MPFR_T_PRECISION, 10).pow(shift);

        let price_a: Float = (r1 / r0) * scale_factor;
        let price_a_f64 = price_a.to_f64();

        if base_token == self.token_a.address {
            Ok(price_a_f64)
        } else {
            if price_a_f64 == 0.0 {
                Ok(0.0)
            } else {
                Ok(1.0 / price_a_f64)
            }
        }
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        // Validate both tokens are in the pool
        let base_is_a = base_token == self.token_a.address;
        let base_is_b = base_token == self.token_b.address;
        let quote_is_a = quote_token == self.token_a.address;
        let quote_is_b = quote_token == self.token_b.address;

        if !base_is_a && !base_is_b {
            return Err(AMMError::TokenNotFound(base_token));
        }
        if !quote_is_a && !quote_is_b {
            return Err(AMMError::TokenNotFound(quote_token));
        }
        if base_token == quote_token {
            return Err(AMMError::Msg("base and quote tokens are the same".to_string()));
        }

        let price = if base_is_a {
            self.token_a_price
        } else {
            self.token_b_price
        };

        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
    }

    fn has_sufficient_liquidity(&self) -> bool {
        self.token_a.has_sufficient_liquidity(self.reserve_0)
            && self.token_b.has_sufficient_liquidity(self.reserve_1)
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.token_a.address {
            self.token_a.decimals
        } else if token == self.token_b.address {
            self.token_b.decimals
        } else {
            0
        }
    }

    /// Aerodrome V2 is only deployed on Base chain (chain ID: 8453)
    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![8453]) // Base mainnet
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool = IAerodromeV2Pool::new(self.address, provider.clone());

        // Fetch pool metadata to get stable flag
        let metadata = pool.metadata().call().block(block_number).await?;
        self.stable = metadata.st;

        // Fetch tokens (use block_number for token queries too)
        self.token_a = Token::new(pool.token0().call().block(block_number).await?, provider.clone()).await?;
        self.token_b = Token::new(pool.token1().call().block(block_number).await?, provider.clone()).await?;

        // Fetch reserves at the specified block
        let reserves = pool.getReserves().call().block(block_number).await?;
        self.reserve_0 = reserves.reserve0.to::<u128>();
        self.reserve_1 = reserves.reserve1.to::<u128>();

        // Set default fee for Aerodrome V2 (from PoolFactory.sol)
        // Fee is in hundredths of a percent (base 10000)
        // Volatile pools: 30 (0.3%)
        // Stable pools: 5 (0.05%)
        self.fee = if self.stable { 5 } else { 30 };

        tracing::trace!(
            target = "amms::aerodrome_v2::init",
            stable = self.stable,
            fee = self.fee,
            "Set pool fee"
        );

        // Update cached prices
        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            }
        }

        tracing::info!(
            target = "amms::aerodrome_v2::init",
            address = ?self.address,
            stable = self.stable,
            token_a = ?self.token_a.address,
            token_b = ?self.token_b.address,
            reserve_0 = self.reserve_0,
            reserve_1 = self.reserve_1,
            "Initialized Aerodrome V2 pool"
        );

        Ok(self)
    }
}

// ============================================================================
// Factory Implementation
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct AerodromeV2Factory {
    pub address: Address,
    pub creation_block: u64,
}

impl AerodromeV2Factory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
        }
    }
}

impl AutomatedMarketMakerFactory for AerodromeV2Factory {
    type PoolVariant = AerodromeV2Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> alloy::primitives::FixedBytes<32> {
        IAerodromeV2Factory::PoolCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let event = IAerodromeV2Factory::PoolCreated::decode_log(&log.inner)?;
        Ok(AMM::AerodromeV2Pool(AerodromeV2Pool {
            address: event.pool,
            token_a: event.token0.into(),
            token_b: event.token1.into(),
            stable: event.stable,
            ..Default::default()
        }))
    }
}

impl DiscoverySync for AerodromeV2Factory {
    fn discover<N, P>(
        &self,
        _to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        async move {
            use alloy::rpc::types::BlockNumberOrTag;
            let from_block: BlockNumberOrTag = self.creation_block.into();

            let filter = Filter::new()
                .address(self.address)
                .event_signature(IAerodromeV2Factory::PoolCreated::SIGNATURE_HASH)
                .from_block(from_block);

            let logs = provider.get_logs(&filter).await?;

            let pools: Vec<AMM> = logs
                .into_iter()
                .filter_map(|log| {
                    if let Ok(event) = IAerodromeV2Factory::PoolCreated::decode_log(&log.inner) {
                        Some(AMM::AerodromeV2Pool(AerodromeV2Pool {
                            address: event.pool,
                            token_a: event.token0.into(),
                            token_b: event.token1.into(),
                            stable: event.stable,
                            ..Default::default()
                        }))
                    } else {
                        None
                    }
                })
                .collect();

            tracing::info!(
                target = "amms::aerodrome_v2::discover",
                factory = ?self.address,
                pool_count = pools.len(),
                "Discovered Aerodrome V2 pools"
            );

            Ok(pools)
        }
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        async move {
            Self::sync_all_pools(amms, to_block, provider).await
        }
    }
}

impl AerodromeV2Factory {
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(amms);
        }

        let step = 80;

        let mut futures = FuturesUnordered::new();
        let pool_addresses: Vec<Vec<Address>> = amms
            .chunks(step)
            .map(|chunk| chunk.iter().map(|amm| amm.address()).collect())
            .collect();

        for group in pool_addresses {
            let provider = provider.clone();

            futures.push(async move {
                let result = IGetAerodromeV2PoolDataBatchRequestInstance::deploy_builder(provider, group.clone())
                    .call_raw()
                    .block(block_number)
                    .await?;

                Ok::<(Vec<Address>, alloy::primitives::Bytes), AMMError>((group, result))
            });
            sleep(Duration::from_millis(500)).await;
        }

        let mut amms_map: HashMap<Address, AMM> = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect();

        while let Some(res) = futures.next().await {
            let (group, return_data) = match res {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_v2::init_batch",
                        error = ?e,
                        "Batch contract call failed, skipping batch"
                    );
                    continue;
                }
            };

            tracing::debug!(
                target = "amms::aerodrome_v2::init_batch",
                return_data_len = return_data.len(),
                return_data_hex = ?return_data,
                "Raw batch return data"
            );

            let return_data = match <Vec<(Address, Address, u128, u128, u32, u32, bool)> as SolValue>::abi_decode(&return_data) {
                Ok(data) => data,
                Err(e) => {
                    tracing::error!(
                        target = "amms::aerodrome_v2::init_batch",
                        error = ?e,
                        return_data_len = return_data.len(),
                        "Failed to decode batch return data"
                    );
                    return Err(AMMError::from(e));
                }
            };

            tracing::debug!(
                target = "amms::aerodrome_v2::init_batch",
                group_len = group.len(),
                return_data_len = return_data.len(),
                "Batch data decoded"
            );

            for (idx, (pool_data, pool_address)) in return_data.iter().zip(group.iter()).enumerate() {
                if pool_data.0.is_zero() {
                    tracing::warn!(
                        target = "amms::aerodrome_v2::init_batch",
                        ?pool_address,
                        idx,
                        "Pool returned zero tokenA address"
                    );
                    continue;
                }

                if let Some(amm) = amms_map.get_mut(pool_address) {
                    let AMM::AerodromeV2Pool(pool) = amm else {
                        continue;
                    };

                    let (token_a, token_b, reserve_0, reserve_1, decimals_a, decimals_b, stable) = pool_data;

                    let decimals_a = *decimals_a as u8;
                    let decimals_b = *decimals_b as u8;
                    if decimals_a == 0 || decimals_b == 0 {
                        tracing::warn!(
                            target = "amms::aerodrome_v2::init_batch",
                            ?pool_address,
                            decimals_a,
                            decimals_b,
                            "Skipping pool with invalid decimals"
                        );
                        continue;
                    }

                    pool.token_a = Token::new_with_decimals(*token_a, decimals_a);
                    pool.token_b = Token::new_with_decimals(*token_b, decimals_b);
                    pool.reserve_0 = *reserve_0;
                    pool.reserve_1 = *reserve_1;
                    pool.stable = *stable;

                    if pool.fee == 0 {
                        pool.fee = 3000;
                    }

                    if let Ok(p) = pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                        pool.token_a_price = p;
                        pool.token_b_price = if p != 0.0 { 1.0 / p } else { 0.0 };
                    }

                    tracing::trace!(
                        target = "amms::aerodrome_v2::init_batch",
                        ?pool_address,
                        stable,
                        ?token_a,
                        ?token_b,
                        reserve_0,
                        reserve_1,
                        "Initialized pool"
                    );
                }
            }
        }

        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms_map
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (addr, _) in &invalid_amms {
                tracing::warn!(
                    target = "amms::aerodrome_v2::init_batch",
                    ?addr,
                    "Filtered out invalid pool"
                );
            }
        }

        tracing::info!(
            target = "amms::aerodrome_v2::init_batch",
            total = valid_amms.len() + invalid_amms.len(),
            valid = valid_amms.len(),
            invalid = invalid_amms.len(),
            "Batch initialization complete"
        );

        Ok(valid_amms.into_iter().map(|(_, amm)| amm).collect())
    }

    /// Batch sync Aerodrome V2 pools by fetching their current reserves.
    ///
    /// This method uses the batch contract to efficiently fetch reserves for multiple pools.
    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(amms);
        }

        let step = 80;

        let mut futures = FuturesUnordered::new();
        let pool_addresses: Vec<Vec<Address>> = amms
            .chunks(step)
            .map(|chunk| chunk.iter().map(|amm| amm.address()).collect())
            .collect();

        for group in pool_addresses {
            let provider = provider.clone();

            futures.push(async move {
                let result = IGetAerodromeV2PoolDataBatchRequestInstance::deploy_builder(provider, group.clone())
                    .call_raw()
                    .block(block_number)
                    .await?;

                Ok::<(Vec<Address>, alloy::primitives::Bytes), AMMError>((group, result))
            });
            sleep(Duration::from_millis(500)).await;
        }

        let mut amms_map: HashMap<Address, AMM> = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect();

        while let Some(res) = futures.next().await {
            let (group, return_data) = match res {
                Ok(data) => data,
                Err(e) => {
                    tracing::error!(
                        target = "amms::aerodrome_v2::sync_all_pools",
                        error = ?e,
                        "Batch contract call failed"
                    );
                    return Err(e);
                }
            };

            let return_data = match <Vec<(Address, Address, u128, u128, u32, u32, bool)> as SolValue>::abi_decode(&return_data) {
                Ok(data) => data,
                Err(e) => {
                    tracing::error!(
                        target = "amms::aerodrome_v2::sync_all_pools",
                        error = ?e,
                        return_data_len = return_data.len(),
                        "Failed to decode batch return data"
                    );
                    return Err(AMMError::from(e));
                }
            };

            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                if pool_data.0.is_zero() {
                    continue;
                }

                if let Some(amm) = amms_map.get_mut(pool_address) {
                    let AMM::AerodromeV2Pool(pool) = amm else {
                        continue;
                    };

                    let (_, _, reserve_0, reserve_1, _, _, stable) = pool_data;

                    pool.reserve_0 = *reserve_0;
                    pool.reserve_1 = *reserve_1;
                    pool.stable = *stable;

                    if let Ok(p) = pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                        pool.token_a_price = p;
                        pool.token_b_price = if p != 0.0 { 1.0 / p } else { 0.0 };
                    }
                }
            }
        }

        Ok(amms_map.into_iter().map(|(_, amm)| amm).collect())
    }
}
