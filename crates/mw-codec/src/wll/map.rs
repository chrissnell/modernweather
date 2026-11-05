//! Applying one `conditions` record to one [`Reading`], role by role.
//!
//! # Units: the device's, which are also the registry's
//!
//! A WeatherLink Live reports **°F, inHg, mph and W/m²**, and `metrics/registry.toml`
//! declares `outtemp` as `degF`, `barometer`/`baromrelin`/`baromabsin` as `inHg`,
//! `windspeed` as `mph` and `radiation` as `W_per_m2`. So the mapping below is almost
//! entirely a field copy, and that is the correct answer rather than a missing step.
//!
//! **This is a behavioural change against the Go driver, and the Go driver is wrong.**
//! `data_transformer.go` converts every temperature `(F - 32) * 5 / 9` and every pressure
//! `* 33.8639`, writing Celsius into a column the registry and the frozen contract both
//! declare as Fahrenheit and hPa into one declared as inHg. The Go Davis driver on the same
//! schema stores °F (`convBigVal10` of the LOOP packet's tenths of a degree) and
//! `/latest` emits `otemp` with no conversion, so the two drivers disagree about what the
//! column means and the WeatherLink Live one disagrees with the contract. Recorded as
//! **DEV-8** in `docs/reconciliation.md` §3.
//!
//! # Rain: counts are not inches
//!
//! `rainfall_daily` and `rain_rate_last` are **tip counts**, and `rain_size` is the size of
//! one tip — 0.01 in, 0.2 mm, 0.1 mm or 0.001 in. The Go driver copies the counts straight
//! into `dayrain` and `rainrate`, which `registry.toml` declares as `in` and
//! `in_per_hour`: a 0.2 mm collector then reports 20× the rainfall it measured, and a
//! 0.01 in collector 100×. Converting needs `rain_size`, so a record that omits it leaves
//! both metrics **`None`** rather than storing a number in unknown units — which is
//! **D-26** applied to the one case where a plausible wrong value is worse than a gap.
//! Also **DEV-8**.

use mw_types::Reading;

use super::mapping::{SensorEntry, SensorRole};
use super::wire::Condition;

/// Millimetres per inch, for the two metric rain collectors.
const MM_PER_INCH: f64 = 25.4;

/// Apply one record to `reading` for one mapping entry.
///
/// `th_index` is how many `th` entries have already matched a record, which is what makes
/// the first one outdoor and the rest `extratemp1..7` (see
/// [`SensorMapping`](super::SensorMapping)).
pub(crate) fn apply(reading: &mut Reading, entry: &SensorEntry, c: &Condition, th_index: usize) {
    match entry.role {
        SensorRole::Th => temp_humidity(reading, c, th_index),
        SensorRole::Wind => wind(reading, c),
        SensorRole::Rain => rain(reading, c),
        SensorRole::Solar => reading.radiation = f32s(c.solar_rad),
        SensorRole::Uv => reading.uv = f32s(c.uv_index),
        SensorRole::WindChill => reading.wind_chill = f32s(c.wind_chill),
        SensorRole::Thw => {
            reading.extra_float3 = f32s(c.thw_index);
            if entry.app_temp {
                reading.extra_float5 = f32s(c.thw_index);
            }
        }
        SensorRole::Thsw => {
            reading.extra_float4 = f32s(c.thsw_index);
            if entry.app_temp {
                reading.extra_float5 = f32s(c.thsw_index);
            }
        }
        SensorRole::Baro => {
            reading.barom_abs_in = f32s(c.bar_absolute);
            reading.barom_rel_in = f32s(c.bar_sea_level);
            // `barometer` is the sea-level value, which is what every consumer of the
            // frozen `bar` key means by "the pressure".
            reading.barometer = f32s(c.bar_sea_level);
        }
        SensorRole::ThIndoor => {
            reading.in_temp = f32s(c.temp_in);
            reading.in_humidity = f32s(c.hum_in);
            reading.extra_float6 = f32s(c.dew_point_in);
            reading.extra_float7 = f32s(c.heat_index_in);
        }
        SensorRole::SoilTemp => soil_temp(reading, c, entry.port),
        SensorRole::SoilMoist => soil_moisture(reading, c, entry.port),
        SensorRole::LeafWet => leaf_wetness(reading, c, entry.port),
        SensorRole::Battery => battery(reading, c, entry.txid),
    }
}

/// Apply a `th` entry at its occurrence index: 0 is outdoor, 1-7 are `extratemp1..7`.
///
/// **The index is consumed when the entry matched a record, not when the record carried a
/// value**, and the distinction is load-bearing. The Go transformer increments its counter
/// immediately after `applyTempHumidity` returns (`data_transformer.go:30`), and
/// `applyTempHumidity`'s `if tempC != nil` guards are *inside* it -- so a configured
/// transmitter that is present in the payload with null fields still consumes its slot.
///
/// That is the behaviour to reproduce, and reproducing it is not merely fidelity. A
/// WeatherLink Live reports a configured-but-unheard transmitter as a **present record with
/// null fields** (`rx_state: 2`), which is what an ordinary dropout looks like. An
/// implementation that skipped the index in that case would move the *next* `th` sensor's
/// temperature into `outtemp` -- the most-read column on the frozen contract -- for as long
/// as the outdoor transmitter was out of sync, and move it back when it returned. A gap is
/// the right answer there; a plausible wrong number is not.
///
/// A `th` entry that matches **no** record is a different case and does not consume an
/// index: `findCondition` returning nil makes the Go loop `continue` before the increment,
/// and `Codec::decode` does the same.
fn temp_humidity(reading: &mut Reading, c: &Condition, index: usize) {
    let temp = f32s(c.temp);
    let hum = f32s(c.hum);
    match index {
        0 => {
            reading.out_temp = temp;
            reading.out_humidity = hum;
            // Three values the unit computes and the registry has no metric for. The slots
            // are the Go driver's, so a side-by-side comparison of the two daemons (M5-2)
            // compares the same column.
            reading.extra_float1 = f32s(c.dew_point);
            reading.extra_float2 = f32s(c.wet_bulb);
            reading.heat_index = f32s(c.heat_index);
        }
        1 => {
            reading.extra_temp1 = temp;
            reading.extra_humidity1 = hum;
        }
        2 => {
            reading.extra_temp2 = temp;
            reading.extra_humidity2 = hum;
        }
        3 => {
            reading.extra_temp3 = temp;
            reading.extra_humidity3 = hum;
        }
        4 => {
            reading.extra_temp4 = temp;
            reading.extra_humidity4 = hum;
        }
        5 => {
            reading.extra_temp5 = temp;
            reading.extra_humidity5 = hum;
        }
        6 => {
            reading.extra_temp6 = temp;
            reading.extra_humidity6 = hum;
        }
        7 => {
            reading.extra_temp7 = temp;
            reading.extra_humidity7 = hum;
        }
        // The eighth and later `th` entries have nowhere to go: the registry has
        // `extratemp1..7`. Dropped rather than overwriting the seventh, which is what an
        // `index.min(7)` would do -- and silently merging two transmitters into one column
        // is worse than not storing one of them.
        _ => {}
    }
}

fn wind(reading: &mut Reading, c: &Condition) {
    reading.wind_speed = f32s(c.wind_speed_last);
    reading.wind_dir = f32s(c.wind_dir_last);
    // The ten-minute average has a column of its own -- `windspeed10`, which the Davis
    // driver fills from the LOOP packet -- and the unit reports it in both transports. The
    // Go WeatherLink Live driver does not map it; there is no decision behind that, the
    // field simply is not in its `Condition`.
    reading.wind_speed10 = f32s(c.wind_speed_avg_last_10_min);
}

fn rain(reading: &mut Reading, c: &Condition) {
    // Without the collector size there is no way to turn a count into a depth. See the
    // module documentation: `None`, not a number in unknown units.
    let Some(inches_per_tip) = tip_inches(c.rain_size) else {
        return;
    };
    reading.day_rain = f32s(c.rainfall_daily.map(|n| n * inches_per_tip));
    reading.rain_rate = f32s(c.rain_rate_last.map(|n| n * inches_per_tip));
}

/// How much one tip of the rain collector is, in inches.
///
/// `None` for an absent or unrecognised `rain_size`: a fifth code would be a collector this
/// build does not know, and guessing at it is how a rain total becomes wrong by a factor of
/// twenty.
fn tip_inches(rain_size: Option<i64>) -> Option<f64> {
    match rain_size? {
        1 => Some(0.01),
        2 => Some(0.2 / MM_PER_INCH),
        3 => Some(0.1 / MM_PER_INCH),
        4 => Some(0.001),
        _ => None,
    }
}

fn soil_temp(reading: &mut Reading, c: &Condition, port: Option<u8>) {
    let (target, value) = match port {
        Some(1) => (&mut reading.soil_temp1, c.temp_1),
        Some(2) => (&mut reading.soil_temp2, c.temp_2),
        Some(3) => (&mut reading.soil_temp3, c.temp_3),
        Some(4) => (&mut reading.soil_temp4, c.temp_4),
        // Unreachable through `SensorMapping::parse`, which requires a port in 1..=4 for
        // this role. Not an `unwrap`: a panic in a codec is a daemon that exits on a
        // firmware quirk.
        _ => return,
    };
    *target = f32s(value);
}

fn soil_moisture(reading: &mut Reading, c: &Condition, port: Option<u8>) {
    let (target, value) = match port {
        Some(1) => (&mut reading.soil_moisture1, c.moist_soil_1),
        Some(2) => (&mut reading.soil_moisture2, c.moist_soil_2),
        Some(3) => (&mut reading.soil_moisture3, c.moist_soil_3),
        Some(4) => (&mut reading.soil_moisture4, c.moist_soil_4),
        _ => return,
    };
    *target = f32s(value);
}

fn leaf_wetness(reading: &mut Reading, c: &Condition, port: Option<u8>) {
    let (target, value) = match port {
        Some(1) => (&mut reading.leaf_wetness1, c.wet_leaf_1),
        Some(2) => (&mut reading.leaf_wetness2, c.wet_leaf_2),
        _ => return,
    };
    *target = f32s(value);
}

fn battery(reading: &mut Reading, c: &Condition, txid: Option<u8>) {
    let flag = c.trans_battery_flag.map(i32s);
    let target = match txid {
        Some(1) => &mut reading.batt1,
        Some(2) => &mut reading.batt2,
        Some(3) => &mut reading.batt3,
        Some(4) => &mut reading.batt4,
        Some(5) => &mut reading.batt5,
        Some(6) => &mut reading.batt6,
        Some(7) => &mut reading.batt7,
        Some(8) => &mut reading.batt8,
        _ => return,
    };
    *target = flag;
}

/// The payload's `f64` as the domain type's `f32`, preserving `None`.
///
/// `f32` is the registry's `float4`, which is the column width; narrowing here rather than
/// at the writer keeps the one lossy step in the one place that knows the column type.
///
/// **A non-finite value becomes `None`.** A device on a LAN is untrusted input, and
/// `{"temp": -1e39}` narrows to `-inf`, which is a perfectly valid `float4`: it reaches the
/// store and then poisons `mean`, `min` and `max` over every window that contains it
/// (M3-4). Dropping it is the same choice already made for rainfall with no `rain_size` --
/// a gap is recoverable and a poisoned aggregate is not -- and it is **D-26**'s rule read in
/// the only direction that makes sense here: a value the instrument cannot have produced is
/// not a reading.
fn f32s(value: Option<f64>) -> Option<f32> {
    value.map(|n| n as f32).filter(|n| n.is_finite())
}

/// The payload's `i64` as the domain type's `i32`, saturating.
///
/// Saturating rather than wrapping: the four battery flags are 0-3, so a value that needs
/// saturating is a firmware fault, and `i32::MAX` is visibly wrong where a wrapped value
/// would be plausibly wrong.
fn i32s(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wll::mapping::SensorMapping;

    fn entry(text: &str) -> SensorEntry {
        SensorMapping::parse(text)
            .expect("a literal mapping")
            .entries()[0]
    }

    #[test]
    fn a_tip_count_becomes_inches_for_each_declared_collector() {
        assert_eq!(tip_inches(Some(1)), Some(0.01));
        assert_eq!(tip_inches(Some(4)), Some(0.001));
        assert_eq!(tip_inches(Some(2)), Some(0.2 / 25.4));
        assert_eq!(tip_inches(Some(3)), Some(0.1 / 25.4));
        assert_eq!(tip_inches(None), None, "no size, no depth");
        assert_eq!(tip_inches(Some(5)), None, "an unknown collector");
    }

    #[test]
    fn rain_without_a_collector_size_stores_nothing() {
        let c = Condition {
            rainfall_daily: Some(31.0),
            rain_rate_last: Some(4.0),
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("rain:1"), &c, 0);
        assert_eq!(r.day_rain, None, "a count is not a depth");
        assert_eq!(r.rain_rate, None);
    }

    #[test]
    fn a_metric_collector_is_converted_rather_than_copied() {
        let c = Condition {
            rain_size: Some(2),
            rainfall_daily: Some(10.0),
            rain_rate_last: Some(5.0),
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("rain:1"), &c, 0);
        // Ten tips of 0.2 mm is 2 mm, which is 0.07874 in.
        let day = r.day_rain.expect("populated");
        assert!((day - 0.078_74).abs() < 1e-5, "{day}");
        let rate = r.rain_rate.expect("populated");
        assert!((rate - 0.039_37).abs() < 1e-5, "{rate}");
    }

    #[test]
    fn temperature_and_pressure_are_not_converted() {
        let c = Condition {
            temp: Some(71.2),
            hum: Some(43.0),
            bar_sea_level: Some(29.92),
            bar_absolute: Some(24.1),
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("th:1"), &c, 0);
        apply(&mut r, &entry("baro"), &c, 0);
        assert_eq!(r.out_temp, Some(71.2), "degF, as the registry declares");
        assert_eq!(r.out_humidity, Some(43.0));
        assert_eq!(r.barometer, Some(29.92), "inHg, as the registry declares");
        assert_eq!(r.barom_rel_in, Some(29.92));
        assert_eq!(r.barom_abs_in, Some(24.1));
    }

    #[test]
    fn the_th_index_chooses_the_column() {
        let c = Condition {
            temp: Some(50.0),
            hum: Some(60.0),
            ..Condition::default()
        };
        for (index, expected) in [(1usize, 1), (7, 7)] {
            let mut r = Reading::default();
            temp_humidity(&mut r, &c, index);
            let got = match expected {
                1 => r.extra_temp1,
                _ => r.extra_temp7,
            };
            assert_eq!(got, Some(50.0), "index {index}");
            assert_eq!(r.out_temp, None, "only index 0 is outdoor");
        }
        // An eighth sensor is dropped rather than merged into the seventh.
        let mut r = Reading::default();
        temp_humidity(&mut r, &c, 8);
        assert_eq!(r, Reading::default());
    }

    /// The index accounting the Go transformer has, stated as the case that distinguishes
    /// it from the obvious wrong answer: a present record with null fields.
    #[test]
    fn a_present_record_with_null_fields_still_consumes_its_th_index() {
        let silent = Condition {
            data_structure_type: Some(1),
            txid: Some(1),
            // `rx_state: 2` -- configured, not heard from. The record is there; the values
            // are not.
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("th:1"), &silent, 0);
        assert_eq!(
            r,
            Reading::default(),
            "nothing to store from a silent sensor"
        );
        // ... and the next entry is therefore index 1, not index 0.
        let reporting = Condition {
            data_structure_type: Some(1),
            txid: Some(2),
            temp: Some(44.0),
            hum: Some(80.0),
            ..Condition::default()
        };
        apply(&mut r, &entry("th:2"), &reporting, 1);
        assert_eq!(r.extra_temp1, Some(44.0));
        assert_eq!(
            r.out_temp, None,
            "the outdoor column stays empty rather than taking the extra sensor's value"
        );
    }

    /// A device on a LAN is untrusted input, and `float4` has room for an infinity.
    #[test]
    fn a_value_the_instrument_cannot_have_produced_is_not_a_reading() {
        let hostile = Condition {
            temp: Some(-1e39),
            hum: Some(f64::NAN),
            rain_size: Some(1),
            rainfall_daily: Some(1e308),
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("th:1"), &hostile, 0);
        apply(&mut r, &entry("rain:1"), &hostile, 0);
        assert_eq!(
            r.out_temp, None,
            "-1e39 narrows to -inf, which is not a reading"
        );
        assert_eq!(r.out_humidity, None, "nor is NaN");
        assert_eq!(r.day_rain, None, "nor is an overflowing rain total");
        // A finite value at the edge of `f32` still gets through: the filter is about
        // representability, not about plausibility, which is the enrich stage's question.
        let edge = Condition {
            temp: Some(-40.0),
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("th:1"), &edge, 0);
        assert_eq!(r.out_temp, Some(-40.0));
    }

    #[test]
    fn a_battery_flag_lands_on_its_transmitter() {
        let c = Condition {
            trans_battery_flag: Some(1),
            ..Condition::default()
        };
        let mut r = Reading::default();
        apply(&mut r, &entry("battery:3"), &c, 0);
        assert_eq!(r.batt3, Some(1));
        assert_eq!(r.batt1, None);
        assert_eq!(i32s(i64::MAX), i32::MAX);
    }
}
