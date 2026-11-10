//! `AmbientQuery` — the Ambient Weather "customized" upload, which is a query string.
//!
//! **This is the structural one.** W2a §4a.5's Ambient row is `HttpIn { /ingest/ambient }`,
//! `FromQuery { id: "ID", secret: Some("PASSWORD") }`, `Cardinality::Many` — one route on
//! the one listener, N stations, identity from `?ID=` resolved against `StationRegistry` and
//! the secret from `?PASSWORD=` compared in constant time. The Go driver is one
//! `http.Server` **per station**, on a port the operator configures per station, and the
//! station's identity is *the port the request arrived on*: any request on that port is
//! trusted as that station. A second Ambient station there is a second listener; here it is
//! a configuration row.
//!
//! What is in this module is only the decoding. The route, the identity, the constant-time
//! comparison and the 400s are `mw_station::ambient`'s, because every one of them needs a
//! socket or the registry. What they share is [`params`]: the driver reads `?ID=` and
//! `?PASSWORD=` out of the same parse this codec maps, so there is one implementation of
//! "`+` means space" rather than one on each side of the station's password.
//!
//! # The parameter set
//!
//! A hundred and thirty-one measurements, which is Ambient's own "customized" vocabulary as
//! `ambientcustomized/station.go:188-367` enumerates it, carried over one for one. Every one
//! is already in the registry's units — Ambient reports °F, inHg, mph and in — so there is no
//! conversion anywhere in this file, which is why there is no conversion table in this
//! comment. The groups:
//!
//! | Group | Parameters |
//! |---|---|
//! | the console | `tempf`, `humidity`, `dewptf`, `winddir`, `windspeedmph`, `windgustmph`, `baromin`, `baromrelin`, `baromabsin`, `solarradiation`, `UV`, `indoortempf`, `indoorhumidity` |
//! | rain | `rainin`, `dailyrainin`, `weeklyrainin`, `monthlyrainin`, `totalrainin` |
//! | extra sensors | `temp1f`-`temp10f`, `humidity1`-`humidity10`, `soiltemp1f`-`soiltemp10f`, `soilhum1`-`soilhum10`, `leafwetness1`-`leafwetness8`, `soiltens1`-`soiltens4` |
//! | agriculture | `gdd`, `etos`, `etrs` |
//! | particulates | `pm25`, `pm25_24h`, `pm25_in`, `pm25_in_24h`, the four `*_aqin` particulates, `co2`, `co2_in_aqin`, `co2_in_24h_aqin`, `pm_in_temp_aqin`, `pm_in_humidity_aqin`, the six `aqi_*` |
//! | lightning | `lightning_day`, `lightning_hour`, `lightning_time`, `lightning_distance` |
//! | hardware | `leak1`-`leak4`, `relay1`-`relay10`, twenty-four battery flags, `lowbatt` |
//! | metadata | `tz`, and `dateutc`, which is the observation instant |
//!
//! # Where this disagrees with the Go driver
//!
//! **1. A parameter the console did not send is absent.** Every Go parser writes into a
//! non-pointer field and leaves it alone on absence or on a parse failure, so a console with
//! no indoor sensor stores `intemp = 0.0` — 0 °F — on every upload. **D-26**. The same
//! correction as Davis's dash values and Campbell's missing keys, and here it is a hundred
//! and thirty fields wide: an Ambient WS-2902 sends about twenty of them.
//!
//! **2. `?tempf=` with nothing after it is absent, not zero.** A console that has lost a
//! sensor sends the key with an empty value, which `strconv.ParseFloat` rejects — and in Go
//! that leaves the previous zero, which is the same number as 0 °F. [`QueryParams::non_empty`]
//! is the lookup, so an empty value never reaches a parse.
//!
//! **3. A value that is not a finite number is refused.** `strconv.ParseFloat` accepts
//! `Inf`, `+Inf` and `NaN`, and Rust's `f32::from_str` accepts `inf` and `NaN` too, so a
//! console with a divide-by-zero in its firmware would otherwise store a `NaN` that every
//! `mean` over that bucket inherits. `mw_pipeline::enrich`'s own `usable()` guard exists for
//! the same class of value one layer later; this is the earliest layer that can see it.
//!
//! **4. `windchill` and `heatindex` are not computed here** — **R1**/**DEV-9**, as for every
//! other driver.
//!
//! **5. `dateutc` is the observation instant and not also a column.** The Go driver parses it
//! twice: once as `"2006-01-02 15:04:05"` for `reading.Timestamp`, and once with
//! `parseInt64` into `reading.DateUTC` — which **always fails**, because the protocol's own
//! format is not an integer, so `dateutc` is `0` in every stored Ambient row. The column is
//! `int8`/`ms_epoch` in `metrics/registry.toml`, the parsed instant is not that, and
//! inventing one would be a value where Go has a zero. Left `None`, recorded here.
//!
//! # `totalrainin` is stateful, so it is not in this file
//!
//! `totalrainin` is the console's all-time rainfall counter, and the Go driver turns it into
//! an increment by **remembering the previous upload** (`station.go:216-230`) and
//! overwriting whatever `rainin` had put in `rainincremental`. A codec cannot remember
//! anything — that is what makes it golden-testable — so the arithmetic is
//! [`rain_since`], a pure function of the two values, and the memory is the driver's.
//! [`total_rain`] is how the driver reads the counter out of the same parse.

use chrono::{DateTime, NaiveDateTime, Utc};
use mw_types::{PartialReading, Reading};

use crate::frame::{Codec, CodecError, Frame};
use crate::query::QueryParams;
use crate::winddir::WindDirCorrection;

/// `?ID=` — the station identifier, which `StationRegistry::resolve` turns into a station.
pub const ID_PARAM: &str = "ID";

/// `?PASSWORD=` — the protocol's shared secret, compared in constant time against
/// `config.stations.ingest_secret_hash` (**D-33**).
pub const PASSWORD_PARAM: &str = "PASSWORD";

/// `?action=` — which the protocol requires and which only ever has one value.
pub const ACTION_PARAM: &str = "action";

/// The only `?action=` an upload may carry.
pub const ACTION_UPDATERAW: &str = "updateraw";

/// `?dateutc=` — the observation instant, in UTC, in the format below.
pub const DATE_UTC_PARAM: &str = "dateutc";

/// `?totalrainin=` — the console's all-time rainfall counter.
pub const TOTAL_RAIN_PARAM: &str = "totalrainin";

/// `?winddir=`, named because the vane correction is applied to it and to nothing else.
pub const WIND_DIR_PARAM: &str = "winddir";

/// How `?dateutc=` is formatted. `time.Parse("2006-01-02 15:04:05", …)` in Go's reference
/// layout, which is this in `chrono`'s.
const DATE_UTC_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// The Ambient "customized" codec for one station's vane calibration.
#[derive(Debug, Clone, Default)]
pub struct AmbientQuery {
    correction: WindDirCorrection,
}

impl AmbientQuery {
    /// The codec for a station whose vane is offset by `correction`.
    #[must_use]
    pub const fn new(correction: WindDirCorrection) -> Self {
        Self { correction }
    }

    /// The vane calibration this codec applies.
    #[must_use]
    pub const fn correction(&self) -> WindDirCorrection {
        self.correction
    }
}

/// Decode one upload's query string.
///
/// The frame is the query, not the request: `mw-station` hands over `Uri::query()` and the
/// transcript-level test hands over a recorded string, so this crate never sees a request.
pub fn params(frame: &Frame) -> Result<QueryParams, CodecError> {
    Ok(QueryParams::parse(frame.as_str()?))
}

/// The observation instant the upload declares.
///
/// An error rather than a substituted arrival time, and that is the protocol's own
/// requirement: `dateutc` is one of the four parameters the Go handler rejects a request for
/// missing, because an Ambient console buffers and retries — so an upload with no timestamp
/// is one whose instant nothing knows, not one that happened now.
pub fn observed_at(params: &QueryParams) -> Result<DateTime<Utc>, CodecError> {
    let Some(text) = params.non_empty(DATE_UTC_PARAM) else {
        return Err(CodecError::Incomplete {
            what: "`dateutc` parameter, which every `updateraw` upload carries",
        });
    };
    NaiveDateTime::parse_from_str(text, DATE_UTC_FORMAT)
        .map(|naive| naive.and_utc())
        .map_err(|_| CodecError::Incomplete {
            what: "`dateutc` in `YYYY-MM-DD HH:MM:SS`",
        })
}

/// The console's all-time rainfall counter, where it sent one.
#[must_use]
pub fn total_rain(params: &QueryParams) -> Option<f32> {
    number(params, TOTAL_RAIN_PARAM)
}

/// Rainfall since the previous upload, from two readings of the console's counter.
///
/// `None` where no increment can be derived, which is three cases and is Go's own test
/// (`station.go:223`) with the zero replaced by an absence:
///
/// - there is no previous value, so this is the first upload since the component started;
/// - the counter went **backwards**, which is a console that was reset or replaced;
/// - the previous value is zero, which Go treats as "no previous value" because its field
///   cannot tell the two apart. Kept, because a console whose counter is genuinely at zero
///   has not rained since it was installed and `dailyrainin` carries the same information.
///
/// Go stores `0` in all three; **D-26** again — "no rain fell" and "we cannot say how much
/// rain fell" are different facts, and the second one is what the first upload after a
/// restart is.
#[must_use]
pub fn rain_since(previous: Option<f32>, total: f32) -> Option<f32> {
    let previous = previous?;
    if previous <= 0.0 || !previous.is_finite() || !total.is_finite() || total < previous {
        return None;
    }
    Some(total - previous)
}

impl Codec for AmbientQuery {
    fn name(&self) -> &'static str {
        "ambient-query"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        let q = &params(frame)?;
        let at = observed_at(q)?;
        let r = Reading {
            wind_dir: self.correction.apply(number(q, WIND_DIR_PARAM)),
            // `lowbatt` is a float in the protocol and an integer flag in the column, which
            // is `uint8(parsed)` at `station.go:362` -- a truncation rather than a rounding,
            // kept because the only values the protocol sends are 0 and 1.
            tx_battery_status: number(q, "lowbatt").map(|v| v as i32),
            tz: params_text(q, "tz"),
            lightning_time: instant(q, "lightning_time"),
            // 81 float parameters, 50 integer parameters
            out_temp: number(q, "tempf"),
            out_humidity: number(q, "humidity"),
            extra_temp1: number(q, "dewptf"),
            wind_speed: number(q, "windspeedmph"),
            wind_speed10: number(q, "windgustmph"),
            rain_incremental: number(q, "rainin"),
            day_rain: number(q, "dailyrainin"),
            extra_float1: number(q, "weeklyrainin"),
            month_rain: number(q, "monthlyrainin"),
            solar_watts: number(q, "solarradiation"),
            uv: number(q, "UV"),
            in_temp: number(q, "indoortempf"),
            in_humidity: number(q, "indoorhumidity"),
            barometer: number(q, "baromin"),
            temp1: number(q, "temp1f"),
            temp2: number(q, "temp2f"),
            temp3: number(q, "temp3f"),
            temp4: number(q, "temp4f"),
            temp5: number(q, "temp5f"),
            temp6: number(q, "temp6f"),
            temp7: number(q, "temp7f"),
            temp8: number(q, "temp8f"),
            temp9: number(q, "temp9f"),
            temp10: number(q, "temp10f"),
            soil_temp1: number(q, "soiltemp1f"),
            soil_temp2: number(q, "soiltemp2f"),
            soil_temp3: number(q, "soiltemp3f"),
            soil_temp4: number(q, "soiltemp4f"),
            soil_temp5: number(q, "soiltemp5f"),
            soil_temp6: number(q, "soiltemp6f"),
            soil_temp7: number(q, "soiltemp7f"),
            soil_temp8: number(q, "soiltemp8f"),
            soil_temp9: number(q, "soiltemp9f"),
            soil_temp10: number(q, "soiltemp10f"),
            humidity1: number(q, "humidity1"),
            humidity2: number(q, "humidity2"),
            humidity3: number(q, "humidity3"),
            humidity4: number(q, "humidity4"),
            humidity5: number(q, "humidity5"),
            humidity6: number(q, "humidity6"),
            humidity7: number(q, "humidity7"),
            humidity8: number(q, "humidity8"),
            humidity9: number(q, "humidity9"),
            humidity10: number(q, "humidity10"),
            soil_hum1: number(q, "soilhum1"),
            soil_hum2: number(q, "soilhum2"),
            soil_hum3: number(q, "soilhum3"),
            soil_hum4: number(q, "soilhum4"),
            soil_hum5: number(q, "soilhum5"),
            soil_hum6: number(q, "soilhum6"),
            soil_hum7: number(q, "soilhum7"),
            soil_hum8: number(q, "soilhum8"),
            soil_hum9: number(q, "soilhum9"),
            soil_hum10: number(q, "soilhum10"),
            leaf_wetness1: number(q, "leafwetness1"),
            leaf_wetness2: number(q, "leafwetness2"),
            leaf_wetness3: number(q, "leafwetness3"),
            leaf_wetness4: number(q, "leafwetness4"),
            leaf_wetness5: number(q, "leafwetness5"),
            leaf_wetness6: number(q, "leafwetness6"),
            leaf_wetness7: number(q, "leafwetness7"),
            leaf_wetness8: number(q, "leafwetness8"),
            soil_tens1: number(q, "soiltens1"),
            soil_tens2: number(q, "soiltens2"),
            soil_tens3: number(q, "soiltens3"),
            soil_tens4: number(q, "soiltens4"),
            etos: number(q, "etos"),
            etrs: number(q, "etrs"),
            barom_rel_in: number(q, "baromrelin"),
            barom_abs_in: number(q, "baromabsin"),
            pm25: number(q, "pm25"),
            pm25_24h: number(q, "pm25_24h"),
            pm25_in: number(q, "pm25_in"),
            pm25_in_24h: number(q, "pm25_in_24h"),
            pm25_in_aqin: number(q, "pm25_in_aqin"),
            pm25_in_24h_aqin: number(q, "pm25_in_24h_aqin"),
            pm10_in_aqin: number(q, "pm10_in_aqin"),
            pm10_in_24h_aqin: number(q, "pm10_in_24h_aqin"),
            co2: number(q, "co2"),
            pm_in_temp_aqin: number(q, "pm_in_temp_aqin"),
            lightning_distance: number(q, "lightning_distance"),
            gdd: integer(q, "gdd"),
            leak1: integer(q, "leak1"),
            leak2: integer(q, "leak2"),
            leak3: integer(q, "leak3"),
            leak4: integer(q, "leak4"),
            batt_out: integer(q, "battout"),
            batt_in: integer(q, "battin"),
            batt1: integer(q, "batt1"),
            batt2: integer(q, "batt2"),
            batt3: integer(q, "batt3"),
            batt4: integer(q, "batt4"),
            batt5: integer(q, "batt5"),
            batt6: integer(q, "batt6"),
            batt7: integer(q, "batt7"),
            batt8: integer(q, "batt8"),
            batt9: integer(q, "batt9"),
            batt10: integer(q, "batt10"),
            batt_25: integer(q, "batt_25"),
            batt_lightning: integer(q, "batt_lightning"),
            bat_leak1: integer(q, "batleak1"),
            bat_leak2: integer(q, "batleak2"),
            bat_leak3: integer(q, "batleak3"),
            bat_leak4: integer(q, "batleak4"),
            batt_sm1: integer(q, "battsm1"),
            batt_sm2: integer(q, "battsm2"),
            batt_sm3: integer(q, "battsm3"),
            batt_sm4: integer(q, "battsm4"),
            batt_co2: integer(q, "batt_co2"),
            batt_cellgateway: integer(q, "batt_cellgateway"),
            relay1: integer(q, "relay1"),
            relay2: integer(q, "relay2"),
            relay3: integer(q, "relay3"),
            relay4: integer(q, "relay4"),
            relay5: integer(q, "relay5"),
            relay6: integer(q, "relay6"),
            relay7: integer(q, "relay7"),
            relay8: integer(q, "relay8"),
            relay9: integer(q, "relay9"),
            relay10: integer(q, "relay10"),
            co2_in_aqin: integer(q, "co2_in_aqin"),
            co2_in_24h_aqin: integer(q, "co2_in_24h_aqin"),
            pm_in_humidity_aqin: integer(q, "pm_in_humidity_aqin"),
            aqi_pm25_aqin: integer(q, "aqi_pm25_aqin"),
            aqi_pm25_24h_aqin: integer(q, "aqi_pm25_24h_aqin"),
            aqi_pm10_aqin: integer(q, "aqi_pm10_aqin"),
            aqi_pm10_24h_aqin: integer(q, "aqi_pm10_24h_aqin"),
            aqi_pm25_in: integer(q, "aqi_pm25_in"),
            aqi_pm25_in_24h: integer(q, "aqi_pm25_in_24h"),
            lightning_day: integer(q, "lightning_day"),
            lightning_hour: integer(q, "lightning_hour"),
            // Every parameter above, and nothing else. See this module's fifth
            // disagreement for why `date_utc` is not here.
            ..Reading::default()
        };
        Ok(vec![PartialReading::at(at, r)])
    }
}

/// One parameter as a finite number.
fn number(params: &QueryParams, key: &str) -> Option<f32> {
    params
        .non_empty(key)
        .and_then(|text| text.parse::<f32>().ok())
        .filter(|value| value.is_finite())
}

/// One parameter as an integer.
///
/// Through `f64` for the values that arrive as `3.0`: Ambient sends `gdd` and the AQI
/// figures as integers and `co2_in_aqin` as either, and `"612.0".parse::<i32>()` fails --
/// which in Go leaves the field at zero and here would leave a sensor silently unreported.
fn integer(params: &QueryParams, key: &str) -> Option<i32> {
    let text = params.non_empty(key)?;
    if let Ok(value) = text.parse::<i32>() {
        return Some(value);
    }
    let value = text.parse::<f64>().ok().filter(|v| v.is_finite())?;
    // Out of an `i32`'s range is not an integer this column can hold, and `as` would
    // saturate it into a plausible wrong one.
    (value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX)).then_some(value as i32)
}

/// One parameter as text.
fn params_text(params: &QueryParams, key: &str) -> Option<String> {
    params.non_empty(key).map(str::to_string)
}

/// One parameter as an RFC 3339 instant, which is what `lightning_time` is.
fn instant(params: &QueryParams, key: &str) -> Option<DateTime<Utc>> {
    params
        .non_empty(key)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests;
