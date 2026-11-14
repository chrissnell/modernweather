//! The one tolerance in this harness, and the argument for its constant.
//!
//! Everything else is compared exactly. These two functions exist because two statistics
//! cannot agree exactly for an arithmetic reason, and the bound on them is derived rather
//! than chosen: it is the same model and the same constant `w9.rollup_report` reconciled the
//! data migration with, so (c)'s tolerance and M5-1's tolerance are one decision.

/// 2^-24: half an ulp of `float4`'s 24-bit mantissa.
///
/// The Go aggregate views both accumulate and store `period_rain` and `windd` in `real`, and
/// the Go `circular_avg_state` declares even its count as `real`. Multiplying a half-ulp by
/// the number of additions is the standard worst-case bound for a running sum in that width.
pub const FLOAT4_HALF_ULP: f64 = 5.960_464_477_539_063e-8;

/// The absolute tolerance for a statistic the Go side accumulated in `float4` over `n`
/// readings.
///
/// `max(|a|, |b|, 1)` rather than `|a|`: the relative bound degenerates at zero, and a
/// rain accumulator is zero for most of the year. The floor of 1 makes the bound absolute
/// there, which is strictly tighter than any relative bound would be.
pub fn float4_accumulated_tolerance(n: u64, a: f64, b: f64) -> f64 {
    let scale = a.abs().max(b.abs()).max(1.0);
    (n.max(1) as f64) * FLOAT4_HALF_ULP * scale
}

/// Whether `a` and `b` agree within the `float4` accumulation bound over `n` readings.
pub fn within_float4_bound(n: u64, a: f64, b: f64) -> bool {
    (a - b).abs() <= float4_accumulated_tolerance(n, a, b)
}

/// Whether `total` is `mean` multiplied by the bucket's reading count, within the bound.
///
/// DEV-7: the Go view stored `avg(solarjoules)` and the registry stores `total`, so the
/// relationship is exact in real arithmetic and bounded in `float4`. The bound applies twice
/// -- once to the mean the Go side accumulated, once to the total the new side accumulated --
/// which is the factor of two, and it is applied at the scale of the total because that is
/// the quantity being compared.
pub fn is_mean_times_count(n: u64, mean: f64, total: f64) -> bool {
    let expected = mean * (n.max(1) as f64);
    (total - expected).abs() <= 2.0 * float4_accumulated_tolerance(n, expected, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_the_migrations_bound() {
        // docs/data-migration.md §2a reconciled `rain_incremental` at 5.77e-7 against a
        // 1.03e-3 bound. 1.03e-3 / 5.96e-8 is ~17,300 readings in a daily bucket, which is
        // production's cadence -- so the same n and the same constant reproduce it.
        let n = 17_300;
        let tol = float4_accumulated_tolerance(n, 0.0, 0.0);
        assert!((1.0e-3..1.1e-3).contains(&tol), "{tol}");
    }

    #[test]
    fn a_difference_outside_the_bound_fails() {
        // 2,160 readings per daily bucket, DEV-4's measured case: the bound is ~1.3e-4
        // relative, so a 1% error is four orders of magnitude outside it.
        assert!(within_float4_bound(2_160, 1.0, 1.0 + 1.0e-7));
        assert!(!within_float4_bound(2_160, 1.0, 1.01));
    }

    #[test]
    fn the_bound_does_not_grow_with_a_bigger_value_alone() {
        // The scale floor is 1, so a tiny rain total gets an absolute bound rather than a
        // relative one that collapses to zero.
        assert!(within_float4_bound(100, 0.0, 5.0e-6));
        assert!(!within_float4_bound(100, 0.0, 5.0e-4));
    }

    #[test]
    fn mean_to_total_is_a_relationship_and_not_an_exemption() {
        // 30 readings in a 1m bucket: the total is the mean times 30, and nothing else
        // passes. A side that left the mean in place is caught, which is the point.
        assert!(is_mean_times_count(30, 0.0005859, 0.0005859 * 30.0));
        assert!(!is_mean_times_count(30, 0.0005859, 0.0005859));
        assert!(!is_mean_times_count(30, 0.0005859, 0.0005859 * 29.0));
    }
}
