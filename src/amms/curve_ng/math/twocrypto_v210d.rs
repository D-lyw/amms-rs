//! TwoCrypto `v2.1.0d` periphery 数学实现（YieldBasis 特殊池）
//!
//! 背景：
//! - 常规 TwoCrypto NG (`v2.1.0`) 走 CryptoSwap 数学路径；
//! - YieldBasis 这组 TwoCrypto 池部署在 `v2.1.0d` 分支，`get_dy` 依赖
//!   `TwocryptoView + StableswapMath` 组合逻辑（与标准路径不同）。
//! - 若对这类池继续使用标准 TwoCrypto 公式，本地 quote 会出现明显偏差，历史上出现过负输出问题。
//!
//! 来源：
//! - <https://github.com/curvefi/twocrypto-ng/blob/yb-pools-study/contracts/main/Twocrypto.vy>
//! - <https://docs.yieldbasis.com/dev/contract-addresses>
//!
//! 维护说明：
//! - 本文件目标是按链上 view 路径进行“位级行为复刻”（尤其是 rounding 和 ramp 分支）；
//! - 后续升级时优先逐项比对 `_calc_D_ramp` / `_fee` / `get_dy` 相关逻辑。

use crate::amms::error::AMMError;
use alloy::primitives::U256;

const N_COINS: U256 = U256::from_limbs([2, 0, 0, 0]);
const N_COINS_SQUARED: U256 = U256::from_limbs([4, 0, 0, 0]);
const A_MULTIPLIER: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const MAX_ITER: usize = 255;
const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);

fn mul_div(a: U256, b: U256, denom: U256, ctx: &str) -> Result<U256, AMMError> {
    if denom.is_zero() {
        return Err(AMMError::Msg(format!("{}: division by zero", ctx)));
    }
    let p = a
        .checked_mul(b)
        .ok_or_else(|| AMMError::Msg(format!("{}: mul overflow", ctx)))?;
    Ok(p / denom)
}

fn abs_diff(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        b - a
    }
}

pub fn stableswap_newton_d(amp: U256, xp: [U256; 2]) -> Result<U256, AMMError> {
    let s = xp[0]
        .checked_add(xp[1])
        .ok_or_else(|| AMMError::Msg("stableswap_newton_d: sum overflow".into()))?;
    if s.is_zero() {
        return Ok(U256::ZERO);
    }

    let ann = amp
        .checked_mul(N_COINS)
        .ok_or_else(|| AMMError::Msg("stableswap_newton_d: ann overflow".into()))?;

    let mut d = s;
    for _ in 0..MAX_ITER {
        let mut d_p = d;
        for x in xp {
            if x.is_zero() {
                return Err(AMMError::Msg("stableswap_newton_d: zero xp".into()));
            }
            d_p = mul_div(d_p, d, x, "stableswap_newton_d d_p")?;
        }
        d_p /= N_COINS_SQUARED;

        let d_prev = d;

        let ann_s = mul_div(ann, s, A_MULTIPLIER, "stableswap_newton_d ann_s")?;
        let d_p_n = d_p
            .checked_mul(N_COINS)
            .ok_or_else(|| AMMError::Msg("stableswap_newton_d: d_p_n overflow".into()))?;
        let num_left = ann_s
            .checked_add(d_p_n)
            .ok_or_else(|| AMMError::Msg("stableswap_newton_d: numerator overflow".into()))?;
        let numerator = num_left
            .checked_mul(d)
            .ok_or_else(|| AMMError::Msg("stableswap_newton_d: numerator mul overflow".into()))?;

        if ann < A_MULTIPLIER {
            return Err(AMMError::Msg(
                "stableswap_newton_d: ann below A_MULTIPLIER".into(),
            ));
        }
        let ann_minus = ann - A_MULTIPLIER;
        let den_left = mul_div(
            ann_minus,
            d,
            A_MULTIPLIER,
            "stableswap_newton_d denominator_left",
        )?;
        let den_right = d_p
            .checked_mul(U256::from(3u8))
            .ok_or_else(|| AMMError::Msg("stableswap_newton_d: denominator overflow".into()))?;
        let denominator = den_left
            .checked_add(den_right)
            .ok_or_else(|| AMMError::Msg("stableswap_newton_d: denominator add overflow".into()))?;

        if denominator.is_zero() {
            return Err(AMMError::Msg(
                "stableswap_newton_d: denominator is zero".into(),
            ));
        }
        d = numerator / denominator;

        if abs_diff(d, d_prev) <= U256::from(1u8) {
            return Ok(d);
        }
    }

    Err(AMMError::Msg(
        "stableswap_newton_d: did not converge".into(),
    ))
}

pub fn stableswap_get_y(amp: U256, xp: [U256; 2], d: U256, i: usize) -> Result<U256, AMMError> {
    if i >= 2 {
        return Err(AMMError::Msg("stableswap_get_y: i out of range".into()));
    }

    let ann = amp
        .checked_mul(N_COINS)
        .ok_or_else(|| AMMError::Msg("stableswap_get_y: ann overflow".into()))?;
    if ann.is_zero() {
        return Err(AMMError::Msg("stableswap_get_y: ann is zero".into()));
    }

    let other = if i == 0 { xp[1] } else { xp[0] };
    if other.is_zero() {
        return Err(AMMError::Msg("stableswap_get_y: other xp is zero".into()));
    }

    let mut c = d;
    c = mul_div(
        c,
        d,
        other
            .checked_mul(N_COINS)
            .ok_or_else(|| AMMError::Msg("stableswap_get_y: other*N overflow".into()))?,
        "stableswap_get_y c1",
    )?;
    c = c
        .checked_mul(d)
        .ok_or_else(|| AMMError::Msg("stableswap_get_y: c*d overflow".into()))?;
    c = mul_div(
        c,
        A_MULTIPLIER,
        ann.checked_mul(N_COINS)
            .ok_or_else(|| AMMError::Msg("stableswap_get_y: ann*N overflow".into()))?,
        "stableswap_get_y c2",
    )?;

    let b = other
        .checked_add(mul_div(d, A_MULTIPLIER, ann, "stableswap_get_y b")?)
        .ok_or_else(|| AMMError::Msg("stableswap_get_y: b overflow".into()))?;

    let mut y = d;
    for _ in 0..MAX_ITER {
        let y_prev = y;
        let y_sq = y
            .checked_mul(y)
            .ok_or_else(|| AMMError::Msg("stableswap_get_y: y^2 overflow".into()))?;
        let numerator = y_sq
            .checked_add(c)
            .ok_or_else(|| AMMError::Msg("stableswap_get_y: numerator overflow".into()))?;

        let two_y = y
            .checked_mul(U256::from(2u8))
            .ok_or_else(|| AMMError::Msg("stableswap_get_y: 2y overflow".into()))?;
        let denominator = two_y
            .checked_add(b)
            .and_then(|v| v.checked_sub(d))
            .ok_or_else(|| AMMError::Msg("stableswap_get_y: denominator underflow".into()))?;

        if denominator.is_zero() {
            return Err(AMMError::Msg(
                "stableswap_get_y: denominator is zero".into(),
            ));
        }

        y = numerator / denominator;
        if abs_diff(y, y_prev) <= U256::from(1u8) {
            return Ok(y);
        }
    }

    Err(AMMError::Msg("stableswap_get_y: did not converge".into()))
}

pub fn calc_d_ramp(
    amp: U256,
    balances: [U256; 2],
    precisions: [U256; 2],
    price_scale: U256,
    stored_d: U256,
    future_a_gamma_time: U256,
    last_timestamp: U256,
) -> Result<U256, AMMError> {
    // 对齐 Twocrypto.vy 的 _calc_D_ramp:
    // ramp 未结束时使用 stableswap_newton_d 重算 D；否则使用存储的 D。
    if future_a_gamma_time > last_timestamp {
        let scaled = [
            balances[0]
                .checked_mul(precisions[0])
                .ok_or_else(|| AMMError::Msg("calc_d_ramp: xp0 overflow".into()))?,
            mul_div(
                balances[1],
                price_scale.checked_mul(precisions[1]).ok_or_else(|| {
                    AMMError::Msg("calc_d_ramp: price_scale*precision overflow".into())
                })?,
                PRECISION,
                "calc_d_ramp xp1",
            )?,
        ];
        stableswap_newton_d(amp, scaled)
    } else {
        Ok(stored_d)
    }
}

pub fn fee(xp: [U256; 2], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> Result<U256, AMMError> {
    let mut b = xp[0]
        .checked_add(xp[1])
        .ok_or_else(|| AMMError::Msg("twocrypto_v210d fee: sum overflow".into()))?;
    if b.is_zero() {
        return Err(AMMError::Msg("twocrypto_v210d fee: zero balances".into()));
    }

    b = mul_div(
        PRECISION
            .checked_mul(N_COINS_SQUARED)
            .ok_or_else(|| AMMError::Msg("twocrypto_v210d fee: precision overflow".into()))?,
        xp[0],
        b,
        "twocrypto_v210d fee b1",
    )?;
    b = mul_div(
        b,
        xp[1],
        xp[0]
            .checked_add(xp[1])
            .ok_or_else(|| AMMError::Msg("twocrypto_v210d fee: sum overflow 2".into()))?,
        "twocrypto_v210d fee b2",
    )?;

    let fee_gamma_b = fee_gamma
        .checked_mul(b)
        .ok_or_else(|| AMMError::Msg("twocrypto_v210d fee: fee_gamma*B overflow".into()))?;
    let denominator = mul_div(
        fee_gamma_b,
        U256::from(1u8),
        PRECISION,
        "twocrypto_v210d fee denominator_left",
    )?
    .checked_add(PRECISION)
    .and_then(|v| v.checked_sub(b))
    .ok_or_else(|| AMMError::Msg("twocrypto_v210d fee: denominator overflow".into()))?;
    if denominator.is_zero() {
        return Err(AMMError::Msg(
            "twocrypto_v210d fee: denominator is zero".into(),
        ));
    }
    b = fee_gamma_b / denominator;

    let numerator = mid_fee
        .checked_mul(b)
        .and_then(|v| {
            out_fee
                .checked_mul(PRECISION - b)
                .and_then(|w| v.checked_add(w))
        })
        .ok_or_else(|| AMMError::Msg("twocrypto_v210d fee: numerator overflow".into()))?;

    Ok(numerator / PRECISION)
}

pub fn get_dy(
    i: usize,
    j: usize,
    dx: U256,
    balances: [U256; 2],
    amp: U256,
    price_scale: U256,
    stored_d: U256,
    precisions: [U256; 2],
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
    future_a_gamma_time: U256,
    last_timestamp: U256,
) -> Result<U256, AMMError> {
    // 对齐 Twocrypto.vy 的 get_dy:
    // 1) 先经 _calc_D_ramp 得到 D
    // 2) 用 stableswap_get_y 求目标币 y
    // 3) 下采样并按 TwocryptoView._fee 扣费
    if i >= 2 || j >= 2 || i == j {
        return Err(AMMError::Msg(
            "twocrypto_v210d get_dy: coin index out of range".into(),
        ));
    }
    if dx.is_zero() {
        return Err(AMMError::Msg("twocrypto_v210d get_dy: zero dx".into()));
    }

    let d = calc_d_ramp(
        amp,
        balances,
        precisions,
        price_scale,
        stored_d,
        future_a_gamma_time,
        last_timestamp,
    )?;

    let mut xp = balances;
    xp[i] = xp[i]
        .checked_add(dx)
        .ok_or_else(|| AMMError::Msg("twocrypto_v210d get_dy: xp[i] overflow".into()))?;

    let mut scaled = [
        xp[0]
            .checked_mul(precisions[0])
            .ok_or_else(|| AMMError::Msg("twocrypto_v210d get_dy: xp0 scale overflow".into()))?,
        mul_div(
            xp[1],
            price_scale.checked_mul(precisions[1]).ok_or_else(|| {
                AMMError::Msg("twocrypto_v210d get_dy: xp1 scale overflow".into())
            })?,
            PRECISION,
            "twocrypto_v210d get_dy xp1 scale",
        )?,
    ];

    let y = stableswap_get_y(amp, scaled, d, j)?;
    if y >= scaled[j] {
        return Err(AMMError::Msg(
            "twocrypto_v210d get_dy: unsafe value for y".into(),
        ));
    }

    let mut dy = scaled[j]
        .checked_sub(y)
        .and_then(|v| v.checked_sub(U256::from(1u8)))
        .ok_or_else(|| AMMError::Msg("twocrypto_v210d get_dy: dy underflow".into()))?;
    scaled[j] = y;

    if j > 0 {
        dy = mul_div(
            dy,
            PRECISION,
            price_scale,
            "twocrypto_v210d get_dy downscale",
        )?;
    }
    if precisions[j].is_zero() {
        return Err(AMMError::Msg(
            "twocrypto_v210d get_dy: precision is zero".into(),
        ));
    }
    dy /= precisions[j];

    let fee_rate = fee(scaled, mid_fee, out_fee, fee_gamma)?;
    let fee_amount = mul_div(dy, fee_rate, FEE_DENOMINATOR, "twocrypto_v210d get_dy fee")?;

    dy.checked_sub(fee_amount)
        .ok_or_else(|| AMMError::Msg("twocrypto_v210d get_dy: fee underflow".into()))
}
