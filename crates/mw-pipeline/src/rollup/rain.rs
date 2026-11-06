//! Where the current rain event began — the one window whose bound is a **gap**.
//!
//! Every other maintained window has a boundary that is a function of the clock or the
//! calendar. The storm's is a function of the data: it starts at the first rain after a dry
//! spell, which is what a `sum()` cannot express and what [`mw_component::ReadingStore`]'s
//! `series` exists for.
//!
//! # The rule, and whose it is
//!
//! A rain event **ends after [`STORM_DRY_GAP`] with no rain**, which is Davis hardware's own
//! rule for resetting its storm total. So the storm is "every hour since the last
//! [`STORM_DRY_GAP`]-long dry spell, provided it rained inside that spell's far side", and
//! there is no storm at all when the last [`STORM_DRY_GAP`] has been dry.
//!
//! `storm_start` therefore means two different things in this system and W3 §2.5's
//! consequence 1 is explicit that it should: the **column** is what the hardware reported,
//! and this is the **computed** window. They share a name today and nothing distinguishes
//! them.
//!
//! What the Go implementation computes instead is the last 24 hours, unconditionally:
//!
//! ```text
//! -- internal/storage/timescaledb/sql.go:2343, calculate_storm_rainfall
//! -- Simple approach: storm is any rainfall in the last 24 hours
//! storm_start_ts := now() - interval '24 hours';
//! ```
//!
//! So a storm that has been raining for three days reports the last 24 hours of it, and a
//! storm that stopped 23 hours ago is still in progress. The comment is honest about being
//! a placeholder; `handlers.go:339` serves the result on `/latest` as `stormrain`.
//!
//! # Hourly buckets, and the horizon
//!
//! The series is read at the **1-hour** level: 720 rows for the thirty-day horizon, against
//! 43,200 at the minute level, and an hour is finer than the 24-hour gap by a factor of 24.
//! A storm's start is therefore accurate to the hour, which is finer than the hardware's own
//! `storm_start` (Davis reports a date).
//!
//! [`STORM_HORIZON`] bounds the walk. A rain event with no 24-hour dry spell for thirty days
//! is beyond what any station's own storm total expresses, and the honest answer at the
//! horizon is to say the storm began there rather than to keep reading — so
//! [`Storm::clamped`] records that it happened and the caller logs it once.

use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::Bucketed;

/// How long it must not rain for a rain event to be over.
///
/// Twenty-four hours, which is Davis hardware's own rule for resetting its storm total — so
/// the computed window and the reported column mean the same kind of thing even though they
/// are different facts.
pub const STORM_DRY_GAP: Duration = Duration::from_secs(24 * 3600);

/// How far back a storm's start is looked for.
pub const STORM_HORIZON: Duration = Duration::from_secs(30 * 24 * 3600);

/// How many hourly buckets [`STORM_HORIZON`] is, which is the series read's `limit`.
pub const STORM_BUCKETS: usize = 30 * 24;

/// The current rain event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Storm {
    /// The start of the first hourly bucket with rain in the event.
    pub start: DateTime<Utc>,
    /// Whether the walk reached [`STORM_HORIZON`] still raining, so `start` is the horizon
    /// rather than a dry spell's far side.
    pub clamped: bool,
}

/// The current rain event's start, or `None` if it has not rained in [`STORM_DRY_GAP`].
///
/// `series` is the hourly rain totals for one station, **oldest first** — which is the order
/// `ReadingStore::series` returns. A bucket is wet when its total is present and positive;
/// a `None` total is a bucket no reading carried rain in, which is not evidence of rain and
/// is therefore dry. That reading of `None` is a choice and it is the conservative one: an
/// offline gauge ends a storm rather than extending it indefinitely.
pub fn current_storm(series: &[Bucketed], now: DateTime<Utc>) -> Option<Storm> {
    let gap = chrono::Duration::seconds(STORM_DRY_GAP.as_secs() as i64);
    let wet: Vec<DateTime<Utc>> = series
        .iter()
        .filter(|b| b.total.is_some_and(|t| t > 0.0))
        .map(|b| b.bucket)
        .collect();
    let newest = *wet.last()?;
    // The event is over if the last rain is older than the gap. `newest` is a bucket's
    // *start*, so the hour it covers ends an hour later -- counted from the end, because a
    // storm that stopped raining 24 hours and ten minutes ago has been over for ten
    // minutes and not for an hour and ten.
    let last_rain_ended = newest + chrono::Duration::hours(1);
    if now.signed_duration_since(last_rain_ended) >= gap {
        return None;
    }
    // Walk back while consecutive wet hours are closer together than the gap.
    let mut start = newest;
    for pair in wet.windows(2).rev() {
        let (earlier, later) = (pair[0], pair[1]);
        if later.signed_duration_since(earlier + chrono::Duration::hours(1)) >= gap {
            break;
        }
        start = earlier;
    }
    // Clamped if the walk ran out of series rather than finding a dry spell, and the series
    // really does reach the horizon.
    let oldest = series.first().map(|b| b.bucket);
    let clamped = oldest == Some(start)
        && now.signed_duration_since(start)
            >= chrono::Duration::seconds(STORM_HORIZON.as_secs() as i64)
                - chrono::Duration::hours(1);
    Some(Storm { start, clamped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    /// Build an hourly series ending at `until`, `hours` long, with rain in the hours whose
    /// index into the series is in `wet`.
    fn series(until: &str, hours: i64, wet: &[i64]) -> Vec<Bucketed> {
        let end = at(until);
        (0..hours)
            .map(|i| Bucketed {
                bucket: end - chrono::Duration::hours(hours - i),
                total: if wet.contains(&i) { Some(0.05) } else { None },
            })
            .collect()
    }

    /// The ordinary case: it rained for three hours this afternoon and it is still
    /// raining, so the storm began at the first of them.
    #[test]
    fn a_storm_begins_at_the_first_wet_hour_of_the_event() {
        let now = at("2026-11-06T15:30:00Z");
        // 48 hours of series; wet at indices 44, 45, 46 -- 11:00, 12:00 and 13:00 today.
        let s = series("2026-11-06T15:00:00Z", 48, &[44, 45, 46]);
        let storm = current_storm(&s, now).expect("it rained within the gap");
        assert_eq!(storm.start, at("2026-11-06T11:00:00Z"));
        assert!(!storm.clamped);
    }

    /// A dry hour inside an event does **not** end it. The gap is 24 hours, so a shower
    /// this morning and a shower this afternoon are one storm — which is the behaviour the
    /// hardware has and the Go function cannot express, because it has no concept of an
    /// event at all.
    #[test]
    fn a_dry_hour_inside_the_gap_does_not_end_the_storm() {
        let now = at("2026-11-06T15:30:00Z");
        // Wet at 02:00 and at 13:00, eleven hours apart.
        let s = series("2026-11-06T15:00:00Z", 48, &[35, 46]);
        let storm = current_storm(&s, now).expect("within the gap");
        assert_eq!(storm.start, at("2026-11-06T02:00:00Z"));
    }

    /// A dry spell as long as the gap **does** end it, so the next rain is a new storm.
    #[test]
    fn a_dry_spell_of_the_full_gap_starts_a_new_storm() {
        let now = at("2026-11-06T15:30:00Z");
        // 96 hours. Wet at index 20 (2026-11-03T11:00Z) and at 94 (2026-11-06T13:00Z):
        // the dry spell between them is 2026-11-03T12:00Z -> 2026-11-06T13:00Z, which is
        // 73 hours.
        let s = series("2026-11-06T15:00:00Z", 96, &[20, 94]);
        let storm = current_storm(&s, now).expect("within the gap");
        assert_eq!(
            storm.start,
            at("2026-11-06T13:00:00Z"),
            "the earlier rain is a previous storm"
        );
    }

    /// Twenty-four hours with no rain is no storm — not a storm of zero inches, and not
    /// "the last 24 hours" the way `calculate_storm_rainfall` reports it.
    #[test]
    fn no_rain_inside_the_gap_is_no_storm_at_all() {
        let now = at("2026-11-06T15:30:00Z");
        // Wet 30 hours before `now`, and dry since.
        let s = series("2026-11-06T15:00:00Z", 48, &[17]);
        assert_eq!(current_storm(&s, now), None);
        // And an empty or all-dry series.
        assert_eq!(
            current_storm(&series("2026-11-06T15:00:00Z", 48, &[]), now),
            None
        );
        assert_eq!(current_storm(&[], now), None);
    }

    /// The boundary of the gap, from both sides, because "ends after 24 hours with no rain"
    /// is measured from when the rain **stopped** and not from the bucket it stopped in.
    #[test]
    fn the_gap_is_measured_from_the_end_of_the_last_wet_hour() {
        // Rain in the hour 2026-11-05T14:00Z..15:00Z. It stopped at 15:00Z.
        let s = series("2026-11-06T15:00:00Z", 48, &[23]);
        assert_eq!(s[23].bucket, at("2026-11-05T14:00:00Z"));
        // 24 hours after it stopped, to the second: over.
        assert_eq!(current_storm(&s, at("2026-11-06T15:00:00Z")), None);
        // One second before: still a storm.
        assert_eq!(
            current_storm(&s, at("2026-11-06T14:59:59Z")).map(|s| s.start),
            Some(at("2026-11-05T14:00:00Z"))
        );
    }

    /// A storm that has been raining for the whole horizon reports the horizon and says so,
    /// rather than reading further or reporting a start it has not seen.
    #[test]
    fn a_storm_that_fills_the_horizon_is_clamped_and_records_it() {
        let now = at("2026-11-06T15:30:00Z");
        let wet: Vec<i64> = (0..STORM_BUCKETS as i64).collect();
        let s = series("2026-11-06T15:00:00Z", STORM_BUCKETS as i64, &wet);
        let storm = current_storm(&s, now).expect("still raining");
        assert_eq!(storm.start, s[0].bucket);
        assert!(storm.clamped, "the walk reached the horizon still raining");

        // A storm that merely began early in a long series is not clamped.
        let s = series("2026-11-06T15:00:00Z", STORM_BUCKETS as i64, &wet[1..]);
        let storm = current_storm(&s, now).expect("still raining");
        assert_eq!(storm.start, s[1].bucket);
        assert!(!storm.clamped);
    }

    /// A zero total is dry. The 1-minute level's `sum()` over a bucket whose readings all
    /// carried `0.0` rain is `0.0`, not `NULL`, and a gauge that reports zero every minute
    /// is not raining.
    #[test]
    fn a_zero_total_is_dry_and_so_is_an_absent_one() {
        let now = at("2026-11-06T15:30:00Z");
        let mut s = series("2026-11-06T15:00:00Z", 48, &[]);
        for b in &mut s {
            b.total = Some(0.0);
        }
        assert_eq!(current_storm(&s, now), None);
        s[46].total = Some(0.01);
        assert_eq!(
            current_storm(&s, now).map(|st| st.start),
            Some(at("2026-11-06T13:00:00Z"))
        );
    }
}
