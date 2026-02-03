use crate::amms::error::AMMError;
use alloy::primitives::U256;

pub const A_PRECISION: U256 = U256::from_limbs([100, 0, 0, 0]);

/// 计算 D 不变量
///
/// # Arguments
/// * `xp` - 缩放后的余额
/// * `amp` - 放大系数 (来自 pool.A() 调用)
/// * `uses_a_precision` - 是否是新版池子 (使用 A_PRECISION=100)
///   - true: 新版 Vyper 0.3.x 池子 (如 FRAX/USDC)
///   - false: 旧版 Vyper 0.2.x 池子 (如 3pool)
fn get_d(xp: &[U256], amp: U256, uses_a_precision: bool) -> Result<U256, AMMError> {
    let n_coins = xp.len();
    let mut s = U256::ZERO;
    for &x in xp {
        s += x;
    }

    if s == U256::ZERO {
        return Ok(U256::ZERO);
    }

    // Check for any zero balances - would cause division by zero
    for (k, &x) in xp.iter().enumerate() {
        if x == U256::ZERO {
            return Err(AMMError::Msg(format!("get_d: balance[{}] is zero", k)));
        }
    }

    let mut d_prev;
    let mut d = s;
    let n_u256 = U256::from(n_coins);

    // 根据版本计算 Ann
    // 新版池子: 存储的 A 需要乘以 A_PRECISION, 公式中也使用 A_PRECISION
    // 旧版池子: 直接使用 A
    let ann = if uses_a_precision {
        amp * A_PRECISION * n_u256
    } else {
        amp * n_u256
    };

    for _ in 0..255 {
        let mut d_p = d;
        for &x in xp {
            // D_P = D_P * D / (_x * N_COINS)
            let divisor = x * n_u256;
            if divisor == U256::ZERO {
                return Err(AMMError::Msg("get_d: divisor is zero".into()));
            }
            d_p = d_p * d / divisor;
        }

        d_prev = d;

        if uses_a_precision {
            // 新版公式 (Vyper 0.3.x):
            // D = (Ann * S / A_PRECISION + D_P * N_COINS) * D / ((Ann - A_PRECISION) * D / A_PRECISION + (N_COINS + 1) * D_P)
            let term1 = (ann * s / A_PRECISION) + (d_p * n_u256);
            let numer = term1 * d;

            let term2 = (ann - A_PRECISION) * d / A_PRECISION;
            let term3 = (n_u256 + U256::from(1)) * d_p;
            let denom = term2 + term3;

            d = numer / denom;
        } else {
            // 旧版公式 (Vyper 0.2.x, 如 3pool):
            // D = (Ann * S + D_P * N_COINS) * D / ((Ann - 1) * D + (N_COINS + 1) * D_P)
            let term1 = (ann * s) + (d_p * n_u256);
            let numer = term1 * d;

            let term2 = (ann - U256::from(1)) * d;
            let term3 = (n_u256 + U256::from(1)) * d_p;
            let denom = term2 + term3;

            d = numer / denom;
        }

        if d > d_prev {
            if d - d_prev <= U256::from(1) {
                return Ok(d);
            }
        } else {
            if d_prev - d <= U256::from(1) {
                return Ok(d);
            }
        }
    }
    Err(AMMError::Msg(
        "D calculation failed/did not converge".into(),
    ))
}

/// 计算 y (新的目标代币余额)
fn get_y(
    i: usize,
    j: usize,
    x: U256,
    xp: &[U256],
    amp: U256,
    d: U256,
    uses_a_precision: bool,
) -> Result<U256, AMMError> {
    let n_coins = xp.len();
    let n_u256 = U256::from(n_coins);

    // 根据版本计算 Ann
    let ann = if uses_a_precision {
        amp * A_PRECISION * n_u256
    } else {
        amp * n_u256
    };

    if ann == U256::ZERO {
        return Err(AMMError::Msg("get_y: ann is zero".into()));
    }

    let mut c = d;
    let mut s = U256::ZERO;
    let mut _x = U256::ZERO;

    let mut y_prev;
    let mut y = d;

    for (k, val) in xp.iter().enumerate() {
        if k == i {
            _x = x;
        } else if k != j {
            _x = *val;
        } else {
            continue;
        }

        if _x == U256::ZERO {
            return Err(AMMError::Msg(format!("get_y: balance[{}] is zero", k)));
        }

        s += _x;
        let divisor = _x * n_u256;
        if divisor == U256::ZERO {
            return Err(AMMError::Msg("get_y: divisor is zero".into()));
        }
        c = c * d / divisor;
    }

    let ann_divisor = ann * n_u256;
    if ann_divisor == U256::ZERO {
        return Err(AMMError::Msg("get_y: ann * n_coins is zero".into()));
    }

    // 根据版本计算 c 和 b
    let (c_final, b) = if uses_a_precision {
        // 新版公式: c = c * D * A_PRECISION / (Ann * N_COINS)
        // 新版公式: b = S + D * A_PRECISION / Ann
        let c_val = c * d * A_PRECISION / ann_divisor;
        let b_val = s + d * A_PRECISION / ann;
        (c_val, b_val)
    } else {
        // 旧版公式: c = c * D / (Ann * N_COINS)
        // 旧版公式: b = S + D / Ann
        let c_val = c * d / ann_divisor;
        let b_val = s + d / ann;
        (c_val, b_val)
    };

    for _ in 0..255 {
        y_prev = y;
        // y = (y*y + c) / (2*y + b - D)
        let numer = y * y + c_final;
        let denom = U256::from(2) * y + b - d;

        y = numer / denom;

        if y > y_prev {
            if y - y_prev <= U256::from(1) {
                return Ok(y);
            }
        } else {
            if y_prev - y <= U256::from(1) {
                return Ok(y);
            }
        }
    }

    Err(AMMError::Msg("get_y did not converge".into()))
}

/// 计算交换输出金额
///
/// # Arguments
/// * `xp` - 缩放后的余额
/// * `amp` - 放大系数
/// * `i` - 输入代币索引
/// * `j` - 输出代币索引
/// * `dx` - 缩放后的输入金额
/// * `fee` - 手续费率 (1e10 = 100%)
/// * `uses_a_precision` - 是否使用 A_PRECISION (新版池子)
pub fn get_dy(
    xp: &[U256],
    amp: U256,
    i: usize,
    j: usize,
    dx: U256,
    _fee: U256, // 保留参数以保持向后兼容，但不在这里使用
    uses_a_precision: bool,
) -> Result<U256, AMMError> {
    // 1. Calculate D from current balances xp
    let d = get_d(&xp, amp, uses_a_precision)?;

    // 2. New x_i
    let x_i = xp[i] + dx;

    // 3. Solve for y (new x_j)
    let y = get_y(i, j, x_i, xp, amp, d, uses_a_precision)?;

    // 4. dy = xp[j] - y - 1 (1 for rounding errors usually in vyper)
    // 注意：不在这里扣费，费率应在反缩放后按真实单位计算
    let dy = xp[j]
        .checked_sub(y)
        .ok_or(AMMError::Msg("dy underflow".into()))?;
    let dy = dy.checked_sub(U256::from(1)).unwrap_or(U256::ZERO);

    Ok(dy)
}
