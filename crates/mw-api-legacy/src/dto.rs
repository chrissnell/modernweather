//! The 133-key DTO, the zero-fill, and Go's number format.
//!
//! **Every struct below was derived mechanically from `contract/legacy-v1.json`'s
//! `go_response_types`**, which W1 generated from the Go sources rather than transcribing.
//! The field order, the `serde(rename)` and the `omitempty` placement are therefore the Go
//! struct's, and [`tests::the_emitted_key_set_is_the_contracts`] reads that same file at
//! compile time and asserts it still is -- so this file cannot drift from the contract
//! without a test failure, which is the protection a hand-transcribed 133-key DTO would not
//! have.
//!
//! # What the contract's oddities are, all in one place
//!
//! * **Two duplicate pairs, emitted from one value.** `ohum`/`outhumidity` and
//!   `dayrain`/`rainday` (Q-1). `transform.go` sets both members of each pair from one
//!   field; the website reads one spelling and the iOS client decodes both, so neither may
//!   be dropped.
//! * **Naming that is inconsistent and must stay that way** (Q-2): `otemp`, `bar`, `winds`,
//!   `windd`, `itemp`, `ihum`, `windch`, `heatidx` run together; `period_rain` is
//!   snake_case; the four lunar keys are camelCase; everything else is lower-concatenated.
//! * **`ts` is milliseconds since the epoch as a bare JSON integer** (Q-3), 13 digits at
//!   present. Swift decodes it as `Int64`.
//! * **`snowdepth` is millimetres on `/latest` and inches on `/snow` and `/span`** (Q-4).
//!   Same key, same endpoint family, two units. The unit split is applied by the caller
//!   -- [`crate::render`] for `/latest`, [`crate::span`] and [`crate::snow`] for the other
//!   two -- because this module holds no conversion and the DTO cannot tell which endpoint
//!   it is being serialised for.
//! * **`omitempty`, exactly as Go means it**: a key is absent when its value is the type's
//!   zero. [`is_zero_f32`] and friends are that rule, and they are the reason
//!   `moonIllumination` really does vanish at a new moon -- which is what ships.
//! * **Go's number format** (Q-13): the shortest decimal that round-trips, so a decimal
//!   point appears if and only if the value has a fractional part. [`GoNumbers`] is that
//!   rule; `serde_json`'s own formatter writes `65.0` where Go writes `65`, and the
//!   conformance runner asserts the difference.
//!
//! # The zero-fill is [`zf`] and nothing else
//!
//! The domain type is `Option<f32>` end to end and the database stores `NULL` for a metric
//! a station did not report (**D-26**). The frozen contract's zeros are produced here, one
//! layer above storage, by one function -- which `ci/layers.toml`'s `one-zero-fill` rule
//! asserts by refusing `unwrap_or(0.` anywhere outside this crate.

use std::io;

use serde::Serialize;

/// The zero-fill: `None` becomes the contract's `0`.
///
/// **The only `unwrap_or` of a float zero in the workspace** (**D-10**,
/// `docs/service-architecture.md` §1.7). A second one anywhere else would mean storage and
/// presentation had been confused, and the symptom would be a zero in the database that
/// nothing can distinguish from a reading a sensor really sent.
///
/// It also rejects a value that is **not finite**, and that is not tidiness. Go's
/// `encoding/json` refuses `NaN` and `±Inf` outright, so the Go server's response for a
/// `NaN` column is a *failed encode* after the `200` header has been written -- a truncated
/// body. `serde_json` would write `null` instead, which fails the iOS decode of the whole
/// object because every number on this surface is a non-optional Swift property. Neither is
/// a response a frozen route may give (**D-48**), and a non-finite value is not a
/// measurement (**D-26**'s own standard), so it is filled like an absent one. Recorded as
/// **DEV-11**.
pub fn zf(v: Option<f32>) -> f32 {
    v.filter(|x| x.is_finite()).unwrap_or(0.0)
}

/// [`zf`] over a `f64`, for the two `/stationinfo` coordinates and the almanac's values.
///
/// A second function rather than a generic one, because the needle `ci/layers.toml` scans
/// for is a literal and a generic `T: Zero` would hide it from the gate that exists to
/// count these.
pub fn zf64(v: Option<f64>) -> f64 {
    v.filter(|x| x.is_finite()).unwrap_or(0.0)
}

/// Go's `omitempty` for a `float32`: the key is absent when the value is the zero value.
///
/// `*v == 0.0` rather than `v.abs() < EPSILON`: Go compares against the zero value, and
/// `-0.0 == 0.0` is true in both languages, so this is the same test.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

/// Go's `omitempty` for a `float64`.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

/// Go's `omitempty` for an integer.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// Go's `omitempty` for a `bool`: `false` is the zero value, so the key is absent.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn is_false(v: &bool) -> bool {
    !*v
}

/// `serde_json`'s formatter with Go's `encoding/json` float rendering.
///
/// Go writes the **shortest decimal that round-trips at the value's own width**, so a
/// decimal point appears if and only if the value has a fractional part: a `float32` field
/// emits `65` when its value is 65 and `65.3` when it is 65.3.
///
/// Two things about that are not what Rust's `Display` does, and this formatter exists for
/// both. Neither is visible to the conformance suite, which is why they are stated here
/// with the evidence rather than left to it.
///
/// **1. The digits.** `encoding/json` calls `strconv.AppendFloat(b, f, fmt, -1, bits)`,
/// which is ryu and breaks a shortest-representation tie **round-half-to-even**. `Display`
/// is Grisu/Dragon and breaks the same tie **round-half-up**, so every value whose exact
/// expansion ends in `…5` one digit past the shortest form gets a different last digit:
///
/// ```text
/// value          Go            Display
/// 46.6328125     46.632812     46.632813
/// -41.3828125    -41.382812    -41.382813
/// 2956007.25     2956007.2     2956007.3
/// ```
///
/// `-41.3828125` is `mw_pipeline::enrich::wind_chill(-17.0, 15.3)` -- an ordinary reading
/// from a station at −17 °F in a 15 mph wind, not a synthetic value.
///
/// **2. The format choice.** Go switches to exponent notation when the magnitude is nonzero
/// and outside `[1e-6, 1e21)`, and spells a one-digit negative exponent without its leading
/// zero: `9.782492e-7`, `1e+21`, `3.4028235e+38`. `Display` never uses an exponent at any
/// magnitude, writing `0.0000009782492` and twenty-two digits respectively. `moonIllumination`
/// genuinely enters that region for about ten minutes either side of every new moon.
///
/// **What the suite does and does not check.** `check_number_literals` walks every numeric
/// literal and fails a fixture whose integral value carries a decimal point, or whose
/// exponent appears *inside* `[1e-6, 1e21)` -- it never compares digits against the recorded
/// body, and it does not require an exponent outside that interval. `body_equals` would
/// catch all of it and no data-bearing fixture enables it. So this is pinned by
/// [`tests::the_digits_are_gos_digits_over_every_f32_bit_pattern_in_a_sweep`] instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoNumbers;

impl serde_json::ser::Formatter for GoNumbers {
    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f32) -> io::Result<()> {
        w.write_all(go_float32(value).as_bytes())
    }

    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(go_float64(value).as_bytes())
    }
}

/// `encoding/json`'s rendering of a `float32`.
pub fn go_float32(value: f32) -> String {
    let mut buf = ryu::Buffer::new();
    // `format_finite` is ryu's shortest round-tripping decimal, which is the same algorithm
    // and the same tie rule `strconv.AppendFloat(…, -1, 32)` uses. Non-finite cannot reach
    // here: `zf` maps it to zero (DEV-11) before the DTO is built, and `serde_json` writes
    // `null` for one rather than calling the formatter.
    let abs = value.abs();
    go_format(
        buf.format_finite(value),
        abs as f64,
        abs != 0.0 && !(1e-6..1e21).contains(&abs),
    )
}

/// `encoding/json`'s rendering of a `float64`. The thresholds are the same; only the width
/// the digits are computed at differs, which `ryu` takes from the type.
pub fn go_float64(value: f64) -> String {
    let mut buf = ryu::Buffer::new();
    let abs = value.abs();
    go_format(
        buf.format_finite(value),
        abs,
        abs != 0.0 && !(1e-6..1e21).contains(&abs),
    )
}

/// Re-spell ryu's output the way Go's `'f'` or `'e'` verb would.
///
/// ryu picks whichever of the two is shorter and Go picks by magnitude, so neither of them
/// can be used as-is: ryu writes `1e21` where Go writes `1e+21`, and `3.4028235e38` where Go
/// writes `3.4028235e+38`, and it writes `0.000001` where Go also writes `0.000001` but
/// `1e-7` where Go writes `1e-7`. Only the *digits* are taken from it.
fn go_format(shortest: &str, _abs: f64, exponent_form: bool) -> String {
    // Split ryu's output into a sign, the significant digits, and the decimal exponent of
    // the first digit -- `point` being how many digits belong before the decimal point.
    let (sign, rest) = match shortest.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", shortest),
    };
    let (mantissa, exp10) = match rest.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().expect("ryu writes a decimal exponent")),
        None => (rest, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((a, b)) => (a, b.trim_end_matches('0')),
        None => (mantissa, ""),
    };
    let all = format!("{int_part}{frac_part}");
    let digits = all.trim_start_matches('0');
    let leading_zeros = all.len() - digits.len();
    let point = int_part.len() as i32 + exp10 - leading_zeros as i32;
    if digits.is_empty() {
        // Zero, whose sign Go keeps: `-0` for a true negative zero.
        return format!("{sign}0");
    }
    let mut out = String::from(sign);
    if exponent_form {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        // Go's exponent is always signed and carries no leading zero: `e-7`, `e+21`. That
        // is `encoding/json`'s own post-pass over `strconv`'s `e-07`.
        let e = point - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.abs().to_string());
    } else if point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-point) as usize));
        out.push_str(digits);
    } else if point as usize >= digits.len() {
        out.push_str(digits);
        out.extend(std::iter::repeat_n('0', point as usize - digits.len()));
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
    out
}

/// Serialise a response body with Go's number format and a trailing newline.
///
/// **The newline is Q-12 and is asserted.** `json.NewEncoder(w).Encode` appends one, and
/// every hand-encoded endpoint on this surface goes through it. `/forecast` is the one that
/// does not, and it does not use this function -- see [`crate::forecast`].
pub fn to_json_line<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut out = Vec::with_capacity(4096);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, GoNumbers);
    value.serialize(&mut ser)?;
    out.push(b'\n');
    Ok(out)
}

/// Go's `time.Time` under `encoding/json`: RFC 3339 with a variable-width fraction.
///
/// `/almanac`'s `timestamp` is the only place on this surface a `time.Time` is marshalled.
/// Go emits RFC 3339 Nano, which **drops trailing zeros** in the fractional second and the
/// whole fraction when it is zero -- `2026-10-28T00:00:00Z`, which is what the fixture
/// recorded. `chrono`'s `SecondsFormat::AutoSi` rounds to 0, 3, 6 or 9 digits instead, so a
/// value with 10 ms would print `.010` where Go prints `.01`.
pub fn go_rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{SecondsFormat, Timelike};
    if t.nanosecond() == 0 {
        return t.to_rfc3339_opts(SecondsFormat::Secs, true);
    }
    let full = t.to_rfc3339_opts(SecondsFormat::Nanos, true);
    // `…T00:00:00.010000000Z` -> `…T00:00:00.01Z`: strip the zeros between the last
    // non-zero fractional digit and the `Z`.
    match full.split_once('.') {
        None => full,
        Some((head, tail)) => {
            let frac = tail.trim_end_matches('Z').trim_end_matches('0');
            if frac.is_empty() {
                format!("{head}Z")
            } else {
                format!("{head}.{frac}Z")
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LegacyWeatherReading {
    /// `stationname` -- Go `StationName string`.
    #[serde(rename = "stationname")]
    pub station_name: String,
    /// `stationtype` -- Go `StationType string`, `omitempty`.
    #[serde(rename = "stationtype", skip_serializing_if = "String::is_empty")]
    pub station_type: String,
    /// `ts` -- Go `ReadingTimestamp int64`.
    #[serde(rename = "ts")]
    pub reading_timestamp: i64,
    /// `otemp` -- Go `OutsideTemperature float32`.
    #[serde(rename = "otemp")]
    pub outside_temperature: f32,
    /// `extratemp1` -- Go `ExtraTemp1 float32`.
    #[serde(rename = "extratemp1")]
    pub extra_temp1: f32,
    /// `extratemp2` -- Go `ExtraTemp2 float32`.
    #[serde(rename = "extratemp2")]
    pub extra_temp2: f32,
    /// `extratemp3` -- Go `ExtraTemp3 float32`.
    #[serde(rename = "extratemp3")]
    pub extra_temp3: f32,
    /// `extratemp4` -- Go `ExtraTemp4 float32`.
    #[serde(rename = "extratemp4")]
    pub extra_temp4: f32,
    /// `extratemp5` -- Go `ExtraTemp5 float32`.
    #[serde(rename = "extratemp5")]
    pub extra_temp5: f32,
    /// `extratemp6` -- Go `ExtraTemp6 float32`.
    #[serde(rename = "extratemp6")]
    pub extra_temp6: f32,
    /// `extratemp7` -- Go `ExtraTemp7 float32`.
    #[serde(rename = "extratemp7")]
    pub extra_temp7: f32,
    /// `soiltemp1` -- Go `SoilTemp1 float32`.
    #[serde(rename = "soiltemp1")]
    pub soil_temp1: f32,
    /// `soiltemp2` -- Go `SoilTemp2 float32`.
    #[serde(rename = "soiltemp2")]
    pub soil_temp2: f32,
    /// `soiltemp3` -- Go `SoilTemp3 float32`.
    #[serde(rename = "soiltemp3")]
    pub soil_temp3: f32,
    /// `soiltemp4` -- Go `SoilTemp4 float32`.
    #[serde(rename = "soiltemp4")]
    pub soil_temp4: f32,
    /// `leaftemp1` -- Go `LeafTemp1 float32`.
    #[serde(rename = "leaftemp1")]
    pub leaf_temp1: f32,
    /// `leaftemp2` -- Go `LeafTemp2 float32`.
    #[serde(rename = "leaftemp2")]
    pub leaf_temp2: f32,
    /// `leaftemp3` -- Go `LeafTemp3 float32`.
    #[serde(rename = "leaftemp3")]
    pub leaf_temp3: f32,
    /// `leaftemp4` -- Go `LeafTemp4 float32`.
    #[serde(rename = "leaftemp4")]
    pub leaf_temp4: f32,
    /// `outhumidity` -- Go `OutHumidity float32`.
    #[serde(rename = "outhumidity")]
    pub out_humidity: f32,
    /// `extrahumidity1` -- Go `ExtraHumidity1 float32`.
    #[serde(rename = "extrahumidity1")]
    pub extra_humidity1: f32,
    /// `extrahumidity2` -- Go `ExtraHumidity2 float32`.
    #[serde(rename = "extrahumidity2")]
    pub extra_humidity2: f32,
    /// `extrahumidity3` -- Go `ExtraHumidity3 float32`.
    #[serde(rename = "extrahumidity3")]
    pub extra_humidity3: f32,
    /// `extrahumidity4` -- Go `ExtraHumidity4 float32`.
    #[serde(rename = "extrahumidity4")]
    pub extra_humidity4: f32,
    /// `extrahumidity5` -- Go `ExtraHumidity5 float32`.
    #[serde(rename = "extrahumidity5")]
    pub extra_humidity5: f32,
    /// `extrahumidity6` -- Go `ExtraHumidity6 float32`.
    #[serde(rename = "extrahumidity6")]
    pub extra_humidity6: f32,
    /// `extrahumidity7` -- Go `ExtraHumidity7 float32`.
    #[serde(rename = "extrahumidity7")]
    pub extra_humidity7: f32,
    /// `ohum` -- Go `OutsideHumidity float32`.
    #[serde(rename = "ohum")]
    pub outside_humidity: f32,
    /// `rainrate` -- Go `RainRate float32`.
    #[serde(rename = "rainrate")]
    pub rain_rate: f32,
    /// `rainincremental` -- Go `RainIncremental float32`.
    #[serde(rename = "rainincremental")]
    pub rain_incremental: f32,
    /// `period_rain` -- Go `PeriodRain float32`.
    pub period_rain: f32,
    /// `solarwatts` -- Go `SolarWatts float32`.
    #[serde(rename = "solarwatts")]
    pub solar_watts: f32,
    /// `potentialsolarwatts` -- Go `PotentialSolarWatts float32`.
    #[serde(rename = "potentialsolarwatts")]
    pub potential_solar_watts: f32,
    /// `solarjoules` -- Go `SolarJoules float32`.
    #[serde(rename = "solarjoules")]
    pub solar_joules: f32,
    /// `uv` -- Go `UV float32`.
    pub uv: f32,
    /// `radiation` -- Go `Radiation float32`.
    pub radiation: f32,
    /// `stormrain` -- Go `StormRain float32`.
    #[serde(rename = "stormrain")]
    pub storm_rain: f32,
    /// `dayrain` -- Go `DayRain float32`.
    #[serde(rename = "dayrain")]
    pub day_rain: f32,
    /// `monthrain` -- Go `MonthRain float32`.
    #[serde(rename = "monthrain")]
    pub month_rain: f32,
    /// `yearrain` -- Go `YearRain float32`.
    #[serde(rename = "yearrain")]
    pub year_rain: f32,
    /// `bar` -- Go `Barometer float32`.
    #[serde(rename = "bar")]
    pub barometer: f32,
    /// `rainfall24h` -- Go `Rainfall24h float32`.
    pub rainfall24h: f32,
    /// `rainfall48h` -- Go `Rainfall48h float32`.
    pub rainfall48h: f32,
    /// `rainfall72h` -- Go `Rainfall72h float32`.
    pub rainfall72h: f32,
    /// `rainfallstorm` -- Go `RainfallStorm float32`.
    #[serde(rename = "rainfallstorm")]
    pub rainfall_storm: f32,
    /// `winds` -- Go `WindSpeed float32`.
    #[serde(rename = "winds")]
    pub wind_speed: f32,
    /// `windgust` -- Go `WindGust float32`.
    #[serde(rename = "windgust")]
    pub wind_gust: f32,
    /// `windd` -- Go `WindDirection float32`.
    #[serde(rename = "windd")]
    pub wind_direction: f32,
    /// `windcard` -- Go `CardinalDirection string`, `omitempty`.
    #[serde(rename = "windcard", skip_serializing_if = "String::is_empty")]
    pub cardinal_direction: String,
    /// `rainday` -- Go `RainfallDay float32`.
    #[serde(rename = "rainday")]
    pub rainfall_day: f32,
    /// `windch` -- Go `WindChill float32`.
    #[serde(rename = "windch")]
    pub wind_chill: f32,
    /// `heatidx` -- Go `HeatIndex float32`.
    #[serde(rename = "heatidx")]
    pub heat_index: f32,
    /// `itemp` -- Go `InsideTemperature float32`.
    #[serde(rename = "itemp")]
    pub inside_temperature: f32,
    /// `ihum` -- Go `InsideHumidity float32`.
    #[serde(rename = "ihum")]
    pub inside_humidity: f32,
    /// `consbatteryvoltage` -- Go `ConsBatteryVoltage float32`.
    #[serde(rename = "consbatteryvoltage")]
    pub cons_battery_voltage: f32,
    /// `stationbatteryvoltage` -- Go `StationBatteryVoltage float32`.
    #[serde(rename = "stationbatteryvoltage")]
    pub station_battery_voltage: f32,
    /// `snowdepth` -- Go `SnowDepth float32`.
    #[serde(rename = "snowdepth")]
    pub snow_depth: f32,
    /// `snowdistance` -- Go `SnowDistance float32`.
    #[serde(rename = "snowdistance")]
    pub snow_distance: f32,
    /// `pm25` -- Go `PM25 float32`.
    pub pm25: f32,
    /// `co2` -- Go `CO2 float32`.
    pub co2: f32,
    /// `aqi_pm25_aqin` -- Go `AQIPM25AQIN float32`.
    pub aqi_pm25_aqin: f32,
    /// `aqi_pm10_aqin` -- Go `AQIPM10AQIN float32`.
    pub aqi_pm10_aqin: f32,
    /// `extrafloat1` -- Go `ExtraFloat1 float32`.
    #[serde(rename = "extrafloat1")]
    pub extra_float1: f32,
    /// `extrafloat2` -- Go `ExtraFloat2 float32`.
    #[serde(rename = "extrafloat2")]
    pub extra_float2: f32,
    /// `extrafloat3` -- Go `ExtraFloat3 float32`.
    #[serde(rename = "extrafloat3")]
    pub extra_float3: f32,
    /// `extrafloat4` -- Go `ExtraFloat4 float32`.
    #[serde(rename = "extrafloat4")]
    pub extra_float4: f32,
    /// `extrafloat5` -- Go `ExtraFloat5 float32`.
    #[serde(rename = "extrafloat5")]
    pub extra_float5: f32,
    /// `extrafloat6` -- Go `ExtraFloat6 float32`.
    #[serde(rename = "extrafloat6")]
    pub extra_float6: f32,
    /// `extrafloat7` -- Go `ExtraFloat7 float32`.
    #[serde(rename = "extrafloat7")]
    pub extra_float7: f32,
    /// `extrafloat8` -- Go `ExtraFloat8 float32`.
    #[serde(rename = "extrafloat8")]
    pub extra_float8: f32,
    /// `extrafloat9` -- Go `ExtraFloat9 float32`.
    #[serde(rename = "extrafloat9")]
    pub extra_float9: f32,
    /// `extrafloat10` -- Go `ExtraFloat10 float32`.
    #[serde(rename = "extrafloat10")]
    pub extra_float10: f32,
    /// `extratext1` -- Go `ExtraText1 string`, `omitempty`.
    #[serde(rename = "extratext1", skip_serializing_if = "String::is_empty")]
    pub extra_text1: String,
    /// `extratext2` -- Go `ExtraText2 string`, `omitempty`.
    #[serde(rename = "extratext2", skip_serializing_if = "String::is_empty")]
    pub extra_text2: String,
    /// `extratext3` -- Go `ExtraText3 string`, `omitempty`.
    #[serde(rename = "extratext3", skip_serializing_if = "String::is_empty")]
    pub extra_text3: String,
    /// `extratext4` -- Go `ExtraText4 string`, `omitempty`.
    #[serde(rename = "extratext4", skip_serializing_if = "String::is_empty")]
    pub extra_text4: String,
    /// `extratext5` -- Go `ExtraText5 string`, `omitempty`.
    #[serde(rename = "extratext5", skip_serializing_if = "String::is_empty")]
    pub extra_text5: String,
    /// `extratext6` -- Go `ExtraText6 string`, `omitempty`.
    #[serde(rename = "extratext6", skip_serializing_if = "String::is_empty")]
    pub extra_text6: String,
    /// `extratext7` -- Go `ExtraText7 string`, `omitempty`.
    #[serde(rename = "extratext7", skip_serializing_if = "String::is_empty")]
    pub extra_text7: String,
    /// `extratext8` -- Go `ExtraText8 string`, `omitempty`.
    #[serde(rename = "extratext8", skip_serializing_if = "String::is_empty")]
    pub extra_text8: String,
    /// `extratext9` -- Go `ExtraText9 string`, `omitempty`.
    #[serde(rename = "extratext9", skip_serializing_if = "String::is_empty")]
    pub extra_text9: String,
    /// `extratext10` -- Go `ExtraText10 string`, `omitempty`.
    #[serde(rename = "extratext10", skip_serializing_if = "String::is_empty")]
    pub extra_text10: String,
    /// `sunrise` -- Go `Sunrise string`, `omitempty`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sunrise: String,
    /// `sunset` -- Go `Sunset string`, `omitempty`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sunset: String,
    /// `moonPhaseName` -- Go `MoonPhaseName string`, `omitempty`.
    #[serde(rename = "moonPhaseName", skip_serializing_if = "String::is_empty")]
    pub moon_phase_name: String,
    /// `moonIllumination` -- Go `MoonIllumination float32`, `omitempty`.
    #[serde(rename = "moonIllumination", skip_serializing_if = "is_zero_f32")]
    pub moon_illumination: f32,
    /// `moonAge` -- Go `MoonAge float32`, `omitempty`.
    #[serde(rename = "moonAge", skip_serializing_if = "is_zero_f32")]
    pub moon_age: f32,
    /// `moonCrescentAngle` -- Go `MoonCrescentAngle float32`, `omitempty`.
    #[serde(rename = "moonCrescentAngle", skip_serializing_if = "is_zero_f32")]
    pub moon_crescent_angle: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SnowResponse {
    /// `stationname` -- Go `StationName string`.
    #[serde(rename = "stationname")]
    pub station_name: String,
    /// `snowdepth` -- Go `SnowDepth float32`.
    #[serde(rename = "snowdepth")]
    pub snow_depth: f32,
    /// `snowtoday` -- Go `SnowToday float32`.
    #[serde(rename = "snowtoday")]
    pub snow_today: f32,
    /// `snowlast24` -- Go `SnowLast24 float32`.
    #[serde(rename = "snowlast24")]
    pub snow_last24: f32,
    /// `snowlast72` -- Go `SnowLast72 float32`.
    #[serde(rename = "snowlast72")]
    pub snow_last72: f32,
    /// `snowseason` -- Go `SnowSeason float32`.
    #[serde(rename = "snowseason")]
    pub snow_season: f32,
    /// `snowstorm` -- Go `SnowStorm float32`.
    #[serde(rename = "snowstorm")]
    pub snow_storm: f32,
    /// `snowfallrate` -- Go `SnowfallRate float32`.
    #[serde(rename = "snowfallrate")]
    pub snowfall_rate: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StationInfoItem {
    /// `id` -- Go `ID int`.
    pub id: i64,
    /// `name` -- Go `Name string`.
    pub name: String,
    /// `type` -- Go `Type string`.
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StationInfo {
    /// `website_name` -- Go `WebsiteName string`.
    pub website_name: String,
    /// `about_text` -- Go `AboutText string`, `omitempty`.
    ///
    /// **DEV-10: the `omitempty` is gone, and that is the one deliberate deviation on
    /// this response.** `StationInfo.aboutText` is a NON-optional `String` in the iOS
    /// client, so Go omitting the key for a website with no about text makes
    /// `JSONDecoder` throw `keyNotFound` and fail the decode of the WHOLE object --
    /// losing the station list, the snow device, the air-quality device and the radar
    /// token, not just this field. Any portal website is permanently in that state.
    /// `docs/reconciliation.md` O-5 option (b), concurred by the owner: emitting the
    /// key cannot break a tolerant client, and the website guards its own read.
    pub about_text: String,
    /// `stations` -- Go `Stations []StationInfoItem`.
    pub stations: Vec<StationInfoItem>,
    /// `weather_device` -- Go `WeatherDevice *int`, `omitempty`.
    ///
    /// **DEV-10, the second half.** `0` when the website has no bound device, rather
    /// than an absent key: `StationInfo.weatherDevice` is a non-optional `Int` and the
    /// absence fails the same decode. See `about_text` above.
    pub weather_device: i64,
    /// `snow_device` -- Go `SnowDevice *string`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snow_device: Option<String>,
    /// `air_quality_device` -- Go `AirQualityDevice *string`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_quality_device: Option<String>,
    /// `latitude` -- Go `Latitude float64`, `omitempty`.
    #[serde(skip_serializing_if = "is_zero_f64")]
    pub latitude: f64,
    /// `longitude` -- Go `Longitude float64`, `omitempty`.
    #[serde(skip_serializing_if = "is_zero_f64")]
    pub longitude: f64,
    /// `radar_enabled` -- Go `RadarEnabled bool`, `omitempty`.
    #[serde(skip_serializing_if = "is_false")]
    pub radar_enabled: bool,
    /// `radar_token` -- Go `RadarToken string`, `omitempty`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub radar_token: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StationWebsite {
    /// `name` -- Go `Name string`.
    pub name: String,
    /// `hostname` -- Go `Hostname string`.
    pub hostname: String,
    /// `page_title` -- Go `PageTitle string`.
    pub page_title: String,
    /// `protocol` -- Go `Protocol string`.
    pub protocol: String,
    /// `port` -- Go `Port int`.
    pub port: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StationSummary {
    /// `id` -- Go `ID int`.
    pub id: i64,
    /// `name` -- Go `Name string`.
    pub name: String,
    /// `type` -- Go `Type string`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `latitude` -- Go `Latitude float64`.
    pub latitude: f64,
    /// `longitude` -- Go `Longitude float64`.
    pub longitude: f64,
    /// `enabled` -- Go `Enabled bool`.
    pub enabled: bool,
    /// `website` -- Go `Website *StationWebsiteData`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<StationWebsite>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AlmanacRecord {
    /// `value` -- Go `Value float32`.
    pub value: f32,
    /// `timestamp` -- Go `Timestamp time.Time`.
    pub timestamp: String,
    /// `wind_dir` -- Go `WindDir *float32`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_dir: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Almanac {
    /// `high_temp` -- Go `HighTemp *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_temp: Option<AlmanacRecord>,
    /// `low_temp` -- Go `LowTemp *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_temp: Option<AlmanacRecord>,
    /// `high_wind_speed` -- Go `HighWindSpeed *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_wind_speed: Option<AlmanacRecord>,
    /// `max_rain_hour` -- Go `MaxRainHour *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rain_hour: Option<AlmanacRecord>,
    /// `max_rain_day` -- Go `MaxRainDay *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rain_day: Option<AlmanacRecord>,
    /// `low_barometer` -- Go `LowBarometer *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_barometer: Option<AlmanacRecord>,
    /// `low_humidity` -- Go `LowHumidity *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_humidity: Option<AlmanacRecord>,
    /// `deepest_snow` -- Go `DeepestSnow *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deepest_snow: Option<AlmanacRecord>,
    /// `max_snow_hour` -- Go `MaxSnowHour *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_snow_hour: Option<AlmanacRecord>,
    /// `max_snow_day` -- Go `MaxSnowDay *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_snow_day: Option<AlmanacRecord>,
    /// `high_solar` -- Go `HighSolar *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_solar: Option<AlmanacRecord>,
    /// `high_pm25` -- Go `HighPM25 *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_pm25: Option<AlmanacRecord>,
    /// `high_pm10_in` -- Go `HighPM10In *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_pm10_in: Option<AlmanacRecord>,
    /// `high_co2` -- Go `HighCO2 *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_co2: Option<AlmanacRecord>,
    /// `high_tvoc` -- Go `HighTVOC *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_tvoc: Option<AlmanacRecord>,
    /// `high_aqi_pm25` -- Go `HighAQIPM25 *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_aqi_pm25: Option<AlmanacRecord>,
    /// `high_aqi_pm10` -- Go `HighAQIPM10 *AlmanacRecord`, `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_aqi_pm10: Option<AlmanacRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// `contract/legacy-v1.json`, read at **compile** time.
    ///
    /// The whole point: this file's structs were derived from that one, and an edit to
    /// either without the other is a test failure rather than a conformance failure
    /// discovered against a live daemon three minutes into a gate run.
    const CONTRACT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contract/legacy-v1.json"
    ));

    /// Every `json` tag the contract declares for one Go response type, in order.
    fn contract_keys(go_type: &str) -> Vec<(String, bool)> {
        let v: Value = serde_json::from_str(CONTRACT).expect("the contract is JSON");
        let fields = v["go_response_types"][go_type]
            .as_array()
            .unwrap_or_else(|| panic!("{go_type} is not a declared response type"));
        fields
            .iter()
            .map(|f| {
                (
                    f["json"].as_str().expect("a json tag").to_string(),
                    f["omitempty"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    }

    /// The keys a struct emits when **every** field is non-zero, in emission order.
    fn emitted(json: &str) -> Vec<String> {
        // `serde_json::Value` is a `BTreeMap` without `preserve_order`, so the order has
        // to be read out of the text. One pass over the top-level object's keys, which is
        // all these DTOs are.
        let mut keys = Vec::new();
        let bytes = json.as_bytes();
        let mut depth = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth -= 1,
                b'"' if depth == 1 => {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() && bytes[j] != b'"' {
                        j += if bytes[j] == b'\\' { 2 } else { 1 };
                    }
                    // A key is a string immediately followed by a colon.
                    if bytes.get(j + 1) == Some(&b':') {
                        keys.push(json[start..j].to_string());
                    }
                    i = j;
                }
                _ => {}
            }
            i += 1;
        }
        keys
    }

    /// **The DTO is the contract's key set, in the contract's order.**
    ///
    /// Both directions and both the full and the `omitempty`-pruned shape, which is what
    /// makes it worth running: a missing key fails the conformance suite, an extra key
    /// fails it, and a key in the wrong *position* fails it too, because six of the
    /// fixtures assert `json_key_order`.
    #[test]
    fn the_emitted_key_set_is_the_contracts() {
        let declared = contract_keys("WeatherReading");
        assert_eq!(
            declared.len(),
            88,
            "W1 recorded 88 json tags on WeatherReading"
        );

        // Every field non-zero, so nothing is pruned: the full declared set must come back
        // in the declared order.
        let full = everything_set();
        let bytes = to_json_line(&full).expect("the DTO serialises");
        let keys = emitted(std::str::from_utf8(&bytes).expect("utf-8"));
        let want: Vec<String> = declared.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, want, "the DTO's key order is the contract's");

        // Every field zero: exactly the `omitempty` ones disappear.
        let bytes = to_json_line(&LegacyWeatherReading::default()).expect("serialises");
        let keys = emitted(std::str::from_utf8(&bytes).expect("utf-8"));
        let want: Vec<String> = declared
            .iter()
            .filter(|(_, omitempty)| !omitempty)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(
            keys, want,
            "a zero-valued `omitempty` field is an ABSENT key, which is what Go means"
        );
    }

    /// The same check for the other five hand-encoded response types.
    #[test]
    fn every_other_response_type_is_the_contracts_too() {
        for (go_type, json) in [
            ("SnowReading", to_json_line(&SnowResponse::default())),
            ("StationInfoItem", to_json_line(&StationInfoItem::default())),
            (
                "StationWebsiteData",
                to_json_line(&StationWebsite::default()),
            ),
            ("AlmanacData", to_json_line(&Almanac::default())),
        ] {
            let bytes = json.expect("serialises");
            let keys = emitted(std::str::from_utf8(&bytes).expect("utf-8"));
            let want: Vec<String> = contract_keys(go_type)
                .into_iter()
                .filter(|(_, omitempty)| !omitempty)
                .map(|(k, _)| k)
                .collect();
            assert_eq!(keys, want, "{go_type}");
        }
    }

    /// **DEV-10 is visible here and nowhere else.** `/stationinfo`'s two hazard keys are
    /// present on a response where the Go server omits both, and the rest of the type is
    /// unchanged -- so the deviation is exactly two keys wide.
    #[test]
    fn the_stationinfo_deviation_is_exactly_two_keys() {
        let bytes = to_json_line(&StationInfo::default()).expect("serialises");
        let keys = emitted(std::str::from_utf8(&bytes).expect("utf-8"));
        assert_eq!(
            keys,
            vec!["website_name", "about_text", "stations", "weather_device"],
            "the two DEV-10 keys are emitted at their declared positions even when empty"
        );
        let declared: Vec<String> = contract_keys("StationInfoResponse")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let full = emitted(
            std::str::from_utf8(
                &to_json_line(&StationInfo {
                    website_name: "w".into(),
                    about_text: "a".into(),
                    stations: vec![StationInfoItem::default()],
                    weather_device: 1,
                    snow_device: Some("s".into()),
                    air_quality_device: Some("a".into()),
                    latitude: 1.0,
                    longitude: 2.0,
                    radar_enabled: true,
                    radar_token: "t".into(),
                })
                .expect("serialises"),
            )
            .expect("utf-8"),
        );
        // The nested `stations` element contributes no top-level key, so the two lists
        // compare directly.
        assert_eq!(full, declared);
    }

    /// Go's number format, which `check_number_literals` asserts on every numeric literal
    /// of every `json_shape` fixture.
    #[test]
    fn an_integral_float_has_no_decimal_point_and_a_fractional_one_does() {
        #[derive(Serialize)]
        struct N {
            a: f32,
            b: f32,
            c: f32,
            d: f64,
            e: f32,
            f: i64,
        }
        let bytes = to_json_line(&N {
            a: 0.0,
            b: 65.0,
            c: 23.730_469,
            d: -0.5,
            e: -0.0,
            f: 1_793_200_000_000,
        })
        .expect("serialises");
        assert_eq!(
            std::str::from_utf8(&bytes).expect("utf-8"),
            "{\"a\":0,\"b\":65,\"c\":23.730469,\"d\":-0.5,\"e\":-0,\"f\":1793200000000}\n"
        );
    }

    /// The zero-fill, including the non-finite arm **DEV-11** records.
    #[test]
    fn the_zero_fill_fills_an_absent_value_and_a_value_that_is_not_a_number() {
        assert_eq!(zf(Some(12.5)), 12.5);
        assert_eq!(zf(None), 0.0);
        assert_eq!(zf(Some(f32::NAN)), 0.0);
        assert_eq!(zf(Some(f32::INFINITY)), 0.0);
        assert_eq!(zf(Some(f32::NEG_INFINITY)), 0.0);
        assert_eq!(zf64(Some(f64::NAN)), 0.0);
        assert_eq!(zf64(Some(-1.5)), -1.5);
    }

    /// Go's `RFC3339Nano`, which drops trailing zeros rather than rounding to a multiple
    /// of three digits the way `chrono`'s `AutoSi` does.
    #[test]
    fn a_timestamp_is_go_rfc3339_with_a_variable_fraction() {
        use chrono::TimeZone as _;
        let at = |ns: u32| {
            go_rfc3339(
                chrono::Utc
                    .with_ymd_and_hms(2026, 10, 28, 0, 0, 0)
                    .single()
                    .expect("a real instant")
                    + chrono::Duration::nanoseconds(i64::from(ns)),
            )
        };
        assert_eq!(at(0), "2026-10-28T00:00:00Z");
        assert_eq!(at(10_000_000), "2026-10-28T00:00:00.01Z");
        assert_eq!(at(123_456_789), "2026-10-28T00:00:00.123456789Z");
    }

    /// Every field of the reading set to something non-zero, so that `omitempty` prunes
    /// nothing. Only the `omitempty` members need a value: the rest are emitted whatever
    /// they hold.
    fn everything_set() -> LegacyWeatherReading {
        let mut r = LegacyWeatherReading {
            station_name: "s".into(),
            station_type: "davis".into(),
            reading_timestamp: 1,
            cardinal_direction: "N".into(),
            sunrise: "7:30 AM".into(),
            sunset: "6:02 PM".into(),
            moon_phase_name: "Full Moon".into(),
            moon_illumination: 1.0,
            moon_age: 1.0,
            moon_crescent_angle: 1.0,
            ..Default::default()
        };
        // The ten `extratext` fields are the only remaining `omitempty` members.
        for slot in [
            &mut r.extra_text1,
            &mut r.extra_text2,
            &mut r.extra_text3,
            &mut r.extra_text4,
            &mut r.extra_text5,
            &mut r.extra_text6,
            &mut r.extra_text7,
            &mut r.extra_text8,
            &mut r.extra_text9,
            &mut r.extra_text10,
        ] {
            *slot = "x".into();
        }
        r
    }

    /// **Go's own renderings, for every value whose digits or format `Display` gets wrong.**
    ///
    /// Each `(bits, text)` pair was produced by `encoding/json.Marshal` on a `float32` of
    /// that exact bit pattern, with Go 1.21 on this machine. The bit pattern rather than a
    /// literal because the point of several of them is the last digit, and a Rust literal
    /// would be read back through Rust's own parser.
    ///
    /// The forty in the second group are tie cases: values whose exact binary expansion ends
    /// in `…5` one digit past the shortest form, where Go rounds to even and `Display`
    /// rounds up. Every one of them shipped a wrong last digit before this was fixed.
    const GO_FLOAT32: &[(u32, &str)] = &[
        // Ordinary values, including the integral ones `check_number_literals` is about.
        (0, "0"),
        (2147483648, "-0"),
        (1115815936, "65"),
        (1092616192, "10"),
        (1120403456, "100"),
        (1232348160, "1000000"),
        (1056964608, "0.5"),
        (1115855258, "65.3"),
        (3246915584, "-17"),
        (1098173645, "15.3"),
        (1036831949, "0.1"),
        (1045220557, "0.2"),
        (1050253722, "0.3"),
        (1051372203, "0.33333334"),
        (1232348159, "999999.94"),
        // The format choice: nonzero and outside [1e-6, 1e21) is exponent notation, and a
        // negative exponent carries no leading zero. `Display` writes none of these.
        (897988541, "0.000001"),
        (897900580, "9.9e-7"),
        (897797219, "9.782492e-7"),
        (869711765, "1e-7"),
        (874581936, "1.5e-7"),
        (746959707, "3.8e-12"),
        (1, "1e-45"),
        (1649989415, "1e+21"),
        (1658378023, "2e+21"),
        (2139095039, "3.4028235e+38"),
        (4286578687, "-3.4028235e+38"),
        // The tie cases named in the module documentation.
        (1111132160, "46.632812"),
        (1149780224, "1090.0312"),
        (1244949405, "2956007.2"),
        (3257239552, "-41.382812"),
        // Forty more, found by sweeping random f32 and keeping the ties.
        (1223258356, "478087.62"),
        (1222511892, "454760.62"),
        (1241846729, "2180338.2"),
        (1235308290, "1321440.2"),
        (1245350611, "3056308.8"),
        (1233860366, "1140449.8"),
        (1223241916, "477573.88"),
        (1243301699, "2544080.8"),
        (1241649509, "2131033.2"),
        (1248660689, "3883828.2"),
        (1217411764, "295381.62"),
        (1219874836, "372352.62"),
        (1219404516, "357655.12"),
        (1248915303, "3947481.8"),
        (1217144964, "287044.12"),
        (1210310296, "167802.38"),
        (1198479344, "61271.938"),
        (1245374017, "3062160.2"),
        (1243047963, "2480646.8"),
        (1192826640, "39191.062"),
        (1243025317, "2474985.2"),
        (1241533369, "2101998.2"),
        (1246767259, "3410470.8"),
        (1242608873, "2370874.2"),
        (1242511645, "2346567.2"),
        (1246054619, "3232310.8"),
        (1219827212, "370864.38"),
        (1247543253, "3604469.2"),
        (1248457093, "3832929.2"),
        (1187857696, "26274.562"),
        (1242113561, "2247046.2"),
        (1238609638, "1734108.8"),
        (1210812184, "175644.38"),
        (1221342668, "418222.38"),
        (1247210857, "3521370.2"),
        (1223791372, "494744.38"),
        (1247089611, "3491058.8"),
        (1198408080, "60993.562"),
        (1245302451, "3044268.8"),
        (1212058280, "195114.62"),
    ];

    #[test]
    fn the_digits_are_gos_digits_over_every_f32_bit_pattern_in_a_sweep() {
        for (bits, want) in GO_FLOAT32 {
            let v = f32::from_bits(*bits);
            assert_eq!(
                &go_float32(v),
                want,
                "bits {bits}: Go writes `{want}`, Display writes `{v}`"
            );
        }
    }

    /// `encoding/json` uses the same thresholds for a `float64`; only the width the digits
    /// are computed at differs.
    #[test]
    fn the_same_rules_apply_to_a_float64() {
        for (v, want) in [
            (9.782492e-7_f64, "9.782492e-7"),
            (1e21, "1e+21"),
            (1e-6, "0.000001"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (0.0, "0"),
            (10.0, "10"),
        ] {
            assert_eq!(go_float64(v), want, "{v}");
        }
    }

    /// The defect in the field it was found in: an ordinary station reading.
    ///
    /// `-17.0 °F` in a `15.3 mph` wind. `Display` writes `-41.382813`; the Go server wrote
    /// `-41.382812`, and so does this.
    #[test]
    fn an_ordinary_wind_chill_renders_the_way_the_go_server_rendered_it() {
        let wc = mw_pipeline::enrich::wind_chill(-17.0, 15.3).expect("in range");
        assert_eq!(
            wc.to_bits(),
            3257239552,
            "the arithmetic itself has not moved"
        );
        assert_eq!(go_float32(wc), "-41.382812");
        // Stated so the test says what it is guarding against.
        assert_eq!(wc.to_string(), "-41.382813");
    }

    /// Go-free, and over a hundred thousand values: whatever is written has to parse back to
    /// the same bits, and it may not be longer than the shortest form that does. That is
    /// every property except the tie direction, which the table above pins.
    /// The significant digits of a decimal rendering, in either notation: the mantissa's
    /// digits with the positional zeros at both ends removed.
    fn significand(s: &str) -> String {
        let mantissa = s.split('e').next().unwrap_or(s);
        let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
        digits
            .trim_start_matches('0')
            .trim_end_matches('0')
            .to_string()
    }

    #[test]
    fn every_rendering_round_trips_and_is_no_longer_than_it_has_to_be() {
        // A deterministic LCG rather than a dependency: `xtask`'s and this crate's
        // dependency lists are reviewed, and a test does not need a better generator.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut checked = 0u32;
        while checked < 100_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = f32::from_bits((state >> 32) as u32);
            if !v.is_finite() {
                continue;
            }
            checked += 1;
            let text = go_float32(v);
            let back: f32 = text.parse().unwrap_or_else(|e| panic!("`{text}`: {e}"));
            assert_eq!(
                back.to_bits(),
                v.to_bits(),
                "`{text}` does not round-trip to the value it came from"
            );
            let mut buf = ryu::Buffer::new();
            let shortest = buf.format_finite(v);
            // ryu writes the shortest round-tripping digit string. Re-spelling it changes the
            // punctuation, the exponent and the run of positional zeros, but never the
            // significand -- so that is what is compared, and a digit lost or gained fails
            // here even where the tie direction cannot be seen.
            assert_eq!(
                significand(&text),
                significand(shortest),
                "`{text}` and `{shortest}` do not carry the same significant digits"
            );
        }
    }
}
