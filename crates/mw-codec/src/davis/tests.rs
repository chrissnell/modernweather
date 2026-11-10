//! The Davis LOOP decoder, against records assembled byte by byte.
//!
//! The *values* here come from `cmd/davis-emulator`'s own `GenerateLoopPacket` where it has
//! an opinion (`ConsBatteryVoltage: 4050`, `ForecastIcon: 2`, the `255`s on every sensor it
//! declares "not connected"), so that a reader comparing this file against the oracle is
//! comparing the same numbers. `tests/davis_golden.rs` is the other half: real records
//! captured from that emulator, replayed byte for byte.

use super::*;
use crate::frame::Frame;

/// A LOOP record with every field dashed, which is what a console with no sensors at all
/// reports. The caller writes the fields it is testing over the top.
fn dashed() -> Vec<u8> {
    let mut bytes = vec![0u8; LOOP_BYTES];
    bytes[..3].copy_from_slice(&LOOP_HEADER);
    bytes[3] = 0; // flavour B, trend zero
    bytes[4] = Loop::LOOP1;
    bytes[7..9].copy_from_slice(&DASH_U16_HIGH.to_le_bytes()); // barometer
    bytes[9..11].copy_from_slice(&DASH_I16.to_le_bytes()); // inside temperature
    bytes[11] = DASH_U8;
    bytes[12..14].copy_from_slice(&DASH_I16.to_le_bytes()); // outside temperature
    bytes[14] = DASH_U8;
    bytes[15] = DASH_U8;
    bytes[16..18].copy_from_slice(&DASH_U16_HIGH.to_le_bytes()); // wind direction
                                                                 // The extra, soil and leaf temperatures and the extra humidities: bytes 18 through 40,
                                                                 // every one of which has the same single-byte dash value.
    bytes[18..41].fill(DASH_U8);
    bytes[41..43].copy_from_slice(&DASH_U16_FULL.to_le_bytes()); // rain rate
    bytes[43] = DASH_U8;
    bytes[44..46].copy_from_slice(&DASH_U16_HIGH.to_le_bytes()); // solar radiation
                                                                 // Soil moisture 1-4 and leaf wetness 1-4.
    bytes[62..70].fill(DASH_U8);
    bytes[95] = b'\n';
    bytes[96] = b'\r';
    bytes
}

/// Append the record's own check bytes, which is what makes it a frame a decoder accepts.
fn sealed(mut bytes: Vec<u8>) -> Frame {
    let crc = crc16::crc16(&bytes[..LOOP_BYTES - 2]);
    bytes[LOOP_BYTES - 2..].copy_from_slice(&crc.to_be_bytes());
    Frame::new(bytes)
}

fn decode(bytes: Vec<u8>) -> Reading {
    let frame = sealed(bytes);
    let batch = DavisLoop::default()
        .decode(&frame)
        .expect("a sealed record decodes");
    assert_eq!(batch.len(), 1, "one record is one observation");
    assert_eq!(
        batch[0].observed_at, None,
        "a LOOP record carries no timestamp, so admission supplies one"
    );
    batch[0].values.clone()
}

/// The emulator's own packet, decoded field by field. Every expected value here is the
/// emulator's literal divided by the conversion the Go driver applies.
#[test]
fn a_populated_record_decodes_to_the_values_the_go_driver_produces() {
    let mut bytes = dashed();
    bytes[7..9].copy_from_slice(&30_000u16.to_le_bytes()); // 30.000 inHg
    bytes[9..11].copy_from_slice(&720i16.to_le_bytes()); // 72.0 F inside
    bytes[11] = 45;
    bytes[12..14].copy_from_slice(&703i16.to_le_bytes()); // 70.3 F outside
    bytes[14] = 12;
    bytes[15] = 8;
    bytes[16..18].copy_from_slice(&183u16.to_le_bytes());
    bytes[25] = 155; // soil temperature 1: 155 - 90 = 65 F
    bytes[29] = 160; // leaf temperature 1: 70 F
    bytes[33] = 50;
    bytes[41..43].copy_from_slice(&3u16.to_le_bytes()); // 3 clicks/hour = 0.03 in/hr
    bytes[43] = 65; // UV 6.5
    bytes[44..46].copy_from_slice(&812u16.to_le_bytes());
    bytes[46..48].copy_from_slice(&11u16.to_le_bytes()); // storm rain 0.11 in
    bytes[50..52].copy_from_slice(&13u16.to_le_bytes()); // day rain 0.13 in
    bytes[52..54].copy_from_slice(&150u16.to_le_bytes()); // month rain 1.50 in
    bytes[54..56].copy_from_slice(&900u16.to_le_bytes()); // year rain 9.00 in
    bytes[56..58].copy_from_slice(&123u16.to_le_bytes()); // day ET 0.123 in
    bytes[58..60].copy_from_slice(&456u16.to_le_bytes()); // month ET 4.56 in
    bytes[60..62].copy_from_slice(&7890u16.to_le_bytes()); // year ET 78.90 in
    bytes[62] = 44; // soil moisture 1, cbar
    bytes[66] = 9; // leaf wetness 1
    bytes[70] = 0x01;
    bytes[86] = 0x02;
    bytes[87..89].copy_from_slice(&4050u16.to_le_bytes());
    bytes[89] = 2;
    bytes[90] = 45;

    let r = decode(bytes);
    assert_eq!(r.barometer, Some(30.0));
    assert_eq!(r.in_temp, Some(72.0));
    assert_eq!(r.in_humidity, Some(45.0));
    assert_eq!(r.out_temp, Some(70.3));
    assert_eq!(r.out_humidity, Some(50.0));
    assert_eq!(r.wind_speed, Some(12.0));
    assert_eq!(r.wind_speed10, Some(8.0));
    assert_eq!(r.wind_dir, Some(183.0));
    assert_eq!(r.soil_temp1, Some(65.0));
    assert_eq!(r.leaf_temp1, Some(70.0));
    assert_eq!(r.rain_rate, Some(0.03));
    assert_eq!(r.uv, Some(6.5));
    assert_eq!(r.solar_watts, Some(812.0));
    assert_eq!(r.radiation, None, "Go's Davis driver writes `solarwatts`");
    assert_eq!(r.storm_rain, Some(0.11));
    assert_eq!(r.day_rain, Some(0.13));
    assert_eq!(r.month_rain, Some(1.5));
    assert_eq!(r.year_rain, Some(9.0));
    assert_eq!(r.day_et, Some(0.123));
    assert_eq!(r.month_et, Some(4.56));
    assert_eq!(r.year_et, Some(78.9));
    assert_eq!(r.soil_moisture1, Some(44.0));
    assert_eq!(r.leaf_wetness1, Some(9.0));
    assert_eq!(r.inside_alarm, Some(1));
    assert_eq!(r.tx_battery_status, Some(2));
    assert_eq!(r.forecast_icon, Some(2));
    assert_eq!(r.forecast_rule, Some(45));
    // `convConsBatteryVoltage(4050)` = 4050 * 300 / 512 / 100 = 23.73046875. The Go
    // emulator's comment calls 4050 "13.5V * 300 + 0.5", which is the console's own
    // encoding and a different scale from the one the Go driver divides by -- so the number
    // below is what the Go DRIVER produces from the Go EMULATOR's packet, and the two
    // disagreeing about the encoding is a property of the pair rather than of this decoder.
    assert_eq!(r.station_battery_voltage, Some(23.730_469));
    assert_eq!(
        r.cons_battery_voltage, None,
        "no Go driver writes this column; `station.go:679` uses the other one"
    );
    // The enrich stage's, not the codec's (DEV-9).
    assert_eq!(r.wind_chill, None);
    assert_eq!(r.heat_index, None);
    // Not expressible without the station's zone, and settled elsewhere.
    assert_eq!(r.storm_start, None);
    assert_eq!(r.sunrise, None);
    assert_eq!(r.sunset, None);
}

/// The DEV-8 correction, stated as the test that fails against the Go conversions.
#[test]
fn a_dashed_sensor_is_absent_rather_than_zero() {
    let r = decode(dashed());
    for (metric, value) in [
        ("barometer", r.barometer),
        ("in_temp", r.in_temp),
        ("in_humidity", r.in_humidity),
        ("out_temp", r.out_temp),
        ("out_humidity", r.out_humidity),
        ("wind_speed", r.wind_speed),
        ("wind_speed10", r.wind_speed10),
        ("wind_dir", r.wind_dir),
        ("extra_temp1", r.extra_temp1),
        ("extra_temp7", r.extra_temp7),
        ("soil_temp1", r.soil_temp1),
        ("soil_temp4", r.soil_temp4),
        ("leaf_temp1", r.leaf_temp1),
        ("leaf_temp4", r.leaf_temp4),
        ("extra_humidity1", r.extra_humidity1),
        ("extra_humidity7", r.extra_humidity7),
        ("rain_rate", r.rain_rate),
        ("uv", r.uv),
        ("solar_watts", r.solar_watts),
        ("soil_moisture1", r.soil_moisture1),
        ("leaf_wetness4", r.leaf_wetness4),
    ] {
        assert_eq!(value, None, "`{metric}` was dashed and decoded to a value");
    }
    // The bitfields and codes are not dashed, so they are present and zero -- which is
    // what "no alarms are set" is.
    assert_eq!(r.inside_alarm, Some(0));
    assert_eq!(r.soil_leaf_alarm4, Some(0));
    assert_eq!(r.tx_battery_status, Some(0));
    // And the rain totals have no dash value at all, so a console with no collector
    // reports zero inches rather than nothing. Faithful: `convVal100` has no sentinel.
    assert_eq!(r.day_rain, Some(0.0));
    assert_eq!(r.year_et, Some(0.0));
}

/// The second disagreement: `32767` is the specification's dashed value and `-32768` is
/// what the Go driver tests for. Both are refused, and the Go behaviour on the first is
/// what makes that worth a test of its own.
#[test]
fn both_sixteen_bit_dash_values_are_refused_and_one_of_them_is_the_go_defect() {
    for dash in [DASH_I16, GO_DASH_I16] {
        let mut bytes = dashed();
        bytes[12..14].copy_from_slice(&dash.to_le_bytes());
        assert_eq!(
            decode(bytes).out_temp,
            None,
            "{dash} should be an absent outside temperature"
        );
    }
    // What Go stores for the specification's dashed value, which is the number this test
    // exists to keep out of the column.
    assert_eq!(f32::from(DASH_I16) / 10.0, 3276.7);
}

#[test]
fn the_vane_calibration_is_applied_to_the_direction_and_to_nothing_else() {
    let mut bytes = dashed();
    bytes[16..18].copy_from_slice(&355u16.to_le_bytes());
    bytes[14] = 12;
    let frame = sealed(bytes);
    let codec = DavisLoop::new(WindDirCorrection::new(10));
    assert_eq!(codec.correction().degrees(), 10);
    let batch = codec.decode(&frame).expect("decodes");
    assert_eq!(batch[0].values.wind_dir, Some(5.0));
    assert_eq!(batch[0].values.wind_speed, Some(12.0));

    // A dashed direction cannot be corrected into existence.
    let mut dashed_dir = dashed();
    dashed_dir[14] = 12;
    let batch = codec.decode(&sealed(dashed_dir)).expect("decodes");
    assert_eq!(batch[0].values.wind_dir, None);
}

#[test]
fn a_record_that_fails_its_own_checksum_is_a_counted_drop_naming_the_checksum() {
    let mut bytes = dashed();
    bytes[12..14].copy_from_slice(&703i16.to_le_bytes());
    let mut corrupt = sealed(bytes).bytes().to_vec();
    corrupt[20] ^= 0xFF;
    let e = DavisLoop::default()
        .decode(&Frame::new(corrupt))
        .expect_err("a corrupted record");
    assert!(matches!(e, CodecError::Check { .. }), "{e}");
    assert_eq!(e.to_string(), "a Davis LOOP record failed its own checksum");
}

#[test]
fn a_short_or_long_frame_names_both_lengths() {
    for len in [0usize, 10, LOOP_BYTES - 1, LOOP_BYTES + 1] {
        let e = DavisLoop::default()
            .decode(&Frame::new(vec![0u8; len]))
            .expect_err("not a record");
        let CodecError::Framing {
            expected, found, ..
        } = e
        else {
            panic!("expected a framing error for {len} bytes, got {e}");
        };
        assert_eq!(expected, LOOP_BYTES);
        assert_eq!(found, len);
    }
}

#[test]
fn ninety_nine_bytes_of_something_else_is_refused_before_the_checksum() {
    let mut bytes = vec![0u8; LOOP_BYTES];
    bytes[..3].copy_from_slice(b"ACK");
    let e = DavisLoop::default()
        .decode(&sealed(bytes))
        .expect_err("not a LOOP record");
    assert!(matches!(e, CodecError::Incomplete { .. }), "{e}");
}

/// The test the Go driver does not make. A LOOP2 record's check bytes are correct, so the
/// only thing that distinguishes it is its own discriminant.
#[test]
fn a_loop2_record_is_refused_rather_than_decoded_as_a_loop_record() {
    let mut bytes = dashed();
    bytes[4] = 1;
    bytes[9..11].copy_from_slice(&720i16.to_le_bytes());
    let e = DavisLoop::default()
        .decode(&sealed(bytes))
        .expect_err("a LOOP2 record");
    assert!(
        matches!(
            e,
            CodecError::Unsupported {
                what: "LOOP record type",
                found: 1
            }
        ),
        "{e}"
    );
}

/// Flavour A and flavour B differ in one byte and in no metric, which is the claim the
/// module documentation makes.
#[test]
fn the_two_packet_flavours_decode_to_the_same_reading() {
    let mut flavour_b = dashed();
    flavour_b[3] = -5i8 as u8;
    flavour_b[12..14].copy_from_slice(&703i16.to_le_bytes());
    let mut flavour_a = flavour_b.clone();
    flavour_a[3] = b'P';

    let codec = DavisLoop::default();
    let b = codec.decode(&sealed(flavour_b)).expect("flavour B decodes");
    let a = codec.decode(&sealed(flavour_a)).expect("flavour A decodes");
    assert_eq!(a, b);
    assert_eq!(a[0].values.out_temp, Some(70.3));
}

#[test]
fn the_codec_names_itself_for_a_log_field_and_claims_no_station() {
    let codec = DavisLoop::default();
    assert_eq!(codec.name(), "davis-loop");
    assert_eq!(
        codec.claimed_station(&sealed(dashed())),
        None,
        "a Davis station's identity is Fixed: the wire is the credential"
    );
}
