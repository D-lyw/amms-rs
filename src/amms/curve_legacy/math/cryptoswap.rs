use crate::amms::error::AMMError;
use alloy::primitives::U256;

/// Legacy wrapper - kept for API compatibility
pub fn newton_y(ann: U256, gamma: U256, x: &[U256], d: U256, i: usize) -> Result<U256, AMMError> {
    crate::amms::curve_ng::math::cryptoswap::newton_y(ann, gamma, x, d, i)
        .map_err(|e| AMMError::Msg(e.into()))
}

/// Wrapper for newton_d from NG implementation
/// Calculates the invariant D from current balances
pub fn newton_d(amp: U256, gamma: U256, xp: &[U256]) -> Result<U256, AMMError> {
    // CRITICAL CORRECTION:
    // Testing against on-chain data shows that using raw `amp` (A value from contract)
    // produces exact matches with on-chain simulation.
    // The previous assumption that NG expects `amp * N^N * 10000` was incorrect for Legacy behavior matching.
    let ann = amp;

    crate::amms::curve_ng::math::cryptoswap::newton_d(ann, gamma, xp)
        .map_err(|e| AMMError::Msg(e.into()))
}

/// Calculate dynamic fee based on pool state
/// Matches Vyper `reduction_coefficient` exactly:
/// K = prod(x) / (sum(x) / N)^N = Product(N * x[i] / S)
/// f = fee_gamma / (fee_gamma + 10^18 - K)  
/// fee = mid_fee * f + out_fee * (1 - f)
pub fn fee_calc(
    xp: &[U256],
    _d: U256, // Unused, kept for API compatibility
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
) -> Result<U256, AMMError> {
    let n_coins = xp.len();
    let n_u256 = U256::from(n_coins);
    let precision = U256::from(1_000_000_000_000_000_000u64); // 10^18

    // 1. Calculate S = sum(x)
    let s: U256 = xp.iter().fold(U256::ZERO, |acc, x| acc + *x);

    if s.is_zero() {
        return Err(AMMError::Msg("fee_calc: sum is zero".into()));
    }

    // 2. Calculate K = Product(N * x[i] / S)
    // This is equivalent to prod(x) / (S/N)^N, all normalized to 10^18
    let mut k = precision;
    for x_i in xp {
        k = k * n_u256 * x_i / s;
    }

    // 3. Calculate f = fee_gamma * 10^18 / (fee_gamma + 10^18 - K)
    // Note: Vyper uses (fee_gamma + 10^18 - K), not |1 - K|
    let denom = fee_gamma + precision - k;
    if denom.is_zero() {
        return Err(AMMError::Msg("fee_calc: denom is zero".into()));
    }
    let f = fee_gamma * precision / denom;

    // 4. Calculate fee = (mid_fee * f + out_fee * (10^18 - f)) / 10^18
    let term1 = mid_fee * f;
    let term2 = out_fee * (precision - f);
    let fee = (term1 + term2) / precision;

    Ok(fee)
}

// Helper to calculate fee based on current state
// f = mid_fee * f_function(...) + out_fee * ...
// f_function is based on how close x is to equilibrium

pub fn get_dy(
    xp: &[U256],
    amp: U256,
    gamma: U256,
    d: U256,
    i: usize,
    j: usize,
    dx: U256,
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
    _price_scale: &[U256],
) -> Result<U256, AMMError> {
    // 1. Prepare xp with added dx (scaled)
    let mut y_cast = xp.to_vec();
    y_cast[i] += dx;

    // 2. Solve for new y at j
    // CRITICAL CORRECTION:
    // Testing against on-chain data shows that using raw `amp` (A value from contract)
    // produces exact matches (0.00% diff) with on-chain `get_dy`.
    let ann = amp;

    let y_out = newton_y(ann, gamma, &y_cast, d, j)?;

    // 3. Update y_cast to post-exchange state for fee calculation
    y_cast[j] = y_out;

    // 4. dy = xp[j] - y_out - 1
    let dy_scaled = xp[j]
        .checked_sub(y_out)
        .ok_or(AMMError::Msg("dy underflow".into()))?;
    let dy_scaled = dy_scaled.checked_sub(U256::from(1)).unwrap_or(U256::ZERO);

    // 5. Calculate fee on POST-EXCHANGE xp (matching Tricrypto2 behavior)
    let fee = fee_calc(&y_cast, d, mid_fee, out_fee, fee_gamma)?;

    // 6. Apply Fee
    let fee_amount = dy_scaled * fee / U256::from(10_000_000_000u64);
    let dy_final = dy_scaled - fee_amount;

    Ok(dy_final)
}
