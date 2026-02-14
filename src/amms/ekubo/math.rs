//! Ekubo V2 Math Module
//!
//! Implements accurate swap math matching on-chain Ekubo V2 calculations.
//! Reference: https://github.com/EkuboProtocol/rust-sdk/tree/main/src/math
//!
//! Key differences from Uniswap V3:
//! - SQRT_RATIO_ONE = 2^128 (vs 2^96 in V3)
//! - Amounts are i128 (vs int256 in V3)
//! - Fee denominator is 2^64 (vs 1_000_000 in V3)

use alloy::primitives::U256;
use thiserror::Error;

// ========== Constants ==========

/// Ekubo's sqrt ratio fixed point representation
/// Per Ekubo docs: sqrt_ratio is a 64.128 fixed-point number
/// price = (sqrt_ratio / 2^128)^2
pub const SQRT_RATIO_ONE: U256 = U256::from_limbs([0, 0, 1, 0]); // 2^128

/// Fee denominator for EVM: 2^64
pub const FEE_DENOMINATOR: u128 = 1u128 << 64;

/// Minimum valid sqrt ratio
pub const MIN_SQRT_RATIO: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Maximum valid sqrt ratio (uint128 max for 64.128 format)
pub const MAX_SQRT_RATIO: U256 = U256::from_limbs([0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF, 0, 0]); // 2^128 - 1

// ========== Error Types ==========

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MathError {
    #[error("division by zero")]
    DivisionByZero,
    #[error("overflow")]
    Overflow,
    #[error("underflow")]
    Underflow,
    #[error("no liquidity")]
    NoLiquidity,
    #[error("zero ratio")]
    ZeroRatio,
    #[error("wrong direction")]
    WrongDirection,
    #[error("amount before fee overflow")]
    AmountBeforeFeeOverflow,
    #[error("signed integer overflow")]
    SignedIntegerOverflow,
}

// ========== Swap Result ==========

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStepResult {
    pub consumed_amount: i128,
    pub calculated_amount: u128,
    pub sqrt_ratio_next: U256,
    pub fee_amount: u128,
}

// ========== Core Math Functions ==========

/// High precision multiplication and division: (x * y) / d
/// Uses 512-bit intermediate arithmetic for overflow protection
pub fn muldiv(x: U256, y: U256, d: U256, round_up: bool) -> Result<U256, MathError> {
    if d.is_zero() {
        return Err(MathError::DivisionByZero);
    }

    // For simple case (no overflow), use direct multiplication
    let (product, overflow) = x.overflowing_mul(y);
    if !overflow {
        let (quotient, remainder) = product.div_rem(d);
        return Ok(if round_up && !remainder.is_zero() {
            quotient
                .checked_add(U256::from(1u64))
                .ok_or(MathError::Overflow)?
        } else {
            quotient
        });
    }

    // For overflow case, compute 512-bit product manually
    let (prod_low, prod_high) = widening_mul_256(x, y);

    // Check for overflow before division: if prod_high >= d, result will overflow U256
    if prod_high >= d {
        return Err(MathError::Overflow);
    }

    // 512-bit division: (prod_high * 2^256 + prod_low) / d
    let result = div_512_by_256(prod_low, prod_high, d, round_up)?;
    Ok(result)
}

/// Manual 512-bit multiplication returning (low_256, high_256)
fn widening_mul_256(x: U256, y: U256) -> (U256, U256) {
    // Split into 128-bit halves
    let x_lo: U256 = x & U256::from(u128::MAX);
    let x_hi: U256 = x >> 128usize;
    let y_lo: U256 = y & U256::from(u128::MAX);
    let y_hi: U256 = y >> 128usize;

    // Compute partial products
    let p0: U256 = x_lo * y_lo; // bits 0-255
    let p1a: U256 = x_hi * y_lo; // shifted by 128
    let p1b: U256 = x_lo * y_hi; // shifted by 128
    let p2: U256 = x_hi * y_hi; // shifted by 256

    // Combine middle terms
    let (p1, carry1) = p1a.overflowing_add(p1b);
    let p1_lo: U256 = p1 << 128usize;
    let p1_hi: U256 = p1 >> 128usize;

    // Form low 256 bits
    let (low, carry2) = p0.overflowing_add(p1_lo);

    // Form high 256 bits
    let mut high = p2 + p1_hi;
    if carry2 {
        high = high + U256::from(1u64);
    }
    if carry1 {
        high = high + (U256::from(1u64) << 128usize);
    }

    (low, high)
}

/// Divide a 512-bit number (low, high) by a 256-bit divisor
/// Returns the 256-bit quotient, errors if it would overflow
fn div_512_by_256(low: U256, high: U256, d: U256, round_up: bool) -> Result<U256, MathError> {
    if d.is_zero() {
        return Err(MathError::DivisionByZero);
    }

    // Use ruint's Uint type for 512-bit arithmetic
    type U512 = ruint::Uint<512, 8>;

    // Form the 512-bit numerator: high * 2^256 + low
    let low_512 = U512::from(low);
    let high_512 = U512::from(high) << 256;
    let num_512: U512 = high_512 + low_512;

    // Divisor as U512
    let d_512 = U512::from(d);

    // Perform division
    let (quotient_512, remainder_512) = num_512.div_rem(d_512);

    // Check for overflow (quotient doesn't fit in U256)
    if quotient_512 > U512::from(U256::MAX) {
        return Err(MathError::Overflow);
    }

    // Convert quotient to U256
    let quotient_bytes = quotient_512.to_le_bytes::<64>();
    let quotient = U256::from_le_slice(&quotient_bytes[..32]);

    // Apply rounding if needed
    if round_up && !remainder_512.is_zero() {
        quotient
            .checked_add(U256::from(1u64))
            .ok_or(MathError::Overflow)
    } else {
        Ok(quotient)
    }
}

/// Calculate token0 delta between two sqrt ratios
/// amount0 = L * (sqrt_upper - sqrt_lower) / (sqrt_upper * sqrt_lower)
pub fn amount0_delta(
    sqrt_ratio_a: U256,
    sqrt_ratio_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<u128, MathError> {
    let (lower, upper) = sort_ratios(sqrt_ratio_a, sqrt_ratio_b)?;

    if liquidity == 0 || lower == upper {
        return Ok(0);
    }

    // numerator = (upper - lower) * liquidity * 2^128 (64.128 fixed-point)
    let liquidity_shifted = U256::from(liquidity) << 128usize;
    let diff = upper - lower;

    // result_0 = (diff * liquidity_shifted) / upper
    let result_0 = muldiv(diff, liquidity_shifted, upper, round_up)?;

    // result = result_0 / lower
    let (result, remainder) = result_0.div_rem(lower);
    let rounded = if round_up && !remainder.is_zero() {
        result
            .checked_add(U256::from(1u64))
            .ok_or(MathError::Overflow)?
    } else {
        result
    };

    // Convert to u128
    if rounded > U256::from(u128::MAX) {
        return Err(MathError::Overflow);
    }
    Ok(rounded.to::<u128>())
}

/// Calculate token1 delta between two sqrt ratios
/// amount1 = L * (sqrt_upper - sqrt_lower) / SQRT_RATIO_ONE
pub fn amount1_delta(
    sqrt_ratio_a: U256,
    sqrt_ratio_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<u128, MathError> {
    let (lower, upper) = sort_ratios(sqrt_ratio_a, sqrt_ratio_b)?;

    if liquidity == 0 || lower == upper {
        return Ok(0);
    }

    let diff = upper - lower;
    let result = muldiv(U256::from(liquidity), diff, SQRT_RATIO_ONE, round_up)?;

    // Convert to u128
    if result > U256::from(u128::MAX) {
        return Err(MathError::Overflow);
    }
    Ok(result.to::<u128>())
}

/// Compute next sqrt ratio given amount0 change
pub fn next_sqrt_ratio_from_amount0(
    sqrt_ratio: U256,
    liquidity: u128,
    amount0: i128,
) -> Result<U256, MathError> {
    if amount0 == 0 {
        return Ok(sqrt_ratio);
    }

    if liquidity == 0 {
        return Err(MathError::NoLiquidity);
    }

    let numerator1: U256 = U256::from(liquidity) << 128usize;

    if amount0 < 0 {
        // Removing token0 (price increases)
        let amount0_abs = U256::from(amount0.unsigned_abs());

        let product = amount0_abs
            .checked_mul(sqrt_ratio)
            .ok_or(MathError::Overflow)?;

        let denominator = numerator1.checked_sub(product).ok_or(MathError::Overflow)?;

        if denominator.is_zero() {
            return Err(MathError::DivisionByZero);
        }

        muldiv(numerator1, sqrt_ratio, denominator, true)
    } else {
        // Adding token0 (price decreases)
        let amount0_u256 = U256::from(amount0 as u128);

        if sqrt_ratio.is_zero() {
            return Err(MathError::ZeroRatio);
        }

        let denom_p1 = numerator1 / sqrt_ratio;

        let denom = denom_p1
            .checked_add(amount0_u256)
            .ok_or(MathError::Overflow)?;

        if denom.is_zero() {
            return Err(MathError::DivisionByZero);
        }

        muldiv(numerator1, U256::from(1u64), denom, true)
    }
}

/// Compute next sqrt ratio given amount1 change
pub fn next_sqrt_ratio_from_amount1(
    sqrt_ratio: U256,
    liquidity: u128,
    amount1: i128,
) -> Result<U256, MathError> {
    if amount1 == 0 {
        return Ok(sqrt_ratio);
    }

    if liquidity == 0 {
        return Err(MathError::NoLiquidity);
    }

    let amount1_abs = U256::from(amount1.unsigned_abs());
    let round_up = amount1 < 0;

    let quotient = muldiv(amount1_abs, SQRT_RATIO_ONE, U256::from(liquidity), round_up)?;

    if amount1 < 0 {
        sqrt_ratio.checked_sub(quotient).ok_or(MathError::Underflow)
    } else {
        sqrt_ratio.checked_add(quotient).ok_or(MathError::Overflow)
    }
}

// ========== Swap Step Calculation ==========

/// Check if price is increasing based on swap direction
#[inline]
pub fn is_price_increasing(amount: i128, is_token1: bool) -> bool {
    (amount < 0) != is_token1
}

/// Compute fee from amount
pub fn compute_fee(amount: u128, fee: u64) -> u128 {
    let num = U256::from(amount) * U256::from(fee);
    let fee_denom = U256::from(FEE_DENOMINATOR);
    let (quotient, remainder) = num.div_rem(fee_denom);

    let unrounded = quotient.to::<u128>();
    if remainder.is_zero() {
        unrounded
    } else {
        unrounded + 1
    }
}

/// Compute amount before fee was applied
pub fn amount_before_fee(after_fee: u128, fee: u64) -> Option<u128> {
    let fee_denom = U256::from(FEE_DENOMINATOR);
    let denominator = fee_denom - U256::from(fee);

    // Prevent division by zero when fee == FEE_DENOMINATOR (100%)
    if denominator.is_zero() {
        return None;
    }

    let shifted: U256 = U256::from(after_fee) << 64usize;
    let (quotient, remainder) = shifted.div_rem(denominator);

    let unrounded = quotient.to::<u128>();
    if remainder.is_zero() {
        Some(unrounded)
    } else {
        unrounded.checked_add(1)
    }
}

/// Main swap step calculation
/// Matches Ekubo SDK's compute_step function
pub fn compute_swap_step(
    sqrt_ratio: U256,
    liquidity: u128,
    sqrt_ratio_limit: U256,
    amount: i128,
    is_token1: bool,
    fee: u64,
) -> Result<SwapStepResult, MathError> {
    // No-op cases
    if amount == 0 || sqrt_ratio == sqrt_ratio_limit {
        return Ok(SwapStepResult {
            consumed_amount: 0,
            calculated_amount: 0,
            sqrt_ratio_next: sqrt_ratio,
            fee_amount: 0,
        });
    }

    let increasing = is_price_increasing(amount, is_token1);

    // Check direction matches limit
    if (sqrt_ratio_limit < sqrt_ratio) == increasing {
        return Err(MathError::WrongDirection);
    }

    // No liquidity - jump to limit
    if liquidity == 0 {
        return Ok(SwapStepResult {
            consumed_amount: 0,
            calculated_amount: 0,
            sqrt_ratio_next: sqrt_ratio_limit,
            fee_amount: 0,
        });
    }

    // Calculate price impact amount (amount after fee for input)
    let price_impact_amount = if amount < 0 {
        amount
    } else {
        let fee_amount: i128 = compute_fee(amount.unsigned_abs(), fee)
            .try_into()
            .map_err(|_| MathError::Overflow)?;
        amount - fee_amount
    };

    // Calculate next sqrt ratio from amount
    let sqrt_ratio_next_from_amount = if is_token1 {
        next_sqrt_ratio_from_amount1(sqrt_ratio, liquidity, price_impact_amount)
    } else {
        next_sqrt_ratio_from_amount0(sqrt_ratio, liquidity, price_impact_amount)
    };

    // Check if we can use the calculated next price
    if let Ok(sqrt_ratio_next) = sqrt_ratio_next_from_amount {
        // Price doesn't exceed limit
        if (sqrt_ratio_next <= sqrt_ratio_limit) == increasing {
            // Price didn't move - consume entire amount as fee
            if sqrt_ratio_next == sqrt_ratio {
                return Ok(SwapStepResult {
                    consumed_amount: amount,
                    calculated_amount: 0,
                    sqrt_ratio_next: sqrt_ratio,
                    fee_amount: amount.unsigned_abs(),
                });
            }

            // Calculate output amount
            let calculated_amount_excluding_fee = if is_token1 {
                amount0_delta(sqrt_ratio_next, sqrt_ratio, liquidity, amount < 0)?
            } else {
                amount1_delta(sqrt_ratio_next, sqrt_ratio, liquidity, amount < 0)?
            };

            if amount < 0 {
                // Exact output - add fee to calculated amount
                let including_fee = amount_before_fee(calculated_amount_excluding_fee, fee)
                    .ok_or(MathError::AmountBeforeFeeOverflow)?;
                return Ok(SwapStepResult {
                    consumed_amount: amount,
                    calculated_amount: including_fee,
                    sqrt_ratio_next,
                    fee_amount: including_fee - calculated_amount_excluding_fee,
                });
            } else {
                // Exact input
                return Ok(SwapStepResult {
                    consumed_amount: amount,
                    calculated_amount: calculated_amount_excluding_fee,
                    sqrt_ratio_next,
                    fee_amount: amount.unsigned_abs() - price_impact_amount.unsigned_abs(),
                });
            }
        }
    }

    // We're trading all the way to the limit
    let (specified_amount_delta, calculated_amount_delta) = if is_token1 {
        (
            amount1_delta(sqrt_ratio_limit, sqrt_ratio, liquidity, amount > 0),
            amount0_delta(sqrt_ratio_limit, sqrt_ratio, liquidity, amount < 0),
        )
    } else {
        (
            amount0_delta(sqrt_ratio_limit, sqrt_ratio, liquidity, amount > 0),
            amount1_delta(sqrt_ratio_limit, sqrt_ratio, liquidity, amount < 0),
        )
    };

    if amount < 0 {
        // Exact output
        let amount_after_fee = calculated_amount_delta?;
        let before_fee =
            amount_before_fee(amount_after_fee, fee).ok_or(MathError::AmountBeforeFeeOverflow)?;
        let consumed: i128 = specified_amount_delta?
            .try_into()
            .map_err(|_| MathError::SignedIntegerOverflow)?;
        Ok(SwapStepResult {
            consumed_amount: -consumed,
            calculated_amount: before_fee,
            fee_amount: before_fee - amount_after_fee,
            sqrt_ratio_next: sqrt_ratio_limit,
        })
    } else {
        // Exact input
        let specified_amount = specified_amount_delta?;
        let before_fee =
            amount_before_fee(specified_amount, fee).ok_or(MathError::AmountBeforeFeeOverflow)?;
        let consumed: i128 = before_fee
            .try_into()
            .map_err(|_| MathError::SignedIntegerOverflow)?;
        let calculated = calculated_amount_delta?;
        Ok(SwapStepResult {
            consumed_amount: consumed,
            calculated_amount: calculated,
            fee_amount: before_fee - specified_amount,
            sqrt_ratio_next: sqrt_ratio_limit,
        })
    }
}

// ========== Helper Functions ==========

fn sort_ratios(a: U256, b: U256) -> Result<(U256, U256), MathError> {
    let (lower, higher) = if a < b { (a, b) } else { (b, a) };

    if lower.is_zero() {
        return Err(MathError::ZeroRatio);
    }

    Ok((lower, higher))
}

// ========== Tests ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_muldiv_basic() {
        // 100 * 50 / 25 = 200
        let result = muldiv(
            U256::from(100u64),
            U256::from(50u64),
            U256::from(25u64),
            false,
        )
        .unwrap();
        assert_eq!(result, U256::from(200u64));
    }

    #[test]
    fn test_muldiv_round_up() {
        // 100 * 3 / 7 = 42.857... -> 43 when rounded up
        let result = muldiv(U256::from(100u64), U256::from(3u64), U256::from(7u64), true).unwrap();
        assert_eq!(result, U256::from(43u64));
    }

    #[test]
    fn test_sqrt_ratio_one() {
        // Verify constant is 2^128 (64.128 fixed-point per Ekubo docs)
        let expected = U256::from(1u128) << 128;
        assert_eq!(SQRT_RATIO_ONE, expected);
    }

    #[test]
    fn test_compute_fee() {
        // Fee = 0.02% = 0.0002 * 2^64 ≈ 3689348814741910
        let fee = 3689348814741910u64;
        let amount = 1_000_000_000_000u128; // 1 trillion units

        let fee_amount = compute_fee(amount, fee);
        // Should be approximately 0.02% of amount
        assert!(fee_amount > 0);
        assert!(fee_amount < amount / 100); // Less than 1%
    }

    #[test]
    fn test_is_price_increasing() {
        // Ekubo SDK: is_price_increasing = (amount < 0) != is_token1
        // Negative amount + token0 (not token1): (-100 < 0) != false = true != false = true -> increasing
        assert!(is_price_increasing(-100, false));
        // Positive amount + token0: (100 < 0) != false = false != false = false -> not increasing
        assert!(!is_price_increasing(100, false));
        // Negative amount + token1: (-100 < 0) != true = true != true = false -> not increasing
        assert!(!is_price_increasing(-100, true));
        // Positive amount + token1: (100 < 0) != true = false != true = true -> increasing
        assert!(is_price_increasing(100, true));
    }

    #[test]
    fn test_compute_swap_step_real_pool() {
        // Real ETH/USDT pool values from mainnet test
        let sqrt_ratio = U256::from_str_radix("19808180247948184959763749394", 10).unwrap();
        let liquidity: u128 = 34207185079984624;
        let fee: u64 = 1972248982;
        let amount: i128 = 100_000_000_000_000_000; // 0.1 ETH

        // zero_for_one = true (selling token0 for token1)
        // is_token1 = !zero_for_one = false
        let is_token1 = false;

        // When selling token0 (ETH), price should decrease
        let increasing = is_price_increasing(amount, is_token1);
        println!("is_price_increasing: {}", increasing);
        assert!(!increasing, "Price should decrease when selling token0");

        // Test next_sqrt_ratio_from_amount0
        let price_impact_amount = {
            let fee_amount = compute_fee(amount as u128, fee) as i128;
            amount - fee_amount
        };
        println!("price_impact_amount: {}", price_impact_amount);

        let sqrt_ratio_next =
            next_sqrt_ratio_from_amount0(sqrt_ratio, liquidity, price_impact_amount);
        println!("sqrt_ratio_next: {:?}", sqrt_ratio_next);

        // The next ratio should be valid and less than current (price decreases)
        assert!(sqrt_ratio_next.is_ok(), "next_sqrt_ratio should succeed");
        let sqrt_ratio_next = sqrt_ratio_next.unwrap();
        assert!(
            sqrt_ratio_next < sqrt_ratio,
            "Price should decrease when adding token0"
        );

        // Now test amount1_delta (output calculation)
        let amount1_out = amount1_delta(sqrt_ratio_next, sqrt_ratio, liquidity, false);
        println!("amount1_delta: {:?}", amount1_out);

        assert!(amount1_out.is_ok(), "amount1_delta should succeed");
        let amount1_out = amount1_out.unwrap();
        println!(
            "Amount out (token1/USDT): {} ({}e6 USDT)",
            amount1_out,
            amount1_out as f64 / 1e6
        );

        // Output should be positive and in reasonable range for 0.1 ETH
        // At ~$3400 ETH price, 0.1 ETH should give ~$340 USDT (340_000_000 units)
        assert!(amount1_out > 0, "Output should be positive");
    }
}
