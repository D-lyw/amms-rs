use crate::amms::balancer_v2::BalancerV2Error;
use crate::amms::consts::MPFR_T_PRECISION;
use crate::amms::float::u256_to_float;
use alloy::primitives::U256;
use rug::ops::Pow;
use rug::Float;

/// Weighted Math: calculateOutGivenIn
/// Formula: Ao = Bo * (1 - (Bi / (Bi + Ai * (1-fee))) ^ (Wi / Wo))
pub fn calculate_out_given_in(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
    amount_in: U256,
    swap_fee: U256,
) -> Result<U256, BalancerV2Error> {
    let balance_in_f =
        u256_to_float(balance_in).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let weight_in_f =
        u256_to_float(weight_in).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let balance_out_f =
        u256_to_float(balance_out).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let weight_out_f =
        u256_to_float(weight_out).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let amount_in_f =
        u256_to_float(amount_in).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let swap_fee_f =
        u256_to_float(swap_fee).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;

    let one_f = Float::with_val(MPFR_T_PRECISION, 1.0);
    let bone_f = Float::with_val(MPFR_T_PRECISION, 1e18);

    // Fee Ratio = fee / BONE
    let fee_ratio = Float::with_val(MPFR_T_PRECISION, &swap_fee_f / &bone_f);

    // One Minus Fee = 1 - Fee Ratio
    let one_minus_fee = Float::with_val(MPFR_T_PRECISION, &one_f - &fee_ratio);

    // Amount In After Fee = AmountIn * (1 - Fee)
    let amount_in_after_fee = Float::with_val(MPFR_T_PRECISION, &amount_in_f * &one_minus_fee);

    // Denominator = BalanceIn + AmountInAfterFee
    let denominator = Float::with_val(MPFR_T_PRECISION, &balance_in_f + &amount_in_after_fee);

    // Base = BalanceIn / Denominator
    let base = Float::with_val(MPFR_T_PRECISION, &balance_in_f / &denominator);

    // Exponent = WeightIn / WeightOut
    if weight_out_f.is_zero() {
        return Err(BalancerV2Error::DivZero);
    }
    let exponent = Float::with_val(MPFR_T_PRECISION, &weight_in_f / &weight_out_f);

    // Power = Base ^ Exponent
    let power = Float::with_val(MPFR_T_PRECISION, base.pow(exponent));

    // Ratio = 1 - Power
    let ratio = Float::with_val(MPFR_T_PRECISION, &one_f - &power);

    // AmountOut = BalanceOut * Ratio
    let amount_out_f = Float::with_val(MPFR_T_PRECISION, &balance_out_f * &ratio);

    // Convert back to U256
    // We assume the result is non-negative and fits in U256.
    // Use floor or round? Usually floor in Solidity (truncation), but let's check.
    // Standard solidity integer division truncates.
    // But here we computed exact value.
    // Balancer usually rounds down for outGivenIn to be safe?
    // Let's use standard string parsing which effectively truncates if we don't round.
    // Float::to_integer()?

    let amount_out_str = amount_out_f.to_string_radix(10, None);
    // Split at decimal point
    let parts: Vec<&str> = amount_out_str.split('.').collect();
    let integer_part = parts[0];

    // Handle potential negative result (should not happen with this math but good to be safe)
    if integer_part.starts_with('-') {
        return Err(BalancerV2Error::SubUnderflow);
    }

    let result =
        U256::from_str_radix(integer_part, 10).map_err(|_| BalancerV2Error::MulOverflow)?;
    Ok(result)
}

/// Calculate spot price for Weighted Pool
/// Official formula: SP = (B_in / W_in) / (B_out / W_out)
/// This gives: how many token_out you get for 1 token_in
/// Simplified: (B_out * W_in) / (B_in * W_out)
pub fn calculate_spot_price(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
) -> Result<f64, BalancerV2Error> {
    let balance_in_f =
        u256_to_float(balance_in).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let weight_in_f =
        u256_to_float(weight_in).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let balance_out_f =
        u256_to_float(balance_out).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let weight_out_f =
        u256_to_float(weight_out).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;

    // Correct formula: (Bout * Win) / (Bin * Wout)
    // This returns "how many token_out for 1 token_in"
    let num = Float::with_val(MPFR_T_PRECISION, &balance_out_f * &weight_in_f);
    let den = Float::with_val(MPFR_T_PRECISION, &balance_in_f * &weight_out_f);

    if den.is_zero() {
        return Err(BalancerV2Error::DivZero);
    }

    let price = Float::with_val(MPFR_T_PRECISION, &num / &den);
    Ok(price.to_f64())
}
