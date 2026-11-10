//! **N-10 / C-W2a-27: `/latest`'s `sunrise` and `sunset` against a fresh Meeus evaluation.**
//!
//! W2a Decision 36 deletes the Go `sun_times` table in favour of computing the two values
//! per request, and OQ-B makes that safe by requiring the served strings to be compared
//! against a fresh evaluation. `docs/reconciliation.md` C-W2a-27 records that the check was
//! never written: W1 D-3 makes the conformance runner value-blind by design, so the
//! fixtures carry the two strings and nothing looks at them. This file is that comparison,
//! and it is the one place where a fixture's *value* is the thing under test.
//!
//! It is a `cargo test --workspace` test rather than a gate because it is pure arithmetic
//! over `(date, latitude, longitude)`: no database, no socket, no Go toolchain, nothing to
//! skip and therefore nothing that can report green without running.
//!
//! # What it found, and it is not what C-W2a-27 predicted
//!
//! C-W2a-27 expected the two to "disagree by a minute somewhere", because the stored table
//! is keyed by day of year and a day-of-year key is slightly wrong in non-leap years. The
//! measurement below says the day-of-year keying is the **smallest** of three divergences
//! and the only one that is about a minute:
//!
//! | Cause | Size | Measured by |
//! |---|---|---|
//! | the omitted horizon altitude | **±4.5 to ±5.1 min** | [`the_horizon_altitude_term_is_the_dominant_cause`] |
//! | the declination approximation | **up to 0.76°, so -3 to +4 min** | [`every_configured_station_diverges_from_a_fresh_meeus_evaluation`] |
//! | the day-of-year key | **1.5 min across 2026-2045** | [`the_day_of_year_key_is_worth_about_a_minute`] |
//!
//! The shipped arithmetic -- `mw_api_legacy::sun::sunrise_sunset`, which is
//! `remoteweather`'s `pkg/solar.CalculateSunriseSunset` at `c3ec9f7` -- puts the sun's
//! **centre on the geometric horizon**: `cos H = -tan(phi) tan(delta)`, with no term for
//! atmospheric refraction (34') or the solar semidiameter (16'). A textbook evaluation uses
//! `h0 = -0.8333°` for those two, which moves sunrise about five minutes earlier and sunset
//! about five minutes later at these latitudes. It also takes the declination from
//! `pkg/solar/asce.go`'s single-term day-of-year expression rather than from the solar
//! coordinates, which is worth up to 0.76° near the equinoxes.
//!
//! So over 102 270 comparisons -- every configured station, both limbs, every day of
//! 2026-2045 -- the served value **never once** lands on the same minute as a fresh Meeus
//! evaluation. The spread is **-8 to +7 minutes** and the mean absolute divergence is
//! **4.52 minutes**. Drop the horizon-altitude term from the oracle -- comparing like with
//! like, both on the geometric horizon -- and the spread falls to **-3 to +4 minutes** with a
//! mean of **1.19**, which is the declination approximation on its own.
//!
//! # What that means for M5-4, which is M5-4's to decide
//!
//! **It does not reinstate the table.** C-W2a-27's risk model has the stored table and the
//! on-demand evaluation as two different answers, one of which might be wrong. They are the
//! same answer: `internal/controllers/restserver/controller.go:118` fills `sun_times` by
//! handing `solar.CalculateSunriseSunset` to `PopulateSunTimes` for all 366 keys, and
//! `handlers.go:367` reads it back at `time.Now().YearDay()` -- the function and the key
//! this crate's `sun::sunrise_sunset` ports. [`the_served_strings_are_the_stored_go_tables`]
//! confirms that empirically: the served strings reproduce all six captured fixture values
//! exactly, across four stations. Deleting the table changes no value, which is the
//! precondition Decision 36 actually needs and it holds.
//!
//! What the comparison does establish is that the **shared** arithmetic is minutes away from
//! the sky, in both implementations, and that reinstating a table filled by the same
//! function would not close that gap. Whether to fix the formula is a change to a frozen
//! value on a deployed surface and therefore not this item's to take.
//!
//! # The oracle, and why it is not a second copy of the thing under test
//!
//! [`meeus`] is written from *Astronomical Algorithms* (2nd ed.) ch. 25 (solar coordinates),
//! ch. 28 (the equation of time) and ch. 15 (rising and setting) and shares no line with
//! `sun.rs`. It takes a **calendar date** rather than a day of year, which is the whole
//! point -- a day-of-year-keyed oracle could not see the day-of-year keying -- and it
//! iterates the solar position to the event instant rather than evaluating it once at noon.
//!
//! An oracle asserted against the values it produces proves nothing, so
//! [`the_oracle_reproduces_published_astronomical_quantities`] checks it against quantities
//! published independently of this repository: the four extremes of the equation of time,
//! the length of the equinox day at the equator, and Denver's longest day. The structural
//! invariants a wrong oracle would break -- symmetry about the transit, a sunrise that walks
//! the right way through the year -- are asserted alongside them.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use mw_api_legacy::{sun, Renderer};
use mw_component::{Resolved, SnapshotKey, SnapshotSource, StationMeta, WebsiteView, Zone};
use mw_types::{Reading, StationKey, WebsiteId};

/// The standard altitude of the sun's centre at the moment of sunrise or sunset: 34' of
/// atmospheric refraction plus the 16' semidiameter of the disc (Meeus ch. 15, table 15.1).
const STANDARD_ALTITUDE: f64 = -0.8333;

/// The geometric horizon, which is what `sun::sunrise_sunset` uses.
///
/// Comparing at this altitude is the like-with-like comparison: it removes the one term the
/// shipped formula omits and leaves the declination approximation on its own.
const GEOMETRIC_HORIZON: f64 = 0.0;

/// The years the sweep covers.
///
/// Twenty, because the divergence that C-W2a-27 predicted is a *day-of-year keying* effect
/// and that is only visible across the leap cycle: a single year's sweep cannot distinguish
/// "the key is wrong in non-leap years" from "the formula is wrong every year".
const YEARS: std::ops::RangeInclusive<i32> = 2026..=2045;

/// The divergence between `/latest`'s strings and a fresh Meeus evaluation, as **data**.
///
/// M5-2's rule, applied one item early: an expected difference encoded as data rather than
/// as prose makes an unexpected one a failure rather than a judgement call. Both ends of
/// each range are asserted, so the envelope cannot be widened to make a regression pass --
/// a loosened bound stops being reached and [`envelope_is_reached`] fails.
struct Envelope {
    /// The horizon altitude the oracle is evaluated at.
    altitude: f64,
    /// The measured signed range, in minutes, of served minus oracle.
    range: (i32, i32),
    /// The smallest `|served - oracle|` the sweep may observe.
    ///
    /// `1` on the textbook comparison is the headline: in 102 270 comparisons the served value
    /// never lands on the same minute as the sky.
    min_divergence: i32,
    /// The largest `|served - oracle|` the sweep must still reach, so that the range above
    /// cannot be widened without this failing.
    reaches: i32,
    /// The measured mean of `|served - oracle|`, in minutes.
    ///
    /// Asserted to within a tenth of a minute, which is what turns "they disagree" into a
    /// measurement. It is also the number that distinguishes the two causes: **4.5 against
    /// the textbook horizon and 1.2 against the geometric one** says the omitted
    /// horizon-altitude term is most of the divergence and the declination approximation is
    /// the rest. `ci/layers.toml`'s `no-stdout-logging` rule means a test cannot print what
    /// it measured, so the measurement is here, where a reader and a regression both find
    /// it.
    mean_abs: f64,
}

/// Against a textbook evaluation: never within a minute, and up to eight minutes out.
///
/// `range` is the measured `[-8, +7]` widened by one minute at each end, and the widening
/// has a named mechanism rather than being slack: `sun::sunrise_sunset` evaluates the
/// equation of time at noon on `Utc::now().year()`, so the integer minute it rounds to can
/// move by one when this test is run in a different year from the one it was measured in
/// (checked over 2026, 2027, 2030, 2045 and 2100: one minute, once).
const TEXTBOOK: Envelope = Envelope {
    altitude: STANDARD_ALTITUDE,
    range: (-9, 8),
    min_divergence: 1,
    reaches: 6,
    mean_abs: 4.52,
};

/// At the same horizon altitude: the declination approximation on its own.
const SAME_HORIZON: Envelope = Envelope {
    altitude: GEOMETRIC_HORIZON,
    range: (-4, 5),
    min_divergence: 0,
    reaches: 2,
    mean_abs: 1.19,
};

/// A fresh Meeus evaluation of sunrise and sunset. No line of this is `sun.rs`.
mod meeus {
    /// Sunrise and sunset as minutes from midnight **UT** on a calendar date, at one
    /// position and one horizon altitude.
    ///
    /// Unwrapped: a value outside `[0, 1440)` means the event falls on an adjacent UT date,
    /// which at 105 W it does for every sunset. The caller wraps, because wrapping here
    /// would throw away the information that distinguishes "just before midnight" from
    /// "just after".
    ///
    /// `None` for polar day or polar night, where there is no event to report.
    pub fn sunrise_sunset(
        date: (i32, u32, u32),
        latitude: f64,
        longitude: f64,
        altitude: f64,
    ) -> Option<(f64, f64)> {
        let midnight = julian_day(date);
        // Each limb is solved separately: the declination at sunrise is not the declination
        // at sunset, and evaluating it once at noon is one of the approximations under test.
        let rise = solve(midnight, latitude, longitude, altitude, -1.0)?;
        let set = solve(midnight, latitude, longitude, altitude, 1.0)?;
        Some((rise, set))
    }

    /// One limb, by fixed-point iteration from the local solar noon.
    ///
    /// Four passes, which is two more than the event instant needs: the position moves by
    /// under a minute of time between the second and the third, and the margin is cheap.
    fn solve(
        midnight: f64,
        latitude: f64,
        longitude: f64,
        altitude: f64,
        sign: f64,
    ) -> Option<f64> {
        // Local solar noon, as the instant the first pass evaluates the sun at.
        let mut at = midnight + 0.5 - longitude / 360.0;
        let mut minutes = 0.0;
        for _ in 0..4 {
            let (declination, equation_of_time) = sun(at);
            // Each degree of longitude is four minutes of time, east earlier; the equation
            // of time is how far ahead of mean time the real sun is.
            let transit = 720.0 - longitude * 4.0 - equation_of_time;
            let phi = latitude.to_radians();
            let delta = declination.to_radians();
            // Meeus eq. 15.1: the hour angle at which the sun's centre reaches `altitude`.
            let cos_h =
                (altitude.to_radians().sin() - phi.sin() * delta.sin()) / (phi.cos() * delta.cos());
            if !(-1.0..=1.0).contains(&cos_h) {
                // Polar day or polar night: the altitude is never crossed.
                return None;
            }
            minutes = transit + sign * cos_h.acos().to_degrees() * 4.0;
            at = midnight + minutes / 1440.0;
        }
        Some(minutes)
    }

    /// The sun's apparent declination in degrees and the equation of time in minutes, for
    /// one Julian Day.
    ///
    /// Meeus ch. 25's low-accuracy solar position (0.01° over this millennium, which is two
    /// orders of magnitude finer than the divergence being measured) and eq. 28.3.
    fn sun(jd: f64) -> (f64, f64) {
        let t = (jd - 2_451_545.0) / 36_525.0;
        // Geometric mean longitude and mean anomaly (eq. 25.2, 25.3).
        let l0 = 280.466_46 + 36_000.769_83 * t + 0.000_303_2 * t * t;
        let m = 357.529_11 + 35_999.050_29 * t - 0.000_153_7 * t * t;
        let e = 0.016_708_634 - t * (0.000_042_037 + 0.000_000_126_7 * t);
        let m_rad = m.rem_euclid(360.0).to_radians();
        // The equation of the centre (p. 164), which turns the mean sun into the true one.
        let centre = (1.914_602 - 0.004_817 * t - 0.000_014 * t * t) * m_rad.sin()
            + (0.019_993 - 0.000_101 * t) * (2.0 * m_rad).sin()
            + 0.000_289 * (3.0 * m_rad).sin();
        // Apparent longitude: nutation and aberration, from the ascending node of the
        // moon's mean orbit (p. 164).
        let omega = 125.04 - 1_934.136 * t;
        let lambda = l0 + centre - 0.005_69 - 0.004_78 * omega.to_radians().sin();
        // True obliquity (eq. 22.2 plus the nutation term of p. 165).
        let epsilon_0 = 23.0
            + (26.0 + (21.448 - t * (46.815_0 + t * (0.000_59 - t * 0.001_813))) / 60.0) / 60.0;
        let epsilon = epsilon_0 + 0.002_56 * omega.to_radians().cos();
        let declination = (epsilon.to_radians().sin() * lambda.to_radians().sin())
            .asin()
            .to_degrees();
        // Eq. 28.3, in degrees of arc, then four minutes of time per degree.
        let y = (epsilon.to_radians() / 2.0).tan().powi(2);
        let l0_rad = l0.rem_euclid(360.0).to_radians();
        let equation_of_time = (y * (2.0 * l0_rad).sin() - 2.0 * e * m_rad.sin()
            + 4.0 * e * y * m_rad.sin() * (2.0 * l0_rad).cos()
            - 0.5 * y * y * (4.0 * l0_rad).sin()
            - 1.25 * e * e * (2.0 * m_rad).sin())
        .to_degrees()
            * 4.0;
        (declination, equation_of_time)
    }

    /// The Julian Day at 00:00 UT on a Gregorian calendar date (Meeus eq. 7.1).
    ///
    /// By the calendar rather than by a unix timestamp, so that the oracle's date arithmetic
    /// is its own and not `chrono`'s -- `sun.rs` builds its reference instant through
    /// `chrono`, and a shared bug there would be invisible to this comparison.
    pub fn julian_day((year, month, day): (i32, u32, u32)) -> f64 {
        let (y, m) = if month <= 2 {
            (year - 1, month + 12)
        } else {
            (year, month)
        };
        let a = (f64::from(y) / 100.0).floor();
        let b = 2.0 - a + (a / 4.0).floor();
        (365.25 * (f64::from(y) + 4716.0)).floor()
            + (30.6001 * (f64::from(m) + 1.0)).floor()
            + f64::from(day)
            + b
            - 1524.5
    }

    /// The equation of time in minutes at noon UT on a date, for the published-extremes
    /// check.
    pub fn equation_of_time(date: (i32, u32, u32)) -> f64 {
        sun(julian_day(date) + 0.5).1
    }
}

// ---------------------------------------------------------------------------------------
// The configured stations, read from the seed rather than copied out of it.

/// One station as `ci/conformance/rust-seed/01-config.sql` configures it.
#[derive(Debug, Clone)]
struct Station {
    key: String,
    latitude: f64,
    longitude: f64,
    zone: String,
}

/// How many stations the seed configures.
///
/// Declared, for the reason `cargo xtask db-tests` declares a test set by name: a sweep over
/// a station list that silently became empty passes, and a station added to the rig without
/// being added to this comparison is exactly the gap this file exists to close.
const CONFIGURED_STATIONS: usize = 7;

/// The seed's `INSERT INTO stations`, parsed.
///
/// Parsed rather than transcribed: a transcription is a second copy of the rig's
/// configuration that drifts silently, and the drift would show up as this comparison
/// quietly not covering a station.
fn configured_stations() -> Vec<Station> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ci/conformance/rust-seed/01-config.sql");
    let sql = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let sql = strip_line_comments(&sql);

    // The `VALUES` list of the `stations` insert, up to its statement terminator. The
    // comment stripping above is what makes a `;` or a `(` inside the seed's prose
    // harmless.
    let insert = sql
        .find("INSERT INTO stations")
        .expect("the seed configures stations");
    let values = sql[insert..]
        .find("VALUES")
        .map(|v| insert + v + "VALUES".len())
        .expect("the stations insert has a VALUES list");
    let end = values + sql[values..].find(';').expect("a terminated statement");

    let rows = tuples(&sql[values..end]);
    let stations: Vec<Station> = rows
        .iter()
        .map(|row| {
            let fields = fields(row);
            assert_eq!(
                fields.len(),
                12,
                "the stations insert names 12 columns; this row has {}: {row}",
                fields.len()
            );
            Station {
                key: unquote(fields[1]),
                latitude: fields[6].parse().expect("a latitude"),
                longitude: fields[7].parse().expect("a longitude"),
                zone: unquote(fields[9]),
            }
        })
        .collect();
    assert_eq!(
        stations.len(),
        CONFIGURED_STATIONS,
        "the rig configures {} stations, not {CONFIGURED_STATIONS} -- a station added to \
         ci/conformance/rust-seed/01-config.sql has to be added to this comparison too",
        stations.len()
    );
    stations
}

/// `--` to end of line, outside a quoted literal.
fn strip_line_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut quoted = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if quoted {
            quoted = c != '\'';
            out.push(c);
        } else if c == '\'' {
            quoted = true;
            out.push(c);
        } else if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// The top-level parenthesised groups of a `VALUES` list.
fn tuples(values: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut current = String::new();
    for c in values.chars() {
        if quoted {
            quoted = c != '\'';
        } else if c == '\'' {
            quoted = true;
        } else if c == '(' {
            depth += 1;
            if depth == 1 {
                current.clear();
                continue;
            }
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                rows.push(std::mem::take(&mut current));
                continue;
            }
        }
        if depth >= 1 {
            current.push(c);
        }
    }
    rows
}

/// One tuple's comma-separated fields, respecting quotes.
fn fields(row: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in row.char_indices() {
        if quoted {
            quoted = c != '\'';
        } else if c == '\'' {
            quoted = true;
        } else if c == ',' {
            out.push(row[start..i].trim());
            start = i + 1;
        }
    }
    out.push(row[start..].trim());
    out
}

fn unquote(field: &str) -> String {
    field.trim().trim_matches('\'').to_string()
}

// ---------------------------------------------------------------------------------------
// `/latest`, driven through the real renderer.

/// `/latest`'s `sunrise` and `sunset` for one station at one instant, as the renderer
/// produces them.
///
/// Through [`Renderer::reading`] rather than through `sun::sunrise_sunset` directly, because
/// the glue is part of what is under test: which day of year the ordinal comes from, which
/// zone formats the strings, and that the keys are populated at all.
fn served(station: &Station, at: DateTime<Utc>) -> (String, String) {
    let key = StationKey::new(station.key.as_str());
    let mut zones = BTreeMap::new();
    if let Ok(zone) = Zone::new(&station.zone) {
        zones.insert(key.clone(), zone);
    }
    let mut positions = BTreeMap::new();
    positions.insert(key.clone(), (station.latitude, station.longitude));
    let renderer = Renderer::new(zones, positions);

    let source = SnapshotSource {
        key: SnapshotKey::new(WebsiteId::new(1), key.clone()),
        website: WebsiteView {
            id: WebsiteId::new(1),
            name: "Suncrest Live Weather".into(),
            // The station is its own website's primary, which is what makes the zone below
            // the one its strings are formatted in. Every station in the rig is in
            // `America/Denver`, so this is not a choice between zones -- it is how the
            // renderer is told which zone to use at all.
            primary_station: Some(key.clone()),
            snow_enabled: false,
            snow_station: None,
        },
        reading: Reading {
            time: at,
            station_name: Some(station.key.clone()),
            station_type: Some("davis".into()),
            out_temp: Some(40.0),
            ..Reading::default()
        },
        meta: StationMeta {
            latitude: Some(station.latitude),
            longitude: Some(station.longitude),
            altitude_m: Some(1800.0),
            snow_base_distance_mm: None,
        },
        // No window declarations: `/span`'s rolling values are zero-filled and none of them
        // is a sun time.
        windows: Resolved::new(at, []),
    };
    let dto = renderer.reading(&source);
    (dto.sunrise.clone(), dto.sunset)
}

/// The oracle's answer for the same station and local date, rendered through the **contract's
/// own formatter**, so that the two sides of the comparison differ only in the arithmetic.
fn oracle(
    station: &Station,
    local: NaiveDate,
    at: DateTime<Utc>,
    altitude: f64,
) -> (String, String) {
    let zone: chrono_tz::Tz = station.zone.parse().expect("an IANA name");
    let (rise, set) = meeus::sunrise_sunset(
        (local.year(), local.month(), local.day()),
        station.latitude,
        station.longitude,
        altitude,
    )
    .expect("none of the rig's stations is polar");
    let wrap = |m: f64| m.round().rem_euclid(1440.0) as i32;
    (
        sun::format_sun_time(wrap(rise), at, &zone),
        sun::format_sun_time(wrap(set), at, &zone),
    )
}

/// `"7:30 AM"` back to minutes from local midnight, so that two strings can be subtracted.
///
/// The comparison is over the strings `/latest` serves -- that is what N-10 asks for -- and
/// a signed minute difference is what "by how much" means, so this is the inverse of the
/// contract's formatter rather than a second path to the numbers.
fn parse_clock(s: &str) -> i32 {
    let (clock, meridiem) = s
        .split_once(' ')
        .unwrap_or_else(|| panic!("`{s}` is not `3:04 PM`"));
    let (hour, minute) = clock
        .split_once(':')
        .unwrap_or_else(|| panic!("`{s}` is not `3:04 PM`"));
    let hour: i32 = hour.parse().expect("an hour");
    let minute: i32 = minute.parse().expect("a minute");
    let hour24 = match (hour % 12, meridiem) {
        (h, "AM") => h,
        (h, "PM") => h + 12,
        (_, other) => panic!("`{other}` is not a meridiem"),
    };
    hour24 * 60 + minute
}

/// Served minus oracle, in minutes, taking the shorter way round the clock.
fn divergence(served: &str, oracle: &str) -> i32 {
    (parse_clock(served) - parse_clock(oracle) + 720).rem_euclid(1440) - 720
}

/// Local noon on `date` in the station's zone, which is the instant the sweep renders at.
///
/// Noon rather than midnight on purpose: `format_sun_time` maps a UT minute onto the local
/// date of the instant it is given, and the Go handler passes `time.Now()`. Midday is the
/// only part of the day where the local date is unambiguous in every zone.
fn local_noon(station: &Station, date: NaiveDate) -> DateTime<Utc> {
    let zone: chrono_tz::Tz = station.zone.parse().expect("an IANA name");
    zone.from_local_datetime(&date.and_hms_opt(12, 0, 0).expect("midday exists"))
        .single()
        .expect("midday is never a DST gap")
        .with_timezone(&Utc)
}

/// Every day of [`YEARS`].
fn sweep_dates() -> impl Iterator<Item = NaiveDate> {
    YEARS
        .flat_map(|year| (1..=366).filter_map(move |ordinal| NaiveDate::from_yo_opt(year, ordinal)))
}

/// What one sweep measured.
#[derive(Debug, Default)]
struct Measured {
    lo: i32,
    hi: i32,
    min_abs: i32,
    max_abs: i32,
    total_abs: i64,
    samples: u32,
    worst: String,
}

impl Measured {
    fn fold(&mut self, d: i32, what: &str) {
        if self.samples == 0 {
            self.lo = d;
            self.hi = d;
            self.min_abs = d.abs();
        }
        self.lo = self.lo.min(d);
        self.hi = self.hi.max(d);
        self.min_abs = self.min_abs.min(d.abs());
        if d.abs() > self.max_abs {
            self.max_abs = d.abs();
            self.worst = format!("{what} ({d:+} min)");
        }
        self.total_abs += i64::from(d.abs());
        self.samples += 1;
    }

    fn mean_abs(&self) -> f64 {
        self.total_abs as f64 / f64::from(self.samples)
    }
}

/// The sweep: every configured station, every day of [`YEARS`], at one horizon altitude.
fn sweep(altitude: f64) -> Measured {
    let mut measured = Measured::default();
    for station in configured_stations() {
        for date in sweep_dates() {
            let at = local_noon(&station, date);
            let (rise, set) = served(&station, at);
            let (rise_oracle, set_oracle) = oracle(&station, date, at, altitude);
            assert!(
                !rise.is_empty() && !set.is_empty(),
                "{}: `/latest` served no sun times on {date}, which it does only for a \
                 station with no coordinates or a polar day -- neither applies here",
                station.key
            );
            measured.fold(
                divergence(&rise, &rise_oracle),
                &format!("{} sunrise {date}: {rise} vs {rise_oracle}", station.key),
            );
            measured.fold(
                divergence(&set, &set_oracle),
                &format!("{} sunset {date}: {set} vs {set_oracle}", station.key),
            );
        }
    }
    measured
}

// ---------------------------------------------------------------------------------------
// The assertions.

/// **The item: `/latest`'s strings against a fresh Meeus evaluation, every configured
/// station.**
///
/// The result is a disagreement, so the disagreement is what is asserted -- as a declared
/// envelope with both ends live, which is the shape that cannot be loosened to make a
/// regression pass. The two envelopes together are the finding: removing the horizon
/// altitude from the oracle takes `[-8, +7]` down to `[-3, +4]`, so the omitted term is most
/// of the gap and the declination approximation is the rest.
#[test]
fn every_configured_station_diverges_from_a_fresh_meeus_evaluation() {
    for envelope in [&TEXTBOOK, &SAME_HORIZON] {
        let m = sweep(envelope.altitude);
        assert!(
            m.lo >= envelope.range.0 && m.hi <= envelope.range.1,
            "at h0 = {:+.4}° the divergence is [{:+}, {:+}], outside the declared \
             [{:+}, {:+}]. Worst: {}. This is a change in the arithmetic on one side or the \
             other, not a bound to widen.",
            envelope.altitude,
            m.lo,
            m.hi,
            envelope.range.0,
            envelope.range.1,
            m.worst
        );
        assert!(
            m.min_abs >= envelope.min_divergence,
            "at h0 = {:+.4}° some comparison agreed to within {} minute(s), where the \
             measurement says the closest approach is {}. An improvement in the served \
             arithmetic is the likely cause and is good news; this assertion is here so \
             that it is reported rather than absorbed.",
            envelope.altitude,
            m.min_abs,
            envelope.min_divergence
        );
        assert!(
            (m.mean_abs() - envelope.mean_abs).abs() < 0.1,
            "at h0 = {:+.4}° the mean |divergence| over {} comparisons is {:.2} minutes, \
             where the measurement says {:.2}. The two declared means are the decomposition \
             of this item's finding, so a change in either is a result to report rather than \
             a constant to update.",
            envelope.altitude,
            m.samples,
            m.mean_abs(),
            envelope.mean_abs
        );
        assert!(
            m.max_abs >= envelope.reaches,
            "at h0 = {:+.4}° the divergence never exceeded {} minutes, so the declared \
             envelope [{:+}, {:+}] is wider than the thing it describes. An envelope that \
             is not reached is a tolerance.",
            envelope.altitude,
            m.max_abs,
            envelope.range.0,
            envelope.range.1
        );
    }
}

/// **The envelopes are reached from both directions**, which is what stops either of them
/// being quietly widened: a bound only has to be wrong by two minutes before the sweep stops
/// touching it, and this is the assertion that notices.
#[test]
fn envelope_is_reached() {
    let m = sweep(TEXTBOOK.altitude);
    assert!(
        m.lo <= -TEXTBOOK.reaches && m.hi >= TEXTBOOK.reaches,
        "the textbook comparison measured [{:+}, {:+}]; both ends should reach \
         {} minutes, because the horizon-altitude term it is missing has the opposite sign \
         at sunrise and at sunset",
        m.lo,
        m.hi,
        TEXTBOOK.reaches
    );
}

/// **The one assertion that does pass to the minute, and the one Decision 36 rests on:**
/// the served strings are the stored Go table's values.
///
/// Every `/latest` fixture that carries `sunrise` and `sunset` is replayed through the
/// renderer at its own `captured_at` instant and its own station, and compared to the
/// captured string **exactly**. Six fixtures, four stations.
///
/// This is the one place a fixture's value is under test (W1 D-3 keeps the conformance runner
/// value-blind, and N-10's note forbids making the fixtures value-sensitive to get here), and
/// the comparison is well founded despite `ci/conformance/README.md`'s warning that the
/// captured strings are "not reproducible by this rig": the Go rig left `devices.timezone`
/// unset, so `handlers.go:371` fell back to `time.Local`, which was **UTC-6** for the
/// 2026-10-28 capture. `America/Denver` -- the zone the Rust rig configures -- is UTC-6 on
/// that date, Mountain Daylight Time running until 1 November. So the two render at the same
/// offset and the strings are comparable.
#[test]
fn the_served_strings_are_the_stored_go_tables() {
    let stations = configured_stations();
    let mut compared = 0;
    for fixture in latest_fixtures() {
        let station = stations
            .iter()
            .find(|s| s.key == fixture.station)
            .unwrap_or_else(|| panic!("{}: no station `{}`", fixture.name, fixture.station));
        let (rise, set) = served(station, fixture.captured_at);
        assert_eq!(
            (rise.as_str(), set.as_str()),
            (fixture.sunrise.as_str(), fixture.sunset.as_str()),
            "{}: the on-demand evaluation does not reproduce the `sun_times` row the Go \
             daemon served for {} on {}. The stored table is filled by \
             `solar.CalculateSunriseSunset` (controller.go:118) and read at \
             `time.Now().YearDay()` (handlers.go:367), which is the function and the key \
             `sun::sunrise_sunset` ports -- so a difference here is a defect in the port, \
             and it is the precondition W2a Decision 36 deletes the table on.",
            fixture.name,
            fixture.station,
            fixture.captured_at
        );
        compared += 1;
    }
    assert_eq!(
        compared, 6,
        "six `/latest` fixtures carry `sunrise` and `sunset`; this compared {compared}. A \
         fixture that stopped carrying them is a change to the frozen surface, and a \
         comparison that silently found none is this check not running."
    );
}

/// **A station whose coordinates are `0, 0` is served the Gulf of Guinea's sun times**, in
/// both implementations, and that completes "every configured station".
///
/// `nolocation` is seeded at `0.000, 0.000` rather than NULL so that the rig exercises the
/// `latitude = 0 AND longitude = 0` predicate `/api/stations` applies
/// (`ci/conformance/rust-seed/01-config.sql`). The read path does **not** apply it:
/// `StationMeta::coordinates` is `latitude.zip(longitude)`, so `(0, 0)` is a position. Go
/// does the same -- `PopulateSunTimes` is called for every device including this one -- so
/// this is a reproduction and not a divergence, and it only stays unobservable because the
/// station has no readings and `/latest` therefore 404s.
///
/// Asserted rather than skipped, because "the station the sweep could not compare" is
/// exactly where a gap hides.
#[test]
fn the_station_with_no_location_is_served_the_equators_sun_times() {
    let stations = configured_stations();
    let nowhere = stations
        .iter()
        .find(|s| s.latitude == 0.0 && s.longitude == 0.0)
        .expect("the rig seeds one station at 0, 0");
    let date = NaiveDate::from_ymd_opt(2026, 10, 28).expect("a real date");
    let at = local_noon(nowhere, date);
    let (rise, set) = served(nowhere, at);
    assert!(
        !rise.is_empty() && !set.is_empty(),
        "`{}` is at 0, 0, which is a position: the read path has no \
         `latitude = 0 AND longitude = 0` guard and both keys are served",
        nowhere.key
    );

    // At the equator the hour angle is 90° whatever the declination, because
    // `cos H = -tan(0) tan(delta)` is zero -- so the declination approximation cannot move
    // the answer and the only divergence left is the horizon-altitude term. That is the
    // decomposition, isolated by geography rather than argued.
    let same = oracle(nowhere, date, at, GEOMETRIC_HORIZON);
    assert_eq!(
        divergence(&rise, &same.0)
            .abs()
            .max(divergence(&set, &same.1).abs()),
        0,
        "at the equator the geometric comparison should be exact: {rise}/{set} vs \
         {}/{}",
        same.0,
        same.1
    );
    let textbook = oracle(nowhere, date, at, STANDARD_ALTITUDE);
    let shift = divergence(&rise, &textbook.0);
    assert!(
        (3..=5).contains(&shift),
        "and the textbook comparison should be the horizon-altitude term alone, about four \
         minutes at the equator; got {shift:+}"
    );
}

/// **The horizon-altitude term is most of the divergence**, measured rather than asserted
/// from the formula.
///
/// Evaluating the oracle at both altitudes for the same station and date isolates the term
/// `sun::sunrise_sunset` omits: it moves sunrise earlier and sunset later by the same amount,
/// which is why the textbook envelope is signed and the same-horizon one is not.
#[test]
fn the_horizon_altitude_term_is_the_dominant_cause() {
    let station = Station {
        key: "davis-full".into(),
        latitude: 39.900,
        longitude: -105.500,
        zone: "America/Denver".into(),
    };
    for (date, expected) in [
        ((2026, 6, 21), 5.1),
        ((2026, 12, 21), 5.1),
        ((2026, 10, 28), 4.5),
    ] {
        let geometric =
            meeus::sunrise_sunset(date, station.latitude, station.longitude, GEOMETRIC_HORIZON)
                .expect("not polar");
        let textbook =
            meeus::sunrise_sunset(date, station.latitude, station.longitude, STANDARD_ALTITUDE)
                .expect("not polar");
        let earlier = geometric.0 - textbook.0;
        let later = textbook.1 - geometric.1;
        assert!(
            (earlier - expected).abs() < 0.1 && (later - expected).abs() < 0.1,
            "{date:?}: the horizon altitude should move each limb {expected} minutes; \
             sunrise moved {earlier:.2} and sunset {later:.2}"
        );
    }
}

/// **The day-of-year key is worth about a minute**, which is what C-W2a-27 predicted and is
/// the smallest of the three divergences.
///
/// A day-of-year key cannot represent a date: `sun_times` row 263 is read on 20 September in
/// a non-leap year and on 19 September in a leap one, and the sun is not in the same place.
/// This measures the size of that -- the spread of the *true* time for a single key across
/// [`YEARS`] -- so that the finding is a number rather than an expectation.
#[test]
fn the_day_of_year_key_is_worth_about_a_minute() {
    let (lat, lon) = (39.900, -105.500);
    let mut worst = (0.0_f64, 0_u32);
    for ordinal in 1..=365_u32 {
        let mut rises = Vec::new();
        let mut sets = Vec::new();
        for year in YEARS {
            let date = NaiveDate::from_yo_opt(year, ordinal).expect("1-365 exists every year");
            let (rise, set) = meeus::sunrise_sunset(
                (date.year(), date.month(), date.day()),
                lat,
                lon,
                STANDARD_ALTITUDE,
            )
            .expect("not polar");
            rises.push(rise);
            sets.push(set);
        }
        // Unwrapped minutes, so a sunset either side of midnight UT is not a 1439-minute
        // spread.
        let spread = |v: &[f64]| {
            v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
        };
        let widest = spread(&rises).max(spread(&sets));
        if widest > worst.0 {
            worst = (widest, ordinal);
        }
    }
    assert!(
        (1.0..2.0).contains(&worst.0),
        "one day-of-year key should span between one and two minutes of true time across \
         {YEARS:?}; the widest was key {} at {:.2} minutes. C-W2a-27 is the claim being \
         measured here, so a different answer is a finding and not a bound to adjust.",
        worst.1,
        worst.0
    );
    assert!(
        worst.0 < TEXTBOOK.reaches as f64,
        "the day-of-year keying ({:.2} min) should be smaller than the divergence the \
         comparison measures ({} min) -- that ordering is the whole of this item's result",
        worst.0,
        TEXTBOOK.reaches
    );
}

/// **The oracle, against quantities published independently of this repository.**
///
/// An oracle checked against its own output is a tautology, so none of these numbers comes
/// from this code:
///
/// - the four extremes of the equation of time (about -14.2 min in mid-February, +3.8 in
///   mid-May, -6.4 in late July, +16.4 in early November), which are the standard published
///   figures for the analemma and which pin the transit;
/// - the length of the equinox day at the equator, which is twelve hours plus twice the
///   refraction allowance and therefore a few minutes over twelve, not twelve exactly;
/// - Denver's longest day, published as 14 h 59 m.
///
/// Plus the two structural invariants a wrong oracle would break: the two limbs are
/// equidistant from the transit, and sunrise at a northern station walks later every day from
/// the June solstice to the December one.
#[test]
fn the_oracle_reproduces_published_astronomical_quantities() {
    // The analemma's four extremes, to a tenth of a minute, each within a day of its
    // published date.
    for (date, expected) in [
        ((2026, 2, 11), -14.2),
        ((2026, 5, 14), 3.8),
        ((2026, 7, 26), -6.5),
        ((2026, 11, 3), 16.4),
    ] {
        let e = meeus::equation_of_time(date);
        assert!(
            (e - expected).abs() < 0.2,
            "{date:?}: the equation of time is published as about {expected} minutes; the \
             oracle says {e:.2}"
        );
    }

    // The equinox at the equator: twelve hours plus the refraction allowance on each limb.
    let (rise, set) = meeus::sunrise_sunset((2026, 3, 20), 0.0, 0.0, STANDARD_ALTITUDE)
        .expect("the equator is not polar");
    let length = set - rise;
    assert!(
        (724.0..=730.0).contains(&length),
        "the equinox day at the equator is twelve hours plus about seven minutes of \
         refraction, not {length:.1} minutes"
    );

    // Denver's longest day, published as 14 h 59 m.
    let (rise, set) = meeus::sunrise_sunset((2026, 6, 21), 39.7392, -104.9903, STANDARD_ALTITUDE)
        .expect("not polar");
    let length = set - rise;
    assert!(
        (898.0..=900.0).contains(&length),
        "Denver's longest day is published as 14 h 59 m (899 minutes); the oracle says \
         {length:.1}"
    );

    // Symmetry: both limbs are the same distance from the transit, which the oracle never
    // computes directly. Checked at a high latitude too, where the iteration works hardest.
    for (lat, lon) in [(39.9, -105.5), (60.0, 25.0), (-33.9, 151.2)] {
        for date in [(2026, 1, 5), (2026, 4, 1), (2026, 9, 23), (2026, 12, 2)] {
            let (rise, set) =
                meeus::sunrise_sunset(date, lat, lon, STANDARD_ALTITUDE).expect("not polar");
            let transit = (rise + set) / 2.0;
            assert!(
                ((set - transit) - (transit - rise)).abs() < 0.01,
                "{date:?} at {lat}, {lon}: the limbs are not equidistant from the transit"
            );
        }
    }

    // And the walk through the year, which a sign error in the declination would reverse.
    let mut previous = f64::MIN;
    let mut date = NaiveDate::from_ymd_opt(2026, 6, 22).expect("a real date");
    let solstice = NaiveDate::from_ymd_opt(2026, 12, 20).expect("a real date");
    while date < solstice {
        let (rise, _) = meeus::sunrise_sunset(
            (date.year(), date.month(), date.day()),
            39.9,
            -105.5,
            STANDARD_ALTITUDE,
        )
        .expect("not polar");
        assert!(
            rise > previous,
            "sunrise at 39.9 N should be later every day from June to December; {date} \
             broke it"
        );
        previous = rise;
        date = date.succ_opt().expect("a next day");
    }

    // Polar day and polar night have no event, which is the branch the sweep must never
    // reach and the oracle must not invent an answer for.
    assert!(meeus::sunrise_sunset((2026, 6, 21), 85.0, 0.0, STANDARD_ALTITUDE).is_none());
    assert!(meeus::sunrise_sunset((2026, 12, 21), 85.0, 0.0, STANDARD_ALTITUDE).is_none());
}

/// **The comparison bites.** W1 D-10: a differ that cannot be made to differ proves nothing,
/// and that applies to this one as much as to the conformance replay.
///
/// Three ways to break it, each of which the sweep's own assertions would catch. Each
/// perturbation is **larger than the envelope**, which is the point: the declared envelope
/// is nine minutes wide, so a one-degree nudge of longitude is four minutes and stays inside
/// it. A control has to be bigger than the tolerance it is testing or it is not a control.
///
/// 1. move the station three degrees east -- twelve minutes of time;
/// 2. ask the oracle for a date a month away, so the comparison is keyed on the date and not
///    merely on the station;
/// 3. tilt the oracle's horizon by two degrees, which is what says the oracle is being
///    evaluated at all rather than short-circuited.
#[test]
fn the_comparison_bites() {
    let station = Station {
        key: "davis-full".into(),
        latitude: 39.900,
        longitude: -105.500,
        zone: "America/Denver".into(),
    };
    let date = NaiveDate::from_ymd_opt(2026, 10, 28).expect("a real date");
    let at = local_noon(&station, date);
    let (rise, set) = served(&station, at);

    let honest = oracle(&station, date, at, TEXTBOOK.altitude);
    let within = |d: i32| (TEXTBOOK.range.0..=TEXTBOOK.range.1).contains(&d);
    assert!(
        within(divergence(&rise, &honest.0)) && within(divergence(&set, &honest.1)),
        "the unperturbed comparison has to be inside the envelope before a perturbation \
         leaving it means anything"
    );

    let mut moved = station.clone();
    moved.longitude += 3.0;
    let shifted = oracle(&moved, date, at, TEXTBOOK.altitude);
    assert!(
        !within(divergence(&rise, &shifted.0)),
        "three degrees of longitude is twelve minutes of time and the comparison did not \
         notice: {rise} vs {}",
        shifted.0
    );

    let next_month = date + chrono::Duration::days(30);
    let wrong_day = oracle(&station, next_month, at, TEXTBOOK.altitude);
    assert!(
        !within(divergence(&set, &wrong_day.1)),
        "the comparison is not keyed on the date -- a month of November moves sunset by \
         half an hour: {set} vs {}",
        wrong_day.1
    );

    let tilted = oracle(&station, date, at, TEXTBOOK.altitude - 2.0);
    assert!(
        !within(divergence(&rise, &tilted.0)),
        "two degrees of horizon altitude is ten minutes of time and the comparison did not \
         notice: {rise} vs {}",
        tilted.0
    );
}

// ---------------------------------------------------------------------------------------
// The fixtures.

/// One `/latest` fixture's sun times, the station it answered for, and when it was captured.
struct Captured {
    name: String,
    station: String,
    captured_at: DateTime<Utc>,
    sunrise: String,
    sunset: String,
}

/// Every `/latest` fixture that carries `sunrise` and `sunset`.
///
/// The station comes from the response body's own `stationname` rather than from the request
/// query, because three of the six address their station by id or by the website's default
/// and the body is the one place every fixture names it.
fn latest_fixtures() -> Vec<Captured> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/fixtures");
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.expect("a readable directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path).expect("a readable fixture");
        let fixture: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        if fixture["request"]["path"] != "/latest" {
            continue;
        }
        let Some(body) = fixture["response"]["body"].as_str() else {
            continue;
        };
        // The body is JSON inside a JSON string, and `preserve_order` is not needed here --
        // only three of its keys are read.
        let Ok(served) = serde_json::from_str::<serde_json::Value>(body) else {
            continue;
        };
        let (Some(sunrise), Some(sunset)) = (served["sunrise"].as_str(), served["sunset"].as_str())
        else {
            continue;
        };
        out.push(Captured {
            name: fixture["name"]
                .as_str()
                .expect("a named fixture")
                .to_string(),
            station: served["stationname"]
                .as_str()
                .expect("a body with sun times names its station")
                .to_string(),
            captured_at: fixture["captured_at"]
                .as_str()
                .expect("a capture instant")
                .parse::<DateTime<Utc>>()
                .expect("an RFC 3339 instant"),
            sunrise: sunrise.to_string(),
            sunset: sunset.to_string(),
        });
    }
    out
}
