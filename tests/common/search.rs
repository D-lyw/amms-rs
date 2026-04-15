use alloy::primitives::U256;
use eyre::Result;
use std::future::Future;

pub async fn find_min_dx_onchain<F, Fut>(mut get_dy: F, target_out: U256, hi: U256) -> Result<U256>
where
    F: FnMut(U256) -> Fut,
    Fut: Future<Output = Result<U256>>,
{
    if hi.is_zero() {
        return Ok(U256::ZERO);
    }

    let mut lo = U256::ZERO;
    let mut hi = hi;

    while lo < hi {
        let mid = (lo + hi) >> 1;
        let out = get_dy(mid).await?;
        if out >= target_out {
            hi = mid;
        } else {
            lo = mid + U256::from(1u8);
        }
    }

    Ok(lo)
}
