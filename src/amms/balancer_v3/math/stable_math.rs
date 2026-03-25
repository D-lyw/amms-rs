use crate::amms::balancer_v3::BalancerV3Error;
use alloy::primitives::U256;

const AMP_PRECISION_U64: u64 = 1_000;

fn div_up_raw(a: U256, b: U256) -> Result<U256, BalancerV3Error> {
    if b.is_zero() {
        return Err(BalancerV3Error::MathError("div zero".to_string()));
    }
    if a.is_zero() {
        return Ok(U256::ZERO);
    }
    Ok((a - U256::from(1u8)) / b + U256::from(1u8))
}

pub fn calculate_invariant(amp: U256, balances: &[U256]) -> Result<U256, BalancerV3Error> {
    let num_tokens = balances.len();
    let mut sum = U256::ZERO;
    for b in balances {
        sum = sum
            .checked_add(*b)
            .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?;
    }
    if sum.is_zero() {
        return Ok(U256::ZERO);
    }

    let mut invariant = sum;
    let amp_times_total = amp
        .checked_mul(U256::from(num_tokens))
        .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;

    for _ in 0..255u16 {
        let mut d_p = invariant;
        for b in balances {
            let denom = b
                .checked_mul(U256::from(num_tokens))
                .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;
            d_p = d_p
                .checked_mul(invariant)
                .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
                .checked_div(denom)
                .ok_or_else(|| BalancerV3Error::MathError("div zero".to_string()))?;
        }

        let prev_invariant = invariant;

        let num = amp_times_total
            .checked_mul(sum)
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
            .checked_div(U256::from(AMP_PRECISION_U64))
            .ok_or_else(|| BalancerV3Error::MathError("div zero".to_string()))?
            .checked_add(d_p.checked_mul(U256::from(num_tokens)).ok_or_else(|| {
                BalancerV3Error::MathError("mul overflow".to_string())
            })?)
            .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?
            .checked_mul(invariant)
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;

        let denom = amp_times_total
            .checked_sub(U256::from(AMP_PRECISION_U64))
            .ok_or_else(|| BalancerV3Error::MathError("sub overflow".to_string()))?
            .checked_mul(invariant)
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
            .checked_div(U256::from(AMP_PRECISION_U64))
            .ok_or_else(|| BalancerV3Error::MathError("div zero".to_string()))?
            .checked_add(
                d_p
                    .checked_mul(U256::from(num_tokens + 1))
                    .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?,
            )
            .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?;

        invariant = num
            .checked_div(denom)
            .ok_or_else(|| BalancerV3Error::MathError("div zero".to_string()))?;

        if invariant > prev_invariant {
            if invariant - prev_invariant <= U256::from(1u8) {
                return Ok(invariant);
            }
        } else if prev_invariant - invariant <= U256::from(1u8) {
            return Ok(invariant);
        }
    }

    Err(BalancerV3Error::MathError(
        "StableInvariantDidNotConverge".to_string(),
    ))
}

pub fn compute_balance(
    amp: U256,
    balances: &[U256],
    invariant: U256,
    token_index: usize,
) -> Result<U256, BalancerV3Error> {
    let num_tokens = balances.len();
    let amp_times_total = amp
        .checked_mul(U256::from(num_tokens))
        .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;

    let mut sum = balances[0];
    let mut p_d = balances[0]
        .checked_mul(U256::from(num_tokens))
        .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;

    for j in 1..num_tokens {
        p_d = p_d
            .checked_mul(balances[j])
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
            .checked_mul(U256::from(num_tokens))
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
            .checked_div(invariant)
            .ok_or_else(|| BalancerV3Error::MathError("div zero".to_string()))?;
        sum = sum
            .checked_add(balances[j])
            .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?;
    }

    sum = sum
        .checked_sub(balances[token_index])
        .ok_or_else(|| BalancerV3Error::MathError("sub overflow".to_string()))?;

    let inv2 = invariant
        .checked_mul(invariant)
        .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;

    let c = div_up_raw(
        inv2
            .checked_mul(U256::from(AMP_PRECISION_U64))
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?,
        amp_times_total
            .checked_mul(p_d)
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?,
    )?
    .checked_mul(balances[token_index])
    .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?;

    let b = sum
        .checked_add(
            invariant
                .checked_mul(U256::from(AMP_PRECISION_U64))
                .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
                .checked_div(amp_times_total)
                .ok_or_else(|| BalancerV3Error::MathError("div zero".to_string()))?,
        )
        .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?;

    let mut token_balance = div_up_raw(inv2.checked_add(c).ok_or_else(|| {
        BalancerV3Error::MathError("add overflow".to_string())
    })?, invariant.checked_add(b).ok_or_else(|| {
        BalancerV3Error::MathError("add overflow".to_string())
    })?)?;

    for _ in 0..255u16 {
        let prev_token_balance = token_balance;

        let num = token_balance
            .checked_mul(token_balance)
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
            .checked_add(c)
            .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?;

        let denom = token_balance
            .checked_mul(U256::from(2u8))
            .ok_or_else(|| BalancerV3Error::MathError("mul overflow".to_string()))?
            .checked_add(b)
            .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?
            .checked_sub(invariant)
            .ok_or_else(|| BalancerV3Error::MathError("sub overflow".to_string()))?;

        token_balance = div_up_raw(num, denom)?;

        if token_balance > prev_token_balance {
            if token_balance - prev_token_balance <= U256::from(1u8) {
                return Ok(token_balance);
            }
        } else if prev_token_balance - token_balance <= U256::from(1u8) {
            return Ok(token_balance);
        }
    }

    Err(BalancerV3Error::MathError(
        "StableComputeBalanceDidNotConverge".to_string(),
    ))
}

pub fn calculate_out_given_in(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
    amount_in: U256,
) -> Result<U256, BalancerV3Error> {
    if token_index_in >= balances.len() || token_index_out >= balances.len() {
        return Err(BalancerV3Error::MathError(
            "stable out_given_in index out of bounds".to_string(),
        ));
    }

    let invariant = calculate_invariant(amp, balances)?;
    let mut new_balances = balances.to_vec();
    new_balances[token_index_in] = new_balances[token_index_in]
        .checked_add(amount_in)
        .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))?;

    let final_balance_out = compute_balance(amp, &new_balances, invariant, token_index_out)?;

    if balances[token_index_out] <= final_balance_out {
        return Ok(U256::ZERO);
    }
    let raw_out = balances[token_index_out]
        .checked_sub(final_balance_out)
        .ok_or_else(|| BalancerV3Error::MathError("sub overflow".to_string()))?;

    if raw_out > U256::ZERO {
        Ok(raw_out - U256::from(1u8))
    } else {
        Ok(U256::ZERO)
    }
}

pub fn calculate_in_given_out(
    amp: U256,
    balances: &[U256],
    token_index_in: usize,
    token_index_out: usize,
    amount_out: U256,
) -> Result<U256, BalancerV3Error> {
    if amount_out.is_zero() {
        return Ok(U256::ZERO);
    }
    if token_index_in >= balances.len() || token_index_out >= balances.len() {
        return Err(BalancerV3Error::MathError(
            "stable in_given_out index out of bounds".to_string(),
        ));
    }
    if amount_out >= balances[token_index_out] {
        return Err(BalancerV3Error::MathError("sub underflow".to_string()));
    }

    let invariant = calculate_invariant(amp, balances)?;
    let mut new_balances = balances.to_vec();
    new_balances[token_index_out] = new_balances[token_index_out]
        .checked_sub(amount_out)
        .ok_or_else(|| BalancerV3Error::MathError("sub underflow".to_string()))?;

    let final_balance_in = compute_balance(amp, &new_balances, invariant, token_index_in)?;

    let amount_in = final_balance_in
        .checked_sub(balances[token_index_in])
        .ok_or_else(|| BalancerV3Error::MathError("sub underflow".to_string()))?;

    amount_in
        .checked_add(U256::from(1u8))
        .ok_or_else(|| BalancerV3Error::MathError("add overflow".to_string()))
}
