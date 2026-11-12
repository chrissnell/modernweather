//! Millimetres, inches, the serving threshold, and the snow season's boundary.
//!
//! # The unit split is Q-4, and it lands here
//!
//! `observations.snowdepth` is **millimetres** and `/snow`'s seven numbers are **inches**
//! (W1 Decision 12, Q-4). W3 assumption 23 puts the conversion in the `snow:*` components
//! — "`snow:*` writes `snow_totals_cache` in inches, not the millimetres `snowdepth`
//! carries on `/latest`" — so every number this module's callers store has been through
//! [`serving_inches`], and the frozen handler does no arithmetic at all.
//!
//! # The threshold and the rounding are contract, not presentation
//!
//! `mmToInchesWithThreshold` (`restserver/types.go:199`) does three things to every one of
//! the seven numbers before they reach the wire: divides by 25.4, **returns zero below a
//! tenth of an inch**, and rounds to tenths. All three are observable on a frozen response,
//! so all three happen here rather than at the handler — which is why the handler's own
//! `/snow` code is `zf(Some(totals.today))` and nothing more.
//!
//! The threshold is the one worth stating twice: a gauge reporting a 1 mm change is
//! reporting sensor noise, and 0.039 inches rendered as `0` is the Go daemon's own answer.
//! Moving it to the handler would make the *stored* number disagree with the served one,
//! and `source_through` and `computed_at` are already the only two columns that are not
//! served.

use chrono::{DateTime, Datelike as _, NaiveDate, Utc};
use mw_component::{LocalTime, Zone};

/// Millimetres per inch.
pub const MM_PER_INCH: f64 = 25.4;

/// Below this many inches, a number is served as zero.
///
/// `mmToInchesWithThreshold`'s `const threshold = 0.1`.
pub const SERVING_THRESHOLD_IN: f64 = 0.1;

/// Millimetres to the inches `/snow` serves: the threshold, then tenths.
///
/// `mmToInchesWithThreshold`, as one function. The `f32` return is the column's type and
/// the wire's: `/snow`'s seven numbers are Go `float32` (W1 §6), so narrowing here rather
/// than at the DTO is what keeps the stored number and the served number the same bits.
///
/// A negative input returns zero, which falls out of the threshold rather than needing a
/// branch — and is worth knowing, because a mis-set `snow_base_distance_mm` produces
/// exactly that (`docs/reconciliation.md` DEV-9, part 3).
pub fn serving_inches(mm: f64) -> f32 {
    serving_from_inches(mm / MM_PER_INCH)
}

/// The same threshold and rounding, for a value already in inches.
///
/// The smoothed series is stored in inches (the rate limit is expressed in inches per
/// hour), so its totals reach the threshold without a round trip through millimetres. Go
/// multiplies by 25.4 on the way out of `SmoothedComputer` and divides by 25.4 on the way
/// into `mmToInchesWithThreshold`; the two cancel in `f64` and not quite in Go's `float32`,
/// which is the one place this function is more accurate than its ancestor rather than
/// identical to it.
pub fn serving_from_inches(inches: f64) -> f32 {
    if !inches.is_finite() || inches < SERVING_THRESHOLD_IN {
        return 0.0;
    }
    ((inches * 10.0).round() / 10.0) as f32
}

/// The snow season's first instant, in the station's zone.
///
/// **1 October**, which is `internal/snow/smoothing.go`'s `getSeasonStart` — the function
/// the computer that actually feeds the cache uses. The SQL functions say 1 September
/// (`calculate_total_season_snowfall`, migration 019) and before that 1 July (migration
/// 011), and all three dates exist in the Go tree at once. October is the operative one:
/// `ComputerTypeSmoothed` is the controller's default, so the seasonal number the Go daemon
/// serves is the one measured from 1 October and the SQL function's September has had no
/// caller since migration 022 dropped it.
///
/// **In the station's zone, where Go uses the server's.** `getSeasonStart` builds the date
/// in `now.Location()`, so a season boundary moves with the daemon's own `TZ` — which is
/// **D-24**'s "every calendar quantity is the station's" and the same defect W3 §2.5's
/// consequence 2 records for the rainfall rollovers. The difference is at most one
/// offset's worth of readings on one day a year.
///
/// `None` only for a zone whose 1 October midnight does not exist and has no instant within
/// three hours of it, which no zone in the database has — [`Zone::at`] walks a DST gap
/// forward and the error path exists so a caller is not holding an `expect`.
pub fn season_start(now: DateTime<Utc>, zone: Zone) -> Option<DateTime<Utc>> {
    let today = zone.date_of(now);
    // Before October, the season began last year. `9` and not `10`: `chrono`'s months are
    // 1-based, so `month() < 10` is "before October".
    let year = if today.month() < 10 {
        today.year() - 1
    } else {
        today.year()
    };
    let first = NaiveDate::from_ymd_opt(year, 10, 1)?;
    zone.at(first, LocalTime::MIDNIGHT)
}

/// The station's local midnight, as an instant.
///
/// `get_new_snow_midnight`'s window is `now - date_trunc('day', now AT TIME ZONE
/// 'America/Denver')`, with the owner's own zone **hard-coded in the SQL** — so every other
/// station's "today" is Colorado's today. This is the same quantity computed in the
/// station's own zone, which is **D-24**.
///
/// The shared expression, rather than a second copy: `Zone::at(zone.date_of(now),
/// MIDNIGHT)` is what `crate::rollup::calendar` uses for the rainfall day, and the
/// `mw_component::Zone::at` documentation has why it lives in that crate — two callers in
/// one layer, so neither may own it.
pub fn local_midnight(now: DateTime<Utc>, zone: Zone) -> Option<DateTime<Utc>> {
    zone.at(zone.date_of(now), LocalTime::MIDNIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_and_the_rounding_are_go_s() {
        // Below a tenth of an inch is zero, which is 2.54 mm.
        assert_eq!(serving_inches(0.0), 0.0);
        assert_eq!(serving_inches(1.0), 0.0, "1 mm is sensor noise");
        assert_eq!(serving_inches(2.53), 0.0);
        assert_eq!(serving_inches(2.54), 0.1, "exactly a tenth");
        // Rounded to tenths, half away from zero -- `math.Round`'s rule.
        assert_eq!(serving_inches(25.4), 1.0);
        assert_eq!(serving_inches(100.0), 3.9);
        assert_eq!(serving_inches(254.0), 10.0);
    }

    /// A mis-set base distance makes the depth negative (DEV-9, part 3). It serves as zero,
    /// and that falls out of the threshold rather than out of a branch.
    #[test]
    fn a_negative_depth_serves_as_zero() {
        assert_eq!(serving_inches(-100.0), 0.0);
        assert_eq!(serving_from_inches(-4.0), 0.0);
        assert_eq!(serving_from_inches(f64::NAN), 0.0);
    }

    #[test]
    fn the_two_entry_points_agree_on_the_same_quantity() {
        for mm in [0.0, 2.54, 25.4, 100.0, 1234.5] {
            assert_eq!(
                serving_inches(mm),
                serving_from_inches(mm / MM_PER_INCH),
                "{mm} mm"
            );
        }
    }

    /// 1 October in the station's zone, and the year depends on which side of October the
    /// instant is.
    #[test]
    fn the_season_begins_on_the_first_of_october_in_the_stations_zone() {
        let denver = Zone::new("America/Denver").expect("a real zone");
        let at = |s: &str| {
            s.parse::<DateTime<chrono::FixedOffset>>()
                .expect("an RFC 3339 instant")
                .with_timezone(&Utc)
        };
        // Mid-season: October's start is this year's.
        assert_eq!(
            season_start(at("2026-11-12T10:00:00Z"), denver).expect("a real boundary"),
            at("2026-10-01T06:00:00Z"),
            "midnight MDT is 06:00 UTC"
        );
        // Before October: last year's.
        assert_eq!(
            season_start(at("2026-05-01T10:00:00Z"), denver).expect("a real boundary"),
            at("2025-10-01T06:00:00Z")
        );
        // The boundary itself is in the season it opens.
        assert_eq!(
            season_start(at("2026-10-01T06:00:00Z"), denver).expect("a real boundary"),
            at("2026-10-01T06:00:00Z")
        );
        // And the instant before it belongs to the previous season.
        assert_eq!(
            season_start(at("2026-10-01T05:59:59Z"), denver).expect("a real boundary"),
            at("2025-10-01T06:00:00Z"),
            "it is still 30 September in Denver"
        );
    }

    /// The zone is the station's, which is **D-24** — and the one assertion that shows the
    /// Go behaviour differs: the same instant is a different season start in two zones.
    #[test]
    fn two_stations_in_two_zones_have_two_season_boundaries() {
        let at = |s: &str| {
            s.parse::<DateTime<chrono::FixedOffset>>()
                .expect("an RFC 3339 instant")
                .with_timezone(&Utc)
        };
        let now = at("2026-11-12T10:00:00Z");
        let denver = season_start(now, Zone::new("America/Denver").expect("a real zone"));
        let helsinki = season_start(now, Zone::new("Europe/Helsinki").expect("a real zone"));
        assert_ne!(denver, helsinki);
        assert_eq!(helsinki, Some(at("2026-09-30T21:00:00Z")));
    }

    #[test]
    fn local_midnight_is_the_stations_and_not_the_servers() {
        let denver = Zone::new("America/Denver").expect("a real zone");
        let at = |s: &str| {
            s.parse::<DateTime<chrono::FixedOffset>>()
                .expect("an RFC 3339 instant")
                .with_timezone(&Utc)
        };
        // 03:00 UTC on the 12th is 20:00 on the 11th in Denver, so "today" began at
        // 07:00 UTC on the 11th -- which is the case a UTC `date_trunc` gets wrong by a
        // whole day.
        assert_eq!(
            local_midnight(at("2026-11-12T03:00:00Z"), denver).expect("a real midnight"),
            at("2026-11-11T07:00:00Z")
        );
    }
}
