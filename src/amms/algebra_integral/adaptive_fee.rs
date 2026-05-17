/// AdaptiveFee — Rust implementation of Algebra Integral's AdaptiveFee library
///
/// Mirrors the Solidity implementation at:
/// packages/dynamic-fee/contracts/libraries/AdaptiveFee.sol
///
/// The fee formula:
///   fee = baseFee + sigmoid1(volatility/15) + sigmoid2(volatility/15)
///
/// where sigmoid(x) = α / (1 + e^((β-x)/γ))
use alloy::primitives::U256;

use super::AlgebraFeeConfig;

/// Returns default initial fee configuration (only for reference/testing).
pub fn initial_fee_configuration() -> AlgebraFeeConfig {
    const INITIAL_MIN_FEE: u16 = 100; // 0.01% in hundredths of a bip
    AlgebraFeeConfig {
        alpha1: 3000 - INITIAL_MIN_FEE, // max value of the first sigmoid
        alpha2: 15000 - 3000,           // max value of the second sigmoid
        beta1: 360,                     // shift along the x-axis for the first sigmoid
        beta2: 60000,                   // shift along the x-axis for the second sigmoid
        gamma1: 59,                     // horizontal stretch factor for the first sigmoid
        gamma2: 8500,                   // horizontal stretch factor for the second sigmoid
        base_fee: INITIAL_MIN_FEE,      // minimum possible fee
    }
}

/// Calculates fee based on formula:
///   baseFee + sigmoid1(volatility) + sigmoid2(volatility)
/// maximum value capped by baseFee + alpha1 + alpha2
///
/// Solidity equivalent: AdaptiveFee.getFee()
pub fn get_fee(volatility: u64, config: &AlgebraFeeConfig) -> u16 {
    // Solidity: volatility /= 15 (normalize for 15 sec interval)
    let normalized_vol = volatility / 15;

    let sum_of_sigmoids =
        sigmoid_uint64(normalized_vol, config.gamma1, config.alpha1, config.beta1)
            + sigmoid_uint64(normalized_vol, config.gamma2, config.alpha2, config.beta2);

    let result = u64::from(config.base_fee) + sum_of_sigmoids;

    // Solidity unchecked: assert(result <= type(uint16).max)
    debug_assert!(result <= u64::from(u16::MAX));
    result as u16
}

/// Inner sigmoid implementation using u64 arithmetic.
/// Minimal `x` parameter type — callers should upcast.
fn sigmoid_uint64(x: u64, g: u16, alpha: u16, beta: u32) -> u64 {
    if x > u64::from(beta) {
        let x_shifted = x - u64::from(beta);
        if x_shifted >= 6 * u64::from(g) {
            return u64::from(alpha);
        }
        let g4_val = u64::from(g).pow(4);
        let g4 = U256::from(g4_val);
        let ex = U256::from(exp_xg4(x_shifted, g, g4_val));
        (U256::from(alpha) * ex / (g4 + ex)).to::<u64>()
    } else {
        let x_shifted = u64::from(beta) - x;
        if x_shifted >= 6 * u64::from(g) {
            return 0;
        }
        let g4_val = u64::from(g).pow(4);
        let g4 = U256::from(g4_val);
        let ex = g4 + U256::from(exp_xg4(x_shifted, g, g4_val));
        (U256::from(alpha) * g4 / ex).to::<u64>()
    }
}

/// Calculates e^(x/g) * g^4 via a Taylor-series expansion.
///
/// Solidity equivalent: AdaptiveFee.expXg4()
///
/// Uses a look-up table for the integer part of (x / g) and a
/// Taylor series around zero for the fractional remainder.
fn exp_xg4(x: u64, g: u16, g_highest_degree: u64) -> u64 {
    // --- assembly block ---
    // xdg = x / g (integer), closestValue = approx round(e^xdg) * 1e20
    // x is replaced by x % g (remainder)
    let (mut closest_value, mut x_rem) = if x < u64::from(g) {
        // xdg = 0 → closestValue = 1e20
        (E_POW_0_TIMES_1E20, x)
    } else {
        let xdg = x / u64::from(g);
        let rem = x % u64::from(g);
        let cv = match xdg {
            0 => E_POW_0_TIMES_1E20,
            1 => E_POW_1_TIMES_1E20,
            2 => E_POW_2_TIMES_1E20,
            3 => E_POW_3_TIMES_1E20,
            4 => E_POW_4_TIMES_1E20,
            _ => E_POW_5_TIMES_1E20,
        };
        (cv, rem)
    };

    // --- unchecked block ---
    // 0.5-step adjustment: if remainder >= g/2, scale closest_value by e^0.5
    if x_rem >= u64::from(g) / 2 {
        x_rem -= u64::from(g) / 2;
        closest_value = (U256::from(closest_value) * U256::from(E_POW_HALF_TIMES_1E20)
            / U256::from(E20))
        .to::<u128>();
    }

    // Taylor series of e^(x/g) * g^4 around 0 (x_rem/g <= 0.5)
    let g_u128 = u128::from(g);
    let x = u128::from(x_rem);
    let mut res = u128::from(g_highest_degree); // g^4

    // g^3 term
    let mut g_pow = u128::from(g_highest_degree) / g_u128;
    res += x * g_pow; // g^4 + x * g^3

    // g^2 term
    g_pow /= g_u128;
    let x2 = x * x;
    res += (x2 * g_pow) / 2; // + (x^2 * g^2) / 2

    // g^1 term (the original g is used in the formula, not g_pow)
    let x3 = x2 * x;
    res += (x3 * g_u128 * 4 + x3 * x) / 24; // + (x^3(4*g + x)) / 24

    // Final: multiply by closest_value / 1e20.
    // Intermediate product can exceed 128 bits (≤ 155 + 75 = 230 bits),
    // so we widen to U256.
    let result = (U256::from(res) * U256::from(closest_value) / U256::from(E20)).to::<u128>();

    debug_assert!(result <= u128::from(u64::MAX));
    result as u64
}

// ---------------------------------------------------------------------------
// Constants:  e^n * 10^20   (matching Solidity assembly look-up-table exactly)
// ---------------------------------------------------------------------------
const E20: u128 = 100_000_000_000_000_000_000; // 10^20

const E_POW_0_TIMES_1E20: u128 = 100_000_000_000_000_000_000; // e^0 = 1
const E_POW_1_TIMES_1E20: u128 = 271_828_182_845_904_523_536; // e^1
const E_POW_2_TIMES_1E20: u128 = 738_905_609_893_065_022_723; // e^2
const E_POW_3_TIMES_1E20: u128 = 2_008_553_692_318_766_774_092; // e^3
const E_POW_4_TIMES_1E20: u128 = 5_459_815_003_314_423_907_811; // e^4
const E_POW_5_TIMES_1E20: u128 = 14_841_315_910_257_660_342_111; // e^5

/// e^0.5 × 10^20 ≈ 1.64872127070012814684 × 10^20
const E_POW_HALF_TIMES_1E20: u128 = 164_872_127_070_012_814_684;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sigmoid_x_gt_beta_saturates_at_alpha() {
        // x - beta >= 6 * gamma → sigmoid returns alpha directly
        let result = sigmoid_uint64(714, 59, 2900, 360);
        assert_eq!(result, 2900);
    }

    #[test]
    fn test_sigmoid_x_lt_beta_saturates_at_zero() {
        // beta - x >= 6 * gamma → sigmoid returns 0 directly
        let result = sigmoid_uint64(5, 59, 2900, 360);
        assert_eq!(result, 0);
    }

    #[test]
    fn test_sigmoid_at_midpoint() {
        // x == beta → sigmoid ≈ alpha / 2  (e^0 == 1, formula becomes alpha/2)
        let result = sigmoid_uint64(360, 59, 2900, 360);
        assert!(result > 1300 && result < 1600, "got {result}");
    }

    #[test]
    fn test_get_fee_zero_volatility() {
        let cfg = initial_fee_configuration();
        let fee = get_fee(0, &cfg);
        // Both sigmoids return 0 → fee = baseFee = 100
        assert_eq!(fee, 100);
    }

    #[test]
    fn test_get_fee_increases_with_volatility() {
        let cfg = initial_fee_configuration();
        let low = get_fee(0, &cfg);
        let high = get_fee(5_000_000, &cfg);
        assert!(
            high >= low,
            "fee should not decrease with higher volatility"
        );
    }

    #[test]
    fn test_get_fee_bounded() {
        let cfg = AlgebraFeeConfig {
            alpha1: 5000,
            alpha2: 5000,
            beta1: 1000,
            beta2: 100_000,
            gamma1: 10,
            gamma2: 100,
            base_fee: 1000,
        };
        let maximum = u64::from(cfg.base_fee) + u64::from(cfg.alpha1) + u64::from(cfg.alpha2);
        let result = get_fee(99_999_999, &cfg);
        assert!(u64::from(result) <= maximum);
    }

    #[test]
    fn test_default_config_sanity() {
        // Verify the default config values match Algebra's spec
        let cfg = initial_fee_configuration();
        assert_eq!(cfg.base_fee, 100);
        assert_eq!(cfg.alpha1, 2900);
        assert_eq!(cfg.alpha2, 12000);
        assert_eq!(cfg.beta1, 360);
        assert_eq!(cfg.beta2, 60000);
        assert_eq!(cfg.gamma1, 59);
        assert_eq!(cfg.gamma2, 8500);
    }
}
