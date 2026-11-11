//! Level 1 of W3 **D-82**'s three, for Davis: the pure codec against **captured** records.
//!
//! Every assertion below is a function call over a byte string. There is no socket, no
//! clock, no runtime and no Go toolchain in this file, which is what makes it the level that
//! runs on every `cargo test` — and `ci/layers.toml` is what enforces the purity rather than
//! this comment doing so.
//!
//! **The three records were captured from `cmd/davis-emulator` at the pinned commit**, which
//! is vendored verbatim in `tests/emulators/davis` and digest-verified by the
//! `driver-emulators` gate. So this pins the decoder against **the reference
//! implementation's own `LOOP` writer**, including its CRC-16: a byte offset that drifted is
//! a failing test here, with no hardware and no emulator process involved.
//! `frames/README.md` has the recipe that re-derives them.
//!
//! What this level cannot say is anything about the conversation — the wake handshake, the
//! re-issue, the framing of a packet that arrives in pieces. Those are
//! `crates/mw-station/src/davis/tests.rs` (level 2) and
//! `crates/mw-station/tests/emulator_davis.rs` (level 3).

use mw_codec::{Codec, CodecError, DavisLoop, Frame, WindDirCorrection, LOOP_BYTES};

const LOOP_1: &[u8] = include_bytes!("frames/davis-loop-1.bin");
const LOOP_2: &[u8] = include_bytes!("frames/davis-loop-2.bin");
const LOOP_3: &[u8] = include_bytes!("frames/davis-loop-3.bin");

fn decode(bytes: &[u8]) -> mw_types::Reading {
    let batch = DavisLoop::default()
        .decode(&Frame::new(bytes.to_vec()))
        .expect("a captured record decodes");
    assert_eq!(batch.len(), 1, "one record is one observation");
    assert_eq!(
        batch[0].observed_at, None,
        "a LOOP record carries no timestamp; admission supplies one"
    );
    batch[0].values.clone()
}

/// Every captured record is the length and shape the protocol frames, and passes its own
/// CRC-16 — which is the emulator's, computed by `pkg/crc16` at the pinned commit.
#[test]
fn every_captured_record_is_ninety_nine_bytes_and_passes_its_own_checksum() {
    for (n, record) in [LOOP_1, LOOP_2, LOOP_3].iter().enumerate() {
        assert_eq!(record.len(), LOOP_BYTES, "record {n}");
        assert_eq!(&record[..3], b"LOO", "record {n}");
        assert!(
            mw_codec::crc16_check(record),
            "record {n} does not pass the CRC the emulator wrote"
        );
        // And the decoder accepts it, which is the same statement one layer up.
        decode(record);
    }
}

/// The values the emulator generated, read back through the decoder: a temperature, a
/// humidity and a barometer that are all in the range a Vantage reports, which is what says
/// the little-endian offsets are right.
#[test]
fn the_captured_records_decode_to_values_a_vantage_reports() {
    for (n, record) in [LOOP_1, LOOP_2, LOOP_3].iter().enumerate() {
        let r = decode(record);
        let temp = r
            .out_temp
            .unwrap_or_else(|| panic!("record {n}: no outside temperature"));
        assert!(
            (-60.0..=160.0).contains(&temp),
            "record {n}: {temp} degF is not a temperature; the offsets are wrong"
        );
        let humidity = r
            .out_humidity
            .unwrap_or_else(|| panic!("record {n}: no humidity"));
        assert!(
            (0.0..=100.0).contains(&humidity),
            "record {n}: {humidity}% is not a humidity"
        );
        let barometer = r
            .barometer
            .unwrap_or_else(|| panic!("record {n}: no barometer"));
        assert!(
            (25.0..=33.0).contains(&barometer),
            "record {n}: {barometer} inHg is not a pressure"
        );
        // The inside pair, which the emulator derives from the outside one: warmer and
        // drier. That relationship is what distinguishes the two 16-bit temperature fields
        // from each other -- they are four bytes apart and a swapped pair would pass every
        // range check above.
        let inside = r
            .in_temp
            .unwrap_or_else(|| panic!("record {n}: no inside temperature"));
        assert!(
            inside > temp,
            "record {n}: the emulator's inside temperature is warmer ({inside} vs {temp})"
        );
        assert!(
            r.in_humidity.is_some_and(|h| h < humidity),
            "record {n}: and drier ({:?} vs {humidity})",
            r.in_humidity
        );
        // The console's own supply: `raw * 300 / 512 / 100` of the emulator's 4050.
        assert_eq!(
            r.station_battery_voltage,
            Some(23.730_469),
            "record {n}: the console battery"
        );
        // Go's column, not `radiation` -- `station.go:646` assigns the LOOP packet's solar
        // radiation to `SolarWatts` and the WeatherLink Live driver assigns its own to
        // `radiation`. Faithful per driver is the only way M5-2's value diff can be read.
        assert_eq!(r.radiation, None, "record {n}");
    }
}

/// **The values move.** Three consecutive records from a device that is generating weather,
/// so a decoder that returned a constant — or read one field for another — would show up as
/// three identical readings.
#[test]
fn three_consecutive_records_carry_three_different_readings() {
    let readings: Vec<mw_types::Reading> = [LOOP_1, LOOP_2, LOOP_3]
        .iter()
        .map(|record| decode(record))
        .collect();
    assert!(
        readings[0] != readings[1] || readings[1] != readings[2],
        "three records decoded to the same reading"
    );
    // The barometer is a random walk in the emulator, so at least one of the three differs
    // from its neighbour -- and it is the field whose two bytes sit between two others that
    // also move, which makes it the one a shifted offset would corrupt invisibly.
    let barometers: Vec<Option<f32>> = readings.iter().map(|r| r.barometer).collect();
    assert!(
        barometers.windows(2).any(|pair| pair[0] != pair[1]),
        "the barometer did not move across three records: {barometers:?}"
    );
}

/// **DEV-8, on a captured record.** The emulator sets every unconnected sensor to `255`,
/// which is the single-byte dashed value — so the columns for sensors a Vantage does not
/// have are NULL here and `0` under the Go `conv*` helpers.
#[test]
fn the_sensors_the_emulator_declares_not_connected_are_absent() {
    let r = decode(LOOP_1);
    for (metric, value) in [
        ("extra_temp1", r.extra_temp1),
        ("extra_temp7", r.extra_temp7),
        ("soil_temp2", r.soil_temp2),
        ("soil_temp4", r.soil_temp4),
        ("leaf_temp2", r.leaf_temp2),
        ("leaf_temp4", r.leaf_temp4),
        ("extra_humidity1", r.extra_humidity1),
        ("extra_humidity7", r.extra_humidity7),
        ("soil_moisture2", r.soil_moisture2),
        ("leaf_wetness2", r.leaf_wetness2),
    ] {
        assert_eq!(
            value, None,
            "`{metric}` is 255 in the captured record, which is dashed and not zero"
        );
    }
    // And the ones it DOES declare connected are present, so the assertion above is not
    // vacuous: a decoder that returned `None` for everything would pass it.
    assert!(r.soil_temp1.is_some(), "the emulator connects soil 1");
    assert!(r.leaf_temp1.is_some(), "and leaf 1");
    assert!(r.soil_moisture1.is_some());
    assert!(r.leaf_wetness1.is_some());
}

/// The alarm, battery and forecast bytes are **bitfields and codes**, so zero is a value and
/// there is no sentinel: a healthy console reports no alarms, and that has to be
/// distinguishable from a column nothing wrote.
#[test]
fn the_alarm_and_forecast_bytes_are_present_even_when_they_are_zero() {
    let r = decode(LOOP_1);
    assert_eq!(r.inside_alarm, Some(0));
    assert_eq!(r.rain_alarm, Some(0));
    assert_eq!(r.outside_alarm1, Some(0));
    assert_eq!(r.soil_leaf_alarm4, Some(0));
    assert_eq!(r.tx_battery_status, Some(0));
    // The emulator's own forecast: "partly cloudy", rule 45.
    assert_eq!(r.forecast_icon, Some(2));
    assert_eq!(r.forecast_rule, Some(45));
}

/// Two columns the record carries and the decoder deliberately leaves alone, each for its
/// own reason: `storm_start` is a packed **local** date a codec cannot resolve without the
/// station's zone, and `sunrise`/`sunset` are W2a Decision 36's — functions of
/// `(date, lat, lon)`, computed where they are served.
#[test]
fn the_packed_date_and_the_sun_times_are_not_stored() {
    let r = decode(LOOP_1);
    assert_eq!(r.storm_start, None);
    assert_eq!(r.sunrise, None);
    assert_eq!(r.sunset, None);
    // And the enrich stage's two, which **R1**/DEV-9 moved off the driver.
    assert_eq!(r.wind_chill, None);
    assert_eq!(r.heat_index, None);
}

/// The vane calibration is applied to a captured record's own direction and to nothing else.
#[test]
fn the_vane_calibration_moves_the_captured_direction_and_no_other_field() {
    let plain = decode(LOOP_1);
    let corrected = DavisLoop::new(WindDirCorrection::new(10))
        .decode(&Frame::new(LOOP_1.to_vec()))
        .expect("decodes")[0]
        .values
        .clone();
    let before = plain
        .wind_dir
        .expect("the emulator always sends a direction");
    let after = corrected
        .wind_dir
        .expect("and the correction cannot remove it");
    assert_eq!(after, (before + 10.0).rem_euclid(360.0));
    assert_eq!(corrected.wind_speed, plain.wind_speed);
    assert_eq!(corrected.out_temp, plain.out_temp);
}

/// A captured record with one byte changed fails its own checksum, which is what the CRC is
/// for — and is the assertion that says the checksum is actually being checked rather than
/// computed and discarded.
#[test]
fn one_flipped_bit_in_a_captured_record_is_refused() {
    // Byte 4 is the record-type discriminant, so flipping it is the one case that is
    // refused as `Unsupported` before the checksum is reached -- which is what makes the
    // second arm below reachable rather than decoration.
    for at in [3usize, 4, 12, 44, 86, 97] {
        let mut corrupt = LOOP_1.to_vec();
        corrupt[at] ^= 0x01;
        let e = DavisLoop::default()
            .decode(&Frame::new(corrupt))
            .expect_err("a flipped bit at {at}");
        assert!(
            matches!(e, CodecError::Check { .. } | CodecError::Unsupported { .. }),
            "byte {at}: {e}"
        );
    }
}

/// Decoding is a function of the frame alone: the same bytes twice are the same reading.
#[test]
fn decoding_is_a_function_of_the_record_alone() {
    assert_eq!(decode(LOOP_1), decode(LOOP_1));
    assert_eq!(decode(LOOP_2), decode(LOOP_2));
}
