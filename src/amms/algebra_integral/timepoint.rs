//! Volatility-oracle timepoint cache for Algebra Integral dynamic fee.
//!
//! Mirrors the on-chain `VolatilityOracle` library at:
//! packages/volatility-oracle/contracts/libraries/VolatilityOracle.sol
//!
//! Maintains a circular buffer of `Timepoint` structs that are written on
//! every swap (via the plugin's `beforeSwap` hook).  The cache is seeded
//! during pool initialisation and incrementally updated during event replay.

// TimepointCache does not depend on alloy primitives.
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single timepoint written by the volatility oracle on each swap.
///
/// Packed into exactly one EVM storage slot (31 bytes ⇒ fits in 32).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Timepoint {
    pub initialized: bool,
    pub block_timestamp: u32,
    /// Running tick accumulator (int56 in Solidity, fits in i64).
    pub tick_cumulative: i64,
    /// Running volatility accumulator (uint88 → uses u128 for safety).
    pub volatility_cumulative: u128,
    /// Tick value at this timepoint (int24).
    pub tick: i32,
    /// Average tick over the WINDOW (1 day) at this timepoint (int24).
    pub average_tick: i32,
    /// Index of the closest timepoint ≤ (blockTimestamp – WINDOW).
    pub window_start_index: u16,
}

/// Locally-maintained circular buffer of timepoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimepointCache {
    /// Circular buffer; allocated with `cardinality` active entries.
    pub timepoints: Vec<Option<Timepoint>>,
    /// Index of the most recently written timepoint.
    pub index: u16,
    /// Number of active slots in the circular buffer.
    pub cardinality: u16,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The oracle window (1 day in seconds).
const WINDOW: u32 = 86_400;

/// Max timepoint array size (as in the VolatilityOracle).
const UINT16_MODULO: u32 = 65536;

/// Helper: cast u16 cardinality to u32 for modulo comparisons.
const fn card_u32(card: u16) -> u32 {
    card as u32
}

// ---------------------------------------------------------------------------
// Construction & seed helpers
// ---------------------------------------------------------------------------

impl TimepointCache {
    /// Create a new empty cache that has not yet been seeded.
    pub fn empty() -> Self {
        Self {
            timepoints: vec![None; UINT16_MODULO as usize],
            index: 0,
            cardinality: 0,
        }
    }

    /// Seed from a batch of timepoints fetched on-chain.
    ///
    /// `timepoints_slice` should contain consecutive entries starting from
    /// `oldest_index` through to (and including) the entry at `current_index`.
    /// The last entry in the slice is assumed to be at `current_index`.
    pub fn seed(
        timepoints_slice: &[(u16, Timepoint)],
        current_index: u16,
        cardinality: u16,
    ) -> Self {
        let mut cache = Self::empty();
        cache.cardinality = cardinality;
        cache.index = current_index;
        for &(idx, ref tp) in timepoints_slice {
            cache.timepoints[idx as usize] = Some(*tp);
        }
        cache
    }

    // -----------------------------------------------------------------------
    // Public queries
    // -----------------------------------------------------------------------

    /// Return a reference to the most recent (last) timepoint.
    pub fn last(&self) -> Option<&Timepoint> {
        if self.cardinality == 0 {
            return None;
        }
        self.timepoints[self.index as usize].as_ref()
    }

    /// Return the index of the oldest initialised timepoint.
    ///
    /// Returns `None` when no timepoint has been written yet.
    /// Scans from 0 upward to handle partially-seeded caches (e.g. init
    /// read only a suffix of the circular buffer).
    pub fn oldest_index(&self) -> Option<u16> {
        if self.cardinality == 0 {
            return None;
        }
        // Scan forward from 0 to find the first seeded timepoint.
        for i in 0..=self.index {
            if self.timepoints[i as usize].is_some() {
                return Some(i);
            }
        }
        // Fallback: wrapped case — oldest is the next slot after index.
        let next = self.index.wrapping_add(1);
        if self.timepoints[next as usize].is_some() {
            Some(next)
        } else {
            Some(0)
        }
    }

    // -----------------------------------------------------------------------
    // Core volatility queries
    // -----------------------------------------------------------------------

    /// Compute the average volatility over the last `WINDOW` (1 day).
    ///
    /// Mirrors `VolatilityOracle.getAverageVolatility()`.
    pub fn get_average_volatility(&self, current_time: u32, tick: i32) -> Option<u64> {
        let last = self.last()?;
        let last_index = self.index;
        let oldest_idx = self.oldest_index()?;
        let oldest = self.timepoints[oldest_idx as usize].as_ref()?;

        let time_at_last = last.block_timestamp == current_time;
        let last_cumulative_vol = if time_at_last {
            last.volatility_cumulative
        } else {
            self.get_volatility_cumulative_at(current_time, 0, tick, last_index, oldest_idx)?
                as u128
        };

        if lte_considering_overflow(
            oldest.block_timestamp,
            current_time.wrapping_sub(WINDOW),
            current_time,
        ) {
            // Oldest timepoint is earlier than 24 hours ago → normal case.
            let cumulative_at_start = if time_at_last {
                // Interpolate between windowStartIndex and windowStartIndex+1.
                let ws = last.window_start_index as usize;
                let tp_ws = self.timepoints[ws].as_ref()?;
                let tp_ws_next = self.timepoints[ws.wrapping_add(1)].as_ref()?;
                let ts_delta = tp_ws_next
                    .block_timestamp
                    .wrapping_sub(tp_ws.block_timestamp);
                let target = current_time.wrapping_sub(WINDOW);
                let target_delta = target.wrapping_sub(tp_ws.block_timestamp);
                if ts_delta == 0 {
                    tp_ws.volatility_cumulative
                } else {
                    tp_ws.volatility_cumulative
                        + (tp_ws_next.volatility_cumulative - tp_ws.volatility_cumulative)
                            * u128::from(target_delta)
                            / u128::from(ts_delta)
                }
            } else {
                self.get_volatility_cumulative_at(
                    current_time,
                    WINDOW,
                    tick,
                    last_index,
                    oldest_idx,
                )? as u128
            };

            let diff = last_cumulative_vol.saturating_sub(cumulative_at_start);
            Some((diff / u128::from(WINDOW)) as u64)
        } else if current_time != oldest.block_timestamp {
            // Not enough data → extrapolate from oldest timepoint.
            let unbiased = if current_time != oldest.block_timestamp {
                let mut d = current_time.wrapping_sub(oldest.block_timestamp);
                if d > 1 {
                    d -= 1;
                }
                d
            } else {
                1
            };
            let diff = last_cumulative_vol.saturating_sub(oldest.volatility_cumulative);
            Some((diff / u128::from(unbiased)) as u64)
        } else {
            Some(0)
        }
    }

    // -----------------------------------------------------------------------
    // write() — called during event sync on every swap
    // -----------------------------------------------------------------------

    /// Write a new timepoint at `block_timestamp` with the current `tick`.
    ///
    /// Mirrors `VolatilityOracle.write()`.  Returns the new `index` and
    /// optionally the new `oldest_index` if the buffer has wrapped.
    pub fn write(&mut self, block_timestamp: u32, tick: i32) -> Option<()> {
        let last = self.last()?;

        // Early return: already written this block.
        if last.block_timestamp == block_timestamp {
            return Some(());
        }

        let next_idx = self.index.wrapping_add(1);
        let mut oldest = self.oldest_index().unwrap_or(0);

        // If the next slot is already initialised, the buffer has wrapped.
        if self.timepoints[next_idx as usize].is_some() {
            oldest = next_idx;
        }

        // Compute average tick and window start index.
        let (avg_tick, window_start_idx) = self.get_average_tick_casted(
            block_timestamp,
            tick,
            self.index,
            oldest,
            last.block_timestamp,
            last.tick_cumulative,
        )?;

        let window_start_idx = if window_start_idx == next_idx {
            window_start_idx.wrapping_add(1)
        } else {
            window_start_idx
        };

        let new_tp = create_new_timepoint(*last, block_timestamp, tick, avg_tick, window_start_idx);

        if card_u32(self.cardinality) < UINT16_MODULO {
            self.cardinality += 1;
        }

        self.index = next_idx;
        self.timepoints[next_idx as usize] = Some(new_tp);

        // After wrapping, advance oldest index.
        if oldest == next_idx {}

        Some(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Mirror of `_getAverageTick()` — compute average tick over WINDOW.
    fn get_average_tick_casted(
        &self,
        current_time: u32,
        tick: i32,
        last_index: u16,
        oldest_index: u16,
        last_timestamp: u32,
        last_tick_cumulative: i64,
    ) -> Option<(i32, u16)> {
        let (avg_tick_raw, window_start_idx) = self.get_average_tick(
            current_time,
            tick,
            last_index,
            oldest_index,
            last_timestamp,
            last_tick_cumulative,
        )?;
        // Overflow in uint16 cast is desired (as in Solidity).
        Some((avg_tick_raw as i32, window_start_idx as u16))
    }

    /// Mirror of `_getAverageTick()`.
    fn get_average_tick(
        &self,
        current_time: u32,
        tick: i32,
        last_index: u16,
        oldest_index: u16,
        last_timestamp: u32,
        last_tick_cumulative: i64,
    ) -> Option<(i64, u32)> {
        let oldest_tp = self.timepoints[oldest_index as usize].as_ref()?;
        let oldest_timestamp = oldest_tp.block_timestamp;
        let oldest_tick_cumulative = oldest_tp.tick_cumulative;

        // Update tickCumulative with current data.
        let current_tick_cumulative = last_tick_cumulative
            + i64::from(tick) * i64::from(current_time.wrapping_sub(last_timestamp));

        if !lte_considering_overflow(
            oldest_timestamp,
            current_time.wrapping_sub(WINDOW),
            current_time,
        ) {
            // Oldest is newer than WINDOW ago.
            let delta = current_time.wrapping_sub(oldest_timestamp);
            if delta == 0 {
                return Some((i64::from(tick), u32::from(oldest_index)));
            }
            let avg = (current_tick_cumulative - oldest_tick_cumulative) / i64::from(delta);
            return Some((avg, u32::from(oldest_index)));
        }

        if lte_considering_overflow(
            last_timestamp,
            current_time.wrapping_sub(WINDOW),
            current_time,
        ) {
            // Last timepoint is older or equal than WINDOW ago.
            return Some((i64::from(tick), u32::from(last_index)));
        }

        // Search between oldest and last timepoints.
        let (tick_cumulative_at_start, window_start_idx) =
            self.get_tick_cumulative_at(current_time, WINDOW, tick, last_index, oldest_index)?;

        let avg = (current_tick_cumulative - tick_cumulative_at_start) / i64::from(WINDOW);
        Some((avg, window_start_idx))
    }

    /// Mirror of `_getTickCumulativeAt()`.
    fn get_tick_cumulative_at(
        &self,
        time: u32,
        seconds_ago: u32,
        tick: i32,
        last_index: u16,
        oldest_index: u16,
    ) -> Option<(i64, u32)> {
        let target = time.wrapping_sub(seconds_ago);
        let (before_or_at, at_or_after, same_point, index_before_or_at) =
            self.get_timepoints_at(time, target, last_index, oldest_index)?;

        let ts_before = before_or_at.block_timestamp;
        let tc_before = before_or_at.tick_cumulative;

        if target == ts_before {
            return Some((tc_before, index_before_or_at));
        }
        if same_point {
            // Target is newer than last timepoint.
            let delta = target.wrapping_sub(ts_before);
            return Some((
                tc_before + i64::from(tick) * i64::from(delta),
                index_before_or_at,
            ));
        }

        let ts_after = at_or_after.block_timestamp;
        let tc_after = at_or_after.tick_cumulative;
        if target == ts_after {
            return Some((tc_after, index_before_or_at.wrapping_add(1)));
        }

        let timepoint_delta = ts_after.wrapping_sub(ts_before);
        let target_delta = target.wrapping_sub(ts_before);
        if timepoint_delta == 0 {
            return Some((tc_before, index_before_or_at));
        }
        let interpolated = tc_before
            + ((tc_after - tc_before) / i64::from(timepoint_delta)) * i64::from(target_delta);
        Some((interpolated, index_before_or_at))
    }

    /// Mirror of `_getVolatilityCumulativeAt()`.
    fn get_volatility_cumulative_at(
        &self,
        time: u32,
        seconds_ago: u32,
        tick: i32,
        last_index: u16,
        oldest_index: u16,
    ) -> Option<u64> {
        let target = time.wrapping_sub(seconds_ago);
        let (before_or_at, at_or_after, same_point, _) =
            self.get_timepoints_at(time, target, last_index, oldest_index)?;

        let ts_before = before_or_at.block_timestamp;
        let vc_before = before_or_at.volatility_cumulative;

        if target == ts_before {
            return Some(vc_before as u64);
        }
        if same_point {
            // Target is newer than last timepoint.
            let (avg_tick, _) = self.get_average_tick_casted(
                target,
                tick,
                last_index,
                oldest_index,
                ts_before,
                before_or_at.tick_cumulative,
            )?;
            let delta = target.wrapping_sub(ts_before);
            let extra = volatility_on_range(
                i64::from(delta),
                i64::from(tick),
                i64::from(tick),
                i64::from(before_or_at.average_tick),
                i64::from(avg_tick),
            );
            return Some((vc_before + extra) as u64);
        }

        let ts_after = at_or_after.block_timestamp;
        let vc_after = at_or_after.volatility_cumulative;
        if target == ts_after {
            return Some(vc_after as u64);
        }

        let timepoint_delta = ts_after.wrapping_sub(ts_before);
        let target_delta = target.wrapping_sub(ts_before);
        if timepoint_delta == 0 {
            return Some(vc_before as u64);
        }
        let interpolated = vc_before
            + ((vc_after - vc_before) / u128::from(timepoint_delta)) * u128::from(target_delta);
        Some(interpolated as u64)
    }

    /// Mirror of `_getTimepointsAt()` — binary search for the timepoints
    /// surrounding `target`.
    fn get_timepoints_at(
        &self,
        current_time: u32,
        target: u32,
        last_index: u16,
        oldest_index: u16,
    ) -> Option<(&Timepoint, &Timepoint, bool, u32)> {
        let last_tp = self.last()?;
        let last_tp_ts = last_tp.block_timestamp;
        let window_start_index = last_tp.window_start_index;

        // Target is newer than last timepoint.
        if target == current_time || lte_considering_overflow(last_tp_ts, target, current_time) {
            return Some((last_tp, last_tp, true, u32::from(last_index)));
        }

        let mut lo = u32::from(oldest_index);
        let hi = if last_index < oldest_index {
            u32::from(last_index) + u32::from(UINT16_MODULO)
        } else {
            u32::from(last_index)
        };

        // Heuristic: narrow search to window if target is close enough.
        if self.timepoints[window_start_index as usize].is_some()
            && last_tp_ts.wrapping_sub(target) <= WINDOW
        {
            lo = u32::from(window_start_index);
        }

        // Quick check at oldest boundary.
        let oldest_tp = self.timepoints[(lo % u32::from(UINT16_MODULO)) as usize].as_ref()?;
        if !lte_considering_overflow(oldest_tp.block_timestamp, target, current_time) {
            return None; // target is too old
        }
        if oldest_tp.block_timestamp == target {
            return Some((oldest_tp, oldest_tp, true, lo));
        }
        if hi == lo + 1 {
            return Some((oldest_tp, last_tp, false, lo));
        }

        // Binary search.
        let result_idx = self.binary_search(current_time, target, lo, hi, true)?;
        let before = self.timepoints[(result_idx % u32::from(UINT16_MODULO)) as usize].as_ref()?;
        let after =
            self.timepoints[((result_idx + 1) % u32::from(UINT16_MODULO)) as usize].as_ref()?;
        Some((before, after, false, result_idx))
    }

    /// Mirror of `_binarySearch()`.
    fn binary_search(
        &self,
        current_time: u32,
        target: u32,
        mut left: u32,
        mut right: u32,
        with_heuristic: bool,
    ) -> Option<u32> {
        let mut idx = if with_heuristic && right - left > 2 {
            left + 1
        } else {
            (left + right) >> 1
        };

        loop {
            let tp = self.timepoints[(idx % u32::from(UINT16_MODULO)) as usize].as_ref()?;
            if !tp.initialized {
                left = idx + 1;
                idx = (left + right) >> 1;
                continue;
            }

            if lte_considering_overflow(tp.block_timestamp, target, current_time) {
                // Before or at target.
                let next_tp =
                    self.timepoints[((idx + 1) % u32::from(UINT16_MODULO)) as usize].as_ref()?;
                if next_tp.initialized
                    && lte_considering_overflow(target, next_tp.block_timestamp, current_time)
                {
                    return Some(idx);
                }
                left = idx + 1;
            } else {
                right = idx - 1;
            }
            idx = (left + right) >> 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Create a new timepoint from the previous one.
///
/// Mirrors `VolatilityOracle._createNewTimepoint()`.
pub fn create_new_timepoint(
    last: Timepoint,
    block_timestamp: u32,
    tick: i32,
    average_tick: i32,
    window_start_index: u16,
) -> Timepoint {
    let delta = block_timestamp.wrapping_sub(last.block_timestamp);
    Timepoint {
        initialized: true,
        block_timestamp,
        tick_cumulative: last.tick_cumulative + i64::from(tick) * i64::from(delta),
        volatility_cumulative: last.volatility_cumulative
            + volatility_on_range(
                i64::from(delta),
                i64::from(tick),
                i64::from(tick),
                i64::from(last.average_tick),
                i64::from(average_tick),
            ),
        tick,
        average_tick,
        window_start_index,
    }
}

/// Compute volatility between two sequential timepoints.
///
/// Mirrors `VolatilityOracle._volatilityOnRange()`.
///
/// For interval [t0, t1] of length `dt`:
///   (tick(t) - avgTick(t))² = ((k-p)²·t² + 2(k-p)(b-q)·t + (b-q)²)
///   where k = tick1-tick0, b = tick0-avgTick0,
///         p = avgTick1-avgTick0, q = avgTick0
///
/// Result always fits in 88 bits.
pub fn volatility_on_range(
    dt: i64,
    tick0: i64,
    tick1: i64,
    avg_tick0: i64,
    avg_tick1: i64,
) -> u128 {
    // Widen to i128 — i64 overflows by >400M× for realistic dt/tick values.
    let dt = i128::from(dt);
    let tick0 = i128::from(tick0);
    let tick1 = i128::from(tick1);
    let avg_tick0 = i128::from(avg_tick0);
    let avg_tick1 = i128::from(avg_tick1);

    // (k - p) = (tick1-tick0) - (avgTick1-avgTick0)
    let k = (tick1 - tick0) - (avg_tick1 - avg_tick0); // (k-p) * dt
    let b = (tick0 - avg_tick0) * dt; // (b-q) * dt

    let sum_of_sequence = dt * (dt + 1); // sumOfSequence * 2
    let sum_of_squares = sum_of_sequence * (2 * dt + 1); // sumOfSquares * 6

    let numerator = k * k * sum_of_squares + 6 * b * k * sum_of_sequence + 6 * dt * b * b;
    let denominator = 6 * dt * dt;
    if denominator == 0 {
        return 0;
    }
    // Result is always non-negative (sum of squares), so `as u128` is safe.
    (numerator / denominator) as u128
}

/// Comparator for 32-bit timestamps (safe for 0 or 1 overflow).
/// Returns `true` if `a ≤ b` given that both are ≤ `current_time`.
fn lte_considering_overflow(a: u32, b: u32, current_time: u32) -> bool {
    let a_gt = a > current_time;
    let b_gt = b > current_time;
    if a_gt == b_gt {
        a <= b
    } else {
        a_gt
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lte_considering_overflow() {
        assert!(lte_considering_overflow(100, 200, 1000));
        assert!(!lte_considering_overflow(200, 100, 1000));
        // Overflow scenario.
        assert!(lte_considering_overflow(u32::MAX, 5, 10));
    }

    #[test]
    fn test_empty_cache_returns_none() {
        let cache = TimepointCache::empty();
        assert!(cache.last().is_none());
        assert!(cache.oldest_index().is_none());
        assert!(cache.get_average_volatility(1000, 0).is_none());
    }

    #[test]
    fn test_write_and_read() {
        let mut cache = TimepointCache::empty();

        // Seed with a single initial timepoint.
        let initial = Timepoint {
            initialized: true,
            block_timestamp: 1000,
            tick_cumulative: 0,
            volatility_cumulative: 0,
            tick: 0,
            average_tick: 0,
            window_start_index: 0,
        };
        cache.timepoints[0] = Some(initial);
        cache.index = 0;
        cache.cardinality = 1;

        // Write a new timepoint.
        cache.write(2000, 100).expect("write failed");
        let last = cache.last().expect("no last after write");
        assert_eq!(last.block_timestamp, 2000);
        assert_eq!(last.tick, 100);
        assert!(last.tick_cumulative > 0);
    }

    #[test]
    fn test_volatility_on_range_same_tick() {
        // dt=1, tick stays 0, avg stays 0 → volatility = 0
        let v = volatility_on_range(1, 0, 0, 0, 0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_volatility_on_range_nonzero() {
        // dt=10, tick goes from 0→100, avg stays 0
        let v = volatility_on_range(10, 0, 100, 0, 0);
        assert!(v > 0);
    }

    #[test]
    fn test_seed_from_slice() {
        let t0 = Timepoint {
            initialized: true,
            block_timestamp: 1000,
            tick_cumulative: 0,
            volatility_cumulative: 0,
            tick: 0,
            average_tick: 0,
            window_start_index: 0,
        };
        let cache = TimepointCache::seed(&[(0, t0)], 0, 1);
        assert!(cache.last().is_some());
        assert_eq!(cache.last().unwrap().block_timestamp, 1000);
    }
}
