//! The dual-threshold accumulation walk — `get_new_snow_dual_threshold_from_table`.
//!
//! One pass over a depth series, carrying a **baseline** and the previous sample, with
//! three rules. It is what produces `snowtoday` (new snow since the station's local
//! midnight) and `snowstorm` (new snow in the last twenty-four hours).
//!
//! # Why it is not `max - min`, and not a sum of positive deltas
//!
//! Both of those were tried in the Go tree and both are recorded as defects in its own
//! migration comments:
//!
//! - **A sum of positive deltas** counts every upward wobble of a sonic sensor as snowfall.
//!   Migration 019: *"simple positive delta (captures every tiny fluctuation ...): counts
//!   every fluctuation (215 mm for 72 h — too high)"*.
//! - **`max - min`** over daily rows misses a storm that fell and settled inside one day.
//!   Same migration: *"simple delta on daily data: misses intraday accumulation (34 mm —
//!   too low)"*.
//!
//! The dual threshold is the third answer and the one that shipped: a **quick** rule for a
//! step change inside one bucket, a **gradual** rule for a slow rise away from the
//! baseline, and a **melt** rule that re-bases without counting anything. The same walk
//! produces 94 mm where the other two produce 215 and 34.
//!
//! # It runs in Rust and not in PL/pgSQL, and that is **R2**
//!
//! Go evaluates this as a `FOR ... IN EXECUTE` loop inside a PL/pgSQL function, once per
//! request on a cache miss and once per 30-second refresh otherwise. `docs/service-architecture.md`
//! §2.5's **R2** puts a value that is a function of a *window* of readings in a component
//! that maintains it, and §1.2 forbids `mw-pipeline` both `sqlx` and `mw-store` — so the
//! series arrives through `mw_component::ReadingStore::mean_series` and the walk is this
//! function. What that buys beyond layering: it is unit-testable against a constructed
//! series, which a PL/pgSQL loop reachable only through a live TimescaleDB is not.

/// A step change inside one bucket that counts as accumulation, in millimetres.
///
/// `quick_threshold FLOAT := 20.0; -- 0.8" rapid accumulation`.
pub const QUICK_MM: f64 = 20.0;

/// A rise above the carried baseline that counts as accumulation, in millimetres.
///
/// `gradual_threshold FLOAT := 15.0; -- 0.6" gradual accumulation`.
pub const GRADUAL_MM: f64 = 15.0;

/// A fall below the carried baseline that re-bases it, in millimetres.
///
/// `melt_threshold FLOAT := 10.0; -- >10mm drop = melting`.
pub const MELT_MM: f64 = 10.0;

/// How close to the base distance a reading may be before it is ignored, in millimetres.
///
/// The source query's `snowdistance < p_base_distance - 2`, which migration 011 added and
/// explained:
///
/// > *When `snowdistance` equals `base_distance` (depth = 0 mm, a valid "no snow" reading),
/// > the baseline gets reset to 0 mm. Later depth increases are then incorrectly counted as
/// > new snow.*
///
/// So bare ground is **excluded from the series** rather than entered as a zero. It is a
/// property of the *filter* and not of the walk, which is why it is applied by
/// [`depths_from`] rather than inside [`accumulated_mm`].
pub const BARE_GROUND_MM: f64 = 2.0;

/// Walk a depth series and return the accumulated new snow, in millimetres.
///
/// `depths` is oldest first, in millimetres, already filtered by [`depths_from`]. The
/// return is never negative: every rule either adds a positive quantity or adds nothing.
///
/// **The first sample sets the baseline and contributes nothing**, which is the Go loop's
/// `IF prev_depth IS NULL THEN ... CONTINUE`. A one-sample window therefore returns zero —
/// correct, and worth stating because it is what a station that has just started reporting
/// returns rather than its whole depth.
pub fn accumulated_mm(depths: &[f64]) -> f64 {
    let mut total = 0.0;
    let mut baseline = f64::NAN;
    let mut previous = f64::NAN;
    for &depth in depths {
        if previous.is_nan() {
            baseline = depth;
            previous = depth;
            continue;
        }
        let delta = depth - previous;
        if delta > QUICK_MM {
            // A step inside one bucket. The whole step counts, and the baseline moves to
            // the new depth so the gradual rule does not count it a second time.
            total += delta;
            baseline = depth;
        } else if depth > baseline + GRADUAL_MM {
            // A slow rise. What counts is the distance from the baseline, not the last
            // delta -- which is how an hour of 5 mm increments becomes one 20 mm event
            // rather than four sub-threshold nothings.
            total += depth - baseline;
            baseline = depth;
        } else if depth < baseline - MELT_MM {
            // Settling, melt or wind. Nothing counts and the baseline follows the snow
            // down, so the next storm is measured from where the pack actually is.
            baseline = depth;
        }
        previous = depth;
    }
    total
}

/// A distance series as a depth series, with bare ground and absent buckets dropped.
///
/// `base_distance_mm - snowdistance`, which is the source query's `$2 - snowdistance`, with
/// the two filters the query carries: `snowdistance IS NOT NULL` (a bucket no reading fell
/// in, which is `None` here) and `snowdistance < base - 2` ([`BARE_GROUND_MM`]).
///
/// The input is `(instant, Option<f64>)` because that is what a bucket series is, and the
/// instants come back with the depths because [`super::pelt`] needs them to label a
/// segment. The walk above does not.
pub fn depths_from<T: Copy>(series: &[(T, Option<f64>)], base_distance_mm: f64) -> Vec<(T, f64)> {
    series
        .iter()
        .filter_map(|&(at, distance)| {
            let distance = distance?;
            if !distance.is_finite() || distance >= base_distance_mm - BARE_GROUND_MM {
                return None;
            }
            Some((at, base_distance_mm - distance))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depths(series: &[(i32, Option<f64>)], base: f64) -> Vec<f64> {
        depths_from(series, base)
            .into_iter()
            .map(|(_, d)| d)
            .collect()
    }

    #[test]
    fn an_empty_or_single_sample_window_accumulates_nothing() {
        assert_eq!(accumulated_mm(&[]), 0.0);
        assert_eq!(
            accumulated_mm(&[120.0]),
            0.0,
            "the first sample is the baseline"
        );
    }

    /// The quick rule: a step larger than 20 mm between two consecutive samples counts
    /// whole.
    #[test]
    fn a_step_change_counts_in_full() {
        assert_eq!(accumulated_mm(&[0.0, 30.0]), 30.0);
        assert_eq!(accumulated_mm(&[0.0, 30.0, 60.0]), 60.0, "two steps");
    }

    /// The gradual rule: four 5 mm increments are below the quick threshold individually
    /// and together clear the gradual one, so they count once, from the baseline.
    #[test]
    fn a_slow_rise_counts_once_from_the_baseline() {
        let total = accumulated_mm(&[0.0, 5.0, 10.0, 15.0, 20.0]);
        assert_eq!(total, 20.0, "20 mm above the baseline, counted once");
        // And a rise that never clears the gradual threshold counts nothing, which is the
        // sensor-noise case the whole rule exists for.
        assert_eq!(accumulated_mm(&[0.0, 5.0, 10.0, 14.0]), 0.0);
    }

    /// The melt rule re-bases without counting, and the **next** storm is then measured
    /// from the settled pack rather than from the old peak.
    #[test]
    fn settling_rebases_the_baseline_and_counts_nothing() {
        // Up 30 (counted), down 20 (re-based to 10), up 30 from there (counted).
        let total = accumulated_mm(&[0.0, 30.0, 10.0, 40.0]);
        assert_eq!(total, 60.0);
        // Melt alone counts nothing.
        assert_eq!(accumulated_mm(&[100.0, 80.0, 60.0, 40.0]), 0.0);
    }

    /// A fall smaller than the melt threshold leaves the baseline where it is, which is
    /// what stops a wobbling sensor from re-basing downwards and then "accumulating" back
    /// to where it was.
    #[test]
    fn a_small_fall_does_not_rebase() {
        // 0 -> 30 counts (quick). Then 25: a 5 mm fall, below the melt threshold, so the
        // baseline stays at 30. Back to 30: a 5 mm rise, below both the quick and the
        // gradual thresholds, so nothing counts.
        assert_eq!(accumulated_mm(&[0.0, 30.0, 25.0, 30.0]), 30.0);
    }

    /// Migration 019's own numbers, as the shape of the three algorithms rather than the
    /// production figures: a series that wobbles and then accumulates is counted once by
    /// this walk and many times by a sum of positive deltas.
    #[test]
    fn the_walk_rejects_what_a_sum_of_positive_deltas_would_count() {
        // Eight 3 mm wobbles about one depth, then nothing.
        let mut series = vec![100.0];
        for i in 0..8 {
            series.push(100.0 + if i % 2 == 0 { 3.0 } else { 0.0 });
        }
        let positive_deltas: f64 = series.windows(2).map(|w| f64::max(w[1] - w[0], 0.0)).sum();
        assert!(positive_deltas >= 12.0, "a naive sum counts every wobble");
        assert_eq!(accumulated_mm(&series), 0.0, "this walk counts none of it");
    }

    /// The bare-ground filter, which migration 011 added because a depth of exactly zero
    /// re-based the baseline and made the next snowfall count twice.
    #[test]
    fn bare_ground_is_dropped_from_the_series_rather_than_entered_as_zero() {
        let base = 1500.0;
        let series = [
            (1, Some(1500.0)), // depth 0 -- bare ground, dropped
            (2, Some(1499.0)), // depth 1 -- within 2 mm of base, dropped
            (3, Some(1498.0)), // depth 2 -- also within 2 mm, dropped
            (4, Some(1400.0)), // depth 100 -- kept
            (5, None),         // no reading in the bucket -- dropped
            (6, Some(1300.0)), // depth 200 -- kept
        ];
        assert_eq!(depths(&series, base), vec![100.0, 200.0]);
    }

    #[test]
    fn a_non_finite_distance_is_dropped() {
        let series = [(1, Some(f64::NAN)), (2, Some(1400.0))];
        assert_eq!(depths(&series, 1500.0), vec![100.0]);
    }

    /// The instants travel with the depths, which is what the changepoint classifier needs
    /// and the walk does not.
    #[test]
    fn the_filter_keeps_each_depths_own_instant() {
        let series = [(7, Some(1400.0)), (9, Some(1300.0))];
        assert_eq!(depths_from(&series, 1500.0), vec![(7, 100.0), (9, 200.0)]);
    }
}
