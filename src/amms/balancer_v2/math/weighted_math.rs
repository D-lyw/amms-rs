use crate::amms::balancer_v2::BalancerV2Error;
use alloy::primitives::{I256, U256};

const MAX_IN_RATIO: u128 = 300_000_000_000_000_000u128; // 0.3e18
const MAX_OUT_RATIO: u128 = 300_000_000_000_000_000u128; // 0.3e18

#[inline]
fn one_u() -> U256 {
    U256::from(1_000_000_000_000_000_000u128)
}

#[inline]
fn two_u() -> U256 {
    U256::from(2_000_000_000_000_000_000u128)
}

#[inline]
fn four_u() -> U256 {
    U256::from(4_000_000_000_000_000_000u128)
}

#[inline]
fn one_i18() -> I256 {
    I256::from_raw(one_u())
}

#[inline]
fn one_i20() -> I256 {
    I256::from_raw(U256::from(100_000_000_000_000_000_000u128))
}

#[inline]
fn one_i36() -> I256 {
    I256::from_raw(U256::from_str_radix("1000000000000000000000000000000000000", 10).unwrap())
}

#[inline]
fn i_from_u(x: U256) -> I256 {
    I256::from_raw(x)
}

#[inline]
fn u_from_i(x: I256) -> Result<U256, BalancerV2Error> {
    if x < I256::ZERO {
        return Err(BalancerV2Error::SubUnderflow);
    }
    Ok(x.into_raw())
}

#[inline]
fn parse_u(s: &str) -> U256 {
    U256::from_str_radix(s, 10).unwrap()
}

#[inline]
fn parse_i_pos(s: &str) -> I256 {
    I256::from_raw(parse_u(s))
}

#[inline]
fn i(v: i64) -> I256 {
    I256::try_from(v).unwrap()
}

#[inline]
fn u(v: u64) -> U256 {
    U256::from(v)
}

#[inline]
fn add_u(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    a.checked_add(b).ok_or(BalancerV2Error::AddOverflow)
}

#[inline]
fn sub_u(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    a.checked_sub(b).ok_or(BalancerV2Error::SubUnderflow)
}

#[inline]
fn mul_u(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    a.checked_mul(b).ok_or(BalancerV2Error::MulOverflow)
}

#[inline]
fn div_u(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    if b.is_zero() {
        return Err(BalancerV2Error::DivZero);
    }
    a.checked_div(b).ok_or(BalancerV2Error::DivZero)
}

#[inline]
fn add_i(a: I256, b: I256) -> Result<I256, BalancerV2Error> {
    let (r, of) = a.overflowing_add(b);
    if of {
        Err(BalancerV2Error::AddOverflow)
    } else {
        Ok(r)
    }
}

#[inline]
fn sub_i(a: I256, b: I256) -> Result<I256, BalancerV2Error> {
    let (r, of) = a.overflowing_sub(b);
    if of {
        Err(BalancerV2Error::SubUnderflow)
    } else {
        Ok(r)
    }
}

#[inline]
fn mul_i(a: I256, b: I256) -> Result<I256, BalancerV2Error> {
    let (r, of) = a.overflowing_mul(b);
    if of {
        Err(BalancerV2Error::MulOverflow)
    } else {
        Ok(r)
    }
}

#[inline]
fn div_i(a: I256, b: I256) -> Result<I256, BalancerV2Error> {
    if b == I256::ZERO {
        return Err(BalancerV2Error::DivZero);
    }
    // Solidity int256 division semantics: truncate toward zero.
    // We implement this explicitly to avoid backend-dependent behavior differences.
    let neg = (a < I256::ZERO) ^ (b < I256::ZERO);
    let a_abs = if a < I256::ZERO { -a } else { a };
    let b_abs = if b < I256::ZERO { -b } else { b };

    let q = a_abs
        .into_raw()
        .checked_div(b_abs.into_raw())
        .ok_or(BalancerV2Error::DivZero)?;
    let q_i = I256::from_raw(q);
    Ok(if neg { -q_i } else { q_i })
}

#[inline]
fn rem_i(a: I256, b: I256) -> Result<I256, BalancerV2Error> {
    // Solidity int256 remainder follows: r = a - trunc(a / b) * b
    let q = div_i(a, b)?;
    sub_i(a, mul_i(q, b)?)
}

fn fp_add(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    add_u(a, b)
}

fn fp_sub(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    sub_u(a, b)
}

fn fp_mul_down(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    div_u(mul_u(a, b)?, one_u())
}

fn fp_mul_up(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    let product = mul_u(a, b)?;
    if product.is_zero() {
        return Ok(U256::ZERO);
    }
    add_u(div_u(sub_u(product, u(1))?, one_u())?, u(1))
}

fn fp_div_down(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    div_u(mul_u(a, one_u())?, b)
}

fn fp_div_up(a: U256, b: U256) -> Result<U256, BalancerV2Error> {
    let a_inflated = mul_u(a, one_u())?;
    if a_inflated.is_zero() {
        return Ok(U256::ZERO);
    }
    add_u(div_u(sub_u(a_inflated, u(1))?, b)?, u(1))
}

fn fp_complement(x: U256) -> U256 {
    if x < one_u() { one_u() - x } else { U256::ZERO }
}

fn max_pow_relative_error() -> U256 {
    U256::from(10_000u64) // 1e-14 in 1e18 fixed-point
}

#[allow(dead_code)]
fn pow_down(x: U256, y: U256) -> Result<U256, BalancerV2Error> {
    if y == one_u() {
        return Ok(x);
    }
    if y == two_u() {
        return fp_mul_down(x, x);
    }
    if y == four_u() {
        let square = fp_mul_down(x, x)?;
        return fp_mul_down(square, square);
    }

    let raw = logexp_pow(x, y)?;
    let max_error = fp_add(fp_mul_up(raw, max_pow_relative_error())?, u(1))?;
    if raw < max_error {
        Ok(U256::ZERO)
    } else {
        fp_sub(raw, max_error)
    }
}

fn pow_up(x: U256, y: U256) -> Result<U256, BalancerV2Error> {
    if y == one_u() {
        return Ok(x);
    }
    if y == two_u() {
        return fp_mul_up(x, x);
    }
    if y == four_u() {
        let square = fp_mul_up(x, x)?;
        return fp_mul_up(square, square);
    }

    let raw = logexp_pow(x, y)?;
    let max_error = fp_add(fp_mul_up(raw, max_pow_relative_error())?, u(1))?;
    fp_add(raw, max_error)
}

// ===== LogExpMath (ported from Balancer V2 Solidity) =====
fn max_natural_exponent() -> I256 {
    parse_i_pos("130000000000000000000")
}

fn min_natural_exponent() -> I256 {
    -parse_i_pos("41000000000000000000")
}

fn ln36_lower_bound() -> I256 {
    parse_i_pos("900000000000000000")
}

fn ln36_upper_bound() -> I256 {
    parse_i_pos("1100000000000000000")
}

fn mild_exponent_bound() -> U256 {
    (U256::from(1u8) << 254) / U256::from(100_000_000_000_000_000_000u128)
}

fn logexp_pow(x: U256, y: U256) -> Result<U256, BalancerV2Error> {
    if y.is_zero() {
        return Ok(one_u());
    }
    if x.is_zero() {
        return Ok(U256::ZERO);
    }
    if (x >> 255) != U256::ZERO {
        return Err(BalancerV2Error::NotSupported("X_OUT_OF_BOUNDS".to_string()));
    }
    if y >= mild_exponent_bound() {
        return Err(BalancerV2Error::NotSupported("Y_OUT_OF_BOUNDS".to_string()));
    }

    let x_i = i_from_u(x);
    let y_i = i_from_u(y);
    let one18 = one_i18();

    let logx_times_y = if ln36_lower_bound() < x_i && x_i < ln36_upper_bound() {
        let ln36x = ln_36(x_i)?;
        let a = div_i(ln36x, one18)?;
        let b = rem_i(ln36x, one18)?;
        let t1 = mul_i(a, y_i)?;
        let t2 = div_i(mul_i(b, y_i)?, one18)?;
        add_i(t1, t2)?
    } else {
        mul_i(ln(x_i)?, y_i)?
    };
    let logx_times_y = div_i(logx_times_y, one18)?;

    if logx_times_y < min_natural_exponent() || logx_times_y > max_natural_exponent() {
        return Err(BalancerV2Error::NotSupported(
            "PRODUCT_OUT_OF_BOUNDS".to_string(),
        ));
    }
    u_from_i(exp(logx_times_y)?)
}

fn exp(mut x: I256) -> Result<I256, BalancerV2Error> {
    if x < min_natural_exponent() || x > max_natural_exponent() {
        return Err(BalancerV2Error::NotSupported("INVALID_EXPONENT".to_string()));
    }

    let one18 = one_i18();
    let one20 = one_i20();
    if x < I256::ZERO {
        return div_i(mul_i(one18, one18)?, exp(-x)?);
    }

    let x0 = parse_i_pos("128000000000000000000");
    let a0 = parse_i_pos("38877084059945950922200000000000000000000000000000000000");
    let x1 = parse_i_pos("64000000000000000000");
    let a1 = parse_i_pos("6235149080811616882910000000");

    let x2 = parse_i_pos("3200000000000000000000");
    let a2 = parse_i_pos("7896296018268069516100000000000000");
    let x3 = parse_i_pos("1600000000000000000000");
    let a3 = parse_i_pos("888611052050787263676000000");
    let x4 = parse_i_pos("800000000000000000000");
    let a4 = parse_i_pos("298095798704172827474000");
    let x5 = parse_i_pos("400000000000000000000");
    let a5 = parse_i_pos("5459815003314423907810");
    let x6 = parse_i_pos("200000000000000000000");
    let a6 = parse_i_pos("738905609893065022723");
    let x7 = parse_i_pos("100000000000000000000");
    let a7 = parse_i_pos("271828182845904523536");
    let x8 = parse_i_pos("50000000000000000000");
    let a8 = parse_i_pos("164872127070012814685");
    let x9 = parse_i_pos("25000000000000000000");
    let a9 = parse_i_pos("128402541668774148407");

    let first_an = if x >= x0 {
        x = sub_i(x, x0)?;
        a0
    } else if x >= x1 {
        x = sub_i(x, x1)?;
        a1
    } else {
        i(1)
    };

    x = mul_i(x, i(100))?;
    let mut product = one20;

    if x >= x2 {
        x = sub_i(x, x2)?;
        product = div_i(mul_i(product, a2)?, one20)?;
    }
    if x >= x3 {
        x = sub_i(x, x3)?;
        product = div_i(mul_i(product, a3)?, one20)?;
    }
    if x >= x4 {
        x = sub_i(x, x4)?;
        product = div_i(mul_i(product, a4)?, one20)?;
    }
    if x >= x5 {
        x = sub_i(x, x5)?;
        product = div_i(mul_i(product, a5)?, one20)?;
    }
    if x >= x6 {
        x = sub_i(x, x6)?;
        product = div_i(mul_i(product, a6)?, one20)?;
    }
    if x >= x7 {
        x = sub_i(x, x7)?;
        product = div_i(mul_i(product, a7)?, one20)?;
    }
    if x >= x8 {
        x = sub_i(x, x8)?;
        product = div_i(mul_i(product, a8)?, one20)?;
    }
    if x >= x9 {
        x = sub_i(x, x9)?;
        product = div_i(mul_i(product, a9)?, one20)?;
    }

    let mut series_sum = one20;
    let mut term = x;
    series_sum = add_i(series_sum, term)?;

    for n in 2..=12 {
        term = div_i(div_i(mul_i(term, x)?, one20)?, i(n))?;
        series_sum = add_i(series_sum, term)?;
    }

    div_i(mul_i(div_i(mul_i(product, series_sum)?, one20)?, first_an)?, i(100))
}

fn ln(a: I256) -> Result<I256, BalancerV2Error> {
    if a <= I256::ZERO {
        return Err(BalancerV2Error::NotSupported("OUT_OF_BOUNDS".to_string()));
    }
    if ln36_lower_bound() < a && a < ln36_upper_bound() {
        div_i(ln_36(a)?, one_i18())
    } else {
        ln_internal(a)
    }
}

fn ln_internal(mut a: I256) -> Result<I256, BalancerV2Error> {
    let one18 = one_i18();
    let one20 = one_i20();
    if a < one18 {
        return Ok(-ln_internal(div_i(mul_i(one18, one18)?, a)?)?);
    }

    let x0 = parse_i_pos("128000000000000000000");
    let a0 = parse_i_pos("38877084059945950922200000000000000000000000000000000000");
    let x1 = parse_i_pos("64000000000000000000");
    let a1 = parse_i_pos("6235149080811616882910000000");
    let x2 = parse_i_pos("3200000000000000000000");
    let a2 = parse_i_pos("7896296018268069516100000000000000");
    let x3 = parse_i_pos("1600000000000000000000");
    let a3 = parse_i_pos("888611052050787263676000000");
    let x4 = parse_i_pos("800000000000000000000");
    let a4 = parse_i_pos("298095798704172827474000");
    let x5 = parse_i_pos("400000000000000000000");
    let a5 = parse_i_pos("5459815003314423907810");
    let x6 = parse_i_pos("200000000000000000000");
    let a6 = parse_i_pos("738905609893065022723");
    let x7 = parse_i_pos("100000000000000000000");
    let a7 = parse_i_pos("271828182845904523536");
    let x8 = parse_i_pos("50000000000000000000");
    let a8 = parse_i_pos("164872127070012814685");
    let x9 = parse_i_pos("25000000000000000000");
    let a9 = parse_i_pos("128402541668774148407");
    let x10 = parse_i_pos("12500000000000000000");
    let a10 = parse_i_pos("113314845306682631683");
    let x11 = parse_i_pos("6250000000000000000");
    let a11 = parse_i_pos("106449445891785942956");

    let mut sum = I256::ZERO;
    if a >= mul_i(a0, one18)? {
        a = div_i(a, a0)?;
        sum = add_i(sum, x0)?;
    }
    if a >= mul_i(a1, one18)? {
        a = div_i(a, a1)?;
        sum = add_i(sum, x1)?;
    }

    sum = mul_i(sum, i(100))?;
    a = mul_i(a, i(100))?;

    if a >= a2 {
        a = div_i(mul_i(a, one20)?, a2)?;
        sum = add_i(sum, x2)?;
    }
    if a >= a3 {
        a = div_i(mul_i(a, one20)?, a3)?;
        sum = add_i(sum, x3)?;
    }
    if a >= a4 {
        a = div_i(mul_i(a, one20)?, a4)?;
        sum = add_i(sum, x4)?;
    }
    if a >= a5 {
        a = div_i(mul_i(a, one20)?, a5)?;
        sum = add_i(sum, x5)?;
    }
    if a >= a6 {
        a = div_i(mul_i(a, one20)?, a6)?;
        sum = add_i(sum, x6)?;
    }
    if a >= a7 {
        a = div_i(mul_i(a, one20)?, a7)?;
        sum = add_i(sum, x7)?;
    }
    if a >= a8 {
        a = div_i(mul_i(a, one20)?, a8)?;
        sum = add_i(sum, x8)?;
    }
    if a >= a9 {
        a = div_i(mul_i(a, one20)?, a9)?;
        sum = add_i(sum, x9)?;
    }
    if a >= a10 {
        a = div_i(mul_i(a, one20)?, a10)?;
        sum = add_i(sum, x10)?;
    }
    if a >= a11 {
        a = div_i(mul_i(a, one20)?, a11)?;
        sum = add_i(sum, x11)?;
    }

    let z = div_i(mul_i(sub_i(a, one20)?, one20)?, add_i(a, one20)?)?;
    let z2 = div_i(mul_i(z, z)?, one20)?;
    let mut num = z;
    let mut series_sum = num;

    for d in [3, 5, 7, 9, 11] {
        num = div_i(mul_i(num, z2)?, one20)?;
        series_sum = add_i(series_sum, div_i(num, i(d))?)?;
    }
    series_sum = mul_i(series_sum, i(2))?;
    div_i(add_i(sum, series_sum)?, i(100))
}

fn ln_36(mut x: I256) -> Result<I256, BalancerV2Error> {
    let one36 = one_i36();
    let one18 = one_i18();

    x = mul_i(x, one18)?;
    let z = div_i(mul_i(sub_i(x, one36)?, one36)?, add_i(x, one36)?)?;
    let z2 = div_i(mul_i(z, z)?, one36)?;
    let mut num = z;
    let mut series_sum = num;

    for d in [3, 5, 7, 9, 11, 13, 15] {
        num = div_i(mul_i(num, z2)?, one36)?;
        series_sum = add_i(series_sum, div_i(num, i(d))?)?;
    }
    mul_i(series_sum, i(2))
}

/// Weighted Math: calculateOutGivenIn (Balancer V2 rounding semantics).
pub fn calculate_out_given_in(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
    amount_in: U256,
) -> Result<U256, BalancerV2Error> {
    if amount_in > fp_mul_down(balance_in, U256::from(MAX_IN_RATIO))? {
        return Err(BalancerV2Error::NotSupported(
            "MAX_IN_RATIO exceeded".to_string(),
        ));
    }

    let denominator = fp_add(balance_in, amount_in)?;
    let base = fp_div_up(balance_in, denominator)?;
    let exponent = fp_div_down(weight_in, weight_out)?;
    let power = pow_up(base, exponent)?;
    fp_mul_down(balance_out, fp_complement(power))
}

/// Weighted Math: calculateInGivenOut (Balancer V2 rounding semantics).
pub fn calculate_in_given_out(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
    amount_out: U256,
) -> Result<U256, BalancerV2Error> {
    if amount_out > fp_mul_down(balance_out, U256::from(MAX_OUT_RATIO))? {
        return Err(BalancerV2Error::NotSupported(
            "MAX_OUT_RATIO exceeded".to_string(),
        ));
    }

    let base = fp_div_up(balance_out, fp_sub(balance_out, amount_out)?)?;
    let exponent = fp_div_up(weight_out, weight_in)?;
    let power = pow_up(base, exponent)?;
    let ratio = fp_sub(power, one_u())?;

    fp_mul_up(balance_in, ratio)
}

/// Spot price (kept as helper for diagnostics).
pub fn calculate_spot_price(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
) -> Result<f64, BalancerV2Error> {
    let bi_over_wi = fp_div_up(balance_in, weight_in)?;
    let bo_over_wo = fp_div_down(balance_out, weight_out)?;
    let sp = fp_div_up(bi_over_wi, bo_over_wo)?;
    let s = sp.to_string();
    let v = s
        .parse::<f64>()
        .map_err(|_| BalancerV2Error::NotSupported("u256->f64 conversion failed".to_string()))?;
    Ok(v / 1e18f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_given_out_inverse_property() {
        let balance_in = U256::from(2_000_000_000u64);
        let balance_out = U256::from(3_500_000_000u64);
        let weight_in = U256::from(8_000_000_000_000_000_000u128); // 0.8e18
        let weight_out = U256::from(2_000_000_000_000_000_000u128); // 0.2e18
        let target_out = U256::from(1_000_000u64);

        let amount_in = calculate_in_given_out(
            balance_in,
            weight_in,
            balance_out,
            weight_out,
            target_out,
        )
        .expect("in_given_out should solve");

        let out = calculate_out_given_in(
            balance_in,
            weight_in,
            balance_out,
            weight_out,
            amount_in,
        )
        .expect("out_given_in should work");
        assert!(out >= target_out);

        if amount_in > U256::ZERO {
            let out_less = calculate_out_given_in(
                balance_in,
                weight_in,
                balance_out,
                weight_out,
                amount_in - U256::from(1u8),
            )
            .expect("out_given_in should work");
            assert!(out_less < target_out);
        }
    }
}
