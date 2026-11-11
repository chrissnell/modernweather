//! Level 1 of W3 **D-82**'s three, for AirGradient: the pure codec against **captured**
//! bodies.
//!
//! The three bodies were captured from `tests/emulators/airgradient`, which is written in
//! this repository — so unlike the Davis and Campbell fixtures they pin the decoder against
//! an emulator written beside the driver, and cannot catch a misunderstanding the two share.
//! `frames/README.md` records that and `tests/emulators/manifest.toml` is where the
//! `driver-emulators` gate prints it on every run.
//!
//! What they *do* pin is the two firmware shapes, which is where the Go driver stores a
//! plausible wrong number.

use mw_codec::{AirGradientJson, Codec, CodecError, Frame, MEASURES_CURRENT};

const CURRENT: &str = include_str!("frames/airgradient-current.json");
const OLD_FIRMWARE: &str = include_str!("frames/airgradient-old-firmware.json");
const NO_SENSOR: &str = include_str!("frames/airgradient-no-sensor.json");

fn decode(body: &str) -> mw_types::Reading {
    let batch = AirGradientJson
        .decode(&Frame::new(body.as_bytes().to_vec()))
        .expect("a captured body decodes");
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].observed_at, None,
        "`/measures/current` is the device's current state and carries no instant"
    );
    batch[0].values.clone()
}

/// Current firmware: the compensated pair is what is read, and the twelve holding-pen
/// slots the Go driver files the rest in are filled.
#[test]
fn a_current_firmware_body_fills_every_metric_the_table_names() {
    let r = decode(CURRENT);
    // °C on the wire, °F in the column. The compensated value is 1.5 °C below the raw one in
    // the emulator, so "the compensated field is the one read" is checkable from the body.
    let temp = r.out_temp.expect("a temperature");
    assert!((60.0..=75.0).contains(&temp), "{temp} degF");
    let humidity = r.out_humidity.expect("a humidity");
    assert!((40.0..=55.0).contains(&humidity), "{humidity}%");
    assert!(
        matches!(r.co2, Some(c) if (500.0..=700.0).contains(&c)),
        "{:?}",
        r.co2
    );
    assert!(r.pm25.is_some(), "the compensated particulate");
    // The nine `extrafloat` and three `extratext` slots, which are the Go driver's filing
    // and are carried forward verbatim.
    assert!(r.extra_float1.is_some(), "pm01");
    assert!(r.extra_float2.is_some(), "pm10");
    assert!(r.extra_float3.is_some(), "tvocIndex");
    assert!(r.extra_float4.is_some(), "noxIndex");
    assert_eq!(r.extra_float5, Some(-58.0), "the WiFi signal, in dBm");
    assert!(r.extra_float6.is_some(), "pm003Count");
    assert!(r.extra_float9.is_some(), "pm02Count");
    assert_eq!(r.extra_text1.as_deref(), Some("ecda3bbd1f2c"));
    assert_eq!(r.extra_text2.as_deref(), Some("I-9PSL"));
    assert_eq!(r.extra_text3.as_deref(), Some("3.1.9"));
    // `tvocRaw` and `noxRaw` are in the Go struct and on no metric, in either
    // implementation.
    assert_eq!(r.extra_float10, None);
    // The AQI columns are the enrich stage's (DEV-9), not this codec's.
    assert_eq!(r.aqi_pm25_in, None);
    assert_eq!(r.aqi_pm25_aqin, None);
    // Nothing this device measures.
    assert_eq!(r.barometer, None);
    assert_eq!(r.wind_speed, None);
}

/// **The 32 °F defect, on a captured body.** The older firmware's body has no
/// `*Compensated` field at all, which is what makes `celsiusToFahrenheit(0.0)` the Go
/// driver's answer.
#[test]
fn an_older_firmwares_body_reads_the_raw_pair_and_never_thirty_two() {
    assert!(
        !OLD_FIRMWARE.contains("Compensated"),
        "the fixture is the firmware that omits them: {OLD_FIRMWARE}"
    );
    let r = decode(OLD_FIRMWARE);
    let temp = r
        .out_temp
        .expect("the raw field is read when the other is absent");
    assert!((68.0..=72.0).contains(&temp), "about 21 C in degF: {temp}");
    assert_ne!(temp, 32.0, "this is the number the Go driver stores");
    assert!(r.out_humidity.is_some(), "`rhum`");
    assert!(r.pm25.is_some(), "`pm02`");
}

/// A module that is not reporting: absent, not zero — and the sensors that *are* reporting
/// still arrive, so the reading is not empty.
#[test]
fn a_body_with_no_temperature_module_stores_no_temperature() {
    let r = decode(NO_SENSOR);
    assert_eq!(r.out_temp, None, "and NOT 32 degF");
    assert_eq!(r.out_humidity, None);
    assert!(
        r.pm25.is_some(),
        "the particulate sensor is a separate device"
    );
    assert!(r.co2.is_some(), "and so is the CO2 one");
}

/// A body that is not the JSON this decodes is a counted drop, which is what a device
/// answering a captive portal or a `404` page looks like.
/// Three bodies a device can answer with that are not measurements, each a counted drop.
#[test]
fn three_bodies_that_are_not_measurements_are_refused() {
    for body in ["<html>404 not found</html>", "", "{\"rco2\":\"nan\"}"] {
        let e = AirGradientJson
            .decode(&Frame::new(body.as_bytes().to_vec()))
            .err()
            .unwrap_or_else(|| panic!("`{body}` should not have decoded"));
        assert!(matches!(e, CodecError::Json { .. }), "`{body}`: {e}");
    }
}

/// Two shapes that **do** decode, to a reading admission refuses as empty — which is one
/// place rather than two where a reading can vanish.
///
/// `[]` is the surprising one and is pinned deliberately: `serde`'s derived
/// `Deserialize` accepts a **sequence** for a struct as well as a map, and every field here
/// is `#[serde(default)]`, so an empty array is a `Measures` with nothing set. No device
/// answers one; a reader who tries this should find the answer rather than the surprise.
#[test]
fn an_empty_object_and_an_empty_array_both_decode_to_nothing() {
    for body in ["{}", "[]"] {
        let batch = AirGradientJson
            .decode(&Frame::new(body.as_bytes().to_vec()))
            .unwrap_or_else(|e| panic!("`{body}`: {e}"));
        assert_eq!(batch.len(), 1, "`{body}`");
        assert_eq!(
            batch[0].values,
            mw_types::Reading::default(),
            "`{body}` populates nothing, which is admission's `Reject::EmptyReading`"
        );
    }
}

#[test]
fn decoding_is_a_function_of_the_body_alone_and_the_path_is_the_one_the_driver_requests() {
    assert_eq!(decode(CURRENT), decode(CURRENT));
    assert_eq!(MEASURES_CURRENT, "/measures/current");
    assert_eq!(AirGradientJson.name(), "airgradient-json");
}
