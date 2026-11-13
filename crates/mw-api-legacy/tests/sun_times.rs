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
//! | Cause | Size | Asserted by |
//! |---|---|---|
//! | the omitted horizon altitude | **4.55 to 5.10 min on each limb** | [`the_horizon_altitude_term_is_worth_about_five_minutes`] |
//! | the declination approximation | **up to 0.757°, which is -3 to +4 min** | [`the_declination_approximation_is_worth_three_quarters_of_a_degree`] |
//! | the day-of-year key | **1.45 min, of which 0.82 is the leap cycle** | [`the_day_of_year_key_costs_about_a_minute`] |
//!
//! The shipped arithmetic -- `mw_api_legacy::sun::sunrise_sunset`, which is
//! `remoteweather`'s `pkg/solar.CalculateSunriseSunset` at `c3ec9f7` -- puts the sun's
//! **centre** on the **geometric horizon**: `cos H = -tan(phi) tan(delta)`, with no term for
//! atmospheric refraction (34') or the solar semidiameter (16'). A textbook evaluation uses
//! `h0 = -0.8333°` for those two, which moves sunrise about five minutes earlier and sunset
//! about five minutes later at these latitudes. It also takes the declination from
//! `pkg/solar/asce.go`'s two nested day-of-year terms rather than from the solar
//! coordinates, which is worth up to 0.757° near the equinoxes.
//!
//! So over **102 270 comparisons** -- every configured station, both limbs, every day of
//! 2026-2045 -- the served value **never once** lands on the same minute as a fresh Meeus
//! evaluation. The divergence has a sign, which is the horizon-altitude term's signature:
//! the served **sunrise is late by 2 to 7 minutes** and the served **sunset early by 1 to 8**,
//! mean |divergence| **4.52 min**. Drop the horizon-altitude term from the oracle -- comparing
//! like with like, both on the geometric horizon -- and the per-limb bands collapse to
//! **-3 to +3** and **-3 to +4** with a mean of **1.19**, and a quarter of the comparisons
//! agree exactly. That residual is the declination approximation on its own.
//!
//! # Three ways the stored table and the on-demand evaluation DO differ, which is M5-4's input
//!
//! The arithmetic is shared, and that much of C-W2a-27's risk model dissolves:
//! `internal/controllers/restserver/controller.go:118` fills `sun_times` by handing
//! `solar.CalculateSunriseSunset` to `PopulateSunTimes` for all 366 keys, and
//! `handlers.go:367` reads it back at `time.Now().YearDay()` -- the function and the key
//! `sun::sunrise_sunset` ports. [`the_served_strings_are_the_stored_go_tables`] confirms it
//! empirically: the served strings reproduce all six captured fixture values exactly.
//!
//! **"Deleting the table therefore changes no value" is nevertheless false**, and the three
//! cases are what M5-4 needs rather than a footnote:
//!
//! 1. **A station at `0, 0` loses both keys in Go and keeps them here.**
//!    `controller.go:107-110` skips a device whose latitude and longitude are both zero, so
//!    no rows are written, `GetSunTimes` returns `(-1, -1)`
//!    (`pkg/config/provider_sqlite.go:2284`), and `handlers.go:370`'s `sunrise >= 0` guard
//!    drops both keys -- which `ci/conformance/README.md` states in as many words. The Rust
//!    read path has no such guard, because `StationMeta::coordinates` is
//!    `latitude.zip(longitude)` and `(0, 0)` is a position. Asserted by
//!    [`the_station_with_no_location_is_a_divergence_from_go`].
//! 2. **The table is never invalidated, so a coordinate edit never reaches it.**
//!    `controller.go:111` populates only when `HasSunTimes` is false, which is
//!    `COUNT(*) = 0`, and `DeleteSunTimes` has **no caller anywhere in the Go tree** outside
//!    its own definition and the interface that declares it. Change a station's latitude and
//!    the 366 stored rows stay at the old position for the life of the database. Here
//!    deleting the table *corrects* served values rather than preserving them.
//! 3. **The stored rows are frozen at the population year's equation of time.**
//!    `pkg/solar/sunrise.go:47` builds its reference instant from `time.Now().Year()`, so a
//!    row carries the year it was written in; `sun::sunrise_sunset` evaluates the same
//!    expression per request against the current year. The difference is under a second of
//!    time and therefore only visible where the unrounded minute sits on a rounding
//!    boundary -- which, over the seven configured stations' 366 keys and both limbs, is
//!    **272 of 5124 values for a 2026 table read in 2027 and 546 read in 2028**, each by
//!    exactly one minute. That count is **not asserted here**, because `sunrise_sunset`
//!    takes no year argument and a test cannot vary it; it was measured by re-running the
//!    same expression with the reference year parameterised, and it is recorded in this
//!    item's pull request. What *is* asserted is nothing -- what is *established* is the
//!    code-level fact it follows from, cited above.
//!
//! None of the three is an argument for keeping a table filled by the same function. What
//! they are is the list of served values that change when it goes, which is what a deletion
//! wants reviewed. Whether to fix the *formula* is a change to a frozen value on a deployed
//! surface and is not this item's to take.
//!
//! # The oracle, and why it is not a second copy of the thing under test
//!
//! [`meeus`] is written from *Astronomical Algorithms* (2nd ed.) ch. 25 (solar coordinates),
//! ch. 28 eq. 28.3 (the equation of time), ch. 15 eq. 15.1 (rising and setting) and eq. 7.1
//! (the Julian Day), and shares no line with `sun.rs`. It takes a **calendar date** rather
//! than a day of year, which is the whole point -- a day-of-year-keyed oracle could not see
//! the day-of-year keying -- and it iterates the solar position to the event instant rather
//! than evaluating it once at noon.
//!
//! An oracle checked against its own output proves nothing, and a day-length check is
//! checked against almost nothing: `set - rise` cancels the transit identically, and at the
//! equator or at a solstice it is insensitive to the declination as well, so an error of a
//! whole degree in the sun's longitude moves nothing a day length can see. So the oracle is
//! pinned where it can actually be pinned, by **Meeus's own published worked examples** --
//! Example 25.b's apparent longitude and declination, Example 28.1's equation of time, and
//! ch. 7's Julian Days ([`the_oracle_reproduces_meeuss_worked_examples`]) -- and the transit,
//! which no day length can see, is pinned by its scaling with longitude and by the published
//! analemma ([`the_oracle_reproduces_published_astronomical_quantities`]). The iteration has
//! its own control: it must converge, and removing it must change the answer
//! ([`the_oracles_iteration_is_load_bearing`]).
//!
//! # What this comparison cannot see, stated rather than implied
//!
//! Both sides format through the contract's own `sun::format_sun_time` at the same instant
//! and the same zone, and both transits carry the same `720 - longitude * 4` term. So the
//! sweep is **blind to the zone, to the choice of ordinal, and to longitude** -- all three
//! cancel. They are pinned instead by [`the_served_strings_are_the_stored_go_tables`], which
//! compares against bytes a different program produced at one instant, and by
//! [`configured_stations`]'s cross-check of each parsed coordinate pair against the
//! `/api/stations` capture. A formatter defect that moves both sides equally is invisible
//! here by construction, and `sun.rs`'s own unit tests are where it is caught.
//!
//! Nor is the sweep a **regression lock on the shipped arithmetic**, and the limit was
//! measured rather than reasoned about. Mutating `sun.rs` and re-running: a declination
//! amplitude of `0.39685` instead of `0.39785` is caught (by the fixture comparison, not by
//! the sweep), and so are a dropped equation-of-time term and an inverted hour angle. But a
//! **sub-minute** perturbation that moves only a few values -- `0.9857` for `0.9856` in the
//! declination's inner term, or dropping eq. 28.3's `1.25 e^2 sin 2M` -- moves no band's
//! extreme and passes. It has to: the reference-year effect in item 3 above moves ten times
//! as many values as that, so no envelope over this comparison can separate the two. The
//! exact-value regression lock on `sun.rs` is `sun.rs`'s own unit tests, which assert
//! against numbers produced by **running** the Go code at six named inputs.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, NaiveDate, Offset, TimeZone, Utc};
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
/// Twenty, because one of the three divergences is a *leap-cycle* effect and that is only
/// visible across the cycle: a single year's sweep cannot separate "the key is wrong in
/// non-leap years" from "the formula is wrong every year", which is the distinction
/// C-W2a-27 turns on.
const YEARS: std::ops::RangeInclusive<i32> = 2026..=2045;

/// How many refinement passes [`meeus::sunrise_sunset`] takes.
const PASSES: usize = 4;

/// The divergence between `/latest`'s strings and a fresh Meeus evaluation, as **data**.
///
/// M5-2's rule, applied one item early: an expected difference encoded as data rather than
/// as prose makes an unexpected one a failure rather than a judgement call.
///
/// **Every field is load-bearing in both directions.** Each band is asserted for
/// containment *and* for attainment -- both of its ends must be reached exactly -- so
/// widening one to absorb a regression makes the attainment half fail. The bands are **per
/// limb** rather than combined, which is what makes them tight: combined they would be
/// `[-8, +7]` and a minute of drift could hide inside, where `sunrise` alone spans
/// `[+2, +7]` and a served sunrise one minute earlier leaves it.
struct Envelope {
    /// The horizon altitude the oracle is evaluated at.
    altitude: f64,
    /// The measured signed range, in minutes, of served minus oracle at **sunrise**.
    sunrise: (i32, i32),
    /// The same at **sunset**. Opposite in sign at the textbook altitude, because the term
    /// the shipped formula omits moves the two limbs apart.
    sunset: (i32, i32),
    /// The measured mean of `|served - oracle|`, in minutes.
    ///
    /// The number that distinguishes the two causes: **4.52 against the textbook horizon
    /// and 1.19 against the geometric one** says the omitted horizon-altitude term is most
    /// of the divergence and the declination approximation is the rest.
    mean_abs: f64,
    /// Whether any comparison at all lands on the same minute.
    ///
    /// `false` for the textbook comparison is this item's headline, and it is asserted as an
    /// exact zero out of 102 270 rather than as a bound. `true` for the geometric one is the
    /// other half of the decomposition: remove the horizon term and a quarter of the
    /// comparisons agree exactly.
    agrees: bool,
}

/// Against a textbook evaluation: never within a minute, and signed.
const TEXTBOOK: Envelope = Envelope {
    altitude: STANDARD_ALTITUDE,
    sunrise: (2, 7),
    sunset: (-8, -1),
    mean_abs: 4.52,
    agrees: false,
};

/// At the same horizon altitude: the declination approximation on its own.
const SAME_HORIZON: Envelope = Envelope {
    altitude: GEOMETRIC_HORIZON,
    sunrise: (-3, 3),
    sunset: (-3, 4),
    mean_abs: 1.19,
    agrees: true,
};

/// A fresh Meeus evaluation of sunrise and sunset. No line of this is `sun.rs`.
mod meeus {
    /// Which end of the day.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Limb {
        Rise,
        Set,
    }

    impl Limb {
        fn sign(self) -> f64 {
            match self {
                Limb::Rise => -1.0,
                Limb::Set => 1.0,
            }
        }
    }

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
        Some((
            limb(
                date,
                latitude,
                longitude,
                altitude,
                Limb::Rise,
                super::PASSES,
            )?,
            limb(
                date,
                latitude,
                longitude,
                altitude,
                Limb::Set,
                super::PASSES,
            )?,
        ))
    }

    /// One limb, with the pass count exposed.
    ///
    /// Public with the knob because the convergence of the iteration is itself asserted:
    /// four passes must agree with twelve, and **one** pass must not, or the refinement is
    /// decoration. See `the_oracles_iteration_is_load_bearing`.
    pub fn limb(
        date: (i32, u32, u32),
        latitude: f64,
        longitude: f64,
        altitude: f64,
        which: Limb,
        passes: usize,
    ) -> Option<f64> {
        solve(
            julian_day(date),
            latitude,
            longitude,
            altitude,
            which.sign(),
            passes,
        )
    }

    /// The transit, as the midpoint of the two limbs.
    ///
    /// Not an independent computation of it -- `solve` never returns one -- which is exactly
    /// why the transit has to be pinned by its *scaling with longitude* rather than by a
    /// symmetry: the midpoint of a pair is equidistant from both of them by definition, so
    /// "the limbs are symmetric about the midpoint" is an algebraic identity and cannot
    /// fail. It was in this file as an assertion until a code review pointed that out.
    pub fn transit(date: (i32, u32, u32), latitude: f64, longitude: f64) -> Option<f64> {
        let (rise, set) = sunrise_sunset(date, latitude, longitude, super::STANDARD_ALTITUDE)?;
        Some((rise + set) / 2.0)
    }

    /// One limb, by fixed-point iteration from the local solar noon.
    ///
    /// Four passes, which is two more than the event instant needs at the latitudes this
    /// file sweeps, and the margin is cheap. The convergence is **not** uniform in latitude:
    /// the pass-to-pass steps for sunrise at 85 N on 2026-09-12 are -37.038, -3.528, -0.360,
    /// -0.037 and -0.004 minutes, so the third pass is still worth a third of a minute there
    /// while at 40 N it is worth a thousandth. Four passes leave under 0.00002 minutes
    /// against twelve everywhere this file evaluates.
    fn solve(
        midnight: f64,
        latitude: f64,
        longitude: f64,
        altitude: f64,
        sign: f64,
        passes: usize,
    ) -> Option<f64> {
        // Local solar noon, as the instant the first pass evaluates the sun at.
        let mut at = midnight + 0.5 - longitude / 360.0;
        let mut minutes = 0.0;
        for _ in 0..passes {
            let (_, declination, equation_of_time) = coordinates(at);
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

    /// The sun's apparent longitude and declination in degrees, and the equation of time in
    /// minutes, for one Julian Day.
    ///
    /// Meeus ch. 25's low-accuracy solar position (0.01° over this millennium, which is two
    /// orders of magnitude finer than the divergence being measured) and eq. 28.3. The
    /// longitude is returned as well as used because it is the first published value
    /// Example 25.b gives, and pinning it is what pins the whole chain feeding the
    /// declination.
    ///
    /// **The argument is treated as dynamical time and fed with UT**, which in 2026 is a
    /// `dT` of about 70 s and moves the result by 0.0002 minutes. Named rather than
    /// corrected: a correction would need a `dT` model, and the error is four orders of
    /// magnitude below the smallest quantity this file measures.
    pub fn coordinates(jd: f64) -> (f64, f64, f64) {
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
        (lambda.rem_euclid(360.0), declination, equation_of_time)
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

/// The column list the parser indexes into.
///
/// Asserted, because the one seed edit that could hand this comparison **silently wrong
/// coordinates** is a column reorder that keeps the count at twelve: swapping `latitude` and
/// `longitude` in the column list and in every row is semantically identical SQL, and a
/// field-count check would pass it. With the names asserted it is a named failure instead.
const STATION_COLUMNS: &str = "id, station_key, owning_node_id, name, type, enabled, latitude, \
                               longitude, altitude_m, timezone, snow_base_distance_mm, \
                               identity_kind";

/// The seed's `INSERT INTO stations`, parsed.
///
/// Parsed rather than transcribed: a transcription is a second copy of the rig's
/// configuration that drifts silently, and the drift would show up as this comparison
/// quietly not covering a station.
///
/// **Longitude needs the cross-check at the bottom of this function and cannot rely on the
/// sweep.** Longitude enters the shipped transit and the oracle's transit through the same
/// `720 - longitude * 4`, so it cancels: shifting every station forty degrees east leaves
/// every number the sweep measures unchanged. Checking the parsed pairs against the
/// `/api/stations` capture -- bytes the Go daemon produced, with a different column order --
/// is what makes a wrong longitude a failure here rather than nowhere.
fn configured_stations() -> Vec<Station> {
    let path = repo_root().join("ci/conformance/rust-seed/01-config.sql");
    let sql = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let sql = strip_line_comments(&sql);

    let insert = sql
        .find("INSERT INTO stations")
        .expect("the seed configures stations");
    let open = insert
        + sql[insert..]
            .find('(')
            .expect("the stations insert names its columns");
    let close = open
        + sql[open..]
            .find(')')
            .expect("a closed column list -- no column name contains a parenthesis");
    let columns = sql[open + 1..close]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        columns, STATION_COLUMNS,
        "the stations insert's column list changed. The parser reads latitude, longitude and \
         timezone by POSITION, so a reorder would hand this comparison another column's \
         value -- and the sweep cannot notice, because longitude cancels in it entirely."
    );

    let values = close
        + sql[close..]
            .find("VALUES")
            .map(|v| v + "VALUES".len())
            .expect("the stations insert has a VALUES list");
    let end = values + end_of_statement(&sql[values..]);

    let stations: Vec<Station> = tuples(&sql[values..end])
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

    // The cross-check, against a value the GO DAEMON served rather than against the file
    // these numbers were just parsed out of. `/api/stations` carries each station's
    // coordinates, `contract/fixtures/api-stations-portal.json` is a capture of it, and the
    // column order there is nothing like the seed's -- so a wrong field index, a column
    // swap and a dropped minus sign all fail here, where the sweep cannot see any of them.
    let captured = coordinates_captured_by_go();
    for station in &stations {
        match captured.get(&station.key) {
            Some(pair) => assert_eq!(
                (station.latitude, station.longitude),
                *pair,
                "`{}` is {:?} in ci/conformance/rust-seed/01-config.sql and {pair:?} in the \
                 `/api/stations` capture. The parser indexes columns by position and the \
                 sweep cannot see a wrong longitude, so this is the check that catches it.",
                station.key,
                (station.latitude, station.longitude)
            ),
            // `/api/stations` excludes a station with no location, which is the one the
            // capture is missing and the predicate the seed's `0, 0` exists to exercise.
            None => assert_eq!(
                (station.latitude, station.longitude),
                (0.0, 0.0),
                "`{}` is absent from the `/api/stations` capture, which excludes only a \
                 station at 0, 0",
                station.key
            ),
        }
    }
    assert_eq!(
        captured.len(),
        CONFIGURED_STATIONS - 1,
        "the capture should carry every configured station but the one at 0, 0"
    );
    stations
}

/// Each station's coordinates as the **Go daemon served them**, from the `/api/stations`
/// capture.
///
/// A second, independent reading of the same configuration: these are bytes a different
/// program produced from its own copy of the rig, so agreeing with them is evidence that the
/// seed parser read the columns it meant to.
fn coordinates_captured_by_go() -> BTreeMap<String, (f64, f64)> {
    let path = repo_root().join("contract/fixtures/api-stations-portal.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let fixture: serde_json::Value = serde_json::from_str(&text).expect("a readable fixture");
    let body = fixture["response"]["body"]
        .as_str()
        .expect("`/api/stations` has a JSON body");
    let stations: Vec<serde_json::Value> =
        serde_json::from_str(body).expect("`/api/stations` answers an array");
    stations
        .iter()
        .map(|s| {
            (
                s["name"].as_str().expect("a named station").to_string(),
                (
                    s["latitude"].as_f64().expect("a latitude"),
                    s["longitude"].as_f64().expect("a longitude"),
                ),
            )
        })
        .collect()
}

/// One configured station by key, so that no test transcribes a position the seed owns.
fn station_named(key: &str) -> Station {
    configured_stations()
        .into_iter()
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("the seed configures no station `{key}`"))
}

/// The repository root, from this crate's manifest directory.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
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

/// The offset of the statement terminator, skipping any `;` inside a quoted literal.
///
/// Quote-aware rather than `find(';')`: a timezone or a station key containing a semicolon
/// would otherwise truncate the row list, and the count assertion would then report "the rig
/// configures 3 stations" for what is really a quoting bug.
fn end_of_statement(sql: &str) -> usize {
    let mut quoted = false;
    for (i, c) in sql.char_indices() {
        if quoted {
            quoted = c != '\'';
        } else if c == '\'' {
            quoted = true;
        } else if c == ';' {
            return i;
        }
    }
    sql.len()
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
        } else if c == '(' {
            depth += 1;
            if depth == 1 {
                current.clear();
                continue;
            }
        } else if c == ')' {
            depth = depth.checked_sub(1).expect("balanced parentheses");
            if depth == 0 {
                rows.push(std::mem::take(&mut current));
                continue;
            }
        } else if c == '\'' {
            quoted = true;
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
/// Through [`Renderer::reading`] rather than through `sun::sunrise_sunset` directly, so that
/// the comparison is against the values the endpoint emits and not against the arithmetic
/// behind them. **That does not put the glue under test in the sweep** -- the ordinal source
/// and the zone both cancel, as the module documentation says -- but it does put it under
/// test in [`the_served_strings_are_the_stored_go_tables`], where the other side of the
/// comparison is bytes a different program produced.
fn served(station: &Station, at: DateTime<Utc>) -> (String, String) {
    let key = StationKey::new(station.key.as_str());
    let mut zones = BTreeMap::new();
    zones.insert(
        key.clone(),
        Zone::new(&station.zone).unwrap_or_else(|e| panic!("{}: {e}", station.zone)),
    );
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
        // The configured device name, which is where `/latest`'s `stationname` comes from
        // since M5-5. The rig's stations are named after their keys, as a configuration
        // migrated from Go would be -- which is also what `latest_fixtures()` below relies
        // on when it takes each fixture's station from its own response body.
        display_name: Some(station.key.clone()),
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
/// contract's formatter rather than a second path to the numbers. All 1440 minutes are
/// round-tripped through `sun::format_sun_time` and back in [`the_comparison_bites`], because
/// every number in this file is a difference of two parsed clock strings and a parser that
/// lost a minute would move all of them together.
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
///
/// Exactly `+-720` is reported as `-720`, which is the one input where the sign is a
/// convention rather than a measurement. Unreachable here: the largest divergence the sweep
/// sees is eight minutes, and a difference that large would fail the bands on magnitude
/// whichever sign it carried.
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

/// Every day of [`YEARS`], including each leap year's 366th.
fn sweep_dates() -> impl Iterator<Item = NaiveDate> {
    YEARS
        .flat_map(|year| (1..=366).filter_map(move |ordinal| NaiveDate::from_yo_opt(year, ordinal)))
}

/// What one limb's comparisons measured.
#[derive(Debug)]
struct Side {
    lo: i32,
    hi: i32,
    agreements: u32,
    total_abs: i64,
    samples: u32,
    worst: String,
}

impl Default for Side {
    fn default() -> Self {
        Self {
            lo: i32::MAX,
            hi: i32::MIN,
            agreements: 0,
            total_abs: 0,
            samples: 0,
            worst: String::new(),
        }
    }
}

impl Side {
    fn fold(&mut self, d: i32, what: &str) {
        if d < self.lo || d > self.hi {
            self.worst = format!("{what} ({d:+} min)");
        }
        self.lo = self.lo.min(d);
        self.hi = self.hi.max(d);
        self.agreements += u32::from(d == 0);
        self.total_abs += i64::from(d.abs());
        self.samples += 1;
    }
}

/// What one sweep measured, per limb.
#[derive(Debug, Default)]
struct Measured {
    rise: Side,
    set: Side,
}

impl Measured {
    fn mean_abs(&self) -> f64 {
        (self.rise.total_abs + self.set.total_abs) as f64 / f64::from(self.samples())
    }

    fn samples(&self) -> u32 {
        self.rise.samples + self.set.samples
    }

    fn agreements(&self) -> u32 {
        self.rise.agreements + self.set.agreements
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
            measured.rise.fold(
                divergence(&rise, &rise_oracle),
                &format!("{} sunrise {date}: {rise} vs {rise_oracle}", station.key),
            );
            measured.set.fold(
                divergence(&set, &set_oracle),
                &format!("{} sunset {date}: {set} vs {set_oracle}", station.key),
            );
        }
    }
    assert_eq!(
        measured.samples(),
        102_270,
        "the sweep is 7 stations x 7305 days x 2 limbs. A different count means the station \
         set or the year range moved, and every constant below was measured over that one."
    );
    measured
}

/// One limb's band, asserted for containment **and** attainment.
fn assert_band(side: &Side, band: (i32, i32), altitude: f64, limb: &str) {
    assert!(
        side.lo >= band.0 && side.hi <= band.1,
        "at h0 = {altitude:+.4}° the {limb} divergence is [{:+}, {:+}], outside the declared \
         [{:+}, {:+}]. Worst: {}. This is a change in the arithmetic on one side or the \
         other, not a bound to widen.",
        side.lo,
        side.hi,
        band.0,
        band.1,
        side.worst
    );
    assert!(
        side.lo == band.0 && side.hi == band.1,
        "at h0 = {altitude:+.4}° the {limb} divergence spans [{:+}, {:+}] where the \
         declaration says [{:+}, {:+}]. A declared band that is not REACHED is a tolerance \
         rather than a measurement, which is what this half of the assertion refuses -- so a \
         bound widened to absorb a regression fails here even though the containment check \
         above passes.",
        side.lo,
        side.hi,
        band.0,
        band.1
    );
}

// ---------------------------------------------------------------------------------------
// The assertions.

/// **The item: `/latest`'s strings against a fresh Meeus evaluation, every configured
/// station.**
///
/// The result is a disagreement, so the disagreement is what is asserted -- per limb, with
/// both ends of every band live, the mean pinned to five hundredths of a minute, and the
/// agreement count pinned exactly. The two envelopes together are the finding: removing the
/// horizon altitude from the oracle takes the served sunrise from "always 2 to 7 minutes
/// late" to "within 3 minutes either way", so the omitted term is most of the gap and the
/// declination approximation is the rest.
#[test]
fn every_configured_station_diverges_from_a_fresh_meeus_evaluation() {
    for envelope in [&TEXTBOOK, &SAME_HORIZON] {
        let m = sweep(envelope.altitude);
        assert_band(&m.rise, envelope.sunrise, envelope.altitude, "sunrise");
        assert_band(&m.set, envelope.sunset, envelope.altitude, "sunset");
        assert!(
            (m.mean_abs() - envelope.mean_abs).abs() < 0.05,
            "at h0 = {:+.4}° the mean |divergence| over {} comparisons is {:.4} minutes, \
             where the measurement says {:.2}. The two declared means are the decomposition \
             of this item's finding, so a change in either is a result to report rather than \
             a constant to update.",
            envelope.altitude,
            m.samples(),
            m.mean_abs(),
            envelope.mean_abs
        );
        let agreements = m.agreements();
        assert_eq!(
            agreements > 0,
            envelope.agrees,
            "at h0 = {:+.4}° {agreements} of {} comparisons landed on the same minute, and \
             the measurement says {}. Zero at the textbook altitude is this item's headline; \
             a nonzero count there would mean the served arithmetic had started agreeing \
             with the sky, which is good news and has to be reported rather than absorbed.",
            envelope.altitude,
            m.samples(),
            if envelope.agrees {
                "some do"
            } else {
                "none do"
            }
        );
        if !envelope.agrees {
            assert_eq!(
                agreements, 0,
                "asserted as an exact zero rather than as a bound"
            );
        }
    }
}

/// **The one assertion that does pass to the minute, and the one Decision 36 rests on:**
/// the served strings are the stored Go table's values.
///
/// Every `/latest` fixture that carries `sunrise` and `sunset` is replayed through the
/// renderer at its own `captured_at` instant and its own station, and compared to the
/// captured string **exactly**. The fixture set is declared in both directions, because a
/// fixture that stopped carrying the keys and a new one that started are different changes
/// and a count would hide both.
///
/// **What the offset rests on, honestly.** `ci/conformance/README.md` says the captured
/// strings are "not reproducible by this rig" because the Go rig left `devices.timezone`
/// unset, so `handlers.go:371` fell back to `time.Local` -- the capture host's zone, which
/// `ci/conformance/go-target.sh` does not record and deliberately does not shift. So the
/// offset is **derived here rather than assumed**: the last block renders the shipped UT
/// minutes at every whole-hour offset and asserts that exactly one of the twenty-seven
/// reproduces all six captures, and that it is UTC-6. `America/Denver` is UTC-6 on
/// 2026-10-28 -- Mountain Daylight Time runs to 1 November 2026 -- which is why rendering in
/// the zone the Rust rig configures reproduces the capture at all.
///
/// **Its reach is one day-of-year key**, which is worth knowing before leaning on it: it
/// cannot see either the reference-year drift or the `0, 0` case from the module
/// documentation's list of three.
#[test]
fn the_served_strings_are_the_stored_go_tables() {
    // The six `/latest` fixtures that carry the two keys, and the station each answered for.
    // `latest-msgpack` carries the same values in a binary body and is not replayable as a
    // JSON string, which is why it is named here as excluded rather than silently skipped.
    const CARRY_SUN_TIMES: [(&str, &str); 6] = [
        ("latest-air-quality-device", "aqmon"),
        ("latest-by-station-id", "davis-full"),
        ("latest-default-station", "davis-full"),
        ("latest-full-complement", "davis-full"),
        ("latest-minimal-complement", "csi-minimal"),
        ("latest-snow-device", "snowgauge"),
    ];

    let stations = configured_stations();
    let found = latest_fixtures();
    let mut names: Vec<(&str, &str)> = found
        .iter()
        .map(|f| (f.name.as_str(), f.station.as_str()))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        CARRY_SUN_TIMES.to_vec(),
        "the set of `/latest` fixtures carrying `sunrise` and `sunset` changed. Both \
         directions matter: a fixture that lost the keys is a change to the frozen surface, \
         and one that gained them is a station this comparison was not covering."
    );

    let mut distinct: Vec<(&str, DateTime<Utc>)> = Vec::new();
    for fixture in &found {
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
             `sun::sunrise_sunset` ports -- so either the port has drifted, or the fixture \
             was recaptured on a host at a different offset from the UTC-6 the block below \
             derives.",
            fixture.name,
            fixture.station,
            fixture.captured_at
        );
        let pair = (fixture.station.as_str(), fixture.captured_at);
        if !distinct.contains(&pair) {
            distinct.push(pair);
        }
    }
    assert_eq!(
        distinct.len(),
        4,
        "the six fixtures are four distinct (station, instant) pairs at one day-of-year key, \
         which is the whole of this check's reach -- stated as a number so that nobody reads \
         `six fixtures` as six independent days."
    );

    // The offset, derived. Each fixture's station, evaluated at the capture's day of year,
    // gives a pair of UT minutes; exactly one whole-hour offset renders all six pairs as the
    // six captured pairs.
    let ordinal = found[0]
        .captured_at
        .with_timezone(&chrono::FixedOffset::east_opt(-6 * 3600).expect("a real offset"))
        .ordinal();
    let reproducing: Vec<i32> = (-12..=14)
        .filter(|hours| {
            let offset =
                chrono::FixedOffset::east_opt(hours * 3600).expect("a whole-hour offset is real");
            found.iter().all(|fixture| {
                let station = stations
                    .iter()
                    .find(|s| s.key == fixture.station)
                    .expect("a configured station");
                let Some((rise, set)) =
                    sun::sunrise_sunset(ordinal, station.latitude, station.longitude)
                else {
                    return false;
                };
                sun::format_sun_time(rise, fixture.captured_at, &offset) == fixture.sunrise
                    && sun::format_sun_time(set, fixture.captured_at, &offset) == fixture.sunset
            })
        })
        .collect();
    assert_eq!(
        reproducing,
        vec![-6],
        "exactly one whole-hour offset should reproduce all six captures, and it should be \
         UTC-6 -- the offset `America/Denver` had on 2026-10-28. Instead {reproducing:?} \
         did. More than one would mean the six fixtures do not constrain the offset; none \
         would mean the shipped arithmetic no longer reproduces the capture at any offset."
    );
}

/// **A station at `0, 0` is a divergence from Go, not a reproduction of it.**
///
/// `nolocation` is seeded at `0.000, 0.000` rather than NULL so that the rig exercises the
/// `latitude = 0 AND longitude = 0` predicate `/api/stations` applies
/// (`ci/conformance/rust-seed/01-config.sql`). The read path does **not** apply it:
/// `StationMeta::coordinates` is `latitude.zip(longitude)`, so `(0, 0)` is a position and
/// `/latest` serves the Gulf of Guinea's sun times.
///
/// **Go omits both keys for the same station.** `controller.go:107-110` skips a device whose
/// latitude and longitude are both zero before it ever reaches `PopulateSunTimes`, so the
/// table has no rows for it, `GetSunTimes` returns `(-1, -1)`
/// (`pkg/config/provider_sqlite.go:2284`) and `handlers.go:370`'s `sunrise >= 0` guard drops
/// both keys -- which `ci/conformance/README.md` states outright: "a device at 0,0 gets no
/// sun table and loses both keys". So this is one of the three ways deleting the table
/// changes a served value, and it is recorded as a divergence for M5-4 rather than as the
/// reproduction an earlier draft of this file claimed it was.
///
/// It stays unobservable on the frozen surface only because the station has no readings and
/// `/latest` therefore 404s. Asserted rather than skipped, because "the station the sweep
/// could not compare" is exactly where a gap hides.
#[test]
fn the_station_with_no_location_is_a_divergence_from_go() {
    let nowhere = station_named("nolocation");
    assert_eq!(
        (nowhere.latitude, nowhere.longitude),
        (0.0, 0.0),
        "this test is about the seed's 0, 0 station"
    );
    let date = NaiveDate::from_ymd_opt(2026, 10, 28).expect("a real date");
    let at = local_noon(&nowhere, date);
    let (rise, set) = served(&nowhere, at);
    assert!(
        !rise.is_empty() && !set.is_empty(),
        "`{}` is at 0, 0, which the Rust read path treats as a position -- both keys are \
         served, where Go skips the station at controller.go:107 and serves neither",
        nowhere.key
    );

    // At the equator the hour angle is 90° whatever the declination, because
    // `cos H = -tan(0) tan(delta)` is zero -- so the declination approximation cannot move
    // the answer and the only divergence left is the horizon-altitude term. That is the
    // decomposition, isolated by geography rather than argued.
    let same = oracle(&nowhere, date, at, GEOMETRIC_HORIZON);
    assert_eq!(
        divergence(&rise, &same.0)
            .abs()
            .max(divergence(&set, &same.1).abs()),
        0,
        "at the equator the geometric comparison should be exact: {rise}/{set} vs {}/{}",
        same.0,
        same.1
    );
    let textbook = oracle(&nowhere, date, at, STANDARD_ALTITUDE);
    let shift = divergence(&rise, &textbook.0);
    assert!(
        (3..=5).contains(&shift),
        "and the textbook comparison should be the horizon-altitude term alone, about four \
         minutes at the equator; got {shift:+}"
    );
}

/// **The horizon-altitude term is worth about five minutes on each limb.**
///
/// This measures the **oracle's** sensitivity to its own `h0` parameter and nothing else:
/// the rest of the solar position cancels between the two evaluations, so it establishes the
/// *size* of the term `sun::sunrise_sunset` omits and not its dominance. Dominance is the two
/// `mean_abs` constants in
/// [`every_configured_station_diverges_from_a_fresh_meeus_evaluation`], which is where it
/// belongs: 4.52 minutes with the term in the oracle, 1.19 without it.
///
/// It moves sunrise earlier and sunset later by the same amount, which is why the textbook
/// bands are signed and opposite and the same-horizon ones are not.
#[test]
fn the_horizon_altitude_term_is_worth_about_five_minutes() {
    let station = station_named("davis-full");
    for (date, expected) in [
        ((2026, 6, 21), 5.10),
        ((2026, 12, 21), 5.06),
        ((2026, 10, 28), 4.55),
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
            (earlier - expected).abs() < 0.01 && (later - expected).abs() < 0.01,
            "{date:?}: the horizon altitude should move each limb {expected} minutes; \
             sunrise moved {earlier:.4} and sunset {later:.4}"
        );
    }
}

/// **The declination approximation is worth up to three quarters of a degree**, which is the
/// second of the three causes and the one that survives when the horizon term is removed.
///
/// `pkg/solar/asce.go`'s expression is two nested day-of-year terms --
/// `asin(0.39785 sin(278.97 + 0.9856 d + 1.9165 sin(356.6 + 0.9856 d)))` -- against the solar
/// coordinates of Meeus ch. 25. Measured at each station-day's **local solar noon**, which is
/// where the shipped formula's single evaluation effectively sits; the figure is
/// framing-sensitive -- at noon UT over one year it is 0.49° -- so the framing is named rather
/// than left to be rediscovered.
///
/// Three quarters of a degree is about three minutes of time at 39.9 N, which is the
/// same-horizon band the sweep measures -- so this is the number that *explains* that band
/// rather than a second opinion about it.
#[test]
fn the_declination_approximation_is_worth_three_quarters_of_a_degree() {
    let station = station_named("davis-full");
    let mut worst = (0.0_f64, String::new());
    for date in sweep_dates() {
        let doy = f64::from(date.ordinal());
        let inner = (356.6 + 0.9856 * doy).to_radians();
        let outer = (278.97 + 0.9856 * doy + 1.9165 * inner.sin()).to_radians();
        let shipped = (0.397_85 * outer.sin()).asin().to_degrees();
        // Local solar noon, to within the equation of time, which does not matter at this
        // resolution: the declination moves 0.4° a day at most.
        let noon = meeus::julian_day((date.year(), date.month(), date.day())) + 0.5
            - station.longitude / 360.0;
        let (_, fresh, _) = meeus::coordinates(noon);
        let error = shipped - fresh;
        if error.abs() > worst.0.abs() {
            worst = (error, date.to_string());
        }
    }
    assert!(
        (0.74..0.77).contains(&worst.0),
        "the shipped declination should be up to about +0.757° away from the fresh one over \
         {YEARS:?}; the worst was {:+.4}° on {}. The sign matters as much as the size: the \
         approximation runs ahead of the sun near the September equinox, which is where the \
         same-horizon band reaches its +4.",
        worst.0,
        worst.1
    );
}

/// **The day-of-year key costs about a minute, of which the leap cycle is most of a
/// minute** -- the third cause, and the one C-W2a-27 named.
///
/// Two quantities, because they are not the same one and conflating them is how the smallest
/// of the three divergences gets credited with all of it:
///
/// 1. **The irreducible cost of any day-of-year key.** The tropical year is not 365 days, so
///    a key lands on a slightly different solar phase every year even where the key-to-date
///    map never moves. Measured as the spread of the *true* time for one key across
///    [`YEARS`]: **1.45 minutes**, and ordinals 1-58 -- where the map is identical in every
///    year -- already reach most of it.
/// 2. **The leap-cycle component, which is C-W2a-27's claim verbatim**: a key read in a
///    non-leap year lands on a different calendar date from the same key in a leap year.
///    Measured as the gap between the leap-year and non-leap-year means for one key:
///    **0.82 minutes**.
///
/// Both are smaller than the horizon-altitude term, and that ordering is this item's result.
#[test]
fn the_day_of_year_key_costs_about_a_minute() {
    let station = station_named("davis-full");
    let truth = |date: NaiveDate, limb| {
        let (rise, set) = meeus::sunrise_sunset(
            (date.year(), date.month(), date.day()),
            station.latitude,
            station.longitude,
            STANDARD_ALTITUDE,
        )
        .expect("not polar");
        if limb == meeus::Limb::Rise {
            rise
        } else {
            set
        }
    };
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;

    let mut spread = (0.0_f64, 0_u32);
    let mut leap_gap = (0.0_f64, 0_u32);
    // 366 included: it is the key whose date mapping is most anomalous, it exists only in
    // leap years, and the Go table is filled for all 366 of them.
    for ordinal in 1..=366_u32 {
        let dates: Vec<NaiveDate> = YEARS
            .filter_map(|year| NaiveDate::from_yo_opt(year, ordinal))
            .collect();
        for limb in [meeus::Limb::Rise, meeus::Limb::Set] {
            // Unwrapped minutes, so a sunset either side of midnight UT is not a
            // 1439-minute spread.
            let all: Vec<f64> = dates.iter().map(|d| truth(*d, limb)).collect();
            let widest = all.iter().cloned().fold(f64::MIN, f64::max)
                - all.iter().cloned().fold(f64::MAX, f64::min);
            if widest > spread.0 {
                spread = (widest, ordinal);
            }

            let mut leap: Vec<f64> = Vec::new();
            let mut common: Vec<f64> = Vec::new();
            for (date, value) in dates.iter().zip(&all) {
                if NaiveDate::from_ymd_opt(date.year(), 2, 29).is_some() {
                    leap.push(*value);
                } else {
                    common.push(*value);
                }
            }
            if leap.is_empty() || common.is_empty() {
                continue;
            }
            let gap = mean(&leap) - mean(&common);
            if gap.abs() > leap_gap.0.abs() {
                leap_gap = (gap, ordinal);
            }
        }
    }

    assert!(
        (1.40..1.50).contains(&spread.0),
        "one day-of-year key should span about 1.45 minutes of true time across {YEARS:?}; \
         the widest was key {} at {:.3} minutes",
        spread.1,
        spread.0
    );
    assert!(
        (0.75..0.90).contains(&leap_gap.0.abs()),
        "the leap-cycle component -- C-W2a-27's own claim -- should be about 0.82 minutes; \
         the widest was key {} at {:+.3}. This is the quantity the item predicted, measured \
         separately from the key's irreducible cost so that the prediction is tested rather \
         than inferred from the larger number.",
        leap_gap.1,
        leap_gap.0
    );
    let smallest_textbook = f64::from(TEXTBOOK.sunrise.0);
    assert!(
        spread.0 < smallest_textbook,
        "the day-of-year key ({:.3} min) has to be smaller than the SMALLEST divergence the \
         textbook comparison measures ({smallest_textbook} min) -- that ordering is this \
         item's result, and if it inverted the result would have to be rewritten rather than \
         the constant adjusted",
        spread.0
    );
}

/// **The oracle against Meeus's own published worked examples.**
///
/// The only checks in this file that pin the solar-longitude chain. A day length cannot:
/// `set - rise` cancels the transit identically, and at the equator or at a solstice the
/// declination's effect on it is second-order, so an error of a whole degree in the sun's
/// longitude moves nothing a day length can see. These numbers are printed in
/// *Astronomical Algorithms* and are not derived from this code.
#[test]
fn the_oracle_reproduces_meeuss_worked_examples() {
    // Examples 7.a and 7.b: the Julian Day, including the January shift that makes a
    // February date belong to the previous year's month 14, and the Gregorian century term.
    for (date, published) in [
        ((1987, 1, 27), 2_446_822.5),
        ((1988, 1, 27), 2_447_187.5),
        ((1600, 1, 1), 2_305_447.5),
        ((1600, 12, 31), 2_305_812.5),
        ((2000, 1, 1), 2_451_544.5),
        ((1992, 10, 13), 2_448_908.5),
    ] {
        assert_eq!(
            meeus::julian_day(date),
            published,
            "{date:?} is JD {published} in Meeus ch. 7"
        );
    }

    // Example 25.b, 1992 October 13.0 TD. The chapter's low-accuracy method -- which is what
    // this oracle uses -- is quoted there as reaching an apparent longitude of 199.90895°
    // and an apparent declination of -7.78507°.
    let (lambda, declination, equation_of_time) =
        meeus::coordinates(meeus::julian_day((1992, 10, 13)));
    assert!(
        (lambda - 199.908_95).abs() < 0.000_01,
        "Example 25.b's apparent longitude is 199.90895°; the oracle says {lambda:.6}°. This \
         is the assertion that pins the equation of the centre, the nutation and aberration \
         term, and the degree/radian boundaries -- nothing else in this file can see them."
    );
    assert!(
        (declination - -7.785_07).abs() < 0.000_01,
        "Example 25.b's apparent declination is -7.78507°; the oracle says {declination:.6}°. \
         This pins the obliquity on top of the longitude chain."
    );

    // Example 28.1, the same instant: Meeus publishes the equation of time as
    // 13m 42.6s = 13.710 min.
    assert!(
        (equation_of_time - 13.710).abs() < 0.001,
        "Example 28.1's equation of time is 13m 42.6s, which is 13.710 minutes; the oracle \
         says {equation_of_time:.4}. This pins eq. 28.3 including its cross term, which the \
         analemma extremes are far too coarse to see."
    );
}

/// **The oracle against quantities published independently of this repository**, and the
/// transit, which no day length can reach.
///
/// None of these numbers comes from this code:
///
/// - the four extremes of the equation of time -- about -14.2 min in mid-February, +3.7 in
///   mid-May, -6.5 in late July and +16.4 in early November, the standard published figures
///   for the analemma;
/// - the length of the equinox day at the equator, which is twelve hours plus twice the
///   refraction allowance and therefore a few minutes over twelve rather than twelve exactly;
/// - Denver's longest day, published as 14 h 59 m.
///
/// Plus the transit's **scaling**, which is the only non-circular way to pin
/// `720 - longitude * 4 - E` from outside: four minutes of time per degree of longitude, east
/// earlier. And the hemisphere ordering at two latitudes nothing else in this file evaluates,
/// which a declination sign error would reverse.
#[test]
fn the_oracle_reproduces_published_astronomical_quantities() {
    // The analemma's four extremes, each within a fifth of a minute. The tolerance is the
    // precision of the published figures and not of the oracle, whose own values here are
    // -14.2262, +3.6404, -6.5628 and +16.4922.
    for (date, published) in [
        ((2026, 2, 11), -14.2),
        ((2026, 5, 14), 3.7),
        ((2026, 7, 26), -6.5),
        ((2026, 11, 3), 16.4),
    ] {
        let (_, _, e) = meeus::coordinates(meeus::julian_day(date) + 0.5);
        assert!(
            (e - published).abs() < 0.2,
            "{date:?}: the equation of time is published as about {published} minutes; the \
             oracle says {e:.4}"
        );
    }

    // The transit, by its scaling with longitude. Each degree east is four minutes earlier,
    // so 105.5° west is 422 minutes later and 25° east is 100 minutes earlier. The residual
    // is the equation of time's own drift across the half-day between the two limbs, which
    // is why the tolerance is hundredths rather than thousandths.
    let at = |lon: f64| meeus::transit((2026, 11, 3), 39.9, lon).expect("not polar");
    let greenwich = at(0.0);
    assert!(
        ((at(-105.5) - greenwich) - 422.0).abs() < 0.05,
        "105.5° west should put the transit 422 minutes later; it moved {:.4}",
        at(-105.5) - greenwich
    );
    assert!(
        ((at(25.0) - greenwich) + 100.0).abs() < 0.05,
        "25° east should put the transit 100 minutes earlier; it moved {:.4}",
        at(25.0) - greenwich
    );
    // And its absolute value, against the equation of time the block above pinned: solar
    // noon at Greenwich is 12:00 UT less the equation of time at that instant. The half
    // minute of slack is the same two-limb drift -- the midpoint of two transits evaluated
    // five hours apart is not either of them.
    let (_, _, e) = meeus::coordinates(meeus::julian_day((2026, 11, 3)) + greenwich / 1440.0);
    assert!(
        (720.0 - greenwich - e).abs() < 0.5,
        "the transit at Greenwich should be 720 minus the equation of time ({e:.4}); it is \
         {greenwich:.4}, leaving {:.4} unexplained",
        720.0 - greenwich - e
    );

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

    // Two latitudes nothing else here evaluates, and the thing about them that is physics
    // rather than a remembered figure: which solstice is the long one, and by hours.
    for (lat, lon, long_day, short_day) in [
        (60.0, 25.0, (2026, 6, 21), (2026, 12, 21)),
        (-33.9, 151.2, (2026, 12, 21), (2026, 6, 21)),
    ] {
        let len = |d| {
            let (r, s) = meeus::sunrise_sunset(d, lat, lon, STANDARD_ALTITUDE).expect("not polar");
            s - r
        };
        assert!(
            len(long_day) > len(short_day) + 120.0,
            "at latitude {lat} the {long_day:?} day must be the long one by hours, not \
             {:.0} against {:.0} minutes -- a declination sign error reverses exactly this",
            len(long_day),
            len(short_day)
        );
        assert!(
            len(long_day) > 840.0 && len(short_day) < 600.0,
            "and the two should be a summer and a winter at latitude {lat}: {:.0} and {:.0} \
             minutes",
            len(long_day),
            len(short_day)
        );
    }

    // The walk through the year, which a sign error in the declination would reverse.
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
            "sunrise at 39.9 N should be later every day from June to December; {date} broke it"
        );
        previous = rise;
        date = date.succ_opt().expect("a next day");
    }

    // Polar day and polar night have no event, which is the branch the sweep must never
    // reach and the oracle must not invent an answer for.
    assert!(meeus::sunrise_sunset((2026, 6, 21), 85.0, 0.0, STANDARD_ALTITUDE).is_none());
    assert!(meeus::sunrise_sunset((2026, 12, 21), 85.0, 0.0, STANDARD_ALTITUDE).is_none());
}

/// **The iteration converges, and it is load-bearing.**
///
/// [`meeus::limb`] refines the solar position towards the event instant rather than
/// evaluating it once at noon, which is one of the two ways this oracle differs in kind from
/// the shipped formula. Both halves are asserted, because only the pair says anything: four
/// passes have to agree with twelve, *and* one pass has to disagree with twelve, or the
/// refinement is decoration whose removal nothing would notice.
#[test]
fn the_oracles_iteration_is_load_bearing() {
    let grid = [
        (0.0, 0.0),
        (39.9, -105.5),
        (60.0, 25.0),
        (85.0, -105.5),
        (-60.0, 0.0),
        (-33.9, 151.2),
    ];
    let dates = [
        (2026, 1, 5),
        (2026, 2, 14),
        (2026, 3, 20),
        (2026, 6, 21),
        (2026, 9, 23),
        (2026, 10, 28),
        (2026, 12, 21),
    ];
    let mut converged = 0.0_f64;
    let mut one_pass = 0.0_f64;
    let mut compared = 0;
    for (lat, lon) in grid {
        for date in dates {
            for which in [meeus::Limb::Rise, meeus::Limb::Set] {
                let Some(settled) = meeus::limb(date, lat, lon, STANDARD_ALTITUDE, which, 12)
                else {
                    continue;
                };
                let four = meeus::limb(date, lat, lon, STANDARD_ALTITUDE, which, PASSES)
                    .expect("a limb that settles at twelve passes settles at four");
                let once = meeus::limb(date, lat, lon, STANDARD_ALTITUDE, which, 1)
                    .expect("one pass always returns something");
                converged = converged.max((four - settled).abs());
                one_pass = one_pass.max((once - settled).abs());
                compared += 1;
            }
        }
    }
    assert!(
        compared > 60,
        "the grid should reach most of its 84 combinations; only {compared} were not polar"
    );
    assert!(
        converged < 0.001,
        "{PASSES} passes should be within a thousandth of a minute of twelve; the worst was \
         {converged:.6}"
    );
    assert!(
        one_pass > 1.0,
        "and ONE pass should be minutes away somewhere, or the refinement is decoration: the \
         worst single-pass error was {one_pass:.4} minutes. Measured at 5.2 minutes, at 85 N \
         on the March equinox."
    );
}

/// **The comparison bites.** W1 D-10: a differ that cannot be made to differ proves nothing,
/// and that applies to this one as much as to the conformance replay.
///
/// Each perturbation is **larger than the band it has to escape**, which is the point: the
/// sunrise band spans six minute-values, so a one-degree nudge of longitude is four minutes
/// and would stay inside it. A control smaller than the tolerance it tests is not a control.
///
/// 1. move the station three degrees east -- twelve minutes of time;
/// 2. ask the oracle for a date a month away, so the comparison is keyed on the date and not
///    merely on the station;
/// 3. tilt the oracle's horizon by two degrees, which is what says the oracle is being
///    evaluated at all rather than short-circuited.
///
/// The formatter's round trip is checked first, because every number in this file is a
/// difference of two parsed clock strings.
#[test]
fn the_comparison_bites() {
    let station = station_named("davis-full");
    let date = NaiveDate::from_ymd_opt(2026, 10, 28).expect("a real date");
    let at = local_noon(&station, date);
    let zone: chrono_tz::Tz = station.zone.parse().expect("an IANA name");

    // Every minute of the day, through the contract's formatter and back.
    let shift = at.with_timezone(&zone).offset().fix().local_minus_utc() / 60;
    for minute in 0..1440 {
        let rendered = sun::format_sun_time(minute, at, &zone);
        assert_eq!(
            parse_clock(&rendered),
            (minute + shift).rem_euclid(1440),
            "minute {minute} rendered as `{rendered}` and did not parse back to itself"
        );
    }

    let (rise, set) = served(&station, at);
    let honest = oracle(&station, date, at, TEXTBOOK.altitude);
    let in_rise = |d: i32| (TEXTBOOK.sunrise.0..=TEXTBOOK.sunrise.1).contains(&d);
    let in_set = |d: i32| (TEXTBOOK.sunset.0..=TEXTBOOK.sunset.1).contains(&d);
    assert!(
        in_rise(divergence(&rise, &honest.0)) && in_set(divergence(&set, &honest.1)),
        "the unperturbed comparison has to be inside the bands before a perturbation leaving \
         them means anything"
    );

    let mut moved = station.clone();
    moved.longitude += 3.0;
    let shifted = oracle(&moved, date, at, TEXTBOOK.altitude);
    assert!(
        !in_rise(divergence(&rise, &shifted.0)),
        "three degrees of longitude is twelve minutes of time and the comparison did not \
         notice: {rise} vs {}",
        shifted.0
    );

    let next_month = date + chrono::Duration::days(30);
    let wrong_day = oracle(&station, next_month, at, TEXTBOOK.altitude);
    assert!(
        !in_set(divergence(&set, &wrong_day.1)),
        "the comparison is not keyed on the date -- a month of November moves sunset by half \
         an hour: {set} vs {}",
        wrong_day.1
    );

    let tilted = oracle(&station, date, at, TEXTBOOK.altitude - 2.0);
    assert!(
        !in_rise(divergence(&rise, &tilted.0)),
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
    let dir = repo_root().join("contract/fixtures");
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
        // A JSON body only: `latest-msgpack` carries the same values in a binary one, and
        // `the_served_strings_are_the_stored_go_tables`'s declared set records it as excluded
        // rather than leaving the exclusion to this filter.
        let Some(body) = fixture["response"]["body"].as_str() else {
            continue;
        };
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
