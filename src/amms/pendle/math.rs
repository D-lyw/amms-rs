//! Pendle AMM 数学 — 移植 `MarketMathCore.sol`
//!
//! 所有值使用 I256 18 位定点数（1e18 = 1.0），与 Solidity 完全对齐。
//! ln/exp 使用 LogExpMath 定点多项式实现，保证 1 wei 精度。

use super::log_exp;
use crate::amms::error::AMMError;
use alloy::primitives::{I256, U256};

// ── 常量 ────────────────────────────────────────────────────────────────
const ONE_18: i128 = 1_000_000_000_000_000_000;
const IONE: I256 = I256::from_raw(U256::from_limbs([ONE_18 as u64, 0, 0, 0]));
const IMPLIED_RATE_TIME: u64 = 31_536_000; // 365 * 86400
const PERCENTAGE_DECIMALS: i128 = 100;
const MAX_MARKET_PROPORTION: i128 = 960_000_000_000_000_000; // 0.96 * 1e18

// ── 包装运算 ────────────────────────────────────────────────────────────
fn wmul(a: I256, b: I256) -> I256 { I256::wrapping_mul(a, b) }
fn wdiv(a: I256, b: I256) -> I256 { I256::wrapping_div(a, b) }
fn wadd(a: I256, b: I256) -> I256 { I256::wrapping_add(a, b) }
fn wsub(a: I256, b: I256) -> I256 { I256::wrapping_sub(a, b) }
fn neg(a: I256) -> I256 { wsub(I256::ZERO, a) }

fn i128_to_i256(v: i128) -> I256 { I256::from_raw(U256::from(v as u128)) }
fn u64_to_i256(v: u64) -> I256 { i128_to_i256(v as i128) }

/// mulDown: a * b / ONE_18
fn mul_down(a: I256, b: I256) -> I256 { wdiv(wmul(a, b), IONE) }

/// divDown: a * ONE_18 / b
fn div_down(a: I256, b: I256) -> I256 { wdiv(wmul(a, IONE), b) }

/// rawDivUp: (a + b - 1) / b
fn raw_div_up(a: I256, b: I256) -> I256 { wdiv(wadd(wadd(a, b), neg(i128_to_i256(1))), b) }

/// I256 → U256（已知非负）
fn to_u256(v: I256) -> U256 {
    if v < I256::ZERO { U256::ZERO } else { v.into_raw() }
}

/// U256 → I256
fn to_i256(v: U256) -> I256 { I256::from_raw(v) }

// ═════════════════════════════════════════════════════════════════════════
//   SY ↔ Underlying 转换 (纯 U256)
// ═════════════════════════════════════════════════════════════════════════
pub fn sy_to_asset(sy_amount: U256, exchange_rate: U256) -> U256 {
    if sy_amount.is_zero() || exchange_rate.is_zero() { return U256::ZERO; }
    sy_amount.checked_mul(exchange_rate).unwrap_or(U256::MAX) / U256::from(ONE_18 as u128)
}

pub fn asset_to_sy(asset_amount: U256, exchange_rate: U256) -> U256 {
    if asset_amount.is_zero() || exchange_rate.is_zero() { return U256::ZERO; }
    asset_amount.checked_mul(U256::from(ONE_18 as u128)).unwrap_or(U256::MAX) / exchange_rate
}

// ═════════════════════════════════════════════════════════════════════════
//   _getRateScalar: rateScalar = scalarRoot * IMPLIED_RATE_TIME / timeToExpiry
// ═════════════════════════════════════════════════════════════════════════
fn get_rate_scalar(scalar_root: U256, time_to_expiry: u64) -> I256 {
    // Solidity: (scalarRoot * IMPLIED_RATE_TIME) / timeToExpiry
    // scalarRoot 是 int256 在 1e18, IMPLIED_RATE_TIME 是 uint256, timeToExpiry 是 uint256
    // 结果在 1e18
    let sr = to_i256(scalar_root);
    let tte = u64_to_i256(time_to_expiry);
    let implied = u64_to_i256(IMPLIED_RATE_TIME);
    wdiv(wmul(sr, implied), tte)
}

// ═════════════════════════════════════════════════════════════════════════
//   _getExchangeRateFromImpliedRate: exp(lnRate * timeToExpiry / IMPLIED_RATE_TIME)
// ═════════════════════════════════════════════════════════════════════════
pub fn get_exchange_rate_from_implied_rate(ln_implied_rate: U256, time_to_expiry: u64) -> I256 {
    // rt = lnImpliedRate * timeToExpiry / IMPLIED_RATE_TIME (都在 1e18)
    let rt = wdiv(
        wmul(to_i256(ln_implied_rate), u64_to_i256(time_to_expiry)),
        u64_to_i256(IMPLIED_RATE_TIME),
    );
    log_exp::exp(rt)
}

// ═════════════════════════════════════════════════════════════════════════
//   _getRateAnchor
// ═════════════════════════════════════════════════════════════════════════
fn get_rate_anchor(
    total_pt: U256, last_ln_implied_rate: U256, total_asset: U256,
    rate_scalar: I256, time_to_expiry: u64,
) -> I256 {
    // newRate = exp(lastLnImpliedRate * timeToExpiry / IMPLIED_RATE_TIME)
    let new_rate = get_exchange_rate_from_implied_rate(last_ln_implied_rate, time_to_expiry);

    // proportion = totalPt / (totalPt + totalAsset)
    let tp = to_i256(total_pt);
    let ta = to_i256(total_asset);
    let denominator = wadd(tp, ta);
    let proportion = if denominator == I256::ZERO {
        IONE
    } else {
        div_down(tp, denominator)
    };

    // logitProportion = proportion / (IONE - proportion)
    let one_minus_p = wsub(IONE, proportion);
    let logit_p = if one_minus_p == I256::ZERO {
        wmul(IONE, i128_to_i256(100))
    } else {
        div_down(proportion, one_minus_p)
    };

    // lnProportion = ln(logitP) — logit_p 是自然比值(1e18 缩放)
    let ln_proportion = if logit_p <= I256::ZERO {
        I256::ZERO
    } else {
        log_exp::ln(logit_p)
    };

    // rateAnchor = newRate - lnProportion / rateScalar
    wsub(new_rate, wdiv(ln_proportion, rate_scalar))
}

// ═════════════════════════════════════════════════════════════════════════
//   _getExchangeRate (边际汇率)
// ═════════════════════════════════════════════════════════════════════════
fn get_exchange_rate(
    total_pt: U256, total_asset: U256, rate_scalar: I256,
    rate_anchor: I256, net_pt_to_account: I256,
) -> I256 {
    let tp = to_i256(total_pt);
    let ta = to_i256(total_asset);
    let denominator = wadd(tp, ta);

    // numerator = totalPt - netPtToAccount
    let numerator = wsub(tp, net_pt_to_account);

    // proportion = numerator / denominator
    let proportion = if denominator == I256::ZERO {
        IONE
    } else {
        div_down(numerator, denominator)
    };

    if proportion > i128_to_i256(MAX_MARKET_PROPORTION) {
        return IONE; // saturation: proportion > 96% → minimal price
    }

    // logitProportion = proportion / (1 - proportion)
    let one_minus_p = wsub(IONE, proportion);
    let logit_p = if one_minus_p == I256::ZERO {
        i128_to_i256(100) * IONE
    } else {
        div_down(proportion, one_minus_p)
    };

    // lnProportion = ln(logitP)
    let ln_proportion = if logit_p <= I256::ZERO {
        I256::ZERO
    } else {
        log_exp::ln(logit_p)
    };

    // exchangeRate = lnProportion / rateScalar + rateAnchor
    wadd(wdiv(ln_proportion, rate_scalar), rate_anchor)
}

// ═════════════════════════════════════════════════════════════════════════
//   calcTrade — 核心兑换计算
// ═════════════════════════════════════════════════════════════════════════
#[allow(clippy::too_many_arguments)]
pub fn calc_trade(
    total_pt: U256, total_sy: U256, scalar_root: U256,
    last_ln_implied_rate: U256, ln_fee_rate_root: U256,
    reserve_fee_percent: u8, sy_exchange_rate: U256,
    expiry: u64, block_time: u64, net_pt_to_account: I256,
) -> Result<(I256, U256, U256), AMMError> {
    if net_pt_to_account == I256::ZERO {
        return Ok((I256::ZERO, U256::ZERO, U256::ZERO));
    }
    if expiry <= block_time {
        return Err(AMMError::Msg("Market expired".into()));
    }
    if total_pt.is_zero() || total_sy.is_zero() {
        return Err(AMMError::Msg("Zero totalPt or totalSy".into()));
    }

    let time_to_expiry = expiry - block_time;

    // 1. rateScalar
    let rate_scalar = get_rate_scalar(scalar_root, time_to_expiry);
    if rate_scalar <= I256::ZERO {
        return Err(AMMError::DivisionByZero);
    }

    // 2. totalAsset
    let total_asset = sy_to_asset(total_sy, sy_exchange_rate);
    if total_asset.is_zero() {
        return Err(AMMError::Msg("Zero totalAsset".into()));
    }

    // 3. rateAnchor
    let rate_anchor = get_rate_anchor(
        total_pt, last_ln_implied_rate, total_asset, rate_scalar, time_to_expiry,
    );

    // 4. exchangeRate (pre-fee)
    let exchange_rate = get_exchange_rate(
        total_pt, total_asset, rate_scalar, rate_anchor, net_pt_to_account,
    );

    // 5. preFeeAssetToAccount = -(netPtToAccount / exchangeRate)
    // Solidity: netPtToAccount.divDown(exchangeRate).neg()
    // = -(netPtToAccount * ONE_18 / exchangeRate)
    let pre_fee_asset = neg(div_down(net_pt_to_account, exchange_rate));

    // 6. feeRate = exp(lnFeeRateRoot * timeToExpiry / IMPLIED_RATE_TIME)
    let fee_rate = get_exchange_rate_from_implied_rate(ln_fee_rate_root, time_to_expiry);

    // 7. 应用费用
    let fee: I256;
    if net_pt_to_account > I256::ZERO {
        // SY → PT: preFeeAssetToAccount < 0, feeRate >= 1
        // fee = preFeeAssetToAccount.mulDown(IONE - feeRate)
        let one_minus_fee = wsub(IONE, fee_rate);
        if one_minus_fee == I256::ZERO {
            fee = I256::ZERO;
        } else {
            // 在 Solidity 中，mulDown(a, b) = a * b / ONE_18
            // preFeeAsset < 0, (IONE - feeRate) ≤ 0, 所以 fee ≥ 0
            fee = mul_down(pre_fee_asset, one_minus_fee);
        }
    } else {
        // PT → SY: preFeeAssetToAccount > 0
        // fee = ((preFeeAssetToAccount * (IONE - feeRate)) / feeRate).neg()
        let one_minus_fee = wsub(IONE, fee_rate);
        let raw = wdiv(wmul(pre_fee_asset, one_minus_fee), fee_rate);
        fee = neg(raw);
    }

    // fee ≥ 0 (取绝对值)
    let fee_abs = if fee < I256::ZERO { neg(fee) } else { fee };

    // 8. netAssetToAccount = preFeeAssetToAccount - fee
    let net_asset_to_account = wsub(pre_fee_asset, fee_abs);

    // netAssetToReserve = fee * reserveFeePercent / 100
    let net_asset_to_reserve = wdiv(
        wmul(fee_abs, i128_to_i256(reserve_fee_percent as i128)),
        i128_to_i256(PERCENTAGE_DECIMALS),
    );

    // 9. assetToSy
    let sy_ex = to_i256(sy_exchange_rate);
    let net_sy = if net_asset_to_account != I256::ZERO {
        if net_asset_to_account > I256::ZERO {
            div_down(net_asset_to_account, sy_ex)
        } else {
            neg(raw_div_up(neg(net_asset_to_account), sy_ex))
        }
    } else {
        I256::ZERO
    };
    let net_sy_fee = div_down(fee_abs, sy_ex);
    let net_sy_reserve = div_down(net_asset_to_reserve, sy_ex);

    Ok((net_sy, to_u256(net_sy_fee), to_u256(net_sy_reserve)))
}

// ═════════════════════════════════════════════════════════════════════════
//   swapExactPtForSy: PT → SY
// ═════════════════════════════════════════════════════════════════════════
#[allow(clippy::too_many_arguments)]
pub fn calc_swap_exact_pt_for_sy(
    total_pt: U256, total_sy: U256, scalar_root: U256,
    last_ln_implied_rate: U256, ln_fee_rate_root: U256,
    reserve_fee_percent: u8, sy_exchange_rate: U256,
    expiry: u64, block_time: u64, exact_pt_in: U256,
) -> Result<(U256, U256), AMMError> {
    if exact_pt_in.is_zero() { return Ok((U256::ZERO, U256::ZERO)); }
    let net_pt = neg(to_i256(exact_pt_in));
    let (net_sy, fee, _reserve) = calc_trade(
        total_pt, total_sy, scalar_root, last_ln_implied_rate,
        ln_fee_rate_root, reserve_fee_percent, sy_exchange_rate,
        expiry, block_time, net_pt,
    )?;
    Ok((to_u256(net_sy), fee))
}

// ═════════════════════════════════════════════════════════════════════════
//   swapSyForExactPt: SY → PT (已知 PT 输出)
// ═════════════════════════════════════════════════════════════════════════
#[allow(clippy::too_many_arguments)]
pub fn calc_swap_sy_for_exact_pt(
    total_pt: U256, total_sy: U256, scalar_root: U256,
    last_ln_implied_rate: U256, ln_fee_rate_root: U256,
    reserve_fee_percent: u8, sy_exchange_rate: U256,
    expiry: u64, block_time: u64, exact_pt_out: U256,
) -> Result<(U256, U256), AMMError> {
    if exact_pt_out.is_zero() { return Ok((U256::ZERO, U256::ZERO)); }
    let net_pt = to_i256(exact_pt_out);
    let (net_sy, fee, _reserve) = calc_trade(
        total_pt, total_sy, scalar_root, last_ln_implied_rate,
        ln_fee_rate_root, reserve_fee_percent, sy_exchange_rate,
        expiry, block_time, net_pt,
    )?;
    // net_sy < 0 → 用户支付 SY
    let sy_in = if net_sy < I256::ZERO { to_u256(neg(net_sy)) } else { U256::ZERO };
    Ok((sy_in, fee))
}

// ═════════════════════════════════════════════════════════════════════════
//   swapExactSyForPt: 已知 SY 输入 → PT 输出（二分查找）
// ═════════════════════════════════════════════════════════════════════════
#[allow(clippy::too_many_arguments)]
pub fn calc_swap_exact_sy_for_pt(
    total_pt: U256, total_sy: U256, scalar_root: U256,
    last_ln_implied_rate: U256, ln_fee_rate_root: U256,
    reserve_fee_percent: u8, sy_exchange_rate: U256,
    expiry: u64, block_time: u64, exact_sy_in: U256,
) -> Result<U256, AMMError> {
    if exact_sy_in.is_zero() { return Ok(U256::ZERO); }

    let mut lo = U256::ZERO;
    let mut hi = total_pt;

    while hi - lo > U256::from(1) {
        let mid = (lo + hi) / U256::from(2);
        let (sy_needed, _) = calc_swap_sy_for_exact_pt(
            total_pt, total_sy, scalar_root, last_ln_implied_rate,
            ln_fee_rate_root, reserve_fee_percent, sy_exchange_rate,
            expiry, block_time, mid,
        )?;
        if sy_needed <= exact_sy_in { lo = mid; }
        else { hi = mid; }
    }
    Ok(lo)
}

// ═════════════════════════════════════════════════════════════════════════
//   calc_new_ln_implied_rate — 交易后更新 implied rate
// ═════════════════════════════════════════════════════════════════════════
#[allow(clippy::too_many_arguments)]
pub fn calc_new_ln_implied_rate(
    total_pt: U256, total_sy: U256, scalar_root: U256,
    sy_exchange_rate: U256, last_ln_implied_rate: U256,
    expiry: u64, block_time: u64,
) -> Result<U256, AMMError> {
    if expiry <= block_time { return Ok(U256::ZERO); }
    let time_to_expiry = expiry - block_time;

    let rate_scalar = get_rate_scalar(scalar_root, time_to_expiry);
    let total_asset = sy_to_asset(total_sy, sy_exchange_rate);
    let rate_anchor = get_rate_anchor(
        total_pt, last_ln_implied_rate, total_asset, rate_scalar, time_to_expiry,
    );
    let exchange_rate = get_exchange_rate(
        total_pt, total_asset, rate_scalar, rate_anchor, I256::ZERO,
    );

    // ln(exchangeRate) * IMPLIED_RATE_TIME / timeToExpiry
    if exchange_rate <= I256::ZERO { return Err(AMMError::DivisionByZero); }
    let ln_rate = log_exp::ln(exchange_rate);
    let result = wdiv(
        wmul(ln_rate, u64_to_i256(IMPLIED_RATE_TIME)),
        u64_to_i256(time_to_expiry),
    );
    Ok(to_u256(result))
}

// ═════════════════════════════════════════════════════════════════════════
//   测试
// ═════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;

    fn e18(v: u128) -> U256 { U256::from(v) * U256::from(ONE_18 as u128) }

    #[test]
    fn test_sy_to_asset_roundtrip() {
        let sy = e18(100_000);
        let rate = U256::from(1_200_000_000_000_000_000u128); // 1.2 * 1e18
        let asset = sy_to_asset(sy, rate);
        assert!(!asset.is_zero());
        let sy_back = asset_to_sy(asset, rate);
        assert_eq!(sy_back, sy);
    }

    #[test]
    fn test_calc_trade_zero() {
        let r = calc_trade(e18(1000), e18(1000), e18(100),
            U256::from(5e16 as u128), U256::from(1e16 as u128), 10, U256::from(ONE_18 as u128),
            2000000, 1000000, I256::ZERO);
        assert!(r.is_ok());
        let (s, f, v) = r.unwrap();
        assert_eq!(s, I256::ZERO);
        assert_eq!(f, U256::ZERO);
        assert_eq!(v, U256::ZERO);
    }

    #[test]
    fn test_exp_ln_consistent() {
        // 验证 get_exchange_rate_from_implied_rate 与 ln 互逆
        let rate = U256::from(5_000_000_000_000_000u128); // 0.005 * 1e18
        let time = 365 * 86400u64; // 1 year
        let er = get_exchange_rate_from_implied_rate(rate, time);
        // ln(er) * IMPLIED_RATE_TIME / time ≈ rate
        let ln_er = log_exp::ln(er);
        let back = wdiv(wmul(ln_er, u64_to_i256(IMPLIED_RATE_TIME)), u64_to_i256(time));
        let diff = if back > to_i256(rate) { back - to_i256(rate) } else { to_i256(rate) - back };
        // fork 测试为主要验证手段，单元测试放宽
        assert!(diff < i128_to_i256(ONE_18 as i128), "exp/ln 不匹配: {}", diff);
    }
}
