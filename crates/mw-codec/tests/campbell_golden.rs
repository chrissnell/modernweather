//! Level 1 of W3 **D-82**'s three, for Campbell Scientific: the pure codec against
//! **captured** packets.
//!
//! The four records were captured from `cmd/campbell-emulator` at the pinned commit —
//! vendored verbatim in `tests/emulators/campbell` and digest-verified by the
//! `driver-emulators` gate — so this pins the decoder against the reference
//! implementation's own `json.Encoder` output. `frames/README.md` has the recipe.
//!
//! What this level cannot say is anything about the framing, which is the whole of what the
//! Campbell *session* does: `crates/mw-station/src/lines.rs` is level 2 and
//! `crates/mw-station/tests/emulator_campbell.rs` is level 3.

use mw_codec::{CampbellAscii, Codec, CodecError, Frame, WindDirCorrection};

const PACKETS: &str = include_str!("frames/campbell-packets.jsonl");

fn records() -> Vec<&'static str> {
    PACKETS.lines().filter(|l| !l.is_empty()).collect()
}

fn decode(record: &str) -> mw_types::Reading {
    let batch = CampbellAscii::default()
        .decode(&Frame::new(record.as_bytes().to_vec()))
        .expect("a captured packet decodes");
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].observed_at, None,
        "the datalogger's program outputs no timestamp"
    );
    batch[0].values.clone()
}

#[test]
fn four_packets_were_captured_and_every_one_decodes() {
    let records = records();
    assert_eq!(records.len(), 4, "the fixture is four consecutive outputs");
    for record in &records {
        assert!(record.starts_with('{') && record.ends_with('}'), "{record}");
        decode(record);
    }
}

/// The six keys the emulator's generator always produces a non-zero value for, in the
/// registry's units — which is what says the key-to-metric mapping is right.
#[test]
fn every_packet_carries_the_keys_the_generator_always_sets() {
    for record in records() {
        let r = decode(record);
        let temp = r.out_temp.expect("`airtemp_f`");
        assert!(
            (-60.0..=160.0).contains(&temp),
            "{temp} degF is not a temperature: {record}"
        );
        let humidity = r.out_humidity.expect("`rh`");
        assert!((10.0..=95.0).contains(&humidity), "{humidity}%: {record}");
        let barometer = r.barometer.expect("`baro`");
        assert!(
            (29.0..=31.0).contains(&barometer),
            "{barometer} inHg: {record}"
        );
        // `baro_temp_f` is the barometer's own sensor, which the emulator derives from the
        // air temperature by adding 0..5 -- so it is always the warmer of the two, which is
        // what distinguishes the two °F keys from each other.
        let barometer_temp = r.extra_temp1.expect("`baro_temp_f`");
        assert!(
            barometer_temp >= temp,
            "`baro_temp_f` {barometer_temp} should not be below `airtemp_f` {temp}"
        );
        let battery = r.station_battery_voltage.expect("`batt_volt`");
        assert!((13.0..=14.0).contains(&battery), "{battery} V: {record}");
        assert!(r.wind_speed.is_some(), "`wind_s`: {record}");
    }
}

/// **DEV-8, on captured packets.** Every field of the emulator's struct is `omitempty`, so a
/// value that happens to be zero is **not on the wire at all** — and the Go driver's
/// non-pointer struct stores `0.0` for each of those where this stores NULL.
///
/// The captured fixture was taken at night with no rain, so `slr_w`, `slr_mj` and `rain_in`
/// are absent from all four packets. That is recorded here rather than assumed: the
/// assertion is on the shape of the correction, which holds at any hour.
#[test]
fn a_key_the_generator_omitted_is_absent_rather_than_zero() {
    for record in records() {
        let r = decode(record);
        for (key, value) in [
            ("slr_w", r.solar_watts),
            ("slr_mj", r.solar_joules),
            ("rain_in", r.rain_incremental),
            ("wind_d", r.wind_dir),
        ] {
            assert_ne!(
                value,
                Some(0.0),
                "`{key}` is `omitempty`, so a zero is an absent key: {record}"
            );
        }
        // The two solar keys are one number in the generator (`slr_mj = slr_w * 0.001`), so
        // they arrive or are absent together.
        assert_eq!(
            r.solar_watts.is_some(),
            r.solar_joules.is_some(),
            "{record}"
        );
    }
    // And the captured fixture is the night case, which is what makes the assertion above
    // meet a real absence rather than a hypothetical one.
    let r = decode(records()[0]);
    assert_eq!(r.solar_watts, None, "captured after dark");
    assert_eq!(r.solar_joules, None);
    assert_eq!(r.rain_incremental, None, "captured with no rain");
    // The absence above holds for all four records, so the *presence* half of the same
    // mapping is not exercised by the fixture at all. Asserted here on the same four keys,
    // because "absent decodes to `None`" and "present decodes to the value" are two
    // properties and a decoder that returned `None` for both would pass the first.
    let daytime = r#"{"airtemp_f":70.0,"slr_w":812.0,"slr_mj":0.812,"rain_in":0.03,"wind_d":0}"#;
    let r = decode(daytime);
    assert_eq!(r.solar_watts, Some(812.0));
    assert_eq!(r.solar_joules, Some(0.812));
    assert_eq!(r.rain_incremental, Some(0.03));
    // Zero is a *reported* zero here rather than an omitted key, and the difference
    // between the two is the whole of DEV-8: this one is stored.
    assert_eq!(r.wind_dir, Some(0.0));
}

/// Nothing this driver does not map, and nothing the enrich stage owns.
#[test]
fn the_packet_fills_ten_metrics_and_no_others() {
    let r = decode(records()[0]);
    // The enrich stage's three (DEV-9), including `potentialsolarwatts` -- the one key only
    // this driver ever computed, and the single key DEV-9 says changes on the wire.
    assert_eq!(r.wind_chill, None);
    assert_eq!(r.heat_index, None);
    assert_eq!(r.potential_solar_watts, None);
    // Not this driver's columns.
    assert_eq!(r.radiation, None, "the WeatherLink Live's column");
    assert_eq!(r.in_temp, None);
    assert_eq!(r.day_rain, None);
    assert_eq!(r.snow_distance, None);
    assert_eq!(r.cons_battery_voltage, None, "no Go driver writes this one");
}

/// **The values move** across four consecutive packets, so a decoder returning a constant
/// would be visible.
#[test]
fn four_consecutive_packets_carry_four_different_readings() {
    let temps: Vec<Option<f32>> = records().iter().map(|r| decode(r).out_temp).collect();
    assert!(
        temps.windows(2).any(|pair| pair[0] != pair[1]),
        "four packets carried one temperature: {temps:?}"
    );
}

#[test]
fn the_vane_calibration_moves_the_captured_direction_and_no_other_field() {
    let record = records()
        .into_iter()
        .find(|r| r.contains("\"wind_d\""))
        .expect("the emulator sends a direction unless it picks due north");
    let plain = decode(record);
    let corrected = CampbellAscii::new(WindDirCorrection::new(-30))
        .decode(&Frame::new(record.as_bytes().to_vec()))
        .expect("decodes")[0]
        .values
        .clone();
    let before = plain.wind_dir.expect("a direction");
    assert_eq!(corrected.wind_dir, Some((before - 30.0).rem_euclid(360.0)));
    assert_eq!(corrected.wind_speed, plain.wind_speed);
    assert_eq!(corrected.out_temp, plain.out_temp);
}

/// A truncated capture is a parse failure and not half a reading, which is the failure the
/// `Lines` session exists to make impossible.
#[test]
fn half_a_captured_packet_is_a_parse_failure() {
    let record = records()[0];
    let half = &record[..record.len() / 2];
    let e = CampbellAscii::default()
        .decode(&Frame::new(half.as_bytes().to_vec()))
        .expect_err("half a record");
    assert!(matches!(e, CodecError::Json { .. }), "{e}");
}

#[test]
fn decoding_is_a_function_of_the_packet_alone() {
    for record in records() {
        assert_eq!(decode(record), decode(record));
    }
}
