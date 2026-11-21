//! Level 1 of W3 **D-82**'s three, for the snow gauge: the pure codec against **captured**
//! `/data` and `/health` bodies.
//!
//! The four fixtures came off `tests/emulators/snowgauge`, whose payload shapes are the ones
//! `chrissnell/snowgauge-rs`'s `docs/integration.md` prints — `tests/emulators/README.md`
//! has the recipe that reproduces each one, and `frames/README.md` records what that
//! provenance is and is not worth.
//!
//! # What it checks that the unit tests do not
//!
//! The unit tests in `src/snowgauge.rs` build their bodies with `format!`, so they assert
//! the decoder against a document **this repository wrote**. These four are documents a
//! device wrote, in the field order and with the spellings it writes them in, and they cover
//! the three states one poll can be in: a measurement, a gauge whose trailing window has
//! not filled, and a gauge whose sensor has never spoken.
//!
//! The one that matters most is `snowgauge-no-data.json`, because it is the body that used
//! to be a reading. `distance` is `0` there and the gRPC predecessor would have stored it —
//! a `snowdistance` of zero millimetres, which under this station's 1500 mm base distance is
//! a 1500 mm snowpack the moment `snowdepth` stops being NULL.
//!
//! It needs no socket, no HTTP and no Go toolchain, which is what makes it level 1 rather
//! than a second copy of `crates/mw-station/tests/emulator_snowgauge.rs`.

use mw_codec::{snow_health, Codec, CodecError, Frame, SnowGaugeData, SnowLiveness};

/// `GET /data` from a gauge whose sensor is reporting and whose trailing window has filled.
const DATA: &str = include_str!("frames/snowgauge-data.json");

/// The same gauge a few hundred milliseconds after boot: one frame in, so `trim_mean` is
/// still `null` and the raw `distance` is the only number there is.
const WARMING: &str = include_str!("frames/snowgauge-warming.json");

/// A gauge whose sensor has never spoken: `status` is `no_data` and `distance` is zero.
/// **Calibrated**, so it still reports its `base_distance` — that number is NVS's, not the
/// sensor's, which is what makes reading it off the device worth doing at all.
const NO_DATA: &str = include_str!("frames/snowgauge-no-data.json");

/// A gauge nobody has calibrated: it measures normally and reports `base_distance` and
/// `snow_depth` as null.
const UNCALIBRATED: &str = include_str!("frames/snowgauge-uncalibrated.json");

/// `GET /health` from the same gauge.
const HEALTH: &str = include_str!("frames/snowgauge-health.json");

fn decode(body: &str) -> Result<Vec<mw_types::PartialReading>, CodecError> {
    SnowGaugeData.decode(&Frame::new(body.as_bytes().to_vec()))
}

/// The trimmed mean is the measurement, and the raw `distance` in the same body is a
/// different number — so a codec reading the wrong field is visible rather than merely
/// untested.
#[test]
fn a_captured_body_decodes_to_the_trimmed_mean_and_not_the_raw_distance() {
    let batch = decode(DATA).expect("a captured body decodes");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].values.snow_distance, Some(1351.4));
    assert_eq!(
        batch[0].observed_at, None,
        "`/data` is the gauge's current state and carries no instant"
    );
    // **The device's depth**, where it used to be the enrich stage's subtraction of a
    // configured base distance (DEV-9). 1500 - round(1351.4) = 149, which is the
    // firmware's arithmetic in the firmware's order.
    assert_eq!(batch[0].values.snow_depth, Some(149.0));
    // And nothing else: a gauge measures one thing.
    assert_eq!(batch[0].values.out_temp, None);
    assert_eq!(batch[0].values.barometer, None);

    // The liveness pair the driver carries across occurrences comes out of the same parse.
    let (data, _) = SnowGaugeData
        .read(&Frame::new(DATA.as_bytes().to_vec()))
        .expect("decodes");
    assert_eq!(
        data.liveness(),
        SnowLiveness {
            readings: 4,
            uptime_ms: 20_000
        }
    );
    assert!(data.is_ok());
    assert_eq!(data.distance, 1351, "the raw field is in the body too");
}

/// The first minute after a boot, as a body a device wrote: `trim_mean` is `null` and the
/// raw distance stands in.
#[test]
fn a_gauge_whose_window_has_not_filled_falls_back_to_the_raw_distance() {
    let batch = decode(WARMING).expect("a captured body decodes");
    assert_eq!(batch[0].values.snow_distance, Some(1384.0));
    let (data, _) = SnowGaugeData
        .read(&Frame::new(WARMING.as_bytes().to_vec()))
        .expect("decodes");
    assert_eq!(data.trim_mean, None, "`null`, which is the fixture's value");
    assert_eq!(data.readings, 1);
}

/// **The body that used to be a reading.** `status` is `no_data`, `distance` is zero, and
/// under a 1500 mm base distance a stored zero is a 1500 mm snowpack.
#[test]
fn a_gauge_whose_sensor_has_never_spoken_carries_no_reading() {
    let e = decode(NO_DATA).expect_err("no measurement");
    let CodecError::Incomplete { what } = e else {
        panic!("expected an incomplete body, got {e}");
    };
    assert!(what.contains("no_data"), "{what}");
    assert!(what.contains("power and wiring"), "{what}");
    // An unconfigured gauge is refused for the same reason: the base distance is what the
    // discard needs and this is about the gauge having nothing to give.
    assert!(matches!(
        decode(NO_DATA),
        Err(CodecError::Incomplete { .. })
    ));
}

/// A gauge nobody has calibrated, as a body a device wrote: a distance, and no depth.
#[test]
fn an_uncalibrated_gauge_carries_a_distance_and_no_depth() {
    let batch = decode(UNCALIBRATED).expect("a captured body decodes");
    assert_eq!(batch[0].values.snow_distance, Some(1383.6));
    assert_eq!(batch[0].values.snow_depth, None);
    let (data, _) = SnowGaugeData
        .read(&Frame::new(UNCALIBRATED.as_bytes().to_vec()))
        .expect("decodes");
    assert!(!data.is_calibrated());
    assert_eq!(data.rssi, Some(-63));
}

/// **The calibration is NVS's, not the sensor's.** A gauge whose sensor has never spoken
/// still reports what it is calibrated to — so the number an operator wants to see is
/// readable off a gauge that is not currently measuring anything, which is exactly when
/// they would go looking for it.
#[test]
fn a_calibrated_gauge_reports_its_base_distance_even_with_a_dead_sensor() {
    // The READING is refused, and rightly: `status` is `no_data`, so there is no distance
    // to store. What survives the refusal is the body, so this asserts over the parse
    // rather than over the batch.
    assert!(decode(NO_DATA).is_err(), "no measurement in it");
    let data: mw_codec::SnowData = serde_json::from_str(NO_DATA).expect("the body is JSON");
    assert!(data.is_calibrated());
    assert_eq!(
        data.base_distance,
        Some(1500),
        "the calibration the fixtures' emulator was given"
    );
    assert_eq!(data.snow_depth, None, "no reading to subtract from");
    assert_eq!(data.rssi, Some(-63), "the WiFi driver's, not the sensor's");
}

/// `/health` is telemetry and not a reading, and the captured body carries every field the
/// fault paths quote.
#[test]
fn the_captured_health_body_carries_the_fields_a_fault_path_quotes() {
    let health = snow_health(&Frame::new(HEALTH.as_bytes().to_vec())).expect("decodes");
    assert_eq!(health.bytes_rx, Some(32), "the sensor cross-check");
    assert_eq!(health.fw_version.as_deref(), Some("0.1.0"));
    assert_eq!(health.fw_slot.as_deref(), Some("ota_0"));
    assert_eq!(health.hostname.as_deref(), Some("snowgauge-f878"));
    assert_eq!(health.rssi, Some(-56));
    assert_eq!(health.heap_free, Some(164_228));
    assert_eq!(health.wifi.as_deref(), Some("connected"));

    // And it is **not** a `/data` body: the endpoints are not interchangeable, and a driver
    // that polled the wrong one gets a counted drop rather than a plausible reading.
    assert!(matches!(decode(HEALTH), Err(CodecError::Json { .. })));
    assert!(snow_health(&Frame::new(DATA.as_bytes().to_vec())).is_ok(),);
}

#[test]
fn decoding_is_a_function_of_the_body_alone() {
    let first = decode(DATA).expect("decodes");
    let again = decode(DATA).expect("decodes");
    assert_eq!(first, again);
    assert_eq!(SnowGaugeData.name(), "snowgauge-data");
    assert_eq!(mw_codec::SNOW_DATA, "/data");
    assert_eq!(mw_codec::SNOW_HEALTH, "/health");
}
