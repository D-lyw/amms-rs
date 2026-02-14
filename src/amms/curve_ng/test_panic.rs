#[cfg(test)]
mod tests {
    use crate::amms::curve_ng::math::cryptoswap;
    use alloy::primitives::U256;

    #[test]
    fn test_panic_repro() {
        println!("Testing curve_ng math functions for panics...");

        let zero = U256::ZERO;
        let one = U256::from(1);

        // Test isqrt
        println!("Testing isqrt(0)...");
        let _ = cryptoswap::isqrt(zero);
        println!("Testing isqrt(1)...");
        let _ = cryptoswap::isqrt(one);

        // Test cbrt
        println!("Testing cbrt(0)...");
        let _ = cryptoswap::cbrt(zero);
        println!("Testing cbrt(1)...");
        let _ = cryptoswap::cbrt(one);

        // Test newton_d with edge cases
        println!("Testing newton_d(0 balance)...");
        let x = vec![zero, U256::from(10).pow(U256::from(18))];
        let ann = U256::from(100);
        let gamma = U256::from(10).pow(U256::from(13));
        let _ = cryptoswap::newton_d(ann, gamma, &x);

        // Test newton_y with zero D
        println!("Testing newton_y(zero D)...");
        let _ = cryptoswap::newton_y(ann, gamma, &x, zero, 0);

        // Test geometric_mean with zero
        println!("Testing geometric_mean(zero)...");
        let _ = cryptoswap::geometric_mean(&x, true);

        // Test reduction_coefficient
        println!("Testing reduction_coefficient(zero sum)...");
        let zero_vec = vec![zero, zero];
        let _ = cryptoswap::reduction_coefficient(&zero_vec, gamma);

        // Test get_y_optimized
        println!("Testing get_y_optimized(normal)...");
        let x_3 = vec![
            U256::from(1e18 as u64),
            U256::from(1e18 as u64),
            U256::from(1e18 as u64),
        ];
        let d = U256::from(3e18 as u64);
        let _ = cryptoswap::get_y_optimized(ann, gamma, &x_3, d, 0);

        println!("All local tests passed without panic.");

        // Test extreme values
        println!("Testing extreme values...");
        let max = U256::MAX;
        let x_huge = vec![max, max, max];
        let d_huge = max;
        // This might error but should not panic
        let _ = cryptoswap::get_y_optimized(ann, gamma, &x_huge, d_huge, 0);

        // Test small non-zero values
        println!("Testing small non-zero values...");
        let small = U256::from(1);
        let x_small = vec![small, small, small];
        let d_small = small;
        let _ = cryptoswap::get_y_optimized(ann, gamma, &x_small, d_small, 0);
    }
}
