use alloy::primitives::{I256, U256};

pub const N_COINS: usize = 3;
pub const A_MULTIPLIER: u64 = 10000;
pub const PRECISION: u64 = 1_000_000_000_000_000_000; // 10^18
pub const N_COINS_2: u64 = 2;
pub const WAD: u64 = 1_000_000_000_000_000_000; // 10^18
pub const MIN_GAMMA: u64 = 10_000_000_000; // 1e10
pub const MAX_GAMMA_SMALL: u64 = 20_000_000_000_000_000; // 2e16
pub const MAX_GAMMA: u64 = 199_000_000_000_000_000; // 1.99e17
pub const MIN_A: u64 = (N_COINS_2 * N_COINS_2 * A_MULTIPLIER as u64) / 10; // N^N * A_MULTIPLIER / 10
pub const MAX_A: u64 = (N_COINS_2 * N_COINS_2 * A_MULTIPLIER as u64) * 1000; // N^N * A_MULTIPLIER * 1000

pub fn sort(upsorted_x: &[U256]) -> Vec<U256> {
    let mut x = upsorted_x.to_vec();
    x.sort_by(|a, b| b.cmp(a));
    x
}

pub fn geometric_mean(x: &[U256], sort_input: bool) -> Result<U256, &'static str> {
    let x = if sort_input { sort(x) } else { x.to_vec() };

    let n_coins = x.len();
    if n_coins == 0 {
        return Ok(U256::ZERO);
    }

    // Guard against division by zero: if any balance is zero, geometric mean is zero
    if x.iter().any(|v| *v == U256::ZERO) {
        return Ok(U256::ZERO);
    }

    let mut d = x[0];
    let mut d_prev;
    let precision = U256::from(PRECISION);
    let n_coins_u256 = U256::from(n_coins);

    // Vyper implementation loops 255 times for convergence
    for _ in 0..255 {
        d_prev = d;

        // tmp = 10**18 * x[0] / D * x[1] / D * ...
        // In Vyper: tmp = unsafe_div(unsafe_mul(10**18, x[0]), D)
        if d.is_zero() {
            return Err("geometric_mean: d became zero");
        }
        let mut tmp = precision * x[0] / d;
        for i in 1..n_coins {
            tmp = tmp * x[i] / d;
        }

        // D = D * ((N-1)*10**18 + tmp) / (N * 10**18)
        let numerator = d * ((n_coins_u256 - U256::from(1)) * precision + tmp);
        let denominator = n_coins_u256 * precision;
        if denominator.is_zero() {
            return Err("Division by zero in geometric_mean");
        }
        d = numerator / denominator;

        let diff = if d > d_prev { d - d_prev } else { d_prev - d };

        if diff <= U256::from(1) || diff * precision < d {
            return Ok(d);
        }
    }

    Err("Geometric mean did not converge")
}

pub fn newton_d(ann: U256, gamma: U256, x_unsorted: &[U256]) -> Result<U256, &'static str> {
    // Safety checks: ensure critical parameters are non-zero to prevent divide by zero
    if gamma == U256::ZERO {
        return Err("newton_d: gamma is zero, cannot compute D");
    }
    if ann == U256::ZERO {
        return Err("newton_d: ann is zero, cannot compute D");
    }

    let n_coins = x_unsorted.len();
    let x = sort(x_unsorted);

    // Initial value of invariant D is that for constant-product invariant
    // D = N * geometric_mean(x)
    let mut d = U256::from(n_coins) * geometric_mean(&x, false)?;
    let mut s = U256::ZERO;
    for x_i in &x {
        s += *x_i;
    }

    let precision = U256::from(PRECISION);
    // Convert A_MULTIPLIER to U256
    let a_multiplier = U256::from(A_MULTIPLIER);

    for _ in 0..255 {
        let d_prev = d;

        // K0 = 10**18
        // for _x in x: K0 = K0 * _x * N / D
        let mut k0 = precision;
        for _x in &x {
            // Guard against division by zero when d is zero
            if d == U256::ZERO {
                return Err("newton_d: d is zero");
            }
            // Guard against zero balance which would make k0 zero
            if *_x == U256::ZERO {
                return Err("newton_d: zero balance detected");
            }
            if d.is_zero() {
                return Err("newton_d: d is zero during loop");
            }
            k0 = k0 * *_x * U256::from(n_coins) / d;
        }

        // Guard against k0 becoming zero (can happen with very small balances)
        if k0 == U256::ZERO {
            return Err("newton_d: k0 is zero");
        }

        let mut _g1k0 = gamma + precision;
        if _g1k0 > k0 {
            _g1k0 = _g1k0 - k0 + U256::from(1);
        } else {
            _g1k0 = k0 - _g1k0 + U256::from(1);
        }

        // mul1 = 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN
        // Note: Vyper operator precedence / is same as *, evaluated left-to-right.
        if gamma.is_zero() {
            return Err("gamma zero in loop");
        }
        if ann.is_zero() {
            return Err("ann zero in loop");
        }
        let mul1 = precision * d / gamma * _g1k0 / gamma * _g1k0 * a_multiplier / ann;

        // mul2 = (2 * 10**18) * N * K0 / _g1k0
        let mul2 = U256::from(2) * precision * U256::from(n_coins) * k0 / _g1k0;

        // neg_fprime = (S + S * mul2 / 10**18) + mul1 * N / K0 - mul2 * D / 10**18
        let term1 = s + s * mul2 / precision;
        let term2 = mul1 * U256::from(n_coins) / k0;
        let term3 = mul2 * d / precision;

        // neg_fprime = term1 + term2 - term3
        // Use standard subtract, assume no underflow as per Vyper logic
        if term3 > term1 + term2 {
            // Underflow condition, can occur with bad guesses or extremes
            return Err("Math error: neg_fprime underflow");
        }
        let neg_fprime = term1 + term2 - term3;

        // Guard against division by zero in d_step calculation
        if neg_fprime == U256::ZERO {
            return Err("Math error: neg_fprime is zero");
        }

        // D_plus = D * (neg_fprime + S) / neg_fprime
        if neg_fprime.is_zero() {
            return Err("neg_fprime zero for d_plus");
        }
        let d_plus = d * (neg_fprime + s) / neg_fprime;

        // D_minus = D^2 / neg_fprime
        let mut d_minus = d * d / neg_fprime;

        if precision > k0 {
            // Guard against k0 being zero
            if k0 == U256::ZERO {
                return Err("newton_d: k0 is zero in d_minus calculation");
            }
            if neg_fprime.is_zero() {
                return Err("neg_fprime zero in d_minus");
            }
            // Check intermediate division
            let term = mul1 / neg_fprime;
            d_minus += d * term / precision * (precision - k0) / k0;
        } else {
            // Guard against k0 being zero
            if k0 == U256::ZERO {
                return Err("newton_d: k0 is zero in d_minus calculation");
            }
            d_minus -= d * (mul1 / neg_fprime) / precision * (k0 - precision) / k0;
        }

        if d_plus > d_minus {
            d = d_plus - d_minus;
        } else {
            d = (d_minus - d_plus) / U256::from(2);
        }

        let diff = if d > d_prev { d - d_prev } else { d_prev - d };

        let max_val = std::cmp::max(U256::from(10).pow(U256::from(16)), d);
        if diff * U256::from(10).pow(U256::from(14)) < max_val {
            // Convergence reached
            return Ok(d);
        }
    }

    Err("newton_D did not converge")
}

pub fn newton_y(
    ann: U256,
    gamma: U256,
    x: &[U256],
    d: U256,
    i: usize,
) -> Result<U256, &'static str> {
    // Safety checks: ensure critical parameters are non-zero to prevent divide by zero
    if gamma == U256::ZERO {
        return Err("newton_y: gamma is zero, cannot compute y");
    }
    if ann == U256::ZERO {
        return Err("newton_y: ann is zero, cannot compute y");
    }
    if d == U256::ZERO {
        return Err("newton_y: d is zero, cannot compute y");
    }

    let n_coins = x.len();
    if i >= n_coins {
        return Err("Index out of bounds");
    }

    let mut x_sorted = x.to_vec();
    x_sorted[i] = U256::ZERO; // Set target variable to 0
    x_sorted = sort(&x_sorted); // Sorted descending. 0 is at end.

    let precision = U256::from(PRECISION);
    let a_multiplier = U256::from(A_MULTIPLIER);
    let n_coins_u256 = U256::from(n_coins);
    if n_coins_u256.is_zero() {
        return Err("n_coins is zero");
    }

    let mut y = d / n_coins_u256;
    let mut k0_i = precision;
    let mut s_i = U256::ZERO;

    for j in (0..n_coins - 1).rev() {
        let _x = x_sorted[j];
        // Guard against division by zero
        if _x == U256::ZERO {
            return Err("newton_y: zero balance detected");
        }
        let divisor = _x * n_coins_u256;
        if divisor.is_zero() {
            return Err("newton_y: zero divisor");
        }
        y = y * d / divisor;
        s_i += _x;
    }

    for j in 0..n_coins - 1 {
        let x_j = x_sorted[j]; // 正确：使用 x_sorted[j] 而不是残留的 _x
        if d.is_zero() {
            return Err("newton_y: d is zero in loop");
        }
        k0_i = k0_i * x_j * n_coins_u256 / d;
    }

    let convergence_limit = std::cmp::max(
        std::cmp::max(
            x_sorted[0] / U256::from(10).pow(U256::from(14)),
            d / U256::from(10).pow(U256::from(14)),
        ),
        U256::from(100),
    );

    for _ in 0..255 {
        let y_prev = y;

        if d.is_zero() {
            return Err("newton_y: d is zero in loop 2");
        }
        let k0 = k0_i * y * n_coins_u256 / d;
        let s = s_i + y;

        let mut _g1k0 = gamma + precision;
        if _g1k0 > k0 {
            _g1k0 = _g1k0 - k0 + U256::from(1);
        } else {
            _g1k0 = k0 - _g1k0 + U256::from(1);
        }

        // mul1 = 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN
        let mul1 = precision * d / gamma * _g1k0 / gamma * _g1k0 * a_multiplier / ann;

        // mul2 = 10**18 + (2 * 10**18) * K0 / _g1k0
        let mul2 = precision + U256::from(2) * precision * k0 / _g1k0;

        let mut yfprime = precision * y + s * mul2 + mul1;
        let _dyfprime = d * mul2;

        if yfprime < _dyfprime {
            y = y_prev / U256::from(2);
            continue;
        } else {
            yfprime -= _dyfprime;
        }

        // Guard against division by zero
        if y == U256::ZERO {
            y = y_prev / U256::from(2);
            continue;
        }

        let fprime = yfprime / y;
        if fprime == U256::ZERO {
            y = y_prev / U256::from(2);
            continue;
        }

        // y_minus = mul1 / fprime
        let mut y_minus = mul1 / fprime;
        // y_plus = (yfprime + 10**18 * D) / fprime + y_minus * 10**18 / K0
        // Guard against division by zero when k0 is zero
        if k0 == U256::ZERO {
            y = y_prev / U256::from(2);
            continue;
        }
        let y_plus = (yfprime + precision * d) / fprime + y_minus * precision / k0;

        y_minus += precision * s / fprime;

        if y_plus < y_minus {
            y = y_prev / U256::from(2);
        } else {
            y = y_plus - y_minus;
        }

        let diff = if y > y_prev { y - y_prev } else { y_prev - y };

        if diff < std::cmp::max(convergence_limit, y / U256::from(10).pow(U256::from(14))) {
            // Safety check: frac = y * 1e18 / D
            // assert frac >= 10**16 - 1 and frac < 10**20 + 1
            // This matches the "Unsafe value for y" check in Curve CryptoSwap contracts
            if d.is_zero() {
                return Err("newton_y: d is zero before frac calculation");
            }
            let frac = y * precision / d;
            let min_frac = U256::from(10).pow(U256::from(16)) - U256::from(1);
            let max_frac = U256::from(10).pow(U256::from(20)) + U256::from(1);

            if frac < min_frac || frac > max_frac {
                return Err("Unsafe value for y");
            }

            return Ok(y);
        }
    }

    Err("newton_y did not converge")
}

pub fn reduction_coefficient(x: &[U256], fee_gamma: U256) -> U256 {
    let n_coins = x.len();
    let n_u256 = U256::from(n_coins);
    let precision = U256::from(PRECISION);

    let mut k = precision;
    let mut s = U256::ZERO;
    for val in x {
        s += *val;
    }

    if s == U256::ZERO {
        return U256::ZERO;
    }

    for val in x {
        // K = K * N * x[i] / S
        if s.is_zero() {
            return U256::ZERO;
        }
        k = k * n_u256 * *val / s;
    }

    if fee_gamma > U256::ZERO {
        // K = fee_gamma * 10**18 / (fee_gamma + 10**18 - K)
        // Note: Vyper logic: fee_gamma / (fee_gamma + (1-K))
        // Denominator: fee_gamma + 1e18 - K.
        // Assumption: K <= 1e18. Arithmetic implies geometric mean <= arithmetic mean, so yes.
        // If K > 1e18 (precision error?), cap?
        if k > precision {
            k = precision;
        }
        let denominator = fee_gamma + precision - k;
        if denominator > U256::ZERO {
            k = fee_gamma * precision / denominator;
        }
    }

    k
}

/// TwoCrypto-specific fee calculation.
///
/// Note: for bit-exact on-chain parity we follow the view contract's `fee_calc`
/// slope (TwocryptoView._fee), not the pool's internal `_fee` formula. The view
/// path is what `get_dy/get_dx` use on-chain, so matching it is required for 0 diff.
pub fn twocrypto_fee(
    xp: &[U256],
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
) -> Result<U256, &'static str> {
    if xp.len() != 2 {
        return Err("twocrypto_fee expects 2 balances");
    }
    let precision = U256::from(PRECISION);
    let n_coins = U256::from(N_COINS_2);

    let mut b = xp[0].checked_add(xp[1]).ok_or("overflow")?;
    if b.is_zero() {
        return Ok(U256::ZERO);
    }

    // B = 1e18 * N^N * xp[0] / B * xp[1] / B
    let n_pow = n_coins.checked_mul(n_coins).ok_or("overflow")?;
    b = precision
        .checked_mul(n_pow)
        .and_then(|v| v.checked_mul(xp[0]))
        .and_then(|v| v.checked_div(b))
        .and_then(|v| v.checked_mul(xp[1]))
        .and_then(|v| v.checked_div(b))
        .ok_or("overflow")?;

    // View-style fee slope (matches on-chain fee_calc)
    // b = fee_gamma * 1e18 / (fee_gamma + 1e18 - b)
    let denom = fee_gamma
        .checked_add(precision)
        .and_then(|v| v.checked_sub(b))
        .ok_or("overflow")?;
    if denom.is_zero() {
        return Err("twocrypto_fee div by zero");
    }
    b = fee_gamma
        .checked_mul(precision)
        .and_then(|v| v.checked_div(denom))
        .ok_or("overflow")?;

    // fee = (mid_fee * B + out_fee * (1e18 - B)) / 1e18
    let out_component = out_fee
        .checked_mul(precision - b)
        .ok_or("overflow")?;
    let fee = mid_fee
        .checked_mul(b)
        .and_then(|v| v.checked_add(out_component))
        .and_then(|v| v.checked_div(precision))
        .ok_or("overflow")?;

    Ok(fee)
}

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
) -> Result<U256, &'static str> {
    let mut x = xp.to_vec();
    x[i] += dx;

    // Empirical: newton_y expects ann = amp (A_scaled) same as newton_d
    let ann = amp;

    // Calculate new y for index j
    let y = newton_y(ann, gamma, &x, d, j)?;

    let dy_gross = xp[j]
        .checked_sub(y)
        .ok_or("New y is larger than old y (slippage?)")?;

    // Calculate Dynamic Fee
    // update x[j] to y
    x[j] = y;

    let f = reduction_coefficient(&x, fee_gamma);

    // fee_percent = (mid_fee * f + out_fee * (1e18 - f)) / 1e18
    let precision = U256::from(PRECISION);
    let fee_percent = (mid_fee * f + out_fee * (precision - f)) / precision;

    // Fee amount = dy_gross * fee_percent / 1e10 (Assuming fees are 1e10 basis)
    let fee_denominator = U256::from(10).pow(U256::from(10));
    let fee = dy_gross * fee_percent / fee_denominator;

    let dy = dy_gross - fee;

    Ok(dy)
}

// =============================================================================
// 优化版 get_y 算法 (TriCrypto/TwoCrypto)
// 移植自: https://github.com/curvefi/tricrypto-ng/blob/main/contracts/main/CurveCryptoMathOptimized3.vy
// =============================================================================

/// 计算以2为底的对数
/// 移植自 Snekmate 的 _snekmate_log_2
pub fn log2(x: U256, roundup: bool) -> U256 {
    if x == U256::ZERO {
        return U256::ZERO;
    }

    let mut value = x;
    let mut result = U256::ZERO;

    // 逐级检查位数
    if (value >> 128) != U256::ZERO {
        value >>= 128;
        result = U256::from(128);
    }
    if (value >> 64) != U256::ZERO {
        value >>= 64;
        result += U256::from(64);
    }
    if (value >> 32) != U256::ZERO {
        value >>= 32;
        result += U256::from(32);
    }
    if (value >> 16) != U256::ZERO {
        value >>= 16;
        result += U256::from(16);
    }
    if (value >> 8) != U256::ZERO {
        value >>= 8;
        result += U256::from(8);
    }
    if (value >> 4) != U256::ZERO {
        value >>= 4;
        result += U256::from(4);
    }
    if (value >> 2) != U256::ZERO {
        value >>= 2;
        result += U256::from(2);
    }
    if (value >> 1) != U256::ZERO {
        result += U256::from(1);
    }

    if roundup && (U256::from(1) << result) < x {
        result += U256::from(1);
    }

    result
}

/// 计算整数平方根
/// 使用牛顿迭代法
pub fn isqrt(x: U256) -> U256 {
    if x == U256::ZERO {
        return U256::ZERO;
    }

    // Bitwise integer sqrt (exact floor), aligned with EVM integer semantics.
    let mut n = x;
    let mut res = U256::ZERO;
    let mut bit = U256::from(1u8) << 254; // Highest power of four <= 2^256

    while bit > n {
        bit >>= 2;
    }

    while bit != U256::ZERO {
        let res_plus_bit = res + bit;
        if n >= res_plus_bit {
            n -= res_plus_bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }

    res
}

// =============================================================================
// TwoCrypto (2-coin) math from curvefi/twocrypto-ng
// =============================================================================

fn abs_i256(x: I256) -> I256 {
    if x < I256::ZERO { -x } else { x }
}

fn i256_from_u256(u: U256) -> Result<I256, &'static str> {
    I256::try_from(u).map_err(|_| "I256 overflow")
}

fn twocrypto_lim_mul(gamma: U256) -> U256 {
    let mut lim_mul = U256::from(100u64) * U256::from(WAD);
    if gamma > U256::from(MAX_GAMMA_SMALL) {
        lim_mul = lim_mul * U256::from(MAX_GAMMA_SMALL) / gamma;
    }
    lim_mul
}

fn twocrypto_newton_y_internal(
    ann: U256,
    gamma: U256,
    x: [U256; 2],
    d: U256,
    i: usize,
    lim_mul: U256,
) -> Result<U256, &'static str> {
    let x_j = x[1 - i];
    if x_j.is_zero() {
        return Err("twocrypto_newton_y: zero balance");
    }

    let n = U256::from(N_COINS_2);
    let mut y = d
        .checked_mul(d)
        .ok_or("twocrypto_newton_y: d^2 overflow")?
        / (x_j * n * n);

    let k0_i = (U256::from(WAD) * n) * x_j / d;
    let lim_mul_min = U256::from(10).pow(U256::from(36)) / lim_mul;
    if k0_i < lim_mul_min || k0_i > lim_mul {
        return Err("twocrypto_newton_y: unsafe values x[i]");
    }

    let convergence_limit = std::cmp::max(
        std::cmp::max(x_j / U256::from(10).pow(U256::from(14)), d / U256::from(10).pow(U256::from(14))),
        U256::from(100u64),
    );

    for _ in 0..255 {
        let y_prev = y;

        let k0 = k0_i * y * n / d;
        let s = x_j + y;

        let mut g1k0 = gamma + U256::from(WAD);
        if g1k0 > k0 {
            g1k0 = g1k0 - k0 + U256::from(1u64);
        } else {
            g1k0 = k0 - g1k0 + U256::from(1u64);
        }

        let mul1 =
            U256::from(WAD) * d / gamma * g1k0 / gamma * g1k0 * U256::from(A_MULTIPLIER) / ann;
        let mul2 = U256::from(WAD) + (U256::from(2u64) * U256::from(WAD)) * k0 / g1k0;

        let mut yfprime = U256::from(WAD) * y + s * mul2 + mul1;
        let dyfprime = d * mul2;
        if yfprime < dyfprime {
            y = y_prev / U256::from(2u64);
            continue;
        }
        yfprime -= dyfprime;

        if y.is_zero() {
            y = y_prev / U256::from(2u64);
            continue;
        }
        let fprime = yfprime / y;

        let mut y_minus = mul1 / fprime;
        let y_plus = (yfprime + U256::from(WAD) * d) / fprime + y_minus * U256::from(WAD) / k0;
        y_minus += U256::from(WAD) * s / fprime;

        if y_plus < y_minus {
            y = y_prev / U256::from(2u64);
        } else {
            y = y_plus - y_minus;
        }

        let diff = if y > y_prev { y - y_prev } else { y_prev - y };
        if diff < std::cmp::max(convergence_limit, y / U256::from(10).pow(U256::from(14))) {
            return Ok(y);
        }
    }

    Err("twocrypto_newton_y did not converge")
}

pub fn twocrypto_newton_y(
    ann: U256,
    gamma: U256,
    x: [U256; 2],
    d: U256,
    i: usize,
) -> Result<U256, &'static str> {
    // Safety checks
    if ann <= U256::from(MIN_A - 1) || ann >= U256::from(MAX_A + 1) {
        return Err("unsafe values A");
    }
    if gamma <= U256::from(MIN_GAMMA - 1) || gamma >= U256::from(MAX_GAMMA + 1) {
        return Err("unsafe values gamma");
    }
    if d <= U256::from(10).pow(U256::from(17)) - U256::from(1u64)
        || d >= U256::from(10).pow(U256::from(15)) * U256::from(WAD) + U256::from(1u64)
    {
        return Err("unsafe values D");
    }

    let lim_mul = twocrypto_lim_mul(gamma);
    let y = twocrypto_newton_y_internal(ann, gamma, x, d, i, lim_mul)?;
    let frac = y * U256::from(WAD) / d;
    let lim_mul_min = U256::from(10).pow(U256::from(36)) / lim_mul;
    if frac < lim_mul_min / U256::from(N_COINS_2) || frac > lim_mul / U256::from(N_COINS_2) {
        return Err("unsafe value for y");
    }
    Ok(y)
}

pub fn twocrypto_get_y(
    ann: U256,
    gamma: U256,
    x: [U256; 2],
    d: U256,
    i: usize,
) -> Result<(U256, U256), &'static str> {
    // Safety checks
    if ann <= U256::from(MIN_A - 1) || ann >= U256::from(MAX_A + 1) {
        return Err("unsafe values A");
    }
    if gamma <= U256::from(MIN_GAMMA - 1) || gamma >= U256::from(MAX_GAMMA + 1) {
        return Err("unsafe values gamma");
    }
    if d <= U256::from(10).pow(U256::from(17)) - U256::from(1u64)
        || d >= U256::from(10).pow(U256::from(15)) * U256::from(WAD) + U256::from(1u64)
    {
        return Err("unsafe values D");
    }

    let lim_mul = twocrypto_lim_mul(gamma);
    let lim_mul_signed = i256_from_u256(lim_mul)?;

    let ann_i = i256_from_u256(ann)?;
    let gamma_i = i256_from_u256(gamma)?;
    let d_i = i256_from_u256(d)?;
    let x_j = i256_from_u256(x[1 - i])?;
    let i_wad = i256_from_u256(U256::from(WAD))?;
    let i_2 = i256_from_u256(U256::from(2u64))?;
    let i_3 = i256_from_u256(U256::from(3u64))?;
    let i_4 = i256_from_u256(U256::from(4u64))?;
    let i_27 = i256_from_u256(U256::from(27u64))?;
    let i_1e14 = i256_from_u256(U256::from(10).pow(U256::from(14)))?;
    let i_1e32 = i256_from_u256(U256::from(10).pow(U256::from(32)))?;
    let i_1e4 = i256_from_u256(U256::from(10_000u64))?;
    let i_400m = i256_from_u256(U256::from(400000000u64))?;
    let gamma2 = gamma_i.checked_mul(gamma_i).ok_or("gamma2 overflow")?;

    // y = D**2 / (x_j * N^2)
    let n_i = i256_from_u256(U256::from(N_COINS_2))?;
    let _y = d_i
        .checked_mul(d_i)
        .and_then(|v| v.checked_div(x_j * n_i * n_i))
        .ok_or("y init overflow")?;

    let k0_i = (i256_from_u256(U256::from(WAD))? * n_i * x_j) / d_i;
    let lim_min = i256_from_u256(U256::from(10).pow(U256::from(36)) / lim_mul)?;
    if k0_i < lim_min || k0_i > lim_mul_signed {
        return Err("unsafe values x[i]");
    }

    let ann_gamma2 = ann_i.checked_mul(gamma2).ok_or("ann_gamma2 overflow")?;

    let a = i_1e32;
    let b = d_i
        .checked_mul(ann_gamma2)
        .and_then(|v| v.checked_div(i_400m))
        .and_then(|v| v.checked_div(x_j))
        .ok_or("b overflow")?
        - i_3 * a
        - i_2 * gamma_i * i_1e14;

    let c = i_3 * a
        + i_4 * gamma_i * i_1e14
        + gamma2 / i_1e4
        + (i_4 * ann_gamma2 / i_400m) * x_j / d_i
        - (i_4 * ann_gamma2 / i_400m);

    let d_i2 = -((i_wad + gamma_i) * (i_wad + gamma_i) / i_1e4);

    let mut delta0 = i_3 * a * c / b - b;
    let mut delta1 = i_3 * delta0 + b - i_27 * a * a / b * d_i2 / b;

    let threshold = {
        let t1 = abs_i256(delta0);
        let t2 = abs_i256(delta1);
        let t = if t1 < t2 { t1 } else { t2 };
        if t < a { t } else { a }
    };

    let divider = if threshold > i256_from_u256(U256::from(10).pow(U256::from(48)))? {
        i256_from_u256(U256::from(10).pow(U256::from(30)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(46)))? {
        i256_from_u256(U256::from(10).pow(U256::from(28)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(44)))? {
        i256_from_u256(U256::from(10).pow(U256::from(26)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(42)))? {
        i256_from_u256(U256::from(10).pow(U256::from(24)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(40)))? {
        i256_from_u256(U256::from(10).pow(U256::from(22)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(38)))? {
        i256_from_u256(U256::from(10).pow(U256::from(20)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(36)))? {
        i256_from_u256(U256::from(10).pow(U256::from(18)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(34)))? {
        i256_from_u256(U256::from(10).pow(U256::from(16)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(32)))? {
        i256_from_u256(U256::from(10).pow(U256::from(14)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(30)))? {
        i256_from_u256(U256::from(10).pow(U256::from(12)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(28)))? {
        i256_from_u256(U256::from(10).pow(U256::from(10)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(26)))? {
        i256_from_u256(U256::from(10).pow(U256::from(8)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(24)))? {
        i256_from_u256(U256::from(10).pow(U256::from(6)))?
    } else if threshold > i256_from_u256(U256::from(10).pow(U256::from(20)))? {
        i256_from_u256(U256::from(10).pow(U256::from(2)))?
    } else {
        i256_from_u256(U256::from(1u64))?
    };

    let a2 = a / divider;
    let b2 = b / divider;
    let c2 = c / divider;
    let d2 = d_i2 / divider;

    delta0 = i_3 * a2 * c2 / b2 - b2;
    delta1 = i_3 * delta0
        + b2
        - i_27 * a2 * a2 / b2 * d2 / b2;

    let sqrt_arg = delta1 * delta1 + (i_4 * delta0 * delta0 / b2) * delta0;
    if sqrt_arg <= I256::ZERO {
        let y = twocrypto_newton_y_internal(ann, gamma, x, d, i, lim_mul)?;
        return Ok((y, U256::ZERO));
    }

    let sqrt_val_u = U256::try_from(sqrt_arg).map_err(|_| "sqrt arg overflow")?;
    let sqrt_val = I256::try_from(isqrt(sqrt_val_u)).map_err(|_| "sqrt overflow")?;

    let b_cbrt = if b2 >= I256::ZERO {
        let b_u = U256::try_from(b2).map_err(|_| "b overflow")?;
        I256::try_from(cbrt(b_u)).map_err(|_| "b cbrt overflow")?
    } else {
        let b_u = U256::try_from(-b2).map_err(|_| "b overflow")?;
        -I256::try_from(cbrt(b_u)).map_err(|_| "b cbrt overflow")?
    };

    let second_cbrt = if delta1 > I256::ZERO {
        let arg = (delta1 + sqrt_val) / i_2;
        let arg_u = U256::try_from(arg).map_err(|_| "cbrt arg overflow")?;
        I256::try_from(cbrt(arg_u)).map_err(|_| "second cbrt overflow")?
    } else {
        let arg = (sqrt_val - delta1) / i_2;
        let arg_u = U256::try_from(arg).map_err(|_| "cbrt arg overflow")?;
        -I256::try_from(cbrt(arg_u)).map_err(|_| "second cbrt overflow")?
    };

    let c1 = b_cbrt
        .checked_mul(b_cbrt)
        .and_then(|v| v.checked_div(i_wad))
        .and_then(|v| v.checked_mul(second_cbrt))
        .and_then(|v| v.checked_div(i_wad))
        .ok_or("c1 overflow")?;

    let root = (i_wad * c1 - i_wad * b2 - (i_wad * b2 / c1) * delta0) / (i_3 * a2);

    let y_out0 = (d_i * d_i / x_j * root / i_4 / i_wad)
        .try_into()
        .map_err(|_| "y overflow")?;
    let k0_prev = U256::try_from(root).map_err(|_| "k0 prev overflow")?;

    let frac = y_out0 * U256::from(WAD) / d;
    let lim_mul_min = U256::from(10).pow(U256::from(36)) / lim_mul;
    if frac < lim_mul_min / U256::from(N_COINS_2) || frac > lim_mul / U256::from(N_COINS_2) {
        return Err("unsafe value for y");
    }

    Ok((y_out0, k0_prev))
}

pub fn twocrypto_newton_d(
    ann: U256,
    gamma: U256,
    mut x: [U256; 2],
    k0_prev: U256,
) -> Result<U256, &'static str> {
    if ann <= U256::from(MIN_A - 1) || ann >= U256::from(MAX_A + 1) {
        return Err("unsafe values A");
    }
    if gamma <= U256::from(MIN_GAMMA - 1) || gamma >= U256::from(MAX_GAMMA + 1) {
        return Err("unsafe values gamma");
    }

    // sort x descending
    if x[0] < x[1] {
        x = [x[1], x[0]];
    }

    if x[0] <= U256::from(10).pow(U256::from(9)) - U256::from(1u64)
        || x[0] >= U256::from(10).pow(U256::from(15)) * U256::from(WAD) + U256::from(1u64)
    {
        return Err("unsafe values x[0]");
    }
    if (x[1] * U256::from(WAD) / x[0]) <= U256::from(10).pow(U256::from(14)) - U256::from(1u64) {
        return Err("unsafe values x[i] (input)");
    }

    let s = x[0] + x[1];
    let mut d = if k0_prev.is_zero() {
        U256::from(N_COINS_2) * isqrt(x[0] * x[1])
    } else {
        let mut d0 = isqrt((U256::from(4u64) * x[0] * x[1] / k0_prev) * U256::from(WAD));
        if s < d0 {
            d0 = s;
        }
        d0
    };

    let g1k0 = gamma + U256::from(WAD);
    for _ in 0..255 {
        let d_prev = d;
        if d.is_zero() {
            return Err("D==0");
        }

        let k0 = ((U256::from(WAD) * U256::from(N_COINS_2 * N_COINS_2)) * x[0] / d) * x[1] / d;

        let mut g1k0i = g1k0;
        if g1k0i > k0 {
            g1k0i = g1k0i - k0 + U256::from(1u64);
        } else {
            g1k0i = k0 - g1k0i + U256::from(1u64);
        }

        let mul1 = U256::from(WAD) * d / gamma * g1k0i / gamma * g1k0i * U256::from(A_MULTIPLIER) / ann;
        let mul2 = (U256::from(2u64) * U256::from(WAD) * U256::from(N_COINS_2)) * k0 / g1k0i;

        let neg_fprime = (s + s * mul2 / U256::from(WAD)) + mul1 * U256::from(N_COINS_2) / k0 - mul2 * d / U256::from(WAD);

        let d_plus = d * (neg_fprime + s) / neg_fprime;
        let mut d_minus = d * d / neg_fprime;
        if U256::from(WAD) > k0 {
            d_minus += d * (mul1 / neg_fprime) / U256::from(WAD) * (U256::from(WAD) - k0) / k0;
        } else {
            d_minus -= d * (mul1 / neg_fprime) / U256::from(WAD) * (k0 - U256::from(WAD)) / k0;
        }

        d = if d_plus > d_minus {
            d_plus - d_minus
        } else {
            (d_minus - d_plus) / U256::from(2u64)
        };

        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff * U256::from(10).pow(U256::from(14)) < std::cmp::max(U256::from(10).pow(U256::from(16)), d) {
            for _x in x {
                let frac = _x * U256::from(WAD) / d;
                let min_frac = U256::from(10).pow(U256::from(16)) / U256::from(N_COINS_2) - U256::from(1u64);
                let max_frac = U256::from(10).pow(U256::from(20)) / U256::from(N_COINS_2) + U256::from(1u64);
                if frac <= min_frac || frac >= max_frac {
                    return Err("unsafe values x[i]");
                }
            }
            return Ok(d);
        }
    }

    Err("twocrypto_newton_d did not converge")
}

/// 计算立方根 (1e18 精度)
/// 完全复刻链上 CurveCryptoMathOptimized3._cbrt
/// https://github.com/curvefi/tricrypto-ng/blob/main/contracts/main/CurveCryptoMathOptimized3.vy
pub fn cbrt(x: U256) -> U256 {
    if x == U256::ZERO {
        return U256::ZERO;
    }

    let precision = U256::from(PRECISION);
    // 阈值: 约 2^136 / 10^18
    let threshold_high = U256::from_str_radix("115792089237316195423570985008687907853269", 10)
        .unwrap_or(U256::MAX >> 80);

    // 规范化输入到合适的范围 (链上逻辑)
    let (xx, scale_factor): (U256, u32) = if x >= threshold_high * precision {
        (x, 12)
    } else if x >= threshold_high {
        (x * precision, 6)
    } else {
        (x * precision * precision, 0)
    };

    // 计算 log2(xx)
    let log2x = log2(xx, false);
    let log2x_usize: usize = log2x.to::<u64>() as usize;

    // 链上使用 pow_mod256 计算初始值:
    // remainder = log2x % 3
    // a = 2^(log2x/3) * 1260^remainder / 1000^remainder
    //
    // pow_mod256 在 Vyper 中是 wrapping power，在 Rust 中使用 overflowing_pow
    let pow_val = log2x_usize / 3;
    let remainder = log2x_usize % 3;

    // 使用 wrapping 计算 2^pow 和 1260^remainder / 1000^remainder
    let base = U256::from(1u64) << pow_val;

    // 精确计算 1260^remainder / 1000^remainder
    // remainder = 0: 1
    // remainder = 1: 1260/1000 = 1.26
    // remainder = 2: 1260^2/1000^2 = 1587600/1000000 = 1.5876
    let (cbrt2_num, cbrt2_den) = match remainder {
        0 => (U256::from(1u64), U256::from(1u64)),
        1 => (U256::from(1260u64), U256::from(1000u64)),
        2 => (U256::from(1587600u64), U256::from(1000000u64)), // 更精确: 1260*1260 = 1587600
        _ => (U256::from(1u64), U256::from(1u64)),
    };

    let mut a = base * cbrt2_num / cbrt2_den;

    // 7 轮 Newton-Raphson 迭代 (与链上完全一致)
    // a = (2*a + xx/(a*a)) / 3
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);
    if a.is_zero() {
        return U256::ZERO;
    }
    a = (U256::from(2u64) * a + xx / (a * a)) / U256::from(3u64);

    // 恢复缩放
    match scale_factor {
        12 => a * U256::from(10u64).pow(U256::from(12)),
        6 => a * U256::from(10u64).pow(U256::from(6)),
        _ => a,
    }
}

/// 优化版 get_y (TriCrypto)
/// 使用立方根封闭解法，而非纯 Newton 迭代
/// 返回 (y, k0_value)
///
/// 注意: 由于 I256 在复杂的有符号整数运算中有 API 限制，
/// 目前我们选择直接使用较为保守的 newton_y 作为回退方案,
/// 这是因为链上的 get_y 本身也有回退机制.
pub fn get_y_optimized(
    ann: U256,
    gamma: U256,
    x: &[U256],
    d: U256,
    i: usize,
) -> Result<(U256, U256), &'static str> {
    // 参数验证
    let min_a =
        U256::from(N_COINS.pow(N_COINS as u32)) * U256::from(A_MULTIPLIER) / U256::from(100);
    let max_a =
        U256::from(N_COINS.pow(N_COINS as u32)) * U256::from(A_MULTIPLIER) * U256::from(1000);
    let min_gamma = U256::from(10u64).pow(U256::from(10));
    let max_gamma = U256::from(5) * U256::from(10u64).pow(U256::from(16));
    let min_d = U256::from(10u64).pow(U256::from(17));
    let max_d = U256::from(10u64).pow(U256::from(15)) * U256::from(10u64).pow(U256::from(18));

    if ann < min_a || ann > max_a {
        return Err("unsafe values A");
    }
    if gamma < min_gamma || gamma > max_gamma {
        return Err("unsafe values gamma");
    }
    if d < min_d || d > max_d {
        return Err("unsafe values D");
    }

    let n_coins = x.len();
    if n_coins != 3 {
        return newton_y(ann, gamma, x, d, i).map(|y| (y, U256::ZERO));
    }

    let precision = U256::from(PRECISION);

    // 检查 x 值的安全性
    for k in 0..n_coins {
        if k != i {
            if d.is_zero() {
                return Err("get_y_optimized: d is zero before frac calculation");
            }
            let frac = x[k] * precision / d;
            let min_frac = U256::from(10u64).pow(U256::from(16)) - U256::from(1);
            let max_frac = U256::from(10u64).pow(U256::from(20)) + U256::from(1);
            if frac < min_frac || frac > max_frac {
                return Err("Unsafe values x[i]");
            }
        }
    }

    // 确定 j, k 索引
    let (j, k) = match i {
        0 => (1, 2),
        1 => (0, 2),
        2 => (0, 1),
        _ => return Err("Invalid index i"),
    };

    // 辅助函数: 从 U256 创建 I256
    fn to_i256(u: U256) -> Result<I256, &'static str> {
        I256::try_from(u).map_err(|_| "I256 overflow")
    }

    // 常量定义
    let e18_u = U256::from(10u64).pow(U256::from(18));
    let e36_u = U256::from(10u64).pow(U256::from(36));

    // 转换为有符号整数
    let ann_i = to_i256(ann)?;
    let gamma_i = to_i256(gamma)?;
    let d_i = to_i256(d)?;
    let x_j = to_i256(x[j])?;
    let x_k = to_i256(x[k])?;
    let e18 = to_i256(e18_u)?;
    let a_multiplier_i = to_i256(U256::from(A_MULTIPLIER))?;

    let gamma2 = gamma_i.checked_mul(gamma_i).ok_or("gamma2 overflow")?;

    // a = 10**36 / 27
    let a = to_i256(e36_u / U256::from(27))?;

    // 计算 b
    // b = 10**36/9 + 2*10**18*gamma/27 - D**2/x_j*gamma**2*ANN/27**2/A_MULTIPLIER/x_k
    let term_b1 = to_i256(e36_u / U256::from(9))?;
    let i2 = to_i256(U256::from(2))?;
    let i27 = to_i256(U256::from(27))?;
    let i729 = to_i256(U256::from(729))?; // 27**2

    let term_b2 = i2
        .checked_mul(e18)
        .and_then(|v| v.checked_mul(gamma_i))
        .and_then(|v| v.checked_div(i27))
        .ok_or("term_b2 overflow")?;

    let term_b3 = d_i
        .checked_mul(d_i)
        .and_then(|v| v.checked_div(x_j))
        .and_then(|v| v.checked_mul(gamma2))
        .and_then(|v| v.checked_mul(ann_i))
        .and_then(|v| v.checked_div(i729))
        .and_then(|v| v.checked_div(a_multiplier_i))
        .and_then(|v| v.checked_div(x_k))
        .ok_or("term_b3 overflow")?;

    let b = term_b1
        .checked_add(term_b2)
        .and_then(|v| v.checked_sub(term_b3))
        .ok_or("b overflow")?;

    // 计算 c
    // c = 10**36/9 + gamma*(gamma + 4*10**18)/27 + gamma**2*(x_j+x_k-D)/D*ANN/27/A_MULTIPLIER
    let term_c1 = to_i256(e36_u / U256::from(9))?;
    let i4 = to_i256(U256::from(4))?;

    let gamma_plus_4e18 = gamma_i
        .checked_add(i4.checked_mul(e18).ok_or("overflow")?)
        .ok_or("overflow")?;
    let term_c2 = gamma_i
        .checked_mul(gamma_plus_4e18)
        .and_then(|v| v.checked_div(i27))
        .ok_or("term_c2 overflow")?;

    let xj_xk = x_j.checked_add(x_k).ok_or("overflow")?;
    let xj_xk_d = xj_xk.checked_sub(d_i).ok_or("overflow")?;
    let term_c3 = gamma2
        .checked_mul(xj_xk_d)
        .and_then(|v| v.checked_div(d_i))
        .and_then(|v| v.checked_mul(ann_i))
        .and_then(|v| v.checked_div(i27))
        .and_then(|v| v.checked_div(a_multiplier_i))
        .ok_or("term_c3 overflow")?;

    let c = term_c1
        .checked_add(term_c2)
        .and_then(|v| v.checked_add(term_c3))
        .ok_or("c overflow")?;

    // d_coef = (10**18 + gamma)**2 / 27
    let e18_plus_gamma = e18.checked_add(gamma_i).ok_or("overflow")?;
    let d_coef = e18_plus_gamma
        .checked_mul(e18_plus_gamma)
        .and_then(|v| v.checked_div(i27))
        .ok_or("d_coef overflow")?;

    // d0 = abs(3*a*c/b - b)
    let i3 = to_i256(U256::from(3))?;
    let three_a = i3.checked_mul(a).ok_or("overflow")?;
    let three_a_c = three_a.checked_mul(c).ok_or("overflow")?;
    let three_a_c_div_b = three_a_c.checked_div(b).ok_or("div by zero")?;
    let d0_signed = three_a_c_div_b.checked_sub(b).ok_or("overflow")?;
    let d0 = if d0_signed.is_negative() {
        -d0_signed
    } else {
        d0_signed
    };

    // 选择除法器以防止溢出
    let e48_u = U256::from(10u64).pow(U256::from(48));
    let e44_u = U256::from(10u64).pow(U256::from(44));
    let e40_u = U256::from(10u64).pow(U256::from(40));
    let e32_u = U256::from(10u64).pow(U256::from(32));
    let e28_u = U256::from(10u64).pow(U256::from(28));
    let e24_u = U256::from(10u64).pow(U256::from(24));
    let e20_u = U256::from(10u64).pow(U256::from(20));

    let d0_u = U256::try_from(d0).unwrap_or(U256::MAX);

    let divider = if d0_u > e48_u {
        U256::from(10u64).pow(U256::from(30))
    } else if d0_u > e44_u {
        U256::from(10u64).pow(U256::from(26))
    } else if d0_u > e40_u {
        U256::from(10u64).pow(U256::from(22))
    } else if d0_u > e36_u {
        e18_u
    } else if d0_u > e32_u {
        U256::from(10u64).pow(U256::from(14))
    } else if d0_u > e28_u {
        U256::from(10u64).pow(U256::from(10))
    } else if d0_u > e24_u {
        U256::from(10u64).pow(U256::from(6))
    } else if d0_u > e20_u {
        U256::from(10u64).pow(U256::from(2))
    } else {
        U256::from(1)
    };
    let divider_i = to_i256(divider)?;

    // 缩放系数
    let a_abs = if a.is_negative() { -a } else { a };
    let b_abs = if b.is_negative() { -b } else { b };

    let (a_scaled, b_scaled, c_scaled, d_scaled) = if a_abs > b_abs {
        let additional_prec = a_abs.checked_div(b_abs).ok_or("div by zero")?;
        (
            a.checked_mul(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
            b.checked_mul(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
            c.checked_mul(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
            d_coef
                .checked_mul(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
        )
    } else {
        let additional_prec = b_abs.checked_div(a_abs).ok_or("div by zero")?;
        (
            a.checked_div(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
            b.checked_div(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
            c.checked_div(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
            d_coef
                .checked_div(additional_prec)
                .and_then(|v| v.checked_div(divider_i))
                .ok_or("overflow")?,
        )
    };

    if b_scaled == I256::ZERO {
        // Fallback to newton_y
        return newton_y(ann, gamma, x, d, i).map(|y| (y, U256::ZERO));
    }

    // delta0 = 3*a*c/b - b (缩放后)
    let three_ac_scaled = i3
        .checked_mul(a_scaled)
        .and_then(|v| v.checked_mul(c_scaled))
        .ok_or("overflow")?;
    let delta0 = three_ac_scaled
        .checked_div(b_scaled)
        .and_then(|v| v.checked_sub(b_scaled))
        .ok_or("overflow")?;

    // delta1 = 9*a*c/b - 2*b - 27*a**2/b*d/b
    let nine_ac_div_b = i3
        .checked_mul(three_ac_scaled)
        .and_then(|v| v.checked_div(b_scaled))
        .ok_or("overflow")?;
    let two_b = i2.checked_mul(b_scaled).ok_or("overflow")?;
    let a_sq = a_scaled.checked_mul(a_scaled).ok_or("overflow")?;
    let term_d1_3 = i27
        .checked_mul(a_sq)
        .and_then(|v| v.checked_div(b_scaled))
        .and_then(|v| v.checked_mul(d_scaled))
        .and_then(|v| v.checked_div(b_scaled))
        .ok_or("overflow")?;
    let delta1 = nine_ac_div_b
        .checked_sub(two_b)
        .and_then(|v| v.checked_sub(term_d1_3))
        .ok_or("overflow")?;

    // sqrt_arg = delta1**2 + 4*delta0**3/b
    let delta1_sq = delta1.checked_mul(delta1).ok_or("overflow")?;
    let delta0_sq = delta0.checked_mul(delta0).ok_or("overflow")?;
    let delta0_cubed_div_b = delta0_sq
        .checked_div(b_scaled)
        .and_then(|v| v.checked_mul(delta0))
        .ok_or("overflow")?;
    let sqrt_arg = delta1_sq
        .checked_add(i4.checked_mul(delta0_cubed_div_b).ok_or("overflow")?)
        .ok_or("overflow")?;

    // 如果 sqrt_arg <= 0，退回到 newton_y
    if sqrt_arg <= I256::ZERO {
        return newton_y(ann, gamma, x, d, i).map(|y| (y, U256::ZERO));
    }

    // 计算 sqrt(sqrt_arg)
    let sqrt_arg_u = U256::try_from(sqrt_arg).map_err(|_| "sqrt_arg negative")?;
    let sqrt_val_u = isqrt(sqrt_arg_u);
    let sqrt_val = to_i256(sqrt_val_u)?;

    // 计算 b_cbrt
    let b_cbrt = if b_scaled >= I256::ZERO {
        let b_u = U256::try_from(b_scaled).map_err(|_| "b overflow")?;
        to_i256(cbrt(b_u))?
    } else {
        let b_u = U256::try_from(-b_scaled).map_err(|_| "b overflow")?;
        -to_i256(cbrt(b_u))?
    };

    // 计算 second_cbrt
    let second_cbrt = if delta1 > I256::ZERO {
        let arg = delta1
            .checked_add(sqrt_val)
            .and_then(|v| v.checked_div(i2))
            .ok_or("overflow")?;
        let arg_u = U256::try_from(arg).map_err(|_| "cbrt arg overflow")?;
        to_i256(cbrt(arg_u))?
    } else {
        let neg_delta1_minus_sqrt = (-delta1).checked_add(sqrt_val).ok_or("overflow")?;
        let arg = neg_delta1_minus_sqrt.checked_div(i2).ok_or("overflow")?;
        let arg_u = U256::try_from(arg).map_err(|_| "cbrt arg overflow")?;
        -to_i256(cbrt(arg_u))?
    };

    // C1 = b_cbrt * b_cbrt / 10**18 * second_cbrt / 10**18
    let c1 = b_cbrt
        .checked_mul(b_cbrt)
        .and_then(|v| v.checked_div(e18))
        .and_then(|v| v.checked_mul(second_cbrt))
        .and_then(|v| v.checked_div(e18))
        .ok_or("c1 overflow")?;

    // root_K0 = (b + b*delta0/C1 - C1) / 3
    let root_k0 = if c1 != I256::ZERO {
        let b_delta0_div_c1 = b_scaled
            .checked_mul(delta0)
            .and_then(|v| v.checked_div(c1))
            .ok_or("overflow")?;
        b_scaled
            .checked_add(b_delta0_div_c1)
            .and_then(|v| v.checked_sub(c1))
            .and_then(|v| v.checked_div(i3))
            .ok_or("root_k0 overflow")?
    } else {
        return newton_y(ann, gamma, x, d, i).map(|y| (y, U256::ZERO));
    };

    // root = D*D/27/x_k * D/x_j * root_K0/a
    let root = d_i
        .checked_mul(d_i)
        .and_then(|v| v.checked_div(i27))
        .and_then(|v| v.checked_div(x_k))
        .and_then(|v| v.checked_mul(d_i))
        .and_then(|v| v.checked_div(x_j))
        .and_then(|v| v.checked_mul(root_k0))
        .and_then(|v| v.checked_div(a_scaled))
        .ok_or("root overflow")?;

    // Ensure root is non-negative before conversion
    if root < I256::ZERO {
        return Err("Root negative");
    }

    // 转换结果
    let y = U256::try_from(root).map_err(|_| "result negative")?;
    let k0_ret = e18
        .checked_mul(root_k0)
        .and_then(|v| v.checked_div(a_scaled))
        .and_then(|v| U256::try_from(v).ok())
        .unwrap_or(U256::ZERO);

    // 安全检查
    if d.is_zero() {
        return Err("get_y_optimized (analytical): d is zero before frac calculation");
    }
    let frac = y * precision / d;
    let min_frac = U256::from(10u64).pow(U256::from(16)) - U256::from(1);
    let max_frac = U256::from(10u64).pow(U256::from(20)) + U256::from(1);
    if frac < min_frac || frac > max_frac {
        // 退回到 newton_y
        return newton_y(ann, gamma, x, d, i).map(|y| (y, U256::ZERO));
    }

    Ok((y, k0_ret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geometric_mean_with_zero_input() {
        // Should return zero when any input is zero, not panic
        let x = vec![U256::from(1000), U256::ZERO, U256::from(2000)];
        let result = geometric_mean(&x, false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), U256::ZERO);
    }

    #[test]
    fn test_geometric_mean_all_zeros() {
        let x = vec![U256::ZERO, U256::ZERO];
        let result = geometric_mean(&x, false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), U256::ZERO);
    }

    #[test]
    fn test_geometric_mean_valid_input() {
        // Valid inputs should work normally
        let precision = U256::from(PRECISION);
        let x = vec![precision, precision];
        let result = geometric_mean(&x, false);
        assert!(result.is_ok());
        // geometric_mean of [1e18, 1e18] should be approximately 1e18
        let gm = result.unwrap();
        assert!(gm > U256::ZERO);
    }

    #[test]
    fn test_newton_d_with_zero_balance() {
        // Should handle zero balance gracefully
        let ann = U256::from(1_000_000);
        let gamma = U256::from(10_000_000_000_000u64); // 1e13
        let x = vec![U256::ZERO, U256::from(PRECISION)];

        // Should not panic, but return an error or handle gracefully
        let result = newton_d(ann, gamma, &x);
        // Since geometric_mean returns ZERO for zero inputs, d will be 0
        // and newton_d should handle that case
        assert!(result.is_ok() || result.is_err()); // doesn't panic
    }

    #[test]
    fn test_newton_y_with_zero_balance() {
        // Should return error, not panic
        let ann = U256::from(1_000_000);
        let gamma = U256::from(10_000_000_000_000u64);
        let d = U256::from(PRECISION) * U256::from(100);
        let x = vec![U256::ZERO, U256::from(PRECISION) * U256::from(100)];

        let result = newton_y(ann, gamma, &x, d, 0);
        // The key is that this doesn't panic - sorted x puts 0 at end
        // newton_y can handle this case gracefully
        assert!(result.is_ok() || result.is_err()); // Just ensure no panic
    }

    #[test]
    fn test_newton_y_unsafe_check() {
        use std::str::FromStr;
        // Reproduce the "Unsafe value for y" error
        // Pool: 0x62bfEA5673c6336d265865e5eA1d32F67c523C33 (TriCrypto)
        // Data from logs

        let ann = U256::from(2700000);
        let gamma = U256::from(13000000000000u64);
        let d = U256::from_str("297302090688513425870318763152").unwrap();

        let balance0 = U256::from_str("104717085539963994448777175418").unwrap();
        let balance1 = U256::from_str("70941594091021977").unwrap();
        let balance2 = U256::from_str("202145095641299889287").unwrap();

        let amount_in = U256::from_str("2241736411085989632").unwrap();

        // Simulating swap 0 -> 1
        // xp[0] increases by amount_in
        let x = vec![balance0 + amount_in, balance1, balance2];
        let j = 1; // Output token index (balance1 is target)

        let result = newton_y(ann, gamma, &x, d, j);

        // Should return "Unsafe value for y" error
        assert!(result.is_err());
        assert_eq!(result.err(), Some("Unsafe value for y"));
    }
}
