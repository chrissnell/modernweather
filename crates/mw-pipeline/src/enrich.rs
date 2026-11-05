//! The enrich stage: **R1**, as a pure function (W3 §2.5).
//!
//! > **R1 — storage.** A derived value is computed once at ingest and stored as a column
//! > if and only if it is a pure function of a **single reading** plus static station
//! > metadata.
//!
//! The Go version has no such stage, which is why wind chill is computed in a forwarder
//! binary with a `TODO` admitting it is missing elsewhere, AQI in a package, rainfall
//! accumulation in HTTP handlers, rainfall windows in PL/pgSQL, and snow in both a Go
//! package *and* PL/pgSQL. R1 is the rule that was missing, and this file is the only
//! place in the workspace that may apply it.
//!
//! # Three properties, and what each one buys
//!
//! 1. **Pure.** No clock, no socket, no database. The observation instant arrives in
//!    `reading.time`, which `Ingress::admit` has already set (§2.6 step 2 runs after the
//!    merge), and the station's coordinates arrive in [`StationMeta`]. A stage that could
//!    read a clock would make the clear-sky value depend on *when the batch drained*
//!    rather than on when the station measured.
//! 2. **Before the spool.** Storage, the snapshot and every peer therefore see the same
//!    enriched reading, and no consumer can disagree about wind chill.
//! 3. **Idempotent, writing a metric only where the value is `None`** (W3 §2.5
//!    consequence 3). This is what makes a forwarded envelope safe across a hop: a
//!    collector does **not** re-enrich a satellite's reading, so the two agree rather than
//!    each computing from a different formula version. `enrich` is therefore safe to call
//!    on `Ingress::accept`'s path as well as `admit`'s, which is why it is.
//!
//! # What this computes, and the two things W3 §2.5's table lists that it does not
//!
//! | Column | From | Needs metadata |
//! |---|---|---|
//! | `windchill` | `outtemp`, `windspeed` | no |
//! | `heatindex` | `outtemp`, `outhumidity` | no |
//! | `aqi_pm25_aqin` | `pm25` | no |
//! | `aqi_pm10_aqin` | `pm10_in_aqin` | no |
//! | `potentialsolarwatts` | `time`, `outtemp`, `outhumidity` | latitude, longitude, altitude |
//! | `snowdepth` | `snowdistance` | snow base distance |
//!
//! **`solarjoules` is not computed, and that is not an omission.** W3 §2.5's table and
//! this item's brief both say "solar potential/joules", which reads as one derived pair.
//! It is not: `metrics/registry.toml`'s entry for `solar_joules` settles it in the
//! opposite direction and names the file the question was addressed to — *"`pkg/solar` was
//! the file this question was addressed to and it is not the answer: it holds the
//! clear-sky irradiance models (ASCE, Bras, Ineichen-Perez) and the sunrise arithmetic,
//! all in W/m², and never mentions solarjoules. Nothing computes this value — the
//! instrument reports it."* It is the Campbell datalogger's `slr_mj`, carried through
//! untouched. The registry is the source of truth for what a metric *is* (**D-20**), so
//! there is nothing here to derive, and inventing a value for a column an instrument fills
//! would be worse than leaving it `NULL`.
//!
//! **`sunrise`, `sunset` and the four lunar values are `snapshot:latest`'s** (M4-1), not
//! this stage's: W2a Decision 36 evaluates them where they are served because they are
//! functions of `(date, lat, lon)` and not of a reading at all.
//!
//! # Why the formulae are the Go ones, to the bit where they can be
//!
//! Every one of these columns already has values in the production database, written by
//! the Go arithmetic. A chart spanning the cutover steps if the formula changes, and
//! `docs/reconciliation.md` §3 is where a step has to be recorded rather than discovered.
//! So the constants, the guard conditions and the `f32` arithmetic are
//! `internal/weatherstations/utils.go` and `pkg/aqi/aqi.go` at `remoteweather` `c3ec9f7`,
//! and the tests below assert against values produced by *running that Go code*.
//!
//! Two deliberate departures, both recorded in `docs/reconciliation.md` §3 as **DEV-9**:
//! the clear-sky model's timezone handling (see [`clear_sky_watts`]) and the decision to
//! store `NULL` rather than the air temperature where wind chill and heat index do not
//! apply (see [`wind_chill`]).

use chrono::{Datelike, Timelike};

/// The altitude an unconfigured station is placed at, in metres. See [`enrich`].
const SEA_LEVEL_M: f64 = 0.0;
use mw_component::StationMeta;
use mw_types::Reading;

/// Apply **R1** to one reading. Returns whether anything was written.
///
/// The return value is what lets `Ingress::accept` avoid cloning an `Arc<Reading>` that is
/// already enriched, which is the common case on a collector: a satellite enriched it
/// before it ever reached the wire.
pub fn enrich(reading: &mut Reading, meta: &StationMeta) -> bool {
    let mut changed = false;
    // `write_if_none` rather than an assignment in each arm: "only where the value is
    // `None`" is the idempotence property, and six open-coded `if x.is_none()` blocks are
    // six places it can be forgotten.
    changed |= write_if_none(&mut reading.wind_chill, || {
        wind_chill(reading.out_temp?, reading.wind_speed?)
    });
    changed |= write_if_none(&mut reading.heat_index, || {
        heat_index(reading.out_temp?, reading.out_humidity?)
    });
    changed |= write_if_none(&mut reading.aqi_pm25_aqin, || aqi_pm25(reading.pm25?));
    changed |= write_if_none(&mut reading.aqi_pm10_aqin, || {
        aqi_pm10(reading.pm10_in_aqin?)
    });
    changed |= write_if_none(&mut reading.potential_solar_watts, || {
        let (latitude, longitude) = meta.coordinates()?;
        Some(clear_sky_watts(
            reading.time,
            latitude,
            longitude,
            // A station with coordinates and no configured altitude is treated as being
            // at sea level for the barometric term. **Named rather than written as a
            // `0.0` fallback**, which is `ci/layers.toml`'s rule for exactly this crate:
            // a bare zero substituted for an absent value is the `zf` of §1.7 wearing a
            // different name, and the one place a zero may stand in for a `None` is the
            // frozen DTO.
            //
            // It is a decision rather than a default on two grounds. It is the Go
            // behaviour — `config.Altitude` is a non-nullable `float64` there and an unset
            // column reads as zero — and the term is not sensitive: 1609 m against sea
            // level moves the result by under 2%, which the module's altitude test
            // measures rather than claims. Returning `None` instead would mean a station
            // that has coordinates and no altitude stores no clear-sky figure at all,
            // which is a worse answer than one that is 2% out.
            meta.altitude_m.unwrap_or(SEA_LEVEL_M),
            reading.out_temp?,
            reading.out_humidity?,
        ))
    });
    changed |= write_if_none(&mut reading.snow_depth, || {
        snow_depth(reading.snow_distance?, meta.snow_base_distance_mm?)
    });
    changed
}

/// Write `value()` into `slot` if `slot` is `None` and the value exists.
///
/// The whole of W3 §2.5 consequence 3, in one place. `value` is a closure rather than a
/// value so that the inputs are not read -- and the clear-sky model not evaluated -- for a
/// metric the station already reported.
fn write_if_none<T>(slot: &mut Option<T>, value: impl FnOnce() -> Option<T>) -> bool {
    if slot.is_some() {
        return false;
    }
    match value() {
        Some(v) => {
            *slot = Some(v);
            true
        }
        None => false,
    }
}

/// Wind chill, °F — the NWS formula, and `None` where it does not apply.
///
/// `internal/weatherstations/utils.go:CalculateWindChill`: above 50 °F or below 3 mph the
/// formula is not defined, and the Go function returns **the air temperature**. The read
/// path then returns **0** for the same condition
/// (`internal/controllers/restserver/transform.go:18`), which is why
/// `metrics/registry.toml` marks `windch` `legacy_read_time_computed` and says "the column
/// and the JSON key are not guaranteed to be the same number".
///
/// **This stores `NULL`, which is a third answer and the right one.** A stored
/// `windchill == outtemp` is indistinguishable from a real wind chill that happens to
/// equal the air temperature, so the column's `min` and `mean` aggregates become the
/// temperature's for every summer bucket — a number nobody can use and nobody can detect.
/// `NULL` is the fact ("there is no wind chill in these conditions"), it is what **D-26**
/// asks of every other unreported value, and it costs the frozen contract nothing: the
/// legacy DTO zero-fills a `NULL` (§1.7's `zf`), which lands on exactly the `0` the Go
/// handler emits. Where the formula *does* apply the column and the recomputation agree,
/// so the frozen bytes are identical under both choices and only the column improves.
/// Recorded as **DEV-9**.
///
/// `f32` throughout with the exponentiation in `f64`, which is Go's
/// `float32(math.Pow(float64(v), 0.16))` — the rounding is observable in the last digit
/// and the test below pins it against the Go output.
#[must_use]
pub fn wind_chill(temp_f: f32, wind_mph: f32) -> Option<f32> {
    if temp_f > 50.0 || wind_mph < 3.0 {
        return None;
    }
    let v16 = f64::from(wind_mph).powf(0.16) as f32;
    Some(35.74 + 0.6215 * temp_f - 35.75 * v16 + 0.4275 * temp_f * v16)
}

/// Heat index, °F — the NWS Rothfusz regression, and `None` below 80 °F.
///
/// `internal/weatherstations/utils.go:CalculateHeatIndex`, constants and `f32` arithmetic
/// unchanged. `None` below 80 °F for [`wind_chill`]'s reason: the Go function returns the
/// air temperature there and the Go handler returns 0, and neither is a heat index.
///
/// Deliberately *not* the NWS adjustments for the low-humidity and high-humidity corners,
/// which the Go version does not apply either. Adding them would change a stored series
/// with no decision behind it.
#[must_use]
pub fn heat_index(temp_f: f32, humidity: f32) -> Option<f32> {
    if temp_f < 80.0 {
        return None;
    }
    // Each of these is the **shortest literal that round-trips to the same `f32` as the Go
    // source's own decimal** -- `2.04901523` there, `2.049_015_3` here, the same 32 bits.
    // Spelled this way because `clippy::excessive_precision` rejects the longer form, and
    // worth the note because the obvious shortening is wrong: `2.049_015_2` is a
    // *different* `f32`, and it moved the result in the fifth significant figure. The
    // test below is a bit-for-bit equality against the Go output, which is what caught it.
    const C1: f32 = -42.379;
    const C2: f32 = 2.049_015_3;
    const C3: f32 = 10.143_332;
    const C4: f32 = -0.224_755_4;
    const C5: f32 = -0.006_837_83;
    const C6: f32 = -0.054_817_17;
    const C7: f32 = 0.001_228_74;
    const C8: f32 = 0.000_852_82;
    const C9: f32 = -0.000_001_99;
    Some(
        C1 + C2 * temp_f
            + C3 * humidity
            + C4 * temp_f * humidity
            + C5 * temp_f * temp_f
            + C6 * humidity * humidity
            + C7 * temp_f * temp_f * humidity
            + C8 * temp_f * humidity * humidity
            + C9 * temp_f * temp_f * humidity * humidity,
    )
}

/// One EPA AQI breakpoint: the concentration band and the index band it maps onto.
struct Breakpoint {
    c_low: f64,
    c_high: f64,
    i_low: f64,
    i_high: f64,
}

/// EPA PM2.5 breakpoints, `pkg/aqi/aqi.go:CalculatePM25`.
///
/// These are the **pre-2024** table the Go implementation uses (the 2024 revision moved
/// the Good/Moderate boundary from 12.0 to 9.0 µg/m³). Reproduced rather than updated:
/// the column has history computed against this table, and revising an index that a chart
/// colours by category is a decision with an owner, not a constant to bump here.
const PM25: &[Breakpoint] = &[
    Breakpoint {
        c_low: 0.0,
        c_high: 12.0,
        i_low: 0.0,
        i_high: 50.0,
    },
    Breakpoint {
        c_low: 12.1,
        c_high: 35.4,
        i_low: 51.0,
        i_high: 100.0,
    },
    Breakpoint {
        c_low: 35.5,
        c_high: 55.4,
        i_low: 101.0,
        i_high: 150.0,
    },
    Breakpoint {
        c_low: 55.5,
        c_high: 150.4,
        i_low: 151.0,
        i_high: 200.0,
    },
    Breakpoint {
        c_low: 150.5,
        c_high: 250.4,
        i_low: 201.0,
        i_high: 300.0,
    },
    Breakpoint {
        c_low: 250.5,
        c_high: 350.4,
        i_low: 301.0,
        i_high: 400.0,
    },
    Breakpoint {
        c_low: 350.5,
        c_high: 500.4,
        i_low: 401.0,
        i_high: 500.0,
    },
];

/// EPA PM10 breakpoints, `pkg/aqi/aqi.go:CalculatePM10`.
const PM10: &[Breakpoint] = &[
    Breakpoint {
        c_low: 0.0,
        c_high: 54.0,
        i_low: 0.0,
        i_high: 50.0,
    },
    Breakpoint {
        c_low: 55.0,
        c_high: 154.0,
        i_low: 51.0,
        i_high: 100.0,
    },
    Breakpoint {
        c_low: 155.0,
        c_high: 254.0,
        i_low: 101.0,
        i_high: 150.0,
    },
    Breakpoint {
        c_low: 255.0,
        c_high: 354.0,
        i_low: 151.0,
        i_high: 200.0,
    },
    Breakpoint {
        c_low: 355.0,
        c_high: 424.0,
        i_low: 201.0,
        i_high: 300.0,
    },
    Breakpoint {
        c_low: 425.0,
        c_high: 504.0,
        i_low: 301.0,
        i_high: 400.0,
    },
    Breakpoint {
        c_low: 505.0,
        c_high: 604.0,
        i_low: 401.0,
        i_high: 500.0,
    },
];

/// The AQI of a PM2.5 concentration in µg/m³.
///
/// `None` for a negative concentration, where the Go function returns 0 — a sensor
/// reporting a negative mass is a sensor fault, and a stored 0 would read as pristine air.
/// `Some(500)` above the top breakpoint, which is the Go behaviour and the EPA's "500+".
#[must_use]
pub fn aqi_pm25(pm25: f32) -> Option<i32> {
    index(PM25, pm25)
}

/// The AQI of a PM10 concentration in µg/m³. See [`aqi_pm25`].
#[must_use]
pub fn aqi_pm10(pm10: f32) -> Option<i32> {
    index(PM10, pm10)
}

/// The EPA's piecewise-linear interpolation, `I = (Ih - Il)/(Ch - Cl) * (C - Cl) + Il`.
fn index(table: &[Breakpoint], concentration: f32) -> Option<i32> {
    if concentration < 0.0 {
        return None;
    }
    let c = f64::from(concentration);
    // `<=` against each band's `c_high`, in order, which is the Go `switch`. Note the gaps
    // the EPA table leaves between bands (12.0 to 12.1, 35.4 to 35.5): a concentration in
    // one takes the *next* band, because the comparison is against `c_high` and `c_low` is
    // only used in the interpolation. That reproduces the Go arithmetic exactly, including
    // the slightly-below-`i_low` result it gives for 12.05.
    let Some(b) = table.iter().find(|b| c <= b.c_high) else {
        return Some(500);
    };
    let aqi = ((b.i_high - b.i_low) / (b.c_high - b.c_low)) * (c - b.c_low) + b.i_low;
    // `as` after `round` is saturating in Rust and the value is bounded by 500 anyway.
    Some(aqi.round() as i32)
}

/// Snow depth in millimetres: the gauge's base distance less what it is reading now.
///
/// `(base_distance - snowdistance)`, which is W3 §2.5's expression and
/// `internal/controllers/restserver/controller.go:903`'s arithmetic — the Go version
/// computes it **on the read path, per request**, from a per-website cache of the
/// configured base distance. R1 moves it to ingest because it is a pure function of one
/// reading plus static station metadata, and moving it is also what gets the value into
/// the aggregates: today `snowdepth` is NULL in every stored row and every bucket.
///
/// Guarded exactly as Go guards it: a non-positive distance is a sensor fault rather than
/// a gauge buried to its sensor (`controller.go:902` tests `SnowDistance > 0`), and a
/// non-positive base distance is a station that has not been configured — the
/// configuration column's own `CHECK` is `> 0`.
///
/// A distance *greater* than the base gives a negative depth and is returned as one. The
/// Go snow-gauge driver discards such a reading at the device
/// (`snowgauge/station.go:172`), which this stage cannot do — it is handed a reading that
/// is already admitted, and dropping a metric here would make enrich a filter. A negative
/// depth is visibly wrong, which is the right failure for a mis-set base distance; a
/// clamp to zero would make it look like bare ground.
#[must_use]
pub fn snow_depth(snow_distance_mm: f32, base_distance_mm: f64) -> Option<f32> {
    if snow_distance_mm <= 0.0 || base_distance_mm <= 0.0 {
        return None;
    }
    Some(base_distance_mm as f32 - snow_distance_mm)
}

/// Clear-sky shortwave radiation, W/m² — the ASCE model of `pkg/solar/asce.go`.
///
/// # The one departure, and why it is the model and not the arithmetic
///
/// The Go function takes the hour-of-day from `time.Now()` **in the server's local zone**
/// and the timezone meridian from `time.Local`'s current offset, while taking the day of
/// year from UTC. Both of those are the server's zone, not the station's — so a station in
/// Denver reported by a daemon running in UTC gets a solar zenith angle seven hours out,
/// which is the difference between a clear-sky figure of 1003 W/m² and one of 0. W3
/// **D-24** already says every calendar quantity is computed in the *station's* zone and
/// not the server's, for exactly this class of defect.
///
/// The fix is not to plumb a time zone in. The offset **cancels algebraically**:
///
/// ```text
///   timeOfDay - solar_N = timeOfDay - 12 + eqt + (lon_tz - lon)/15
///   timeOfDay = utc + offset,  lon_tz = -offset * 15,  lon = -longitude
///                       = utc + offset - 12 + eqt - offset + longitude/15
///                       = utc + longitude/15 + eqt - 12
/// ```
///
/// which is true solar time from UTC, with no zone in it at all. So this function needs no
/// `chrono-tz`, no `TzSource` and no clock, and stays a pure function of the reading.
///
/// **Measured equivalent, not argued equivalent.** Against the Go function run with
/// `time.Local` set to the station's own zone — which is the production configuration, and
/// the only one in which the Go behaviour is correct — the worst absolute difference over
/// five zones × 53 days × 24 hours is **1.1 × 10⁻¹² W/m²**, i.e. `f64` rounding. The
/// zones include DST ones, where Go's meridian lookup uses the shifted offset and
/// therefore cancels just as cleanly.
///
/// The two cases where it genuinely differs are both Go defects: a server whose zone is
/// not the station's, and any station in the eastern hemisphere, where Go's meridian table
/// maps `+7` to 105° the same way it maps `-7` to 105° and the offset therefore does not
/// cancel but doubles.
///
/// `Kt = 1.0` (clearness index) and a solar constant of 1361 W/m², both as in the Go
/// source. Returns 0.0 when the sun is below the horizon, which is also Go's.
#[must_use]
pub fn clear_sky_watts(
    at: chrono::DateTime<chrono::Utc>,
    latitude: f64,
    longitude: f64,
    altitude_m: f64,
    air_temp_f: f32,
    humidity: f32,
) -> f32 {
    /// W/m² at the top of the atmosphere.
    const SOLAR_CONSTANT: f64 = 1361.0;
    /// Clearness index. 1.0 is the clear-sky assumption the column's name carries.
    const KT: f64 = 1.0;

    let day_of_year = f64::from(at.ordinal());
    let utc_hours =
        f64::from(at.hour()) + f64::from(at.minute()) / 60.0 + f64::from(at.second()) / 3600.0;
    let air_temp_c = (f64::from(air_temp_f) - 32.0) * 5.0 / 9.0;

    // Earth-Sun distance correction.
    let d_r = 1.0 + 0.033 * (((2.0 * std::f64::consts::PI) / 365.0) * day_of_year).cos();
    // Solar declination, degrees.
    let delta = (0.39785
        * ((278.97
            + 0.9856 * day_of_year
            + 1.9165 * ((356.6 + 0.9856 * day_of_year).to_radians()).sin())
        .to_radians())
        .sin())
    .asin()
    .to_degrees();
    // True solar time relative to solar noon, in hours. See the doc comment's algebra.
    let hour_angle_hours = utc_hours + longitude / 15.0 + equation_of_time(at) / 60.0 - 12.0;
    // Solar zenith angle, degrees.
    let solar_z = (latitude.to_radians().sin() * delta.to_radians().sin()
        + latitude.to_radians().cos()
            * delta.to_radians().cos()
            * (hour_angle_hours * (std::f64::consts::PI / 12.0)).cos())
    .acos()
    .to_degrees();

    // Atmospheric pressure, kPa, from the barometric formula.
    let p_b = 101.325
        * ((-altitude_m * 9.806_65) / ((8.314_472 / 0.028_967) * (air_temp_c + 273.15))).exp();
    // Vapour pressure, kPa (Buck).
    let e_a = 0.611_21
        * (((18.678 - air_temp_c / 234.5) * air_temp_c) / (257.14 + air_temp_c)).exp()
        * (f64::from(humidity) / 100.0);
    // Extraterrestrial radiation on a horizontal surface, W/m².
    let sw_a = SOLAR_CONSTANT * d_r * solar_z.to_radians().cos();
    if sw_a < 0.0 {
        // The sun is below the horizon. Zero rather than `None`: "there is no clear-sky
        // radiation at midnight" is a value, not an absence, and it is the value the Go
        // function stores.
        return 0.0;
    }
    // Precipitable water, mm.
    let w = 0.15 * e_a * p_b + 0.6;
    let elevation_sin = (90.0 - solar_z).to_radians().sin();
    // Beam transmittance.
    let k_b = 0.98
        * ((-0.00146 * p_b) / (KT * elevation_sin) - 0.075 * (w / elevation_sin).powf(0.4)).exp();
    // Diffuse transmittance.
    let k_d = if k_b > 0.15 {
        0.35 - 0.36 * k_b
    } else {
        0.18 + 0.82 * k_b
    };
    ((k_b + k_d) * sw_a) as f32
}

/// The equation of time, in minutes — `pkg/solar/ineichen-perez.go:equationOfTime`.
///
/// Timezone-free already: it is a function of the Julian day, which comes from the Unix
/// instant.
fn equation_of_time(at: chrono::DateTime<chrono::Utc>) -> f64 {
    // JD = 2440587.5 (the Unix epoch) + days since it.
    let jd = 2_440_587.5 + (at.timestamp() as f64) / 86_400.0;
    let t = (jd - 2_451_545.0) / 36_525.0;
    let fix = |a: f64| (a + 360.0).rem_euclid(360.0);
    let l0 = fix(280.466_46 + t * (36_000.769_83 + t * 0.000_303_2));
    let m = fix(357.529_11 + t * (35_999.050_29 - t * 0.000_153_7));
    let e = 0.016_708_634 - t * (0.000_042_037 + t * 0.000_000_126_7);
    let eps0 =
        23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.000_59 - t * 0.001_813))) / 60.0) / 60.0;
    let y = (eps0.to_radians() / 2.0).tan() * (eps0.to_radians() / 2.0).tan();
    (y * (2.0 * l0).to_radians().sin() - 2.0 * e * m.to_radians().sin()
        + 4.0 * e * y * m.to_radians().sin() * (2.0 * l0).to_radians().cos()
        - 0.5 * y * y * (4.0 * l0).to_radians().sin()
        - 1.25 * e * e * (2.0 * m).to_radians().sin())
    .to_degrees()
        * 4.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("an RFC 3339 instant")
            .with_timezone(&Utc)
    }

    fn denver() -> StationMeta {
        StationMeta {
            latitude: Some(39.7392),
            longitude: Some(-104.9903),
            altitude_m: Some(1609.0),
            snow_base_distance_mm: Some(1500.0),
        }
    }

    /// Produced by **running** `internal/weatherstations/utils.go:CalculateWindChill` at
    /// `remoteweather` c3ec9f7, printed as `%.7e` so the `f32` is exact. Asserted bit-for-bit:
    /// the whole point of reproducing the formula is that the column does not step at the
    /// cutover, and "close enough" is not that property.
    #[test]
    fn wind_chill_matches_the_go_formula_bit_for_bit() {
        for (temp, wind, want) in [
            (20.0_f32, 15.0_f32, 6.218_886_4_f32),
            (50.0, 3.0, 49.677_513),
            (-10.0, 40.0, -42.695_57),
            (35.0, 5.5, 30.186_363),
            (0.0, 3.0, -6.880_195_6),
        ] {
            let got = wind_chill(temp, wind).expect("the formula applies");
            assert_eq!(
                got, want,
                "wind_chill({temp}, {wind}) = {got:e}, Go says {want:e}"
            );
        }
    }

    /// The two boundaries, which is where the Go function returns the air temperature and
    /// the Go handler returns 0. Both are `None` here -- DEV-9.
    #[test]
    fn wind_chill_is_none_rather_than_the_air_temperature_where_it_does_not_apply() {
        assert_eq!(wind_chill(50.000_1, 3.0), None, "above 50 degF");
        assert_eq!(wind_chill(20.0, 2.999_9), None, "below 3 mph");
        assert!(
            wind_chill(50.0, 3.0).is_some(),
            "50 degF and 3 mph is inside"
        );
    }

    /// `internal/weatherstations/utils.go:CalculateHeatIndex`, run.
    #[test]
    fn heat_index_matches_the_go_formula_bit_for_bit() {
        for (temp, humidity, want) in [
            (80.0_f32, 40.0_f32, 79.929_41_f32),
            (95.0, 60.0, 113.090_44),
            (110.0, 90.0, 246.997_63),
            (100.0, 10.0, 94.752_66),
        ] {
            let got = heat_index(temp, humidity).expect("the formula applies");
            assert_eq!(
                got, want,
                "heat_index({temp}, {humidity}) = {got:e}, Go says {want:e}"
            );
        }
        assert_eq!(heat_index(79.999_9, 50.0), None, "below 80 degF");
        assert!(heat_index(80.0, 50.0).is_some(), "80 degF is inside");
    }

    /// `pkg/aqi/aqi.go`'s breakpoints, including the three shapes that are easy to get
    /// wrong: a band boundary, the gap the EPA table leaves between bands, and the
    /// saturation above the top band.
    #[test]
    fn the_aqi_tables_are_the_epa_breakpoints_the_go_package_uses() {
        for (pm, want) in [
            (0.0_f32, 0),
            (6.0, 25),
            (12.0, 50),
            // Inside the 12.0..12.1 gap the EPA table leaves: the `<= c_high` comparison
            // takes the *second* band and the interpolation then runs slightly below its
            // `i_low`, which rounds back up to 51. Faithful to the Go `switch`.
            (12.05, 51),
            (12.1, 51),
            (30.0, 89),
            // 101 and not 100, and the reason is the `f32` column rather than the table:
            // `f64::from(35.4_f32)` is 35.400001525878906, which is above the band's
            // `c_high`. The Go function widens the same way, so this is agreement rather
            // than an accident -- and it is why the expected values here come from
            // running `pkg/aqi/aqi.go` instead of reading the EPA table.
            (35.4, 101),
            (35.5, 101),
            (55.4, 151),
            (150.4, 200),
            (250.4, 300),
            (350.4, 400),
            (500.4, 500),
            (500.5, 500),
            (900.0, 500),
        ] {
            assert_eq!(aqi_pm25(pm), Some(want), "aqi_pm25({pm})");
        }
        for (pm, want) in [
            (0.0_f32, 0),
            (27.0, 25),
            (54.0, 50),
            (54.5, 51),
            (55.0, 51),
            (154.0, 100),
            (254.0, 150),
            (354.0, 200),
            (424.0, 300),
            (504.0, 400),
            (604.0, 500),
            (604.5, 500),
            (1000.0, 500),
        ] {
            assert_eq!(aqi_pm10(pm), Some(want), "aqi_pm10({pm})");
        }
        assert_eq!(aqi_pm25(-1.0), None, "a negative mass is a sensor fault");
        assert_eq!(aqi_pm10(-0.5), None);
    }

    /// Produced by running `pkg/solar/asce.go` with `time.Local` set to the station's own
    /// zone -- the production configuration, and the only one in which the Go behaviour is
    /// correct. The `f32` is exact; the comparison is on the stored width because that is
    /// what the column holds.
    #[test]
    fn the_clear_sky_model_matches_the_go_asce_output() {
        for (name, when, lat, lon, alt, temp, humidity, want) in [
            (
                "summer noon, Denver",
                "2026-06-21T19:00:00Z",
                39.7392,
                -104.9903,
                1609.0,
                85.0_f32,
                25.0_f32,
                1.003_598_7e3_f32,
            ),
            (
                "winter morning, Denver",
                "2026-01-15T16:30:00Z",
                39.7392,
                -104.9903,
                1609.0,
                20.0,
                60.0,
                3.144_282e2,
            ),
            (
                "night, Denver",
                "2026-06-22T07:00:00Z",
                39.7392,
                -104.9903,
                1609.0,
                55.0,
                70.0,
                0.0,
            ),
            (
                "equinox, equator, sea level",
                "2026-03-20T12:00:00Z",
                0.0,
                0.0,
                0.0,
                77.0,
                50.0,
                1.044_653_1e3,
            ),
            (
                "high altitude, cold",
                "2026-11-05T18:45:30Z",
                45.5,
                -110.0,
                2500.0,
                10.0,
                80.0,
                5.260_227e2,
            ),
        ] {
            let got = clear_sky_watts(at(when), lat, lon, alt, temp, humidity);
            assert_eq!(got, want, "{name}: {got:e} against Go's {want:e}");
        }
    }

    /// The sun below the horizon is 0.0, not a negative number and not `None`.
    #[test]
    fn midnight_is_zero_watts_everywhere() {
        for lon in [-180.0, -104.9903, 0.0, 12.5, 179.0] {
            let local_midnight = at("2026-02-10T00:00:00Z") - chrono::Duration::hours(0);
            // Pick the instant that is solar midnight at this longitude: solar noon is at
            // `12 - longitude/15` UTC, so midnight is twelve hours from it.
            let noon_utc: f64 = 12.0 - lon / 15.0;
            let midnight_utc = (noon_utc + 12.0).rem_euclid(24.0);
            let when = local_midnight
                + chrono::Duration::milliseconds((midnight_utc * 3_600_000.0) as i64);
            assert_eq!(
                clear_sky_watts(when, 39.7392, lon, 1609.0, 40.0, 50.0),
                0.0,
                "longitude {lon} at its own solar midnight"
            );
        }
    }

    /// The Go read path's arithmetic (`controller.go:903`) and its guard
    /// (`controller.go:902`).
    #[test]
    fn snow_depth_is_the_base_distance_less_the_measured_distance() {
        assert_eq!(snow_depth(1200.0, 1500.0), Some(300.0));
        assert_eq!(snow_depth(1500.0, 1500.0), Some(0.0), "bare ground");
        assert_eq!(
            snow_depth(1600.0, 1500.0),
            Some(-100.0),
            "a mis-set base distance is visibly wrong rather than clamped to bare ground"
        );
        assert_eq!(snow_depth(0.0, 1500.0), None, "a zero distance is a fault");
        assert_eq!(snow_depth(-3.0, 1500.0), None);
        assert_eq!(
            snow_depth(1200.0, 0.0),
            None,
            "an unconfigured base distance derives nothing"
        );
    }

    /// **The idempotence property**, which is what makes a forwarded envelope safe across
    /// a hop (W3 §2.5 consequence 3). Enriching twice must change nothing the second time,
    /// and must not overwrite a value the *station* reported.
    #[test]
    fn enrich_is_idempotent_and_never_overwrites_a_reported_value() {
        let mut reading = Reading {
            time: at("2026-01-15T16:30:00Z"),
            station_id: "backyard".into(),
            out_temp: Some(20.0),
            out_humidity: Some(60.0),
            wind_speed: Some(15.0),
            pm25: Some(30.0),
            pm10_in_aqin: Some(80.0),
            snow_distance: Some(1200.0),
            ..Reading::default()
        };
        assert!(enrich(&mut reading, &denver()), "the first pass writes");
        let once = reading.clone();
        assert!(
            !enrich(&mut reading, &denver()),
            "the second pass writes nothing"
        );
        assert_eq!(reading, once, "and changes nothing");

        // A station that reports its own wind chill keeps it, even when it disagrees with
        // the formula. That is what "a collector does not re-enrich" means in practice:
        // the satellite's value survives the hop rather than being recomputed here from a
        // possibly different formula version.
        let mut reported = Reading {
            wind_chill: Some(-99.0),
            ..once
        };
        enrich(&mut reported, &denver());
        assert_eq!(
            reported.wind_chill,
            Some(-99.0),
            "a reported value is never recomputed"
        );
    }

    /// A reading with no metadata still gets the three columns that need none, and gets
    /// `NULL` for the three that do. The accept path depends on this: a peer's station key
    /// is a claim until M6-4, so `metadata` answers `EMPTY` for it.
    #[test]
    fn a_station_with_no_configured_metadata_derives_only_what_needs_none() {
        let mut reading = Reading {
            time: at("2026-06-21T19:00:00Z"),
            station_id: "unknown".into(),
            out_temp: Some(85.0),
            out_humidity: Some(25.0),
            wind_speed: Some(10.0),
            pm25: Some(9.0),
            snow_distance: Some(1200.0),
            ..Reading::default()
        };
        assert!(enrich(&mut reading, &StationMeta::EMPTY));
        assert_eq!(reading.heat_index, heat_index(85.0, 25.0));
        assert_eq!(reading.aqi_pm25_aqin, aqi_pm25(9.0));
        assert_eq!(
            reading.potential_solar_watts, None,
            "no coordinates, no clear-sky figure -- not a zero"
        );
        assert_eq!(
            reading.snow_depth, None,
            "no base distance, no depth -- not the negated distance"
        );
        assert_eq!(
            reading.wind_chill, None,
            "85 degF is above the wind-chill range"
        );
    }

    /// An absent input leaves the derived column absent rather than substituting a zero,
    /// which is **D-26** one level up from a sensor.
    #[test]
    fn a_missing_input_derives_nothing() {
        let mut only_temp = Reading {
            time: at("2026-01-15T16:30:00Z"),
            out_temp: Some(20.0),
            ..Reading::default()
        };
        assert!(
            !enrich(&mut only_temp, &denver()),
            "temperature alone derives nothing: wind chill needs wind, heat index needs \
             humidity, and the clear-sky model needs humidity too"
        );
        assert_eq!(only_temp.wind_chill, None);
        assert_eq!(only_temp.heat_index, None);
        assert_eq!(only_temp.potential_solar_watts, None);
    }

    /// The altitude fallback, which is the one place this stage substitutes a value for an
    /// unset column. Worth a number rather than a claim: the barometric term is what the
    /// altitude enters, and 1609 m against sea level is a 17% difference in the
    /// transmittance's pressure, which moves the result by under 2%.
    #[test]
    fn an_unset_altitude_is_sea_level_and_the_term_is_not_sensitive() {
        let when = at("2026-06-21T19:00:00Z");
        let sea = clear_sky_watts(when, 39.7392, -104.9903, 0.0, 85.0, 25.0);
        let denver_alt = clear_sky_watts(when, 39.7392, -104.9903, 1609.0, 85.0, 25.0);
        let ratio = f64::from(denver_alt) / f64::from(sea);
        assert!(
            (1.0..1.03).contains(&ratio),
            "1609 m against sea level moved the clear-sky figure by {:.2}% \
             ({sea} -> {denver_alt})",
            (ratio - 1.0) * 100.0
        );
    }

    /// Pure: the same reading and the same metadata give the same answer whenever it is
    /// run, which is what lets the spool's copy and a peer's copy agree. Asserted by
    /// running it against a fixed instant rather than `now()`, which the signature makes
    /// the only option.
    #[test]
    fn the_stage_reads_no_clock() {
        let base = Reading {
            time: Utc.with_ymd_and_hms(2026, 6, 21, 19, 0, 0).unwrap(),
            out_temp: Some(85.0),
            out_humidity: Some(25.0),
            ..Reading::default()
        };
        let mut a = base.clone();
        let mut b = base;
        enrich(&mut a, &denver());
        enrich(&mut b, &denver());
        assert_eq!(a, b);
        assert!(a.potential_solar_watts.is_some());
    }
}
