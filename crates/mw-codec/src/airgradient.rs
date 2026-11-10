//! `AirGradientJson` — the body of `GET /measures/current` from an AirGradient ONE or
//! Open Air.
//!
//! An air-quality monitor rather than a weather station: W2a §4a.5 gives it
//! `HttpOut { /measures/current }`, `Fixed` identity and `Every { 3s }`, which is the
//! device's own measurement cadence and the Go driver's `pollInterval`. One response is one
//! observation, so the codec is one JSON object in and one reading out.
//!
//! # What lands where
//!
//! | JSON key | Metric | Note |
//! |---|---|---|
//! | `atmpCompensated`, else `atmp` | `outtemp` | °C on the wire, °F in the column |
//! | `rhumCompensated`, else `rhum` | `outhumidity` | % |
//! | `rco2` | `co2` | ppm |
//! | `pm02Compensated`, else `pm02` | `pm25` | µg/m³ |
//! | `pm01` | `extrafloat1` | µg/m³ |
//! | `pm10` | `extrafloat2` | µg/m³ |
//! | `tvocIndex` | `extrafloat3` | 1-500 |
//! | `noxIndex` | `extrafloat4` | 1-500 |
//! | `wifi` | `extrafloat5` | dBm |
//! | `pm003Count` | `extrafloat6` | particles/100 ml |
//! | `pm005Count` | `extrafloat7` | particles/100 ml |
//! | `pm01Count` | `extrafloat8` | particles/100 ml |
//! | `pm02Count` | `extrafloat9` | particles/100 ml |
//! | `serialno` | `extratext1` | |
//! | `model` | `extratext2` | |
//! | `firmware` | `extratext3` | |
//!
//! The nine `extrafloat` and three `extratext` assignments are the Go driver's
//! (`airgradient/station.go:169-192`) and are carried forward verbatim. They are a holding
//! pen rather than a model — `tvocIndex` is not "extra value 3" to anybody — and giving
//! each of them a registry metric of its own is a registry change with an owner decision
//! behind it, which this item is not. Recorded so the next reader knows the shape is
//! inherited.
//!
//! # Where this disagrees with the Go driver
//!
//! **1. An absent key is absent, and this is the driver where that matters most.** Go's
//! response struct is non-pointer `float32`, so a firmware that does not report
//! `atmpCompensated` leaves it `0.0` — and the driver then stores
//! `celsiusToFahrenheit(0.0)` = **32 °F**, every three seconds, for a device that is
//! working. That is not a gap an operator can see; it is a plausible number. **D-26**: a
//! key the device did not send is `None`.
//!
//! **2. The uncompensated field is read when the compensated one is absent.** Go reads only
//! `atmpCompensated`, `rhumCompensated` and `pm02Compensated`, which is right for current
//! firmware and is where the 32 °F comes from on older firmware that reports `atmp` and
//! `rhum` alone. The compensation is a firmware-side correction to the same sensor's
//! reading, so the raw field is the same quantity less one adjustment — which is a better
//! answer than both 32 °F and nothing. **This is a behavioural difference from the Go
//! driver and is deliberate**; it is recorded here and in the pull request rather than
//! discovered as a value diff in M5-2.

use mw_types::{PartialReading, Reading};
use serde::Deserialize;

use crate::frame::{Codec, CodecError, Frame};

/// The response body, as this build reads it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Measures {
    #[serde(default)]
    atmp: Option<f32>,
    #[serde(default)]
    atmp_compensated: Option<f32>,
    #[serde(default)]
    rhum: Option<f32>,
    #[serde(default)]
    rhum_compensated: Option<f32>,
    #[serde(default)]
    rco2: Option<f32>,
    #[serde(default)]
    pm01: Option<f32>,
    #[serde(default)]
    pm02: Option<f32>,
    #[serde(default)]
    pm02_compensated: Option<f32>,
    #[serde(default)]
    pm10: Option<f32>,
    #[serde(default, rename = "pm003Count")]
    pm003_count: Option<f32>,
    #[serde(default, rename = "pm005Count")]
    pm005_count: Option<f32>,
    #[serde(default, rename = "pm01Count")]
    pm01_count: Option<f32>,
    #[serde(default, rename = "pm02Count")]
    pm02_count: Option<f32>,
    #[serde(default)]
    tvoc_index: Option<f32>,
    #[serde(default)]
    nox_index: Option<f32>,
    #[serde(default)]
    wifi: Option<f32>,
    #[serde(default, rename = "serialno")]
    serial_no: Option<String>,
    #[serde(default)]
    firmware: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// The AirGradient codec. Stateless: the device reports in its own units and the mapping is
/// the same for every unit, so there is nothing per-station to hold.
#[derive(Debug, Clone, Copy, Default)]
pub struct AirGradientJson;

/// `GET /measures/current` — the path this codec's driver requests.
pub const MEASURES_CURRENT: &str = "/measures/current";

/// Degrees Celsius as degrees Fahrenheit.
fn fahrenheit(celsius: f32) -> f32 {
    celsius * 9.0 / 5.0 + 32.0
}

impl Codec for AirGradientJson {
    fn name(&self) -> &'static str {
        "airgradient-json"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        let m: Measures =
            serde_json::from_str(frame.as_str()?).map_err(|source| CodecError::Json { source })?;

        let r = Reading {
            out_temp: m.atmp_compensated.or(m.atmp).map(fahrenheit),
            out_humidity: m.rhum_compensated.or(m.rhum),
            co2: m.rco2,
            pm25: m.pm02_compensated.or(m.pm02),
            extra_float1: m.pm01,
            extra_float2: m.pm10,
            extra_float3: m.tvoc_index,
            extra_float4: m.nox_index,
            extra_float5: m.wifi,
            extra_float6: m.pm003_count,
            extra_float7: m.pm005_count,
            extra_float8: m.pm01_count,
            extra_float9: m.pm02_count,
            extra_text1: m.serial_no,
            extra_text2: m.model,
            extra_text3: m.firmware,
            // The table in this module's documentation, and nothing else. The AQI columns
            // are the enrich stage's (DEV-9) and the weather columns are no sensor this
            // device has.
            ..Reading::default()
        };

        // No `observed_at`: `/measures/current` is the device's current state and carries no
        // timestamp.
        Ok(vec![PartialReading::new(r)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A current-firmware body, with the compensated fields present.
    const CURRENT: &str = r#"{
        "pm01": 1.0, "pm02": 3.0, "pm10": 4.0, "pm02Compensated": 2.5,
        "pm003Count": 410.0, "pm005Count": 120.0, "pm01Count": 33.0, "pm02Count": 4.0,
        "atmp": 21.5, "atmpCompensated": 20.0, "rhum": 44.0, "rhumCompensated": 46.0,
        "rco2": 612.0, "tvocIndex": 101.0, "tvocRaw": 31000.0,
        "noxIndex": 1.0, "noxRaw": 16000.0, "wifi": -58.0,
        "serialno": "ecda3bbd1f2c", "firmware": "3.1.9", "model": "I-9PSL"
    }"#;

    fn decode(text: &str) -> Reading {
        let batch = AirGradientJson
            .decode(&Frame::new(text.as_bytes().to_vec()))
            .expect("the body decodes");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].observed_at, None);
        batch[0].values.clone()
    }

    #[test]
    fn every_key_the_table_names_lands_on_its_metric() {
        let r = decode(CURRENT);
        assert_eq!(r.out_temp, Some(68.0), "20 C is 68 F");
        assert_eq!(r.out_humidity, Some(46.0));
        assert_eq!(r.co2, Some(612.0));
        assert_eq!(r.pm25, Some(2.5), "the compensated particulate");
        assert_eq!(r.extra_float1, Some(1.0));
        assert_eq!(r.extra_float2, Some(4.0));
        assert_eq!(r.extra_float3, Some(101.0));
        assert_eq!(r.extra_float4, Some(1.0));
        assert_eq!(r.extra_float5, Some(-58.0));
        assert_eq!(r.extra_float6, Some(410.0));
        assert_eq!(r.extra_float7, Some(120.0));
        assert_eq!(r.extra_float8, Some(33.0));
        assert_eq!(r.extra_float9, Some(4.0));
        assert_eq!(r.extra_text1.as_deref(), Some("ecda3bbd1f2c"));
        assert_eq!(r.extra_text2.as_deref(), Some("I-9PSL"));
        assert_eq!(r.extra_text3.as_deref(), Some("3.1.9"));
        // Nothing this device measures, and nothing the enrich stage has not run yet.
        assert_eq!(r.barometer, None);
        assert_eq!(r.wind_speed, None);
        assert_eq!(r.aqi_pm25_in, None);
        // `tvocRaw` and `noxRaw` are in the Go struct and on no metric, in either
        // implementation.
        assert_eq!(r.extra_float10, None);
    }

    /// The 32 °F defect, as the test that distinguishes the two implementations.
    #[test]
    fn a_firmware_with_no_compensated_temperature_reads_the_raw_one_and_never_thirty_two() {
        let r = decode(r#"{"atmp": 21.5, "rhum": 44.0, "pm02": 3.0, "rco2": 612.0}"#);
        assert_eq!(r.out_temp, Some(70.7), "21.5 C is 70.7 F");
        assert_eq!(r.out_humidity, Some(44.0));
        assert_eq!(r.pm25, Some(3.0));
        // What the Go driver stores for the same body.
        assert_eq!(fahrenheit(0.0), 32.0);
    }

    #[test]
    fn a_device_that_reports_no_temperature_at_all_reports_none() {
        let r = decode(r#"{"rco2": 612.0, "wifi": -58.0}"#);
        assert_eq!(r.out_temp, None);
        assert_eq!(r.out_humidity, None);
        assert_eq!(r.pm25, None);
        assert_eq!(r.co2, Some(612.0));
    }

    /// A reading with nothing populated is still returned: "no metric was populated" is
    /// admission's `Reject::EmptyReading`, counted there with every other refusal.
    #[test]
    fn an_empty_object_decodes_to_a_reading_admission_will_refuse() {
        let batch = AirGradientJson
            .decode(&Frame::new(b"{}".to_vec()))
            .expect("structurally valid");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].values, Reading::default());
    }

    #[test]
    fn a_body_that_is_not_the_json_this_decodes_is_a_parse_failure() {
        let e = AirGradientJson
            .decode(&Frame::new(b"<html>404</html>".to_vec()))
            .expect_err("not JSON");
        assert!(matches!(e, CodecError::Json { .. }), "{e}");
        // A field of the wrong type is a parse failure too, which is the right answer: a
        // string where a measurement goes is not a measurement.
        let e = AirGradientJson
            .decode(&Frame::new(br#"{"rco2":"nan"}"#.to_vec()))
            .expect_err("wrong type");
        assert!(matches!(e, CodecError::Json { .. }), "{e}");
    }

    #[test]
    fn the_codec_names_itself_and_claims_no_station() {
        assert_eq!(AirGradientJson.name(), "airgradient-json");
        assert_eq!(
            AirGradientJson.claimed_station(&Frame::new(CURRENT.as_bytes().to_vec())),
            None
        );
        assert_eq!(MEASURES_CURRENT, "/measures/current");
    }
}
