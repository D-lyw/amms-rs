use crate::amms::balancer_v2::BalancerV2Error;
use crate::amms::consts::BONE;
use alloy::primitives::U256;

/// Balancer V2 Stable Pool AMP_PRECISION constant
/// The amplification parameter is stored with this precision factor
/// See: https://github.com/balancer/balancer-v2-monorepo/blob/master/pkg/pool-stable/contracts/StableMath.sol
const AMP_PRECISION: u64 = 1000;
const MAX_STABLE_ITERATIONS: usize = 255;

fn abs_diff(a: U256, b: U256) -> U256 {
    if a >= b { a - b } else { b - a }
}

fn div_down(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    if b.is_zero() {
        return Err(BalancerV2Error::DivZero);
    }
    Ok(a / b)
}

fn mul_div_down(a: U256, b: U256, d: U256) -> Result<U256, BalancerV2Error> {
    let p = a.checked_mul(b).ok_or(BalancerV2Error::MulOverflow)?;
    div_down(p, d)
}

fn div_up(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    if b.is_zero() {
        return Err(BalancerV2Error::DivZero);
    }
    let q = a / b;
    let r = a % b;
    if r.is_zero() {
        Ok(q)
    } else {
        q.checked_add(U256::from(1u8))
            .ok_or(BalancerV2Error::AddOverflow)
    }
}

#[allow(dead_code)]
fn mul_div_up(a: U256, b: U256, d: U256) -> Result<U256, BalancerV2Error> {
    let p = a.checked_mul(b).ok_or(BalancerV2Error::MulOverflow)?;
    div_up(p, d)
}

fn calculate_invariant_u256(amp: U256, balances: &[U256]) -> Result<U256, BalancerV2Error> {
    let num_tokens = balances.len();
    if num_tokens == 0 {
        return Ok(U256::ZERO);
    }
    let n = U256::from(num_tokens as u64);
    let n_plus_1 = U256::from((num_tokens + 1) as u64);
    let amp_precision = U256::from(AMP_PRECISION);

    let sum = balances.iter().try_fold(U256::ZERO, |acc, b| {
        acc.checked_add(*b).ok_or(BalancerV2Error::AddOverflow)
    })?;
    if sum.is_zero() {
        return Ok(U256::ZERO);
    }

    let mut invariant = sum;
    let amp_times_total = amp.checked_mul(n).ok_or(BalancerV2Error::MulOverflow)?;

    for _ in 0..MAX_STABLE_ITERATIONS {
        let mut p_d = invariant;
        for balance in balances {
            let bn = balance.checked_mul(n).ok_or(BalancerV2Error::MulOverflow)?;
            p_d = mul_div_down(p_d, invariant, bn)?;
        }

        let prev = invariant;

        let term1 = mul_div_down(amp_times_total, sum, amp_precision)?;
        let term2 = p_d.checked_mul(n).ok_or(BalancerV2Error::MulOverflow)?;
        let numerator_base = term1.checked_add(term2).ok_or(BalancerV2Error::AddOverflow)?;
        let numerator = numerator_base
            .checked_mul(invariant)
            .ok_or(BalancerV2Error::MulOverflow)?;

        let amp_minus_precision = amp_times_total
            .checked_sub(amp_precision)
            .ok_or(BalancerV2Error::SubUnderflow)?;
        let den_term1 = mul_div_down(amp_minus_precision, invariant, amp_precision)?;
        let den_term2 = n_plus_1
            .checked_mul(p_d)
            .ok_or(BalancerV2Error::MulOverflow)?;
        let denominator = den_term1
            .checked_add(den_term2)
            .ok_or(BalancerV2Error::AddOverflow)?;

        invariant = div_down(numerator, denominator)?;
        if abs_diff(invariant, prev) <= U256::from(1u8) {
            return Ok(invariant);
        }
    }

    Ok(invariant)
}

fn get_token_balance_given_invariant_and_all_other_balances_u256(
    amp: U256,
    balances: &[U256],
    token_index: usize,
    invariant: U256,
) -> Result<U256, BalancerV2Error> {
    let num_tokens = balances.len();
    let n = U256::from(num_tokens as u64);
    let amp_precision = U256::from(AMP_PRECISION);
    let amp_times_total = amp.checked_mul(n).ok_or(BalancerV2Error::MulOverflow)?;
    if balances.is_empty() || token_index >= balances.len() {
        return Err(BalancerV2Error::NotSupported(
            "stable get_token_balance index out of bounds".to_string(),
        ));
    }

    // Solidity parity:
    // sum = balances[0] + ... + balances[n-1] - balances[tokenIndex]
    // P_D = balances[0] * n; for j=1..n-1: P_D = divDown(P_D * balances[j] * n, invariant)
    let mut sum = balances[0];
    let mut p_d = balances[0]
        .checked_mul(n)
        .ok_or(BalancerV2Error::MulOverflow)?;
    for balance in balances.iter().skip(1) {
        let pd_mul_b = p_d
            .checked_mul(*balance)
            .ok_or(BalancerV2Error::MulOverflow)?;
        let pd_mul_bn = pd_mul_b.checked_mul(n).ok_or(BalancerV2Error::MulOverflow)?;
        p_d = div_down(pd_mul_bn, invariant)?;
        sum = sum.checked_add(*balance).ok_or(BalancerV2Error::AddOverflow)?;
    }
    sum = sum
        .checked_sub(balances[token_index])
        .ok_or(BalancerV2Error::SubUnderflow)?;

    let inv2 = invariant
        .checked_mul(invariant)
        .ok_or(BalancerV2Error::MulOverflow)?;

    // Solidity parity:
    // c = divUp(inv2, ampTimesTotal * P_D) * AMP_PRECISION * balances[tokenIndex]
    let amp_pd = amp_times_total
        .checked_mul(p_d)
        .ok_or(BalancerV2Error::MulOverflow)?;
    let c_div = div_up(inv2, amp_pd)?;
    let c_scaled = c_div
        .checked_mul(amp_precision)
        .ok_or(BalancerV2Error::MulOverflow)?;
    let c = c_scaled
        .checked_mul(balances[token_index])
        .ok_or(BalancerV2Error::MulOverflow)?;

    // b = sum + divDown(invariant, ampTimesTotal) * AMP_PRECISION
    let b_term = div_down(invariant, amp_times_total)?
        .checked_mul(amp_precision)
        .ok_or(BalancerV2Error::MulOverflow)?;
    let b = sum.checked_add(b_term).ok_or(BalancerV2Error::AddOverflow)?;

    // Newton iteration for token balance
    let mut token_balance = div_up(
        inv2.checked_add(c).ok_or(BalancerV2Error::AddOverflow)?,
        invariant
            .checked_add(b)
            .ok_or(BalancerV2Error::AddOverflow)?,
    )?;

    for _ in 0..MAX_STABLE_ITERATIONS {
        let prev = token_balance;
        let num = token_balance
            .checked_mul(token_balance)
            .ok_or(BalancerV2Error::MulOverflow)?
            .checked_add(c)
            .ok_or(BalancerV2Error::AddOverflow)?;
        let den = token_balance
            .checked_mul(U256::from(2u8))
            .ok_or(BalancerV2Error::MulOverflow)?
            .checked_add(b)
            .ok_or(BalancerV2Error::AddOverflow)?
            .checked_sub(invariant)
            .ok_or(BalancerV2Error::SubUnderflow)?;
        token_balance = div_up(num, den)?;
        if abs_diff(token_balance, prev) <= U256::from(1u8) {
            return Ok(token_balance);
        }
    }

    Ok(token_balance)
}

pub fn calculate_out_given_in(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
    amount_in: U256,
) -> Result<U256, BalancerV2Error> {
    if amount_in.is_zero() {
        return Ok(U256::ZERO);
    }
    if token_index_in >= balances.len() || token_index_out >= balances.len() {
        return Err(BalancerV2Error::NotSupported(
            "stable out_given_in index out of bounds".to_string(),
        ));
    }

    let invariant = calculate_invariant_u256(amp, balances)?;
    let mut new_balances = balances.to_vec();
    new_balances[token_index_in] = new_balances[token_index_in]
        .checked_add(amount_in)
        .ok_or(BalancerV2Error::AddOverflow)?;

    let final_balance_out = get_token_balance_given_invariant_and_all_other_balances_u256(
        amp,
        &new_balances,
        token_index_out,
        invariant,
    )?;

    if balances[token_index_out] <= final_balance_out {
        return Ok(U256::ZERO);
    }
    let raw_out = balances[token_index_out] - final_balance_out;
    // round down to stay conservative for GIVEN_IN queries.
    if raw_out > U256::ZERO {
        Ok(raw_out - U256::from(1u8))
    } else {
        Ok(U256::ZERO)
    }
}

/// Stable math exact-out solver in fixed-point domain.
pub fn calculate_in_given_out(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
    amount_out: U256,
) -> Result<U256, BalancerV2Error> {
    if amount_out.is_zero() {
        return Ok(U256::ZERO);
    }
    if token_index_in >= balances.len() || token_index_out >= balances.len() {
        return Err(BalancerV2Error::NotSupported(
            "stable in_given_out index out of bounds".to_string(),
        ));
    }
    if amount_out >= balances[token_index_out] {
        return Err(BalancerV2Error::SubUnderflow);
    }
    let invariant = calculate_invariant_u256(amp, balances)?;
    let mut new_balances = balances.to_vec();
    new_balances[token_index_out] = new_balances[token_index_out]
        .checked_sub(amount_out)
        .ok_or(BalancerV2Error::SubUnderflow)?;

    let final_balance_in = get_token_balance_given_invariant_and_all_other_balances_u256(
        amp,
        &new_balances,
        token_index_in,
        invariant,
    )?;

    let amount_in = final_balance_in
        .checked_sub(balances[token_index_in])
        .ok_or(BalancerV2Error::SubUnderflow)?;
    // GIVEN_OUT path rounds input up overall in StableMath.
    amount_in
        .checked_add(U256::from(1u8))
        .ok_or(BalancerV2Error::AddOverflow)
}

pub fn calculate_spot_price(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
) -> Result<f64, BalancerV2Error> {
    let unit = BONE;
    let out = calculate_out_given_in(amp, balances, token_index_in, token_index_out, unit)?;
    let out_f = out
        .to_string()
        .parse::<f64>()
        .map_err(|_| BalancerV2Error::NotSupported("u256->f64 conversion failed".to_string()))?;
    Ok(out_f / 1e18f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_in_given_out_inverse_property() {
        let amp = U256::from(20_000u64); // includes AMP precision scale
        let balances = vec![
            U256::from(5_000_000_000_000u64),
            U256::from(4_900_000_000_000u64),
            U256::from(5_100_000_000_000u64),
        ];
        let target_out = U256::from(50_000_000u64);

        let amount_in = calculate_in_given_out(amp, &balances, 0, 1, target_out)
            .expect("stable in_given_out should solve");

        let out = calculate_out_given_in(amp, &balances, 0, 1, amount_in)
            .expect("stable out_given_in should work");
        let diff = if out > target_out {
            out - target_out
        } else {
            target_out - out
        };
        let tolerance = std::cmp::max(target_out / U256::from(100_000u64), U256::from(1u8));
        assert!(
            diff <= tolerance,
            "stable inverse mismatch target_out={} amount_in={} out={} diff={} tolerance={}",
            target_out,
            amount_in,
            out,
            diff,
            tolerance
        );

        if amount_in > U256::ZERO {
            let out_less = calculate_out_given_in(amp, &balances, 0, 1, amount_in - U256::from(1u8))
                .expect("stable out_given_in should work");
            assert!(out_less <= out);
        }
    }
}
