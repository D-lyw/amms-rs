use alloy::primitives::U256;
use std::sync::OnceLock;
use uniswap_v3_math::{
    error::UniswapV3MathError,
    tick_math::{self, MAX_TICK, MIN_TICK},
};

static V3_TICK_SQRT_RATIO_TABLE: OnceLock<Box<[U256]>> = OnceLock::new();

const V3_TICK_COUNT: usize = (MAX_TICK - MIN_TICK + 1) as usize;

#[inline]
fn tick_to_index(tick: i32) -> usize {
    (tick - MIN_TICK) as usize
}

fn build_v3_tick_sqrt_ratio_table() -> Result<Box<[U256]>, UniswapV3MathError> {
    let mut table = Vec::with_capacity(V3_TICK_COUNT);
    for tick in MIN_TICK..=MAX_TICK {
        table.push(tick_math::get_sqrt_ratio_at_tick(tick)?);
    }
    Ok(table.into_boxed_slice())
}

pub fn prewarm_v3_tick_sqrt_ratio_cache() -> Result<(), UniswapV3MathError> {
    if V3_TICK_SQRT_RATIO_TABLE.get().is_some() {
        return Ok(());
    }

    let table = build_v3_tick_sqrt_ratio_table()?;
    let _ = V3_TICK_SQRT_RATIO_TABLE.set(table);
    Ok(())
}

#[inline]
pub fn v3_tick_sqrt_ratio_cache_ready() -> bool {
    V3_TICK_SQRT_RATIO_TABLE.get().is_some()
}

#[inline]
pub fn sqrt_ratio_at_tick_cached_or_compute(tick: i32) -> Result<U256, UniswapV3MathError> {
    if let Some(table) = V3_TICK_SQRT_RATIO_TABLE.get() {
        if (MIN_TICK..=MAX_TICK).contains(&tick) {
            return Ok(table[tick_to_index(tick)]);
        }
    }

    tick_math::get_sqrt_ratio_at_tick(tick)
}

#[cfg(test)]
mod tests {
    use super::{
        prewarm_v3_tick_sqrt_ratio_cache, sqrt_ratio_at_tick_cached_or_compute,
        v3_tick_sqrt_ratio_cache_ready,
    };
    use uniswap_v3_math::tick_math::{self, MAX_TICK, MIN_TICK};

    #[test]
    fn cached_or_compute_matches_tick_math() {
        for tick in [MIN_TICK, -100, -1, 0, 1, 100, MAX_TICK] {
            let expected = tick_math::get_sqrt_ratio_at_tick(tick).unwrap();
            let actual = sqrt_ratio_at_tick_cached_or_compute(tick).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn prewarm_is_idempotent() {
        prewarm_v3_tick_sqrt_ratio_cache().unwrap();
        assert!(v3_tick_sqrt_ratio_cache_ready());

        prewarm_v3_tick_sqrt_ratio_cache().unwrap();
        assert!(v3_tick_sqrt_ratio_cache_ready());

        let expected = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
        let actual = sqrt_ratio_at_tick_cached_or_compute(0).unwrap();
        assert_eq!(actual, expected);
    }
}
