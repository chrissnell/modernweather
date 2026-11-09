//! Sunrise, sunset and the four lunar values -- functions of `(date, latitude, longitude)`
//! and not of a reading at all.
//!
//! **Served here rather than stored** (W2a Decision 36, and `mw_pipeline::enrich`'s module
//! documentation says the same from the other side): R1 moved the *derived columns* to
//! ingest, and these are not derived from a reading, so there is nothing to derive at
//! ingest and nothing to store. The six keys are computed per request from the station's
//! configured coordinates.
//!
//! # Why the arithmetic is the Go arithmetic
//!
//! `/latest`'s `sunrise` and `sunset` are strings a client renders, and the four lunar keys
//! drive an icon's rotation. Nothing compares them numerically, so the risk is not
//! precision -- it is a phase name that changes, or a rotation that flips a crescent. So
//! the constants, the term sets and the guard conditions below are
//! `remoteweather`'s `pkg/solar/sunrise.go`, `pkg/solar/ineichen-perez.go`'s
//! `equationOfTime` and `pkg/lunar/lunar.go` at `c3ec9f7`, and the tests assert against
//! values produced by **running** that Go code.
//!
//! # One deviation, and it is the sunrise table rather than the formula
//!
//! The Go handler does not compute sunrise at all: it reads a **precomputed `sun_times`
//! table** through `configProvider.GetSunTimes(stationName, dayOfYear)`, populated by a
//! provisioning step. The new configuration schema has no such table and no work item
//! creates one, so this computes the same formula directly -- which is what the
//! provisioner computed to fill that table in the first place
//! (`pkg/solar.CalculateSunriseSunset` is the function both call). The observable
//! difference is that a station whose row was missing lost both keys in Go and does not
//! here; a station with **no coordinates** loses them in both, because the formula has no
//! input.

use std::f64::consts::PI;

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};

/// The average length of the lunar cycle, in days (`pkg/lunar`'s `SynodicMonth`).
const SYNODIC_MONTH: f64 = 29.530_588_853;

/// Sunrise and sunset as minutes from midnight **UTC** for one day at one position.
///
/// `None` for polar day or polar night -- the Go function returns `(-1, -1)` and the
/// handler skips both keys, which is the same outcome with a name.
///
/// The day of year is the station's, which is the caller's to decide: a station in
/// `Pacific/Auckland` is a day ahead of UTC for part of its own day, and taking the day
/// from UTC would shift its sunrise by a day near midnight.
pub fn sunrise_sunset(day_of_year: u32, latitude: f64, longitude: f64) -> Option<(i32, i32)> {
    // Solar declination, from the same expression `pkg/solar/asce.go` uses.
    let doy = f64::from(day_of_year);
    let inner = (356.6 + 0.9856 * doy).to_radians();
    let outer = (278.97 + 0.9856 * doy + 1.9165 * inner.sin()).to_radians();
    let declination = (0.397_85 * outer.sin()).asin();

    // At sunrise and sunset the sun is on the horizon: cos(H) = -tan(lat) tan(dec).
    let cos_h = -latitude.to_radians().tan() * declination.tan();
    if !(-1.0..=1.0).contains(&cos_h) {
        // The sun never sets, or never rises.
        return None;
    }
    let hour_angle_minutes = cos_h.acos().to_degrees() / 15.0 * 60.0;

    // Solar noon in UTC minutes. Each degree of longitude is four minutes, east earlier.
    let reference = reference_noon(day_of_year);
    let solar_noon = 720.0 - longitude * 4.0 - equation_of_time(reference);

    let wrap = |m: f64| (m + 1440.0).rem_euclid(1440.0);
    Some((
        wrap(solar_noon - hour_angle_minutes).round() as i32,
        wrap(solar_noon + hour_angle_minutes).round() as i32,
    ))
}

/// Noon UTC on `day_of_year` of the current year, which is what the equation of time is
/// evaluated at.
///
/// The Go function builds it as `Jan 1 12:00 UTC + (dayOfYear - 1) days` of
/// `time.Now().Year()`, and this is that. A year is needed because the equation of time is
/// a function of the Julian date; the difference one year makes to it is under a second of
/// time, so the reference year is not load-bearing and is taken from the clock for the same
/// reason Go takes it.
fn reference_noon(day_of_year: u32) -> DateTime<Utc> {
    let year = Utc::now().year();
    let jan1 = Utc
        .with_ymd_and_hms(year, 1, 1, 12, 0, 0)
        .single()
        .unwrap_or_else(Utc::now);
    jan1 + chrono::Duration::days(i64::from(day_of_year) - 1)
}

/// The equation of time in minutes (`pkg/solar/ineichen-perez.go`).
fn equation_of_time(t: DateTime<Utc>) -> f64 {
    let jd = julian_day(t);
    let c = (jd - 2_451_545.0) / 36_525.0;
    let l0 = normalise_degrees(280.466_46 + c * (36_000.769_83 + c * 0.000_303_2));
    let m = normalise_degrees(357.529_11 + c * (35_999.050_29 - c * 0.000_153_7));
    let e = 0.016_708_634 - c * (0.000_042_037 + c * 0.000_000_126_7);
    let eps0 =
        23.0 + (26.0 + (21.448 - c * (46.815 + c * (0.000_59 - c * 0.001_813))) / 60.0) / 60.0;
    let y = (eps0.to_radians() / 2.0).tan() * (eps0.to_radians() / 2.0).tan();
    (y * (2.0 * l0).to_radians().sin() - 2.0 * e * m.to_radians().sin()
        + 4.0 * e * y * m.to_radians().sin() * (2.0 * l0).to_radians().cos()
        - 0.5 * y * y * (4.0 * l0).to_radians().sin()
        - 1.25 * e * e * (2.0 * m).to_radians().sin())
    .to_degrees()
        * 4.0
}

/// `sunrise`/`sunset` as the contract's string: a 12-hour clock in the station's zone.
///
/// `"7:30 AM"` -- Go's `3:04 PM` layout, which is an hour with no leading zero, a
/// zero-padded minute and an upper-case meridiem.
///
/// **The UTC minute may fall on an adjacent UTC date.** Sunset at 19:00 in
/// `America/Los_Angeles` is 02:00 UTC the next day, so the Go function searches the local
/// date and its two UTC neighbours for the one that maps back to the expected local date.
/// This does the same, in the same order, which is also what makes it right across a DST
/// transition.
pub fn format_sun_time<Tz: TimeZone>(utc_minutes: i32, on: DateTime<Utc>, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if utc_minutes < 0 {
        return String::new();
    }
    // **The hour may be 24, and that is a real value rather than a guard to add.**
    // `sunrise_sunset` returns `wrap(...).round()` over `[0, 1440)`, so a true event in the
    // last half-minute before midnight UTC rounds to exactly 1440. Go's
    // `time.Date(y, m, d, 24, minutes, …)` normalises that into the next day; chrono's
    // `and_hms_opt(24, …)` returns `None`, so all three offsets below used to `continue`,
    // the fallback returned the same `None`, and `format_sun_time` produced the empty
    // string -- which `skip_serializing_if = "String::is_empty"` then turns into an **absent
    // `sunset` key**. A scan over latitude 39.9 and every longitude found 896 (day,
    // longitude) pairs that land on it, so for a fixed station it is roughly twice a year.
    //
    // Carried into the date rather than clamped, which is what `time.Date` does.
    let carry = i64::from(utc_minutes / 1440);
    let hours = u32::try_from((utc_minutes / 60) % 24).unwrap_or(0);
    let minutes = u32::try_from(utc_minutes % 60).unwrap_or(0);
    let local_date = on.with_timezone(zone).date_naive();
    for offset in [0_i64, 1, -1] {
        let Some(day) = local_date.checked_add_signed(chrono::Duration::days(offset + carry))
        else {
            continue;
        };
        let Some(naive) = day.and_hms_opt(hours, minutes, 0) else {
            continue;
        };
        let utc = naive.and_utc();
        let local = utc.with_timezone(zone);
        // Compared against the date the event belongs to, which the carry does not move:
        // Go's loop tests `local.Date() == (y, m, d)` with `d` the un-normalised local day.
        if local.date_naive() == local_date {
            return twelve_hour(local.hour(), local.minute());
        }
    }
    // Unreachable for a valid input: one of the three offsets always maps back, because a
    // zone's offset is within a day of UTC. The Go function has the same fallback.
    local_date
        .checked_add_signed(chrono::Duration::days(carry))
        .and_then(|day| day.and_hms_opt(hours, minutes, 0))
        .map(|n| {
            let local = n.and_utc().with_timezone(zone);
            twelve_hour(local.hour(), local.minute())
        })
        .unwrap_or_default()
}

/// Go's `3:04 PM`.
fn twelve_hour(hour24: u32, minute: u32) -> String {
    let meridiem = if hour24 < 12 { "AM" } else { "PM" };
    let hour = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    format!("{hour}:{minute:02} {meridiem}")
}

/// What `/latest`'s four lunar keys carry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moon {
    /// `moonPhaseName` -- one of the eight names `pkg/lunar`'s `phaseName` returns.
    pub phase_name: &'static str,
    /// `moonIllumination` -- the illuminated fraction, `[0, 1]`.
    ///
    /// **`omitempty`, so a new moon really does drop the key.** That is Go's behaviour and
    /// it ships; it is noted here because it is the one value on this surface whose
    /// legitimate value is the one `omitempty` removes.
    pub illumination: f64,
    /// `moonAge` -- days since the new moon.
    pub age_days: f64,
    /// `moonCrescentAngle` -- the CSS rotation to apply to the icon, degrees clockwise.
    pub crescent_angle: f64,
}

/// The moon phase for one instant, and the crescent rotation for one observer.
///
/// `pkg/lunar`'s `Calculate` and `CalculateCrescentAngle` in one call, because `/latest`
/// needs both for the same instant and the Go handler calls them one after the other.
///
/// `position` is the observer's `(latitude, longitude)` in degrees, east positive. `None`
/// -- a station with no coordinates -- skips the parallactic correction, which is what the
/// Go function does for `(0, 0)`.
pub fn moon(t: DateTime<Utc>, position: Option<(f64, f64)>) -> Moon {
    let jd = julian_day(t);
    let c = julian_centuries(jd);

    let lambda_sun = sun_ecliptic_longitude(c);
    let lambda_moon = moon_ecliptic_longitude(c);
    let elongation = normalise_degrees(lambda_moon - lambda_sun);
    let illumination = (1.0 - elongation.to_radians().cos()) / 2.0;
    let is_waxing = elongation < 180.0;

    Moon {
        phase_name: phase_name(illumination, is_waxing),
        illumination,
        age_days: elongation / 360.0 * SYNODIC_MONTH,
        crescent_angle: crescent_rotation(jd, c, position),
    }
}

/// `pkg/lunar`'s `phaseName`: eight names from the illumination and the direction.
fn phase_name(illumination: f64, is_waxing: bool) -> &'static str {
    if illumination < 0.01 {
        "New Moon"
    } else if illumination > 0.99 {
        "Full Moon"
    } else if (0.49..=0.51).contains(&illumination) {
        if is_waxing {
            "First Quarter"
        } else {
            "Third Quarter"
        }
    } else if illumination < 0.50 {
        if is_waxing {
            "Waxing Crescent"
        } else {
            "Waning Crescent"
        }
    } else if is_waxing {
        "Waxing Gibbous"
    } else {
        "Waning Gibbous"
    }
}

/// `pkg/lunar`'s `CalculateCrescentAngle(...).Rotation`.
fn crescent_rotation(jd: f64, c: f64, position: Option<(f64, f64)>) -> f64 {
    let eps = obliquity(c);
    let (ra_sun, dec_sun) = ecliptic_to_equatorial(sun_ecliptic_longitude(c), 0.0, eps);
    let (ra_moon, dec_moon) =
        ecliptic_to_equatorial(moon_ecliptic_longitude(c), moon_ecliptic_latitude(c), eps);

    // The position angle of the bright limb (Meeus eq. 48.5), then the terminator at a
    // right angle to it.
    let delta_ra = ra_sun - ra_moon;
    let chi_y = dec_sun.cos() * delta_ra.sin();
    let chi_x = dec_sun.sin() * dec_moon.cos() - dec_sun.cos() * dec_moon.sin() * delta_ra.cos();
    let chi = normalise_radians(chi_y.atan2(chi_x));
    let theta = normalise_radians(chi + PI / 2.0);

    // The parallactic angle, which rotates celestial north onto the observer's zenith. The
    // Go function skips it for `(0, 0)` -- the sentinel its caller passes for a station
    // with no coordinates -- and so does this, for a station that really has none.
    let Some((lat, lon)) = position else {
        return -theta.to_degrees();
    };
    if lat == 0.0 && lon == 0.0 {
        return -theta.to_degrees();
    }
    let phi = lat.to_radians();
    let hour_angle = local_sidereal_time(jd, lon) - ra_moon;
    let q = hour_angle
        .sin()
        .atan2(phi.tan() * dec_moon.cos() - dec_moon.sin() * hour_angle.cos());
    // CSS clockwise rotation with zero pointing up, so the local terminator is negated.
    -normalise_radians(theta - q).to_degrees()
}

/// The Julian Day for a UTC instant.
fn julian_day(t: DateTime<Utc>) -> f64 {
    2_440_587.5 + t.timestamp() as f64 / 86_400.0
}

/// Julian centuries since J2000.0.
fn julian_centuries(jd: f64) -> f64 {
    (jd - 2_451_545.0) / 36_525.0
}

/// An angle wrapped into `[0, 360)`.
fn normalise_degrees(angle: f64) -> f64 {
    angle.rem_euclid(360.0)
}

/// An angle wrapped into `[0, 2pi)`.
fn normalise_radians(angle: f64) -> f64 {
    angle.rem_euclid(2.0 * PI)
}

/// The Sun's ecliptic longitude in degrees.
fn sun_ecliptic_longitude(c: f64) -> f64 {
    let l0 = 280.466_46 + 36_000.769_83 * c + 0.000_303_2 * c * c;
    let m = 357.529_11 + 35_999.050_29 * c - 0.000_153_7 * c * c;
    let m_rad = normalise_degrees(m).to_radians();
    let centre = (1.914_602 - 0.004_817 * c - 0.000_014 * c * c) * m_rad.sin()
        + (0.019_993 - 0.000_101 * c) * (2.0 * m_rad).sin()
        + 0.000_289 * (3.0 * m_rad).sin();
    normalise_degrees(l0 + centre)
}

/// The Moon's ecliptic longitude in degrees, with `pkg/lunar`'s five dominant terms.
fn moon_ecliptic_longitude(c: f64) -> f64 {
    let l = 218.316_447_7 + 481_267.881_234_21 * c - 0.001_578_6 * c * c + c * c * c / 538_841.0
        - c * c * c * c / 65_194_000.0;
    let d = moon_mean_elongation(c);
    let mp = moon_mean_anomaly(c);
    let d_rad = normalise_degrees(d).to_radians();
    let mp_rad = normalise_degrees(mp).to_radians();
    normalise_degrees(
        l + 6.289 * mp_rad.sin()
            + 1.274 * (2.0 * d_rad - mp_rad).sin()
            + 0.658 * (2.0 * d_rad).sin()
            + 0.214 * (2.0 * mp_rad).sin()
            + 0.110 * d_rad.sin(),
    )
}

/// The Moon's ecliptic latitude in degrees (Meeus table 47.B, dominant terms).
fn moon_ecliptic_latitude(c: f64) -> f64 {
    let f = 93.272_095_0 + 483_202.017_523_3 * c - 0.003_653_9 * c * c - c * c * c / 3_526_000.0
        + c * c * c * c / 863_310_000.0;
    let d = moon_mean_elongation(c);
    let mp = moon_mean_anomaly(c);
    let f_rad = normalise_degrees(f).to_radians();
    let d_rad = normalise_degrees(d).to_radians();
    let mp_rad = normalise_degrees(mp).to_radians();
    5.128 * f_rad.sin()
        + 0.280_6 * (mp_rad + f_rad).sin()
        + 0.277_7 * (mp_rad - f_rad).sin()
        + 0.173_2 * (2.0 * d_rad - f_rad).sin()
}

fn moon_mean_elongation(c: f64) -> f64 {
    297.850_192_1 + 445_267.111_403_4 * c - 0.001_881_9 * c * c + c * c * c / 545_868.0
        - c * c * c * c / 113_065_000.0
}

fn moon_mean_anomaly(c: f64) -> f64 {
    134.963_396_4 + 477_198.867_505_5 * c + 0.008_741_4 * c * c + c * c * c / 69_699.0
        - c * c * c * c / 14_712_000.0
}

/// The mean obliquity of the ecliptic in degrees (IAU).
fn obliquity(c: f64) -> f64 {
    23.439_291_111 - 0.013_004_167 * c - 0.000_001_64 * c * c + 0.000_000_504 * c * c * c
}

/// Ecliptic `(lambda, beta)` in degrees to equatorial `(ra, dec)` in radians.
fn ecliptic_to_equatorial(lambda: f64, beta: f64, epsilon: f64) -> (f64, f64) {
    let lam = lambda.to_radians();
    let bet = beta.to_radians();
    let eps = epsilon.to_radians();
    let dec = (bet.sin() * eps.cos() + bet.cos() * eps.sin() * lam.sin()).asin();
    let y = lam.sin() * eps.cos() - bet.tan() * eps.sin();
    let ra = normalise_radians(y.atan2(lam.cos()));
    (ra, dec)
}

/// Greenwich mean sidereal time in degrees (IAU 1982, Meeus eq. 12.4).
fn greenwich_mean_sidereal_time(jd: f64) -> f64 {
    let jd0 = (jd - 0.5).floor() + 0.5;
    let c = (jd0 - 2_451_545.0) / 36_525.0;
    let mut gmst =
        6.697_374_558 + 2_400.051_336_9 * c + 0.000_025_862_2 * c * c - 1.7222e-9 * c * c * c;
    gmst += 1.002_737_909_35 * ((jd - jd0) * 24.0);
    gmst.rem_euclid(24.0) * 15.0
}

/// Local sidereal time in radians.
fn local_sidereal_time(jd: f64, longitude: f64) -> f64 {
    normalise_degrees(greenwich_mean_sidereal_time(jd) + longitude).to_radians()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("a real instant")
    }

    /// **Cross-checked against W1's own capture, which is a stronger oracle than a value
    /// this code produced.**
    ///
    /// `ci/conformance/README.md` records the captured `/latest` strings for the rig's
    /// Davis station as `"7:30 AM"` and `"6:02 PM"`, from the real Go server at
    /// 39.9 N, 105.5 W on 2026-10-28 (day 301) with the runner in `UTC-6`. This formula
    /// returns 810 and 2 minutes from midnight UTC, which render as exactly those two
    /// strings at that offset -- so the arithmetic agrees with the shipped server on the
    /// one day the contract has evidence for.
    #[test]
    fn sunrise_and_sunset_are_the_go_values() {
        // 39.9 N, 105.5 W -- `davis-full` in ci/conformance/config-seed.sql.
        let (rise, set) = sunrise_sunset(301, 39.900, -105.500).expect("not polar");
        assert_eq!((rise, set), (810, 2));
        let day = at(2026, 10, 28, 18, 0);
        let mountain: chrono_tz::Tz = "America/Denver".parse().expect("an IANA name");
        assert_eq!(
            format_sun_time(rise, day, &mountain),
            "7:30 AM",
            "the string W1 captured from the Go server"
        );
        assert_eq!(
            format_sun_time(set, day, &mountain),
            "6:02 PM",
            "and the sunset it captured, which is 00:02 UTC the following day"
        );

        // Midsummer at the same place: a longer day. Both values are minutes from midnight
        // **UTC**, and at 105.5 W the sunset is past midnight UTC -- so the length is the
        // wrapped difference and not a subtraction, which is the same arithmetic the
        // formula's own `rem_euclid` does.
        let (rise, set) = sunrise_sunset(172, 39.900, -105.500).expect("not polar");
        let length = (set - rise).rem_euclid(1440);
        assert!(
            (870..900).contains(&length),
            "a midsummer day at 39.9 N is about 14 h 45 m, got {length} minutes"
        );
    }

    /// Polar day and polar night have no answer, which the Go function reports as
    /// `(-1, -1)` and this reports as `None` -- the handler drops both keys either way.
    #[test]
    fn a_polar_day_has_no_sunrise() {
        assert!(sunrise_sunset(172, 85.0, 0.0).is_none(), "polar day");
        assert!(sunrise_sunset(355, 85.0, 0.0).is_none(), "polar night");
        assert!(
            sunrise_sunset(172, -85.0, 0.0).is_none(),
            "polar night, south"
        );
    }

    /// The contract's string: a 12-hour clock with no leading zero on the hour.
    #[test]
    fn a_sun_time_is_formatted_the_way_go_formats_it() {
        let day = at(2026, 10, 28, 12, 0);
        assert_eq!(format_sun_time(816, day, &Utc), "1:36 PM");
        assert_eq!(format_sun_time(0, day, &Utc), "12:00 AM");
        assert_eq!(format_sun_time(720, day, &Utc), "12:00 PM");
        assert_eq!(format_sun_time(60 * 13 + 5, day, &Utc), "1:05 PM");
        assert_eq!(format_sun_time(-1, day, &Utc), "", "polar: no key at all");
    }

    /// **Minute 1440, which `sunrise_sunset` really returns.** It is `wrap(...).round()`
    /// over `[0, 1440)`, so a true event in the last half-minute before midnight UTC rounds
    /// to exactly 1440 and the hour is 24.
    ///
    /// `chrono`'s `and_hms_opt(24, …)` is `None`, so this produced the empty string -- and
    /// `skip_serializing_if = "String::is_empty"` turns that into an **absent `sunset`
    /// key**, on a frozen endpoint, for a whole day. Go's `time.Date(y, m, d, 24, …)`
    /// normalises into the next day instead.
    ///
    /// Every expectation here is `solar.FormatSunTime` at `c3ec9f7`, run against the same
    /// inputs.
    #[test]
    fn the_last_minute_of_the_utc_day_carries_into_the_next_one() {
        let denver: chrono_tz::Tz = "America/Denver".parse().expect("an IANA name");
        let la: chrono_tz::Tz = "America/Los_Angeles".parse().expect("an IANA name");
        let day = at(2026, 1, 1, 12, 0);

        assert_eq!(format_sun_time(1440, day, &denver), "5:00 PM");
        assert_eq!(format_sun_time(1440, day, &la), "4:00 PM");
        assert_eq!(format_sun_time(1440, day, &Utc), "12:00 AM");
        // The neighbours, so the carry cannot be the only thing that works.
        assert_eq!(format_sun_time(1439, day, &denver), "4:59 PM");
        assert_eq!(format_sun_time(0, day, &denver), "5:00 PM");
        assert_eq!(format_sun_time(1, day, &denver), "5:01 PM");
        assert_eq!(format_sun_time(1380, day, &denver), "4:00 PM");

        // A DST transition date, and 29 February: the two cases the carry could have got
        // wrong by arithmetic rather than by type.
        let dst = at(2026, 3, 8, 12, 0);
        assert_eq!(format_sun_time(1440, dst, &denver), "6:00 PM");
        assert_eq!(format_sun_time(800, dst, &denver), "7:20 AM");
        assert_eq!(format_sun_time(1300, dst, &denver), "3:40 PM");
        assert_eq!(
            format_sun_time(1440, at(2028, 2, 29, 12, 0), &denver),
            "5:00 PM"
        );
    }

    /// The boundary and the pole, as values rather than as `is_none`.
    ///
    /// Every expectation is `solar.CalculateSunriseSunset` at `c3ec9f7`, run here. The
    /// Arctic Circle in January still has a short day; latitude 90 is `None` because
    /// `tan(PI/2)` is finite rather than `NaN`, which is the one way this could have been an
    /// unwrap rather than a branch.
    ///
    /// `(1, 39.9, -109.9)` is the pair that motivated the carry in `format_sun_time`: its
    /// sunset is minute **1440**.
    #[test]
    fn the_arctic_circle_still_has_a_sunrise_and_the_pole_does_not() {
        assert_eq!(sunrise_sunset(1, 66.5, 0.0), Some((677, 770)));
        assert_eq!(sunrise_sunset(1, 39.9, -109.9), Some((887, 1440)));
        assert_eq!(sunrise_sunset(172, 0.0, 0.0), Some((362, 1082)));
        assert!(sunrise_sunset(172, 90.0, 0.0).is_none(), "the north pole");
        assert!(sunrise_sunset(355, 90.0, 0.0).is_none(), "and in winter");
        assert!(
            sunrise_sunset(172, -90.0, 0.0).is_none(),
            "and the south pole"
        );
    }

    /// The case the three-offset search exists for: sunset in a western zone falls on the
    /// **next** UTC date, and naive formatting would report the wrong local day.
    #[test]
    fn a_sun_time_lands_on_the_local_date_even_when_its_utc_date_differs() {
        let zone: chrono_tz::Tz = "America/Denver".parse().expect("an IANA name");
        // 00:07 UTC is 18:07 the previous day in Denver. The local date is 2026-10-28, so
        // the UTC instant that maps back to it is 2026-10-29T00:07Z.
        let rendered = format_sun_time(7, at(2026, 10, 28, 20, 0), &zone);
        assert_eq!(rendered, "6:07 PM");
    }

    /// **Against the numbers `pkg/lunar` produces, not against bounds.**
    ///
    /// This test asserted `illumination ∈ [0,1]`, `age ∈ [0, SYNODIC_MONTH)` and
    /// `rotation ∈ [-360,360]` -- properties that hold by construction -- while its name
    /// claimed an oracle it did not have. A term dropped from any of the three series would
    /// have passed it, in the file most likely to break that way, and the conformance suite
    /// cannot see it either: the data-bearing `/latest` fixtures assert shape, key order and
    /// numeric convention, never values.
    ///
    /// The three expectations are `lunar.Calculate` and `lunar.CalculateCrescentAngle` at
    /// `c3ec9f7`, run against this instant and narrowed to `f32` the way `handlers.go:385`
    /// does. Note which field each legacy key reads: `moonIllumination` and `moonAge` come
    /// from `Calculate`, and `moonCrescentAngle` from `CalculateCrescentAngle().Rotation` --
    /// whose own `Illumination` is a different number (0.9133 here) and is not served.
    #[test]
    fn the_moon_phase_is_the_go_value() {
        // 2026-10-28T16:56:42Z, the instant `latest-full-complement` was captured at.
        let t = Utc
            .with_ymd_and_hms(2026, 10, 28, 16, 56, 42)
            .single()
            .expect("a real instant");
        let m = moon(t, Some((39.900, -105.500)));
        assert_eq!(m.phase_name, "Waning Gibbous");
        assert_eq!((m.illumination as f32).to_string(), "0.9148452");
        assert_eq!((m.age_days as f32).to_string(), "17.548801");
        assert_eq!((m.crescent_angle as f32).to_string(), "-121.45631");

        // The geocentric fallback, which is what the Go function does for its `(0, 0)`
        // sentinel and what a station with no coordinates gets.
        let geocentric = moon(t, None);
        assert_eq!((geocentric.crescent_angle as f32).to_string(), "-163.63496");
        // The phase name follows the direction, and the two have to agree.
        assert!(m.age_days > SYNODIC_MONTH / 2.0, "waning at {}", m.age_days);
    }

    /// The eight names, driven from the two inputs that choose them.
    #[test]
    fn the_eight_phase_names_are_the_go_table() {
        assert_eq!(phase_name(0.0, true), "New Moon");
        assert_eq!(phase_name(0.005, false), "New Moon");
        assert_eq!(phase_name(1.0, false), "Full Moon");
        assert_eq!(phase_name(0.5, true), "First Quarter");
        assert_eq!(phase_name(0.5, false), "Third Quarter");
        assert_eq!(phase_name(0.25, true), "Waxing Crescent");
        assert_eq!(phase_name(0.25, false), "Waning Crescent");
        assert_eq!(phase_name(0.75, true), "Waxing Gibbous");
        assert_eq!(phase_name(0.75, false), "Waning Gibbous");
    }

    /// A full synodic cycle: the phase name advances through all eight in order and the age
    /// is monotonic inside one cycle. This is the check that would catch a sign error in a
    /// term, which a single-instant assertion cannot.
    #[test]
    fn the_phase_advances_through_the_cycle() {
        let start = at(2026, 1, 1, 0, 0);
        let mut seen = Vec::new();
        for day in 0..30 {
            let m = moon(start + chrono::Duration::days(day), None);
            if seen.last().map(|n| *n != m.phase_name).unwrap_or(true) {
                seen.push(m.phase_name);
            }
        }
        assert!(
            seen.len() >= 6,
            "a 30-day walk should pass through most of the eight phases, saw {seen:?}"
        );
        assert!(
            seen.contains(&"Full Moon") || seen.contains(&"Waxing Gibbous"),
            "{seen:?}"
        );
    }

    /// The reference instant the equation of time is evaluated at is noon on the right day.
    #[test]
    fn the_equation_of_time_reference_is_noon_on_that_day_of_year() {
        let t = reference_noon(1);
        assert_eq!((t.month(), t.day(), t.hour()), (1, 1, 12));
        let t = reference_noon(301);
        assert_eq!((t.month(), t.day(), t.hour()), (10, 28, 12));
    }
}
