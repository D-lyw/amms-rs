//! 移植自 Balancer/Pendle 的 `LogExpMath.sol`
//!
//! 提供 18 位定点数的自然对数 (ln) 和自然指数 (exp)。
//! 所有输入输出均为 `I256`（= Solidity int256），1e18 固定点。
//! 完全对齐 Solidity `unchecked` 包装语义，保证与链上结果 1 wei 一致。

use alloy::primitives::{I256, U256};

const ONE_18: i128 = 1_000_000_000_000_000_000;
const ONE_20: i128 = 100_000_000_000_000_000_000;
const ONE_36: i128 = 1_000_000_000_000_000_000_000_000_000_000_000_000;

/// 将非负 u128 转为 I256
fn i(v: u128) -> I256 {
    I256::from_raw(U256::from(v))
}

/// 将 i128 转为 I256（支持负值）
fn i128_to_i256(v: i128) -> I256 {
    if v >= 0 {
        I256::from_raw(U256::from(v as u128))
    } else {
        I256::wrapping_neg(I256::from_raw(U256::from((-v) as u128)))
    }
}

/// 将字符串(十进制)解析为 I256，用于超长常量
fn i_from_str(s: &str) -> I256 {
    let u = U256::from_str_radix(s, 10).expect("无效的 I256 常量");
    I256::from_raw(u)
}

// ═════════════════════════════════════════════════════════════════════════
//   exp 分段常量
// ═════════════════════════════════════════════════════════════════════════
// 前两项无小数位（原始整数）
const X0: i128 = 128_000_000_000_000_000_000; // 2^7 * 1e18
const A0_STR: &str = "38877084059945950922200000000000000000000000000000000";
const X1: i128 = 64_000_000_000_000_000_000; // 2^6 * 1e18
const A1_STR: &str = "6235149080811616882910000000";

// 后续项用 20 位小数 (ONE_20)
const X2: i128 = 3_200_000_000_000_000_000_000; // 2^5 * 1e20
const A2_STR: &str = "7896296018268069516100000000000000";
const X3: i128 = 1_600_000_000_000_000_000_000; // 2^4 * 1e20
const A3_STR: &str = "888611052050787263676000000";
const X4: i128 = 800_000_000_000_000_000_000;
const A4_STR: &str = "298095798704172827474000";
const X5: i128 = 400_000_000_000_000_000_000;
const A5_STR: &str = "5459815003314423907810";
const X6: i128 = 200_000_000_000_000_000_000;
const A6_STR: &str = "738905609893065022723";
const X7: i128 = 100_000_000_000_000_000_000;
const A7_STR: &str = "271828182845904523536";
const X8: i128 = 50_000_000_000_000_000_000;
const A8_STR: &str = "164872127070012814685";
const X9: i128 = 25_000_000_000_000_000_000;
const A9_STR: &str = "128402541668774148407";
const X10: i128 = 12_500_000_000_000_000_000;
const A10_STR: &str = "113314845306682631683";
const X11: i128 = 6_250_000_000_000_000_000;
const A11_STR: &str = "106449445891785942956";

// ═════════════════════════════════════════════════════════════════════════
//   exp 域
// ═════════════════════════════════════════════════════════════════════════
const MAX_NATURAL_EXPONENT: i128 = 130_000_000_000_000_000_000; // 130e18
const MIN_NATURAL_EXPONENT: i128 = -41_000_000_000_000_000_000; // -41e18

// ln_36 域: [0.9e18, 1.1e18]
const LN_36_LOWER: i128 = 900_000_000_000_000_000; // 0.9e18
const LN_36_UPPER: i128 = 1_100_000_000_000_000_000; // 1.1e18

/// Wrapping 乘法，匹配 Solidity `unchecked { a * b }`
fn wmul(a: I256, b: I256) -> I256 {
    I256::wrapping_mul(a, b)
}

/// Wrapping 除法，匹配 Solidity `unchecked { a / b }`
fn wdiv(a: I256, b: I256) -> I256 {
    I256::wrapping_div(a, b)
}

/// Wrapping 加法
fn wadd(a: I256, b: I256) -> I256 {
    I256::wrapping_add(a, b)
}

/// Wrapping 减法
fn wsub(a: I256, b: I256) -> I256 {
    I256::wrapping_sub(a, b)
}

fn neg(x: I256) -> I256 { wsub(I256::ZERO, x) }

// ═════════════════════════════════════════════════════════════════════════
//   exp(x) — e^x, 18 位定点数
// ═════════════════════════════════════════════════════════════════════════
pub fn exp(x: I256) -> I256 {
    let min_exp = i128_to_i256(MIN_NATURAL_EXPONENT);
    let max_exp = i128_to_i256(MAX_NATURAL_EXPONENT);
    assert!(x >= min_exp && x <= max_exp, "exp: 指数越界");

    // e^(-x) = 1 / e^x
    if x < I256::ZERO {
        return wdiv(
            wmul(i(ONE_18 as u128), i(ONE_18 as u128)),
            exp(neg(x)),
        );
    }

    // 分解 x = sum(x_n), 其中 x_n = 2^(7-n), e^x_n = a_n
    let one20 = i(ONE_20 as u128);

    let first_an: I256;
    let mut xx = x;
    if xx >= i(X0 as u128) {
        xx = wsub(xx, i(X0 as u128));
        first_an = i_from_str(A0_STR);
    } else if xx >= i(X1 as u128) {
        xx = wsub(xx, i(X1 as u128));
        first_an = i_from_str(A1_STR);
    } else {
        first_an = i(1); // 1, no decimal places（同 Solidity firstAN = 1）
    }

    // 转到 20 位精度
    xx = wmul(xx, i(100));
    let mut product = one20;

    if xx >= i(X2 as u128) {
        xx = wsub(xx, i(X2 as u128));
        product = wdiv(wmul(product, i_from_str(A2_STR)), one20);
    }
    if xx >= i(X3 as u128) {
        xx = wsub(xx, i(X3 as u128));
        product = wdiv(wmul(product, i_from_str(A3_STR)), one20);
    }
    if xx >= i(X4 as u128) {
        xx = wsub(xx, i(X4 as u128));
        product = wdiv(wmul(product, i_from_str(A4_STR)), one20);
    }
    if xx >= i(X5 as u128) {
        xx = wsub(xx, i(X5 as u128));
        product = wdiv(wmul(product, i_from_str(A5_STR)), one20);
    }
    if xx >= i(X6 as u128) {
        xx = wsub(xx, i(X6 as u128));
        product = wdiv(wmul(product, i_from_str(A6_STR)), one20);
    }
    if xx >= i(X7 as u128) {
        xx = wsub(xx, i(X7 as u128));
        product = wdiv(wmul(product, i_from_str(A7_STR)), one20);
    }
    if xx >= i(X8 as u128) {
        xx = wsub(xx, i(X8 as u128));
        product = wdiv(wmul(product, i_from_str(A8_STR)), one20);
    }
    if xx >= i(X9 as u128) {
        xx = wsub(xx, i(X9 as u128));
        product = wdiv(wmul(product, i_from_str(A9_STR)), one20);
    }

    // 泰勒展开 e^x = 1 + x + x^2/2! + x^3/3! + ...
    let mut series = one20;
    let mut term = xx;

    // term 1: x
    series = wadd(series, term);

    // term 2~12: 依次乘 x 除以 n
    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(2));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(3));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(4));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(5));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(6));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(7));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(8));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(9));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(10));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(11));
    series = wadd(series, term);

    term = wdiv(wmul(term, xx), one20);
    term = wdiv(term, i(12));
    series = wadd(series, term);

    // (product * series / ONE_20) * first_an / 100
    let result = wdiv(wmul(product, series), one20);
    let result = wdiv(wmul(result, first_an), i(100));

    result
}

// ═════════════════════════════════════════════════════════════════════════
//   ln(x) — 自然对数, 18 位定点数
// ═════════════════════════════════════════════════════════════════════════
pub fn ln(x: I256) -> I256 {
    assert!(x > I256::ZERO, "ln: 参数必须为正");

    let lower = i(LN_36_LOWER as u128);
    let upper = i(LN_36_UPPER as u128);

    if x > lower && x < upper {
        wdiv(ln_36(x), i(ONE_18 as u128))
    } else {
        ln_general(x)
    }
}

/// _ln(a) — 通用对数
fn ln_general(mut a: I256) -> I256 {
    let one18 = i(ONE_18 as u128);
    let one20 = i(ONE_20 as u128);

    if a < one18 {
        // ln(a) = -ln(1/a)，必须为负
        let inv = wdiv(wmul(one18, one18), a);
        return neg(ln_general(inv));
    }

    // 分解 a 为 a_n 的乘积: ln(a * b) = ln(a) + ln(b)
    let mut sum: I256 = I256::ZERO;

    if a >= wmul(i_from_str(A0_STR), one18) {
        a = wdiv(a, i_from_str(A0_STR));
        sum = wadd(sum, i(X0 as u128));
    }
    if a >= wmul(i_from_str(A1_STR), one18) {
        a = wdiv(a, i_from_str(A1_STR));
        sum = wadd(sum, i(X1 as u128));
    }

    // 转到 20 位精度
    sum = wmul(sum, i(100));
    a = wmul(a, i(100));

    if a >= i_from_str(A2_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A2_STR));
        sum = wadd(sum, i(X2 as u128));
    }
    if a >= i_from_str(A3_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A3_STR));
        sum = wadd(sum, i(X3 as u128));
    }
    if a >= i_from_str(A4_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A4_STR));
        sum = wadd(sum, i(X4 as u128));
    }
    if a >= i_from_str(A5_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A5_STR));
        sum = wadd(sum, i(X5 as u128));
    }
    if a >= i_from_str(A6_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A6_STR));
        sum = wadd(sum, i(X6 as u128));
    }
    if a >= i_from_str(A7_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A7_STR));
        sum = wadd(sum, i(X7 as u128));
    }
    if a >= i_from_str(A8_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A8_STR));
        sum = wadd(sum, i(X8 as u128));
    }
    if a >= i_from_str(A9_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A9_STR));
        sum = wadd(sum, i(X9 as u128));
    }
    if a >= i_from_str(A10_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A10_STR));
        sum = wadd(sum, i(X10 as u128));
    }
    if a >= i_from_str(A11_STR) {
        a = wdiv(wmul(a, one20), i_from_str(A11_STR));
        sum = wadd(sum, i(X11 as u128));
    }

    // z = (a - ONE_20) / (a + ONE_20)
    let z_num = wsub(a, one20);
    let z_den = wadd(a, one20);
    let z = wdiv(wmul(z_num, one20), z_den);
    let z_sq = wdiv(wmul(z, z), one20);

    let mut num = z;
    let mut series = num;

    num = wdiv(wmul(num, z_sq), one20);
    series = wadd(series, wdiv(num, i(3)));

    num = wdiv(wmul(num, z_sq), one20);
    series = wadd(series, wdiv(num, i(5)));

    num = wdiv(wmul(num, z_sq), one20);
    series = wadd(series, wdiv(num, i(7)));

    num = wdiv(wmul(num, z_sq), one20);
    series = wadd(series, wdiv(num, i(9)));

    num = wdiv(wmul(num, z_sq), one20);
    series = wadd(series, wdiv(num, i(11)));

    series = wmul(series, i(2));

    wdiv(wadd(sum, series), i(100))
}

/// _ln_36(x) — 高精度对数，x 应接近 1
fn ln_36(mut x: I256) -> I256 {
    let one36 = i(ONE_36 as u128);
    let one18 = i(ONE_18 as u128);

    x = wmul(x, one18);

    // z = (x - ONE_36) / (x + ONE_36)
    let z_num = wsub(x, one36);
    let z_den = wadd(x, one36);
    let z = wdiv(wmul(z_num, one36), z_den);
    let z_sq = wdiv(wmul(z, z), one36);

    let mut num = z;
    let mut series = num;

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(3)));

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(5)));

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(7)));

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(9)));

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(11)));

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(13)));

    num = wdiv(wmul(num, z_sq), one36);
    series = wadd(series, wdiv(num, i(15)));

    wmul(series, i(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e18(v: i128) -> I256 {
        i128_to_i256(v)
    }

    #[test]
    fn test_exp_zero() {
        let result = exp(I256::ZERO);
        assert_eq!(result, e18(ONE_18));
    }

    #[test]
    fn test_exp_one() {
        // e^1 ≈ 2.718281828459045235
        let result = exp(e18(ONE_18));
        let expected = "2718281828459045236"; // ~2.718 * 1e18
        let upper = U256::from_str_radix(expected, 10).unwrap();
        assert!(result.into_raw() < upper + U256::from(1000));
        assert!(result.into_raw() > upper - U256::from(1000));
    }

    #[test]
    fn test_ln_one() {
        let result = ln(e18(ONE_18));
        assert_eq!(result, I256::ZERO);
    }

    #[test]
    fn test_exp_ln_roundtrip() {
        // exp(ln(x)) == x
        let vals = [1e17 as i128, ONE_18, 2 * ONE_18, 5 * ONE_18, 10 * ONE_18];
        for &v in &vals {
            let x = e18(v);
            let l = ln(x);
            let e = exp(l);
            let diff = if e > x { e - x } else { x - e };
            assert!(diff < e18(100), "exp(ln({})) deviates by {}", v, diff);
        }
    }

    #[test]
    fn test_ln_exp_roundtrip() {
        // ln(exp(x)) == x
        let vals = [-10i128.pow(17), -ONE_18 / 10, 0, ONE_18 / 10, ONE_18 / 2, ONE_18];
        for &v in &vals {
            let x = e18(v);
            let e = exp(x);
            let l = ln(e);
            let diff = if l > x { l - x } else { x - l };
            assert!(diff < e18(1000), "ln(exp({})) deviates by {}", v, diff);
        }
    }

    #[test]
    fn test_exp_ln_known_values() {
        // e^0.05 ≈ 1.051271096376024526
        let exp_005 = exp(e18(5 * 10i128.pow(16))); // 0.05e18
        let exp_raw = exp_005.into_raw();
        let expected = U256::from(1_051_271_096_376_024_526u128);
        let diff = if exp_raw > expected { exp_raw - expected } else { expected - exp_raw };
        assert!(diff < U256::from(1000), "exp(0.05)偏差: {}", diff);

        // ln(1.05) ≈ 0.048790164169432
        let ln_105 = ln(i(1_050_000_000_000_000_000));
        let ln_raw = ln_105.into_raw();
        let expected_ln = U256::from(48_790_164_169_432_000u128);
        let diff_ln = if ln_raw > expected_ln { ln_raw - expected_ln } else { expected_ln - ln_raw };
        assert!(diff_ln < U256::from(1000), "ln(1.05)偏差: {}", diff_ln);
    }
}
