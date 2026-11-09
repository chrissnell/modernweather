//! The station-local boundaries, and the decomposition that reads a long range cheaply.
//!
//! Two pure functions and nothing else, which is what makes them unit-testable against
//! `chrono-tz` with no database — the test strategy `implementation-plan.md` M3-4 asks for.
//!
//! # The boundaries
//!
//! [`day_start`], [`month_start`] and [`year_start`] answer "the first instant of the local
//! day / month / year that `at` falls in, **in the station's zone**". Today
//! `rainfall_calc.go:30` writes `date_trunc('day', NOW())`, which truncates in the
//! *database server's* zone: every rollover that claims to use the device's declared
//! `timezone` actually uses the server's, so the column is decorative (W3 §2.5,
//! consequence 2; W2a §1.6 measured the same thing).
//!
//! The DST rules are [`Zone::at`]'s, in `mw-component`, so the boundary a rollover
//! accumulates from and the instant the supervisor fires that rollover at are **the same
//! arithmetic**. The cases that bite are the zones whose transition is *at* midnight, and
//! they are not exotic: `America/Havana` has both of them every year — local midnight does
//! not exist on the March date and happens twice on the November one.
//!
//! # The decomposition
//!
//! [`decompose`] tiles a half-open instant range with aggregate buckets, coarsest first.
//! A local year is 525,600 one-minute buckets and about 440 buckets once day and hour
//! buckets carry the middle of it, which is the difference between a query a 60-second tick
//! can afford and one it cannot.
//!
//! **It is exact, not an approximation**, and **D-13** is why: a 1-day bucket's `_total` is
//! the sum of its 24 hourly totals, which are the sums of their 1-minute totals, every
//! addition in double precision (**D-14**). Replacing sixty 1-minute buckets with the hour
//! that contains them changes no value — that is the second payoff of storing composable
//! state that `docs/component-model.md` §4b.3 names.
//!
//! The Go implementation stitches the same levels and gets it *nearly* right.
//! `update_rainfall_summary` computes its 5-minute boundary as
//! `date_trunc('hour', NOW()) + (EXTRACT(MINUTE FROM NOW())::INT / 5 * 5) minutes` and then
//! reads raw rows `>= last_5min`, so the 5-minute bucket **in progress** is counted by
//! neither term: up to five minutes of rain is missing from every 24/48/72-hour figure it
//! writes, always, and nothing says so.

use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use mw_component::{BucketLevel, LocalTime, Piece, Zone};
use smallvec::SmallVec;

/// The levels [`decompose`] tiles with, coarsest first.
///
/// The 5-minute level is deliberately absent. It would save about fifty rows out of four
/// hundred and it would add a case to the one function in this item whose correctness is
/// an exactness claim; `/span` is what W4 added that level for.
const TILING: [BucketLevel; 3] = [BucketLevel::Day, BucketLevel::Hour, BucketLevel::Minute];

/// How long after a bucket closes before its level's refresh policy has certainly
/// materialised it.
///
/// **Every level is `materialized_only = true`** (the W4 migration says why: TimescaleDB
/// cannot build an aggregate on a real-time aggregate), so a bucket that has not been
/// refreshed yet does not return a partial value — it returns **no row**. Reading a day
/// bucket that closed ten minutes ago would therefore silently drop a day of rain from a
/// monthly total, and it would do it for up to half an hour after every UTC midnight.
///
/// So a level is only used for buckets that closed at least this long ago, and the finer
/// levels tile the rest. The figures are each level's `schedule_interval` from
/// `migrations/20261029234500_w4_station_dimension_caches_indexes_and_policies.sql` plus
/// half again:
///
/// | Level | `schedule_interval` | here |
/// |---|---|---|
/// | `1d` | 30 minutes | 45 minutes |
/// | `1h` | 5 minutes | 7.5 minutes |
/// | `5m` | 1 minute | 90 seconds |
/// | `1m` | 1 minute, phased 3 s past the boundary | **none** |
///
/// The 1-minute level has none because it is the floor: there is nothing finer to fall back
/// to, and `window:rollup` fires [`crate::rollup::PHASE`] past the boundary, which is after
/// W4 D-26's measured 3.06–3.22 s refresh lag. A 1-minute bucket that is somehow still
/// missing costs one minute of staleness and is picked up by the next tick, because the
/// refresh is incremental.
fn settled_for(level: BucketLevel) -> chrono::Duration {
    chrono::Duration::seconds(match level {
        BucketLevel::Day => 45 * 60,
        BucketLevel::Hour => 450,
        BucketLevel::FiveMinute => 90,
        BucketLevel::Minute => 0,
    })
}

/// The first instant of the local day `at` falls in, in `zone`.
pub fn day_start(zone: Zone, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    zone.at(zone.date_of(at), LocalTime::MIDNIGHT)
}

/// The first instant of the local month `at` falls in, in `zone`.
pub fn month_start(zone: Zone, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let local = zone.date_of(at);
    let first = NaiveDate::from_ymd_opt(local.year(), local.month(), 1)?;
    zone.at(first, LocalTime::MIDNIGHT)
}

/// The first instant of the local year `at` falls in, in `zone`.
pub fn year_start(zone: Zone, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let local = zone.date_of(at);
    let first = NaiveDate::from_ymd_opt(local.year(), 1, 1)?;
    zone.at(first, LocalTime::MIDNIGHT)
}

/// Tile `[from, until)` with aggregate buckets, coarsest in the middle.
///
/// `now` is what decides how coarse the middle may be: see [`settled_for`]. The result is
/// in ascending time order and every piece is bucket-aligned at its own level, which is
/// what makes the sum of the pieces equal the sum over `[from, until)` exactly.
///
/// Both ends are floored to the minute first, because a 1-minute bucket is the finest state
/// there is: a range beginning at `12:00:30` is not expressible, and silently rounding it is
/// better than pretending otherwise — every caller in this item passes minute-aligned
/// instants (a local midnight in every IANA zone is on a whole minute, and the rolling
/// windows are measured from a floored `now`).
pub fn decompose(
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> SmallVec<[Piece; 6]> {
    let from = floor_to(from, BucketLevel::Minute.span());
    let until = floor_to(until, BucketLevel::Minute.span());
    let mut out = SmallVec::new();
    tile(&TILING, from, until, now, &mut out);
    out
}

/// One level of [`decompose`]'s recursion.
fn tile(
    levels: &[BucketLevel],
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    now: DateTime<Utc>,
    out: &mut SmallVec<[Piece; 6]>,
) {
    if until <= from {
        return;
    }
    let Some((coarse, finer)) = levels.split_first() else {
        return;
    };
    if finer.is_empty() {
        // The floor. Whatever is left is this level's, aligned or not: there is nothing
        // finer to defer to.
        out.push(Piece {
            level: *coarse,
            from,
            until,
        });
        return;
    }
    let span = coarse.span();
    // The newest instant this level's buckets are certainly materialised up to.
    let settled = floor_to(
        now.checked_sub_signed(settled_for(*coarse)).unwrap_or(now),
        span,
    );
    let lo = ceil_to(from, span);
    let hi = floor_to(until, span).min(settled);
    if hi <= lo {
        // No whole settled bucket of this level fits, so the whole range belongs to the
        // finer ones.
        tile(finer, from, until, now, out);
        return;
    }
    tile(finer, from, lo, now, out);
    out.push(Piece {
        level: *coarse,
        from: lo,
        until: hi,
    });
    tile(finer, hi, until, now, out);
}

/// The greatest multiple of `span` at or before `at`, on the grid `time_bucket` uses.
///
/// `time_bucket(INTERVAL 'n', time)` with no origin and no zone is a fixed grid anchored at
/// the Unix epoch, so this is integer division on the epoch second — **not** a calendar
/// truncation. That is why a 1-day bucket is a UTC day and a station's local day has to be
/// stitched from hour buckets at its edges.
pub fn floor_to(at: DateTime<Utc>, span: Duration) -> DateTime<Utc> {
    let secs = span.as_secs().max(1) as i64;
    let t = at.timestamp();
    // `rem_euclid`, not `%`: a negative epoch second is before 1970, which no reading has
    // and which would floor the wrong way if it did.
    let floored = t - t.rem_euclid(secs);
    DateTime::from_timestamp(floored, 0).unwrap_or(at)
}

/// The least multiple of `span` at or after `at`.
pub fn ceil_to(at: DateTime<Utc>, span: Duration) -> DateTime<Utc> {
    let floored = floor_to(at, span);
    if floored == at {
        at
    } else {
        floored
            .checked_add_signed(chrono::Duration::seconds(span.as_secs().max(1) as i64))
            .unwrap_or(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn zone(name: &str) -> Zone {
        Zone::new(name).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// The ordinary case, to fix what "the station's zone" means before the awkward ones.
    /// 2026-11-06 is MST (UTC-7) in Denver, so the local day began at 07:00Z — which is
    /// seven hours away from what `date_trunc('day', NOW())` returns on a server in UTC.
    #[test]
    fn a_local_day_begins_in_the_stations_zone_and_not_the_servers() {
        let denver = zone("America/Denver");
        let noon_local = at("2026-11-06T19:00:00Z");
        assert_eq!(
            day_start(denver, noon_local),
            Some(at("2026-11-06T07:00:00Z"))
        );
        assert_eq!(
            month_start(denver, noon_local),
            Some(at("2026-11-01T06:00:00Z")),
            "1 November is still MDT (UTC-6) in Denver; the month and the day do not share \
             an offset"
        );
        assert_eq!(
            year_start(denver, noon_local),
            Some(at("2026-01-01T07:00:00Z"))
        );
    }

    /// **W2a Decision 13, first half, at the rollover slot.** `America/Havana` jumps from
    /// 00:00 to 01:00 on the March date, so the local day's *first instant* is 01:00 local.
    /// The day is 23 hours long, and a boundary computed as "the previous boundary plus
    /// 86,400 seconds" would be an hour wrong from then until the next transition.
    #[test]
    fn a_local_day_whose_midnight_does_not_exist_begins_after_the_gap() {
        let havana = zone("America/Havana");
        // 2026-03-08 in Havana: CST (UTC-5) until 00:00, then CDT (UTC-4).
        let start = day_start(havana, at("2026-03-08T12:00:00Z")).expect("a day begins");
        assert_eq!(start, at("2026-03-08T05:00:00Z"));
        assert_eq!(
            start
                .with_timezone(&havana.tz())
                .format("%H:%M")
                .to_string(),
            "01:00",
            "the first instant that exists after the gap"
        );
        let next = day_start(havana, at("2026-03-09T12:00:00Z")).expect("a day begins");
        assert_eq!(next, at("2026-03-09T04:00:00Z"));
        assert_eq!(
            (next - start).num_hours(),
            23,
            "the local day is 23 hours long, which is why a Window::Day is not a Duration"
        );
    }

    /// **W2a Decision 13, second half, at the rollover slot.** On the November date Havana
    /// goes back from 01:00 to 00:00, so local midnight happens twice — 04:00Z and 05:00Z.
    /// The day begins at the **first** of them, and every instant in that 25-hour day
    /// resolves to the same boundary, including the ones after the second pass.
    #[test]
    fn a_local_day_whose_midnight_happens_twice_begins_at_the_first_one() {
        let havana = zone("America/Havana");
        let first = at("2026-11-01T04:00:00Z");
        for inside in [
            "2026-11-01T04:00:00Z", // 00:00 CDT, the first pass
            "2026-11-01T04:30:00Z", // 00:30 CDT
            "2026-11-01T05:00:00Z", // 00:00 CST, the second pass
            "2026-11-01T05:30:00Z", // 00:30 CST
            "2026-11-01T20:00:00Z", // the middle of the afternoon
        ] {
            assert_eq!(
                day_start(havana, at(inside)),
                Some(first),
                "{inside} is in the local day that began at {first}"
            );
        }
        let next = day_start(havana, at("2026-11-02T12:00:00Z")).expect("a day begins");
        assert_eq!(
            (next - first).num_hours(),
            25,
            "the local day is 25 hours long"
        );
    }

    /// Both transitions again, as the property that matters rather than two instants:
    /// **every local date in the year has exactly one boundary, and the boundaries are
    /// strictly increasing**. A zone whose transition is at midnight is where a naive
    /// implementation produces either a duplicate or a gap, and `America/Havana` has one of
    /// each every year.
    #[test]
    fn every_local_date_has_exactly_one_strictly_increasing_boundary() {
        for name in [
            "America/Havana",   // midnight does not exist in March, happens twice in November
            "America/Santiago", // midnight does not exist in September
            "America/Denver",   // the transitions are at 02:00, so midnight is ordinary
            "Asia/Kathmandu",   // +05:45: a local midnight that is not on the hour
            "America/Phoenix",  // no DST at all, and still a local midnight
        ] {
            let z = zone(name);
            let mut previous: Option<DateTime<Utc>> = None;
            let mut seen = std::collections::BTreeSet::new();
            for day in 0..366 {
                let noon = at("2026-01-01T12:00:00Z") + chrono::Duration::days(day);
                let local_date = z.date_of(noon);
                let start = day_start(z, noon).unwrap_or_else(|| panic!("{name} {local_date}"));
                assert!(
                    seen.insert((local_date, start)),
                    "{name}: {local_date} resolved twice"
                );
                if let Some(p) = previous {
                    assert!(
                        start > p,
                        "{name}: {local_date} begins at {start}, not after {p}"
                    );
                    let hours = (start - p).num_hours();
                    assert!(
                        (23..=25).contains(&hours),
                        "{name}: {local_date} is {hours} hours after the previous boundary"
                    );
                }
                previous = Some(start);
            }
            // One boundary per local date the loop walked over, and the loop steps a day
            // at a time -- so a zone that produced a duplicate or skipped a date fails the
            // insert above rather than this.
            assert!(seen.len() >= 365, "{name}: {} boundaries", seen.len());
        }
    }

    /// A 45-minute zone, because it is the case that makes "align to the hour grid" wrong.
    /// `Asia/Kathmandu` is `+05:45`, so a local midnight is `18:15Z` — on the minute grid
    /// and on no coarser one.
    #[test]
    fn a_three_quarter_hour_offset_puts_a_local_midnight_off_the_hour_grid() {
        let kathmandu = zone("Asia/Kathmandu");
        let start = day_start(kathmandu, at("2026-11-06T12:00:00Z")).expect("a day begins");
        assert_eq!(start, at("2026-11-05T18:15:00Z"));
        assert_ne!(
            start,
            floor_to(start, BucketLevel::Hour.span()),
            "not on the hour grid, which is what the decomposition's minute edges are for"
        );
        assert_eq!(start, floor_to(start, BucketLevel::Minute.span()));
    }

    /// The grid is the epoch one `time_bucket` uses, so flooring a day is **not** a
    /// calendar truncation — which is exactly why a local month is stitched rather than
    /// read from day buckets alone.
    #[test]
    fn the_bucket_grid_is_anchored_at_the_epoch() {
        assert_eq!(
            floor_to(at("2026-11-06T15:42:31Z"), BucketLevel::Minute.span()),
            at("2026-11-06T15:42:00Z")
        );
        assert_eq!(
            floor_to(at("2026-11-06T15:42:31Z"), BucketLevel::Hour.span()),
            at("2026-11-06T15:00:00Z")
        );
        assert_eq!(
            floor_to(at("2026-11-06T15:42:31Z"), BucketLevel::Day.span()),
            at("2026-11-06T00:00:00Z")
        );
        assert_eq!(
            ceil_to(at("2026-11-06T15:42:31Z"), BucketLevel::Hour.span()),
            at("2026-11-06T16:00:00Z")
        );
        assert_eq!(
            ceil_to(at("2026-11-06T16:00:00Z"), BucketLevel::Hour.span()),
            at("2026-11-06T16:00:00Z"),
            "already on the grid, so unchanged"
        );
    }

    /// **The decomposition's exactness claim, as a property.** For a range of every awkward
    /// shape, the pieces are contiguous, in order, cover exactly `[from, until)`, and every
    /// piece is aligned at its own level — so the sum of the pieces is the sum over the
    /// range with no seam counted twice and none dropped.
    #[test]
    fn the_pieces_tile_the_range_exactly_and_each_is_aligned_at_its_own_level() {
        let now = at("2026-11-06T15:05:00Z");
        let cases = [
            // A local year, which is the range the 1-day level exists for.
            (at("2026-01-01T07:00:00Z"), at("2026-11-06T15:00:00Z")),
            // A local month in a 45-minute zone -- off every grid but the minute one.
            (at("2026-11-04T18:15:00Z"), at("2026-11-06T15:00:00Z")),
            // Seventy-two hours.
            (at("2026-11-03T15:00:00Z"), at("2026-11-06T15:00:00Z")),
            // A local day.
            (at("2026-11-06T07:00:00Z"), at("2026-11-06T15:00:00Z")),
            // Ten minutes: finer than any coarse bucket, so one minute-level piece.
            (at("2026-11-06T14:50:00Z"), at("2026-11-06T15:00:00Z")),
            // One minute.
            (at("2026-11-06T14:59:00Z"), at("2026-11-06T15:00:00Z")),
            // A range that straddles a day boundary without containing a whole day.
            (at("2026-11-05T23:30:00Z"), at("2026-11-06T00:30:00Z")),
        ];
        for (from, until) in cases {
            let pieces = decompose(from, until, now);
            assert!(!pieces.is_empty(), "{from} -> {until} produced nothing");
            assert_eq!(pieces[0].from, from, "{from} -> {until}");
            assert_eq!(
                pieces.last().expect("non-empty").until,
                until,
                "{from} -> {until}"
            );
            for pair in pieces.windows(2) {
                assert_eq!(
                    pair[0].until, pair[1].from,
                    "{from} -> {until}: a seam between {:?} and {:?}",
                    pair[0], pair[1]
                );
            }
            for piece in &pieces {
                assert!(piece.until > piece.from, "{piece:?}");
                let span = piece.level.span();
                assert_eq!(
                    floor_to(piece.from, span),
                    piece.from,
                    "{piece:?} starts off its own grid"
                );
                assert_eq!(
                    floor_to(piece.until, span),
                    piece.until,
                    "{piece:?} ends off its own grid"
                );
            }
        }
    }

    /// A year costs about four hundred rows instead of half a million, which is the whole
    /// reason the decomposition exists — and it is asserted rather than claimed, because it
    /// is the number that decides whether a 60-second tick can afford the query.
    #[test]
    fn a_local_year_is_a_few_hundred_buckets_rather_than_half_a_million() {
        let now = at("2026-12-31T23:05:00Z");
        let pieces = decompose(at("2026-01-01T07:00:00Z"), at("2026-12-31T23:00:00Z"), now);
        let rows: u64 = pieces
            .iter()
            .map(|p| {
                let secs = (p.until - p.from).num_seconds().max(0) as u64;
                secs / p.level.span().as_secs()
            })
            .sum();
        assert!(
            (300..=600).contains(&rows),
            "a local year decomposes to {rows} buckets across {} pieces",
            pieces.len()
        );
        // All of a year at the minute level would be this, which is the figure the
        // decomposition is measured against.
        assert_eq!(364 * 24 * 60 + 16 * 60, 525_120);
    }

    /// **The materialisation lag, which is the half of this that is not arithmetic.** Every
    /// level is `materialized_only`, so an unrefreshed bucket returns no row rather than a
    /// partial one. A range whose end is inside the 1-day level's refresh interval must
    /// therefore not be read from day buckets, or a monthly total loses a whole day for
    /// half an hour after every UTC midnight.
    #[test]
    fn a_level_is_not_read_for_buckets_its_refresh_policy_may_not_have_reached() {
        // 00:10Z: the UTC day that closed ten minutes ago is inside the 1-day level's
        // 30-minute schedule interval.
        let now = at("2026-11-06T00:10:00Z");
        let pieces = decompose(at("2026-10-01T06:00:00Z"), at("2026-11-06T00:10:00Z"), now);
        let newest_day_piece = pieces
            .iter()
            .filter(|p| p.level == BucketLevel::Day)
            .map(|p| p.until)
            .max()
            .expect("a month contains whole days");
        assert!(
            newest_day_piece <= at("2026-11-05T00:00:00Z"),
            "a day bucket that closed {} ago was read: {newest_day_piece}",
            now - newest_day_piece
        );
        // And the same range half an hour later does include it, so the margin is a
        // margin and not a permanent exclusion.
        let later = at("2026-11-06T01:00:00Z");
        let pieces = decompose(
            at("2026-10-01T06:00:00Z"),
            at("2026-11-06T01:00:00Z"),
            later,
        );
        let newest = pieces
            .iter()
            .filter(|p| p.level == BucketLevel::Day)
            .map(|p| p.until)
            .max()
            .expect("whole days");
        assert_eq!(newest, at("2026-11-06T00:00:00Z"));
    }

    /// An empty or inverted range is no pieces rather than a panic. The caller that can
    /// produce one is the storm window during the first minute of a storm.
    #[test]
    fn an_empty_range_is_no_query_at_all() {
        let now = at("2026-11-06T15:05:00Z");
        assert!(decompose(at("2026-11-06T15:00:00Z"), at("2026-11-06T15:00:00Z"), now).is_empty());
        assert!(decompose(at("2026-11-06T15:00:00Z"), at("2026-11-06T14:00:00Z"), now).is_empty());
        // Sub-minute ranges floor to nothing, which is the documented rounding: a
        // 1-minute bucket is the finest state there is.
        assert!(decompose(at("2026-11-06T15:00:10Z"), at("2026-11-06T15:00:50Z"), now).is_empty());
    }
}
