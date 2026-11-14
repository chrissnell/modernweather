//! Local upper-quantile smoothing and rate limiting — `internal/snow/smoothing.go`.
//!
//! The two passes that turn a sonic gauge's distance series into a **physically plausible**
//! depth series, which is what `snowlast24`, `snowlast72` and `snowseason` are `max - min`
//! and `max - first` of. `snow_depth_estimates`' own migration has why a maintained table
//! is the only available shape; this module is the arithmetic that fills it.
//!
//! # What the two passes are each for
//!
//! A sonic snow gauge pointed at a snowpack reports a series that is noisy in one direction
//! and real in the other: a gust, a bird or a clump falling off the mast reads as a sudden
//! *decrease* in distance (an increase in depth) and recovers, while settling is a genuine
//! slow decrease in depth.
//!
//! 1. **The upper quantile** ([`local_quantile`]) takes the 85th percentile of the depths
//!    within ±30 minutes of each sample. An upper quantile rather than a median because the
//!    signal is one-sided: the deepest plausible reading in the window is the one nearest
//!    the truth, and a median would track the sensor's dropouts down.
//! 2. **The rate limit** ([`rate_limit`]) caps the change between consecutive samples at
//!    4 inches an hour up and 1.5 down. Snow does not fall faster than the first or settle
//!    faster than the second, so anything that does is the sensor.
//!
//! Both are ported with their constants and their quirks. The one quirk worth naming before
//! reading the code is the quantile's **three-point band**, which is not a percentile: Go
//! averages the quantile index and the two samples above it, clamped at the end of the
//! window. It is reproduced exactly, including the clamp's asymmetry, because changing it
//! would change every stored estimate and therefore three numbers on a frozen response.

use chrono::{DateTime, Utc};

/// The half-window for the quantile pass, in minutes.
///
/// `WindowMinutes: 30` — ±30 minutes, so an hour of context per sample.
pub const WINDOW_MINUTES: i64 = 30;

/// Which quantile of the local window to take.
///
/// `Quantile: 0.85`.
pub const QUANTILE: f64 = 0.85;

/// The fastest plausible accumulation, inches per hour.
///
/// `MaxUpRateInPerHour: 4.0`.
pub const MAX_UP_IN_PER_HOUR: f64 = 4.0;

/// The fastest plausible settling, inches per hour.
///
/// `MaxDownRateInPerHour: 1.5`.
pub const MAX_DOWN_IN_PER_HOUR: f64 = 1.5;

/// One `(instant, depth in inches)` sample.
///
/// Inches, because the rate limit is expressed in inches per hour: converting at the end
/// would mean limiting in one unit and storing in another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// When.
    pub at: DateTime<Utc>,
    /// The depth, inches.
    pub depth_in: f64,
}

/// The local upper quantile of each sample's ±[`WINDOW_MINUTES`] window.
///
/// `LocalQuantileSmooth`, including its three-point band. Returns one value per input
/// sample, in the same order.
///
/// # The band, exactly as Go computes it
///
/// ```text
/// idx  = floor(0.85 * (n - 1))        the quantile index into the sorted window
/// low  = idx
/// high = min(idx + 2, n - 1)          three points, clamped at the top
/// if high - low < 2 and low > 0:      the clamp cost a point, so take it from below
///     low = max(high - 2, 0)
/// value = mean(sorted[low ..= high])
/// ```
///
/// So it is the mean of up to three order statistics near the 85th percentile rather than
/// the percentile itself — "optionally average a small band around the quantile for extra
/// stability", in the original's words. Two consequences are reproduced rather than fixed:
/// the band is **asymmetric** (it reaches upward first), and on a window of one or two
/// samples it collapses to the whole window.
///
/// # Linear in the ordinary case, quadratic only if the series is out of order
///
/// Go rescans the whole series for every sample, which is `O(n²)` — and the input is always
/// **ascending by instant**, because it is a bucket series read `ORDER BY bucket`. On a
/// sorted series the window is a contiguous range, so two pointers find it in `O(n)` total.
/// That matters for one case, and it is the case that would otherwise not finish: the first
/// pass for a station has no previous estimate and therefore backfills **from the season's
/// start**, which at 5-minute buckets is about 26 000 samples — 676 million window
/// comparisons under Go's scan, inside a 60-second deadline, on a release target measured
/// at ARM1176.
///
/// The sweep is an exact reformulation and not an approximation: on ascending input it
/// selects the same samples. Sortedness is **checked** rather than assumed, and an
/// out-of-order series falls back to the scan — which gives the same answer, and is what
/// stops this being a silent difference if a caller ever hands it something else.
pub fn local_quantile(samples: &[Sample]) -> Vec<f64> {
    let window = chrono::Duration::minutes(WINDOW_MINUTES);
    let ascending = samples.windows(2).all(|pair| pair[0].at <= pair[1].at);
    let mut out = Vec::with_capacity(samples.len());
    let mut depths: Vec<f64> = Vec::new();
    // The sweep's two pointers: `lo` is the first sample at or after the window's start and
    // `hi` is one past the last at or before its end. Neither ever goes backwards, which is
    // what makes the whole pass linear.
    let mut lo = 0usize;
    let mut hi = 0usize;
    for sample in samples {
        let from = sample.at - window;
        let until = sample.at + window;
        depths.clear();
        if ascending {
            while lo < samples.len() && samples[lo].at < from {
                lo += 1;
            }
            hi = hi.max(lo);
            while hi < samples.len() && samples[hi].at <= until {
                hi += 1;
            }
            depths.extend(samples[lo..hi].iter().map(|s| s.depth_in));
        } else {
            depths.extend(
                samples
                    .iter()
                    .filter(|s| s.at >= from && s.at <= until)
                    .map(|s| s.depth_in),
            );
        }
        if depths.is_empty() {
            // Unreachable -- a sample is inside its own window -- and answered rather than
            // asserted, for Go's own reason: "shouldn't happen, but use original value as
            // fallback".
            out.push(sample.depth_in);
            continue;
        }
        depths.sort_by(f64::total_cmp);
        out.push(band_mean(&depths));
    }
    out
}

/// The mean of the three-point band around the quantile index. See [`local_quantile`].
fn band_mean(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    let last = n - 1;
    let idx = (QUANTILE * last as f64).floor() as usize;
    let mut low = idx;
    let high = (idx + 2).min(last);
    // The clamp cost the band a point at the top, so it takes one from below -- but only if
    // there is a below to take it from, which is Go's `lowIdx > 0` guard.
    if high - low < 2 && low > 0 {
        low = high.saturating_sub(2);
    }
    let band = &sorted[low..=high];
    band.iter().sum::<f64>() / band.len() as f64
}

/// Cap the change between consecutive samples at the plausible rates.
///
/// `ApplyRateLimiting`. `smoothed` is [`local_quantile`]'s output and must be the same
/// length as `samples`; `previous` is the last estimate from **before** this pass's window,
/// which is what carries the limiter across an incremental update's seam.
///
/// # `previous` is the whole reason this is not a one-liner
///
/// Without it, every incremental pass restarts the limiter at its own first smoothed value —
/// so a step the limiter flattened at the end of the last pass reappears at the start of
/// this one, every fifteen minutes, forever. With it, the first sample in the window is
/// limited against a real earlier estimate.
///
/// `None` is the backfill case, and the port keeps Go's shape exactly: the **first** output
/// is the first smoothed value unlimited, and limiting starts at the second sample. That is
/// the one point in the series with no prior to limit against, and taking the smoothed value
/// is the only available answer.
///
/// # A non-advancing instant holds the previous depth
///
/// `if dtHours <= 0 { limited[i] = prevDepth; continue }` — and the `continue` does **not**
/// advance `prevTime`, so a duplicate instant is skipped rather than dividing by zero. The
/// bucket series this reads cannot contain one (the level's bucket is a primary key), and
/// the branch is kept because its absence would be a division by zero on an input shape
/// nothing forbids.
pub fn rate_limit(samples: &[Sample], smoothed: &[f64], previous: Option<Sample>) -> Vec<f64> {
    let n = samples.len().min(smoothed.len());
    if n == 0 {
        return Vec::new();
    }
    let mut out = vec![0.0; n];
    let (mut previous_depth, mut previous_at, start) = match previous {
        Some(prior) => (prior.depth_in, prior.at, 0),
        None => {
            // The backfill's seam: the first value is taken as smoothed, and the limiter
            // starts at the second sample.
            out[0] = smoothed[0];
            (smoothed[0], samples[0].at, 1)
        }
    };
    for i in start..n {
        let hours = (samples[i].at - previous_at).num_milliseconds() as f64 / 3_600_000.0;
        if hours <= 0.0 {
            out[i] = previous_depth;
            continue;
        }
        let rate = (smoothed[i] - previous_depth) / hours;
        let capped = rate.clamp(-MAX_DOWN_IN_PER_HOUR, MAX_UP_IN_PER_HOUR);
        out[i] = previous_depth + capped * hours;
        previous_depth = out[i];
        previous_at = samples[i].at;
    }
    out
}

/// Both passes, as the one function the component calls.
pub fn estimate(samples: &[Sample], previous: Option<Sample>) -> Vec<f64> {
    let smoothed = local_quantile(samples);
    rate_limit(samples, &smoothed, previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_763_000_000 + minutes * 60, 0).expect("a valid instant")
    }

    fn series(depths: &[f64]) -> Vec<Sample> {
        depths
            .iter()
            .enumerate()
            .map(|(i, &depth_in)| Sample {
                at: at(i as i64 * 5),
                depth_in,
            })
            .collect()
    }

    #[test]
    fn an_empty_series_smooths_and_limits_to_nothing() {
        assert!(local_quantile(&[]).is_empty());
        assert!(rate_limit(&[], &[], None).is_empty());
        assert!(estimate(&[], None).is_empty());
    }

    /// A flat series is unchanged by both passes, which is the property that makes the
    /// smoother safe to run on a settled pack.
    #[test]
    fn a_flat_series_is_unchanged() {
        let samples = series(&[12.0; 20]);
        let smoothed = local_quantile(&samples);
        assert!(
            smoothed.iter().all(|&v| (v - 12.0).abs() < 1e-9),
            "{smoothed:?}"
        );
        let limited = rate_limit(&samples, &smoothed, None);
        assert!(
            limited.iter().all(|&v| (v - 12.0).abs() < 1e-9),
            "{limited:?}"
        );
    }

    /// **The point of the upper quantile.** A single dropout -- the sensor reading the mast
    /// instead of the snow -- does not pull the estimate down, where a median or a mean
    /// would.
    #[test]
    fn a_single_dropout_does_not_pull_the_estimate_down() {
        let mut depths = [12.0; 13];
        depths[6] = 0.5;
        let samples = series(&depths);
        let smoothed = local_quantile(&samples);
        assert!(
            (smoothed[6] - 12.0).abs() < 1e-9,
            "the dropout is rejected, not averaged in: {}",
            smoothed[6]
        );
        let mean: f64 = depths.iter().sum::<f64>() / depths.len() as f64;
        assert!(mean < 11.2, "a mean would have been pulled down to {mean}");
    }

    /// The band is the mean of up to three order statistics near the 85th percentile.
    ///
    /// Go's `lowIdx > 0` guard is **dead code**, faithfully reproduced: enumerated over every
    /// window size from 1 to 5 000, `high - low < 2` fires only when `idx == 0`, which
    /// requires `last <= 1`, which makes `max(high - 2, 0)` equal to `idx` already. So no
    /// input distinguishes its presence, and the `n = 2` and `n = 1` cases below exercise the
    /// clamp rather than the guard.
    #[test]
    fn the_band_is_three_order_statistics_with_an_asymmetric_clamp() {
        // n = 11, last = 10, idx = floor(0.85 * 10) = 8, high = 10, low = 8: the top three.
        let sorted: Vec<f64> = (0..11).map(f64::from).collect();
        assert_eq!(band_mean(&sorted), 9.0, "mean of 8, 9, 10");
        // n = 3, last = 2, idx = floor(0.85 * 2) = 1, high = 2, low = 1 -- the clamp leaves
        // two points, so `low` drops to 0 and the band is the whole window.
        assert_eq!(band_mean(&[1.0, 2.0, 3.0]), 2.0);
        // n = 2: idx = 0, high = 1, low = 0 -- two points, and `low` is already 0.
        assert_eq!(band_mean(&[1.0, 3.0]), 2.0);
        // n = 1: the one sample.
        assert_eq!(band_mean(&[7.0]), 7.0);
    }

    /// The rate limit's upward cap: a step the sensor claims is 10 inches in five minutes
    /// becomes four inches an hour, which over five minutes is a third of an inch.
    #[test]
    fn an_implausible_jump_is_capped_at_four_inches_an_hour() {
        let samples = vec![
            Sample {
                at: at(0),
                depth_in: 0.0,
            },
            Sample {
                at: at(5),
                depth_in: 10.0,
            },
        ];
        let smoothed = vec![0.0, 10.0];
        let limited = rate_limit(&samples, &smoothed, None);
        assert_eq!(limited[0], 0.0, "the first value is taken as smoothed");
        let expected = MAX_UP_IN_PER_HOUR * (5.0 / 60.0);
        assert!(
            (limited[1] - expected).abs() < 1e-9,
            "{} should be {expected}",
            limited[1]
        );
    }

    /// And the downward cap, which is tighter: snow settles more slowly than it falls.
    #[test]
    fn an_implausible_collapse_is_capped_at_an_inch_and_a_half_an_hour() {
        let samples = vec![
            Sample {
                at: at(0),
                depth_in: 20.0,
            },
            Sample {
                at: at(60),
                depth_in: 0.0,
            },
        ];
        let limited = rate_limit(&samples, &[20.0, 0.0], None);
        assert!(
            (limited[1] - (20.0 - MAX_DOWN_IN_PER_HOUR)).abs() < 1e-9,
            "an hour at 1.5 in/h: {}",
            limited[1]
        );
    }

    /// A change inside the limits passes through untouched, which is what keeps the limiter
    /// from flattening real snowfall.
    #[test]
    fn a_plausible_change_passes_through() {
        let samples = vec![
            Sample {
                at: at(0),
                depth_in: 0.0,
            },
            Sample {
                at: at(60),
                depth_in: 3.0,
            },
        ];
        let limited = rate_limit(&samples, &[0.0, 3.0], None);
        assert!((limited[1] - 3.0).abs() < 1e-9, "{}", limited[1]);
    }

    /// **The seam.** With a prior estimate the limiter starts from it, so the first sample
    /// in an incremental window is limited rather than taken whole — which is the defect
    /// `previous` exists to prevent.
    #[test]
    fn the_carried_estimate_limits_the_first_sample_of_an_incremental_pass() {
        let samples = vec![Sample {
            at: at(5),
            depth_in: 10.0,
        }];
        let prior = Sample {
            at: at(0),
            depth_in: 0.0,
        };
        let limited = rate_limit(&samples, &[10.0], Some(prior));
        let expected = MAX_UP_IN_PER_HOUR * (5.0 / 60.0);
        assert!(
            (limited[0] - expected).abs() < 1e-9,
            "limited against the prior, not taken whole: {}",
            limited[0]
        );
        // Without the prior the same input is taken whole, which is the comparison.
        assert_eq!(rate_limit(&samples, &[10.0], None)[0], 10.0);
    }

    /// Go's `dtHours <= 0` branch: a duplicate instant holds the previous depth rather than
    /// dividing by zero.
    #[test]
    fn a_non_advancing_instant_holds_the_previous_depth() {
        let samples = vec![
            Sample {
                at: at(0),
                depth_in: 1.0,
            },
            Sample {
                at: at(0),
                depth_in: 9.0,
            },
        ];
        let limited = rate_limit(&samples, &[1.0, 9.0], None);
        assert_eq!(limited, vec![1.0, 1.0]);
        assert!(limited.iter().all(|v| v.is_finite()), "no division by zero");
    }

    /// Both passes together, on the shape the component actually sees: a storm that
    /// accumulates over six hours, with one dropout in the middle.
    #[test]
    fn a_storm_with_a_dropout_estimates_monotonically() {
        let mut depths: Vec<f64> = (0..72).map(|i| f64::from(i) * 0.1).collect();
        depths[30] = 0.0;
        let samples = series(&depths);
        let estimated = estimate(&samples, None);
        assert_eq!(estimated.len(), samples.len());
        for pair in estimated.windows(2) {
            assert!(
                pair[1] >= pair[0] - 1e-9,
                "the dropout did not make the estimate fall: {pair:?}"
            );
        }
        let total = estimated[estimated.len() - 1] - estimated[0];
        assert!(
            total > 6.0 && total < 7.2,
            "about seven inches over six hours: {total}"
        );
    }

    /// **The sweep and the scan are the same function.** The linear path is only taken on
    /// ascending input, and this asserts it agrees with the quadratic one that Go uses --
    /// which is the whole licence for the reformulation.
    #[test]
    fn the_linear_sweep_agrees_with_the_quadratic_scan() {
        let depths: Vec<f64> = (0..200)
            .map(|i| f64::from(i) * 0.05 + f64::from(i % 7) * 0.3)
            .collect();
        let ascending = series(&depths);
        let swept = local_quantile(&ascending);
        // The same samples, reversed: not ascending, so the scan runs -- and every sample's
        // window is the same set, so every value must match its counterpart.
        let mut descending = ascending.clone();
        descending.reverse();
        let scanned = local_quantile(&descending);
        let mut scanned_back = scanned;
        scanned_back.reverse();
        assert_eq!(swept.len(), scanned_back.len());
        for (i, (a, b)) in swept.iter().zip(&scanned_back).enumerate() {
            assert!((a - b).abs() < 1e-12, "sample {i}: {a} != {b}");
        }
    }

    /// `estimate` is the two passes in order and nothing else — asserted on a signal with
    /// a spike in it, so that a version which skipped the limiter would differ.
    #[test]
    fn estimate_is_the_quantile_then_the_limit() {
        // Long enough that the ±30-minute windows differ along it -- a six-sample series at
        // five-minute spacing is one window, which the quantile flattens and the limiter
        // then has nothing to do to.
        let depths: Vec<f64> = (0..40).map(|i| if i < 20 { 0.0 } else { 40.0 }).collect();
        let samples = series(&depths);
        let smoothed = local_quantile(&samples);
        let limited = rate_limit(&samples, &smoothed, None);
        assert_eq!(estimate(&samples, None), limited);
        // The two passes are not the same function: a forty-inch step clears the 4 in/h cap
        // by an order of magnitude, so a version that returned the smoothed values would
        // fail here.
        assert_ne!(limited, smoothed, "the rate limit did something");
    }
}
