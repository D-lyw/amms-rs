use alloy::primitives::U256;

pub fn sample_amounts(balance: U256, decimals: u8) -> Vec<U256> {
    let base = U256::from(10).pow(U256::from(decimals));
    let a1 = std::cmp::min(balance / U256::from(1000u64), base * U256::from(10u64));
    let a2 = std::cmp::min(balance / U256::from(100u64), base * U256::from(100u64));

    let mut out = Vec::new();
    out.push(if a1.is_zero() { U256::from(1u64) } else { a1 });

    let a2v = if a2.is_zero() { U256::from(1u64) } else { a2 };
    if a2v != out[0] {
        out.push(a2v);
    }

    out
}
