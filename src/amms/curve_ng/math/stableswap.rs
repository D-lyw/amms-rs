//! StableSwap 数学计算
//!
//! 实现 Curve StableSwap 不变量的核心计算：
//! - `get_d`: 计算 D 不变量 (Newton-Raphson 迭代)
//! - `get_y`: 计算交换后的输出代币余额
//! - `get_dy`: 计算交换输出金额
//!
//! ## StableSwap 不变量公式
//! ```text
//! A · n^n · Σx_i + D = A · D · n^n + D^(n+1) / (n^n · Πx_i)
//! ```
//!
//! 其中：
//! - A: 放大系数，控制曲线"平坦度"
//! - D: 虚拟总余额
//! - n: 代币数量
//! - x_i: 各代币余额

use crate::amms::error::AMMError;
use alloy::primitives::U256;

/// 精度常量: 1e18
pub const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

/// A 参数精度: 100
pub const A_PRECISION: U256 = U256::from_limbs([100u64, 0, 0, 0]);

/// 最大迭代次数
pub const MAX_ITERATIONS: usize = 255;

/// 计算 D 不变量 (Newton-Raphson 迭代)
///
/// # Arguments
/// * `balances` - 各代币余额 (已标准化到 18 位精度)
/// * `amp` - 放大系数 A
///
/// # Returns
/// D 不变量值
pub fn get_d(balances: &[U256], amp: U256) -> Result<U256, AMMError> {
    let n = balances.len();
    if n == 0 {
        return Err(AMMError::Msg("Empty balances".into()));
    }

    let n_coins = U256::from(n);

    // S = sum(x_i)
    let s: U256 = balances.iter().fold(U256::ZERO, |acc, b| acc + *b);
    if s.is_zero() {
        return Ok(U256::ZERO);
    }

    let mut d = s;
    let ann = amp * n_coins; // A * n^n

    // Newton 迭代
    for _ in 0..MAX_ITERATIONS {
        // D_P = D^(n+1) / (n^n * prod(x_i))
        let mut d_p = d;
        for balance in balances {
            // d_p = d_p * d / (balance * n)
            // 防止除零
            if balance.is_zero() {
                return Err(AMMError::Msg("Zero balance".into()));
            }
            d_p = d_p * d / (*balance * n_coins);
        }

        let d_prev = d;

        // d = (Ann * S / A_PRECISION + D_P * n) * D / ((Ann - A_PRECISION) * D / A_PRECISION + (n + 1) * D_P)
        let numerator = (ann * s / A_PRECISION + d_p * n_coins) * d;
        let denominator = ((ann - A_PRECISION) * d / A_PRECISION) + (n_coins + U256::from(1)) * d_p;

        if denominator.is_zero() {
            return Err(AMMError::Msg("Division by zero in get_d".into()));
        }
        d = numerator / denominator;

        // 收敛检查
        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff <= U256::from(1) {
            return Ok(d);
        }
    }

    Err(AMMError::Msg("D calculation did not converge".into()))
}

/// 计算 y (给定其他代币余额后，某个代币的新余额)
///
/// # Arguments
/// * `balances` - 各代币余额
/// * `amp` - 放大系数 A
/// * `i` - 输入代币索引
/// * `j` - 输出代币索引
/// * `x` - 输入代币新余额 (balance[i] + dx)
///
/// # Returns
/// 输出代币新余额 y
pub fn get_y(balances: &[U256], amp: U256, i: usize, j: usize, x: U256) -> Result<U256, AMMError> {
    let n = balances.len();
    if i >= n || j >= n || i == j {
        return Err(AMMError::Msg("Invalid token indices".into()));
    }

    let n_coins = U256::from(n);
    let d = get_d(balances, amp)?;
    let ann = amp * n_coins;

    // c = D^(n+1) / (n^n * prod(x_k for k != j))
    let mut c = d;
    let mut s = U256::ZERO;

    for (k, balance) in balances.iter().enumerate() {
        let x_k = if k == i { x } else { *balance };
        if k != j {
            s += x_k;
            if x_k.is_zero() {
                return Err(AMMError::Msg("Zero balance in get_y".into()));
            }
            c = c * d / (x_k * n_coins);
        }
    }

    c = c * d * A_PRECISION / (ann * n_coins);
    let b = s + d * A_PRECISION / ann;

    // Newton 迭代求 y
    let mut y = d;
    for _ in 0..MAX_ITERATIONS {
        let y_prev = y;
        // y = (y^2 + c) / (2y + b - d)
        let numerator = y * y + c;
        let denominator = y * U256::from(2) + b - d;

        if denominator.is_zero() {
            return Err(AMMError::Msg("Division by zero in get_y".into()));
        }
        y = numerator / denominator;

        let diff = if y > y_prev { y - y_prev } else { y_prev - y };
        if diff <= U256::from(1) {
            return Ok(y);
        }
    }

    Err(AMMError::Msg("y calculation did not converge".into()))
}

/// 计算 Curve StableSwap NG 动态手续费
///
/// Ref: https://github.com/curvefi/stableswap-ng/blob/main/contracts/main/CurveStableSwapNG.vy#L368
pub fn dynamic_fee(xp_i: U256, xp_j: U256, fee: U256, offpeg_fee_multiplier: U256) -> U256 {
    let fee_denominator = U256::from(10).pow(U256::from(10));
    if offpeg_fee_multiplier <= fee_denominator {
        return fee;
    }

    let xps2 = (xp_i + xp_j).pow(U256::from(2));
    if xps2.is_zero() {
        return fee;
    }

    // safe unwrap logic: offpeg_fee_multiplier > fee_denominator is checked
    let numerator = offpeg_fee_multiplier * fee;
    let diff_multiplier = offpeg_fee_multiplier - fee_denominator;

    // term = (offpeg_fee_multiplier - FEE_DENOMINATOR) * 4 * xpi * xpj / xps2
    let term = diff_multiplier * U256::from(4) * xp_i * xp_j / xps2;
    let denominator = term + fee_denominator;

    if denominator.is_zero() {
        return fee;
    }

    numerator / denominator
}

/// 计算交换输出金额 (get_dy)
///
/// # Arguments
/// * `balances` - 各代币余额 (已标准化)
/// * `amp` - 放大系数 A
/// * `i` - 输入代币索引
/// * `j` - 输出代币索引
/// * `dx` - 输入金额 (已标准化)
/// * `fee` - 手续费 (1e10 = 100%)
///
/// # Returns
/// 输出金额 dy (已标准化)
pub fn get_dy(
    balances: &[U256],
    amp: U256,
    i: usize,
    j: usize,
    dx: U256,
    fee: U256,
) -> Result<U256, AMMError> {
    let x = balances[i] + dx;
    let y = get_y(balances, amp, i, j, x)?;

    // dy = y_old - y_new - 1 (舍入保护)
    let dy = balances[j]
        .checked_sub(y)
        .ok_or(AMMError::Msg("Underflow in get_dy".into()))?
        .checked_sub(U256::from(1))
        .ok_or(AMMError::Msg("Underflow in get_dy".into()))?;

    // 扣除手续费
    let fee_amount = dy * fee / U256::from(10).pow(U256::from(10));
    let dy_after_fee = dy - fee_amount;

    Ok(dy_after_fee)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_d_equal_balances() {
        // 两个代币，各 1000e18
        let balances = vec![U256::from(1000) * PRECISION, U256::from(1000) * PRECISION];
        let amp = U256::from(100);

        let d = get_d(&balances, amp).unwrap();
        // D 应该约等于 2000e18
        assert!(d > U256::from(1999) * PRECISION);
        assert!(d < U256::from(2001) * PRECISION);
    }

    #[test]
    fn test_get_dy_small_swap() {
        // 两个代币，各 1000000e18 (模拟百万级稳定币池)
        let balances = vec![
            U256::from(1_000_000) * PRECISION,
            U256::from(1_000_000) * PRECISION,
        ];
        let amp = U256::from(100);
        let dx = U256::from(1000) * PRECISION; // swap 1000
        let fee = U256::from(4_000_000); // 0.04% fee

        let dy = get_dy(&balances, amp, 0, 1, dx, fee).unwrap();
        // 输出应该接近 1000，略小于输入（费用 + 滑点）
        assert!(dy > U256::from(999) * PRECISION);
        assert!(dy < U256::from(1000) * PRECISION);
    }
}
