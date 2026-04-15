use alloy::primitives::U256;

pub fn assert_diff_within_ppm(local: U256, chain: U256, threshold_ppm: u64) {
    if local == chain {
        return;
    }

    if chain.is_zero() {
        panic!(
            "Quote parity invalid: chain=0 local={} threshold_ppm={}",
            local, threshold_ppm
        );
    }

    let diff = if local > chain {
        local - chain
    } else {
        chain - local
    };

    let ratio = diff * U256::from(1_000_000u64) / chain;
    println!(
        "Diff: {}, Ratio: {} ppm (Threshold: {})",
        diff, ratio, threshold_ppm
    );

    assert!(ratio <= U256::from(threshold_ppm), "Deviation too high!");
}
