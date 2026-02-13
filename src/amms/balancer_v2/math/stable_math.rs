use crate::amms::balancer_v2::BalancerV2Error;
use crate::amms::consts::MPFR_T_PRECISION;
use crate::amms::float::u256_to_float;
use alloy::primitives::U256;
use rug::Float;

/// Balancer V2 Stable Pool AMP_PRECISION constant
/// The amplification parameter is stored with this precision factor
/// See: https://github.com/balancer/balancer-v2-monorepo/blob/master/pkg/pool-stable/contracts/StableMath.sol
const AMP_PRECISION: f64 = 1000.0;

pub fn calculate_invariant(amp: U256, balances: &[U256]) -> Result<Float, BalancerV2Error> {
    let mut sum = Float::with_val(MPFR_T_PRECISION, 0.0);
    let n_coins_u = balances.len();
    let n_coins = Float::with_val(MPFR_T_PRECISION, n_coins_u as f64);

    if n_coins_u == 0 {
        return Ok(Float::with_val(MPFR_T_PRECISION, 0.0));
    }

    let mut balances_f = Vec::with_capacity(n_coins_u);
    for b in balances {
        let bf = u256_to_float(*b).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
        sum = Float::with_val(MPFR_T_PRECISION, &sum + &bf);
        balances_f.push(bf);
    }

    if sum.is_zero() {
        return Ok(Float::with_val(MPFR_T_PRECISION, 0.0));
    }

    let amp_f = u256_to_float(amp).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    // Ann = A * n (already includes AMP_PRECISION from getAmplificationParameter)
    let amp_times_n = Float::with_val(MPFR_T_PRECISION, &amp_f * &n_coins);
    let amp_precision = Float::with_val(MPFR_T_PRECISION, AMP_PRECISION);

    let mut d = sum.clone();
    let mut prev_d;

    let one = Float::with_val(MPFR_T_PRECISION, 1.0);

    // Newton's method - following Balancer V2 StableMath._calculateInvariant
    for _ in 0..255 {
        let mut d_p = d.clone();
        for b in &balances_f {
            // D_P = (D_P * D) / (balances[j] * numTokens)
            // If balance is zero, denom is zero -> division by zero
            // We should catch this early or check b
            if b.is_zero() {
                return Err(BalancerV2Error::DivZero);
            }
            let denom = Float::with_val(MPFR_T_PRECISION, &n_coins * b);
            let num = Float::with_val(MPFR_T_PRECISION, &d_p * &d);
            d_p = Float::with_val(MPFR_T_PRECISION, num / denom);
        }

        prev_d = d.clone();

        // Numerator: ((ampTimesTotal * sum) / AMP_PRECISION + D_P * numTokens) * invariant
        let term1 = Float::with_val(MPFR_T_PRECISION, &amp_times_n * &sum);
        let term1_scaled = Float::with_val(MPFR_T_PRECISION, &term1 / &amp_precision);
        let term2 = Float::with_val(MPFR_T_PRECISION, &d_p * &n_coins);
        let sum_terms = Float::with_val(MPFR_T_PRECISION, &term1_scaled + &term2);
        let num = Float::with_val(MPFR_T_PRECISION, &sum_terms * &d);

        // Denominator: ((ampTimesTotal - AMP_PRECISION) * invariant) / AMP_PRECISION + (numTokens + 1) * D_P
        let amp_minus_precision = Float::with_val(MPFR_T_PRECISION, &amp_times_n - &amp_precision);
        let term3_unscaled = Float::with_val(MPFR_T_PRECISION, &amp_minus_precision * &d);
        let term3 = Float::with_val(MPFR_T_PRECISION, &term3_unscaled / &amp_precision);

        let n_plus_1 = Float::with_val(MPFR_T_PRECISION, &n_coins + &one);
        let term4 = Float::with_val(MPFR_T_PRECISION, &n_plus_1 * &d_p);

        let den = Float::with_val(MPFR_T_PRECISION, &term3 + &term4);

        d = Float::with_val(MPFR_T_PRECISION, num / den);

        let diff = if d > prev_d {
            Float::with_val(MPFR_T_PRECISION, &d - &prev_d)
        } else {
            Float::with_val(MPFR_T_PRECISION, &prev_d - &d)
        };

        if diff <= one {
            break;
        }
    }

    Ok(d)
}

pub fn calculate_out_given_in(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
    amount_in: U256,
    fee: U256,
) -> Result<U256, BalancerV2Error> {
    // 1. Current Invariant
    let d = calculate_invariant(amp, balances)?;

    let balances_f: Vec<Float> = balances
        .iter()
        .map(|b| u256_to_float(*b).map_err(|e| BalancerV2Error::NotSupported(e.to_string())))
        .collect::<Result<_, _>>()?;
    let amount_in_f =
        u256_to_float(amount_in).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let fee_f = u256_to_float(fee).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let bone_f = Float::with_val(MPFR_T_PRECISION, 1e18);

    // 2. New Balance In (after fee)
    let fee_ratio = Float::with_val(MPFR_T_PRECISION, &fee_f / &bone_f);
    let one = Float::with_val(MPFR_T_PRECISION, 1.0);
    let one_minus_fee = Float::with_val(MPFR_T_PRECISION, &one - &fee_ratio);
    let amount_in_after_fee = Float::with_val(MPFR_T_PRECISION, &amount_in_f * one_minus_fee);
    let new_balance_in = Float::with_val(
        MPFR_T_PRECISION,
        &balances_f[token_index_in] + &amount_in_after_fee,
    );

    // 3. Solve for New Balance Out
    let n_coins_u = balances.len();
    let n_coins = Float::with_val(MPFR_T_PRECISION, n_coins_u as f64);
    let amp_f = u256_to_float(amp).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let ann = Float::with_val(MPFR_T_PRECISION, &amp_f * &n_coins);

    let mut s_prime = Float::with_val(MPFR_T_PRECISION, 0.0);
    let mut p = d.clone();

    for (i, b) in balances_f.iter().enumerate() {
        if i == token_index_out {
            continue;
        }
        let balance = if i == token_index_in {
            &new_balance_in
        } else {
            b
        };
        s_prime = Float::with_val(MPFR_T_PRECISION, &s_prime + balance);

        let denom = Float::with_val(MPFR_T_PRECISION, &n_coins * balance);
        let p_num = Float::with_val(MPFR_T_PRECISION, &p * &d);
        p = Float::with_val(MPFR_T_PRECISION, p_num / denom);
    }

    // Apply AMP_PRECISION: ann (from getAmplificationParameter) already includes 1000x factor
    let amp_precision = Float::with_val(MPFR_T_PRECISION, AMP_PRECISION);

    // c = (p * d * AMP_PRECISION) / (ann * n)
    // This is equivalent to: c = D^(n+1) / (A * n^2n * P) where A is already scaled by PRECISION
    let term = Float::with_val(MPFR_T_PRECISION, &ann * &n_coins);
    let p_d = Float::with_val(MPFR_T_PRECISION, &p * &d);
    let p_d_scaled = Float::with_val(MPFR_T_PRECISION, &p_d * &amp_precision);
    let c = Float::with_val(MPFR_T_PRECISION, p_d_scaled / term);

    // b = s_prime + (d * AMP_PRECISION) / ann
    let d_scaled = Float::with_val(MPFR_T_PRECISION, &d * &amp_precision);
    let d_div_ann = Float::with_val(MPFR_T_PRECISION, &d_scaled / &ann);
    let b_term = Float::with_val(MPFR_T_PRECISION, &s_prime + &d_div_ann);

    // Quadratic: y^2 + (b - D)y - c = 0
    let b_minus_d = Float::with_val(MPFR_T_PRECISION, &b_term - &d);
    let b_minus_d_sq = Float::with_val(MPFR_T_PRECISION, &b_minus_d * &b_minus_d);
    let c_4 = Float::with_val(MPFR_T_PRECISION, &c * 4.0);
    let discriminant = Float::with_val(MPFR_T_PRECISION, b_minus_d_sq + c_4);
    let sqrt_disc = Float::with_val(MPFR_T_PRECISION, discriminant.sqrt());

    let num_y = Float::with_val(MPFR_T_PRECISION, sqrt_disc - &b_minus_d);
    let y = Float::with_val(MPFR_T_PRECISION, num_y / 2.0);

    let new_balance_out = y;
    let amount_out_f = Float::with_val(
        MPFR_T_PRECISION,
        &balances_f[token_index_out] - &new_balance_out,
    );

    let amount_out_str = amount_out_f.to_string_radix(10, None);
    let parts: Vec<&str> = amount_out_str.split('.').collect();
    let integer_part = parts[0];

    let result =
        U256::from_str_radix(integer_part, 10).map_err(|_| BalancerV2Error::MulOverflow)?;
    Ok(result)
}

pub fn calculate_spot_price(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
) -> Result<f64, BalancerV2Error> {
    let d = calculate_invariant(amp, balances)?;
    let balances_f: Vec<Float> = balances
        .iter()
        .map(|b| u256_to_float(*b).map_err(|e| BalancerV2Error::NotSupported(e.to_string())))
        .collect::<Result<_, _>>()?;
    let amp_f = u256_to_float(amp).map_err(|e| BalancerV2Error::NotSupported(e.to_string()))?;
    let n_coins = Float::with_val(MPFR_T_PRECISION, balances.len() as f64);
    let amp_precision = Float::with_val(MPFR_T_PRECISION, AMP_PRECISION);
    // ann = A * n, but A already includes AMP_PRECISION factor, so we need to divide
    let ann_unscaled = Float::with_val(MPFR_T_PRECISION, &amp_f * &n_coins);
    let ann = Float::with_val(MPFR_T_PRECISION, &ann_unscaled / &amp_precision);

    let x_in = &balances_f[token_index_in];
    let x_out = &balances_f[token_index_out];

    let mut alpha = d.clone();
    for b in &balances_f {
        let denom = Float::with_val(MPFR_T_PRECISION, &n_coins * b);
        let num = Float::with_val(MPFR_T_PRECISION, &alpha * &d);
        alpha = Float::with_val(MPFR_T_PRECISION, num / denom);
    }

    let ann_x_in = Float::with_val(MPFR_T_PRECISION, &ann * x_in);
    let term_in = Float::with_val(MPFR_T_PRECISION, ann_x_in + &alpha);

    let ann_x_out = Float::with_val(MPFR_T_PRECISION, &ann * x_out);
    let term_out = Float::with_val(MPFR_T_PRECISION, ann_x_out + &alpha);

    let num = Float::with_val(MPFR_T_PRECISION, x_out * term_in);
    let den = Float::with_val(MPFR_T_PRECISION, x_in * term_out);

    if den.is_zero() {
        return Err(BalancerV2Error::DivZero);
    }

    let price = Float::with_val(MPFR_T_PRECISION, num / den);
    Ok(price.to_f64())
}
