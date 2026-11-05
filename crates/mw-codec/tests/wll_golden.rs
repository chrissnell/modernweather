//! Level 1 of the three W3 **D-82** specifies: the pure codec against captured frames.
//!
//! Every assertion below is a function call over a byte string. There is no socket, no
//! clock and no runtime in this file, which is the property that makes a station driver's
//! sensor mapping testable at all — and `ci/layers.toml` enforces it rather than this
//! comment doing so (`mw-codec`'s `forbidden` and `forbidden_source`).
//!
//! The frames are `include_str!`'d rather than read from disk for the same reason: this
//! crate may not name the filesystem, and a test that read its own fixtures would be the
//! one file in it that could.
//!
//! What each fixture is, and where it came from, is `tests/frames/README.md` — including
//! the limit, which is that they are the published local API's documented payloads rather
//! than captures from a physical unit.

use mw_codec::{real_time, Broadcast, Codec, CodecError, Frame, SensorMapping, WllConditions};
use mw_types::Reading;

/// The Go tree's `vp2_plus` template, which is the mapping a Vantage Pro2 Plus gets.
const VP2_PLUS: &str = "th:1, rain:1, wind:1, uv:1, solar:1, windchill:1, thw:1, \
                        thsw:1:appTemp, th_indoor, baro, battery:1";

/// Its `vp2_plus_agricultural` template.
const AGRICULTURAL: &str = "th:1, rain:1, wind:1, uv:1, solar:1, windchill:1, thw:1, \
                            thsw:1:appTemp, soil_temp:2:1, soil_temp:2:2, soil_temp:2:3, \
                            soil_temp:2:4, soil_moist:2:1, soil_moist:2:2, soil_moist:2:3, \
                            soil_moist:2:4, leaf_wet:2:1, leaf_wet:2:2, th_indoor, baro, \
                            battery:1, battery:2";

const BROADCAST: &str = include_str!("frames/broadcast.json");
const BROADCAST_METRIC: &str = include_str!("frames/broadcast-metric-collector.json");
const CURRENT: &str = include_str!("frames/current-conditions.json");
const AGRI_FRAME: &str = include_str!("frames/current-conditions-agricultural.json");
const TWO_TRANSMITTERS: &str = include_str!("frames/current-conditions-two-transmitters.json");
const REAL_TIME: &str = include_str!("frames/real-time.json");
const DEVICE_ERROR: &str = include_str!("frames/device-error.json");

fn codec(mapping: &str) -> WllConditions {
    WllConditions::new(SensorMapping::parse(mapping).expect("a template parses"))
}

fn one(mapping: &str, frame: &str) -> mw_types::PartialReading {
    let decoded = codec(mapping)
        .decode(&Frame::new(frame.as_bytes().to_vec()))
        .expect("the fixture decodes");
    assert_eq!(decoded.len(), 1, "one frame is one observation");
    decoded.into_iter().next().expect("one")
}

/// Within a tenth of a thousandth, which is finer than any of these instruments and
/// coarser than `f32`'s representation of a decimal literal.
#[track_caller]
fn near(got: Option<f32>, want: f32) {
    let got = got.expect("the metric is populated");
    assert!((got - want).abs() < 1e-4, "got {got}, want {want}");
}

#[test]
fn a_vantage_pro2_plus_poll_maps_every_sensor_the_template_names() {
    let reading = one(VP2_PLUS, CURRENT);
    assert_eq!(
        reading.observed_at.map(|t| t.timestamp()),
        Some(1_531_754_005),
        "the payload's own `ts` is the observation instant"
    );
    let r = reading.values;

    // Temperature and humidity, in the registry's own units. The Go driver writes
    // (62.7 - 32) * 5 / 9 = 17.055... into a column declared `degF` (DEV-8).
    near(r.out_temp, 62.7);
    near(r.out_humidity, 56.8);
    near(r.heat_index, 62.3);
    near(r.extra_float1, 46.5); // dew point
    near(r.extra_float2, 52.3); // wet bulb

    // Wind. `windspeed10` has a column and the Go driver does not fill it.
    near(r.wind_speed, 2.0);
    near(r.wind_dir, 165.0);
    near(r.wind_speed10, 2.2);

    // Rain: 63 tips of a 0.2 mm collector is 12.6 mm, which is 0.496063 in. The Go driver
    // stores 63 into a column declared `in` (DEV-8).
    near(r.day_rain, 0.496_063);
    near(r.rain_rate, 0.0);

    // `solar_rad` is irradiance, so it lands on `radiation` and not on `solarwatts`,
    // which the Campbell logger reports as a separate quantity.
    assert_eq!(r.solar_watts, None);
    near(r.radiation, 747.0);
    near(r.uv, 2.7);
    near(r.wind_chill, 62.7);
    near(r.extra_float3, 62.3); // THW
    near(r.extra_float4, 64.5); // THSW
    near(r.extra_float5, 64.5); // apparent temperature, from `thsw:1:appTemp`

    // The unit's own records. `in_temp` is the one the Go driver reads out of the
    // barometer record and therefore never stores (DEV-8).
    near(r.in_temp, 78.0);
    near(r.in_humidity, 41.1);
    near(r.extra_float6, 52.5);
    near(r.extra_float7, 77.0);
    near(r.barom_rel_in, 29.897);
    near(r.barometer, 29.897);
    near(r.barom_abs_in, 29.846);

    assert_eq!(r.batt1, Some(0), "transmitter 1's battery flag");
    assert_eq!(r.batt2, None, "there is no transmitter 2 on this unit");
}

#[test]
fn an_unreported_sensor_stays_none_all_the_way_through() {
    let r = one(VP2_PLUS, CURRENT).values;
    // Nothing in this payload is a snow gauge, an air-quality sensor or a second
    // anemometer, and D-26's whole content is that none of them becomes 0.0.
    assert_eq!(r.snow_depth, None);
    assert_eq!(r.pm25, None);
    assert_eq!(r.extra_temp1, None);
    assert_eq!(r.soil_temp1, None);
    assert_eq!(r.leaf_wetness1, None);
    assert_eq!(r.month_rain, None, "M3-4 owns the rainfall rollovers");
    assert_eq!(
        r.station_id,
        String::new(),
        "admission supplies the station"
    );
    assert_eq!(r.station_type, None, "the enrich stage supplies the type");
}

#[test]
fn a_broadcast_datagram_decodes_with_the_same_codec_and_the_same_mapping() {
    let reading = one(VP2_PLUS, BROADCAST);
    assert_eq!(
        reading.observed_at.map(|t| t.timestamp()),
        Some(1_531_754_005)
    );
    let r = reading.values;
    // The broadcast carries wind and rain and nothing else, which is the whole of what
    // "two transports, one codec family" (W2a **D-44**) costs: the same mapping over a
    // sparser record.
    near(r.wind_speed, 2.0);
    near(r.wind_dir, 165.0);
    near(r.day_rain, 0.496_063);
    assert_eq!(r.out_temp, None, "the datagram carries no temperature");
    assert_eq!(r.barometer, None, "nor the unit's barometer");
    assert_eq!(r.in_temp, None);
}

#[test]
fn a_tenth_millimetre_collector_is_not_two_hundred_and_fifty_four_inches() {
    let r = one(VP2_PLUS, BROADCAST_METRIC).values;
    // 254 tips of 0.1 mm is 25.4 mm, which is exactly one inch. The Go driver stores 254.
    near(r.day_rain, 1.0);
    // 12 tips/hour of 0.1 mm is 1.2 mm/h, which is 0.047244 in/h.
    near(r.rain_rate, 0.047_244);
    assert_eq!(r.wind_dir, None, "`wind_dir_last` is null in this frame");
    near(r.wind_speed, 0.0);
}

#[test]
fn the_agricultural_station_lands_on_its_four_soil_and_two_leaf_channels() {
    let r = one(AGRICULTURAL, AGRI_FRAME).values;
    near(r.soil_temp1, 51.2);
    near(r.soil_temp2, 52.7);
    near(r.soil_temp3, 53.1);
    assert_eq!(r.soil_temp4, None, "port 4 reported null");
    near(r.soil_moisture1, 12.0);
    near(r.soil_moisture2, 31.5);
    near(r.soil_moisture3, 44.0);
    assert_eq!(r.soil_moisture4, None);
    near(r.leaf_wetness1, 8.0);
    near(r.leaf_wetness2, 0.0);
    // Both transmitters' batteries, which is what `battery:1, battery:2` is for.
    assert_eq!(r.batt1, Some(1));
    assert_eq!(r.batt2, Some(0));
    // The outdoor sensors on transmitter 1 are unaffected by a second transmitter whose
    // records share none of their fields.
    near(r.out_temp, 44.1);
    near(r.day_rain, 0.17); // 17 tips of 0.01 in
    near(r.in_temp, 66.4);
    near(r.barometer, 30.112);
}

/// **The ordering rule, end to end through `Codec::decode`.** Three configured T/H
/// transmitters; the middle one is present in the payload with null fields, which is what a
/// WeatherLink Live reports for a configured transmitter it has not heard from
/// (`rx_state: 2`).
///
/// The assertion that matters is the third one: a dropout must NOT promote a later sensor
/// into `outtemp`, and must NOT renumber the ones after it. Go's counter increments on a
/// matched *record* rather than on a populated *value*, so the slots are stable across a
/// dropout — and this is the test that pins it, because every other assertion about
/// `extratemp*` in this file is a `None`.
#[test]
fn a_dropped_transmitter_does_not_renumber_the_temperature_columns() {
    let r = one("th:1, th:2, th:3", TWO_TRANSMITTERS).values;
    // Index 0: the outdoor sensor, with the three values only index 0 carries.
    near(r.out_temp, 58.4);
    near(r.out_humidity, 71.0);
    near(r.extra_float1, 49.3); // dew point
    near(r.extra_float2, 52.8); // wet bulb
    near(r.heat_index, 58.4);
    // Index 1: present, silent. A gap, not a value borrowed from somewhere else.
    assert_eq!(r.extra_temp1, None, "a silent transmitter stores nothing");
    assert_eq!(r.extra_humidity1, None);
    // Index 2: the third transmitter, still in ITS slot.
    near(r.extra_temp2, 39.9);
    near(r.extra_humidity2, 88.0);
    assert_eq!(r.extra_temp3, None, "there is no fourth sensor");

    // And the converse case, which is the one that does not consume an index: a `th` entry
    // that matches NO record at all. `th:4` is configured and absent, so `th:3` is still
    // index 1 rather than index 2.
    let r = one("th:1, th:4, th:3", TWO_TRANSMITTERS).values;
    near(r.out_temp, 58.4);
    near(r.extra_temp1, 39.9);
    assert_eq!(r.extra_temp2, None);
}

#[test]
fn arming_the_broadcast_decodes_to_a_port_and_a_duration() {
    assert_eq!(
        real_time(&Frame::new(REAL_TIME.as_bytes().to_vec())).expect("decodes"),
        Broadcast {
            port: 22222,
            duration_seconds: 3600,
        }
    );
}

#[test]
fn a_refusal_carrying_a_two_hundred_is_a_device_error_and_not_a_reading() {
    let frame = Frame::new(DEVICE_ERROR.as_bytes().to_vec());
    let e = codec(VP2_PLUS)
        .decode(&frame)
        .expect_err("the unit refused");
    assert!(matches!(e, CodecError::Device { code: 409, .. }), "{e}");
    // ... and the same body on the arming path, which is the call that actually gets
    // refused: the unit answers 409 when another client already holds the broadcast.
    let e = real_time(&frame).expect_err("the unit refused");
    assert_eq!(
        e.to_string(),
        "the device reported error 409: Request Rejected"
    );
}

#[test]
fn every_fixture_claims_the_station_its_serial_names() {
    for (frame, did) in [
        (BROADCAST, "001D0A700002"),
        (BROADCAST_METRIC, "001D0A710ABC"),
        (CURRENT, "001D0A700002"),
        (AGRI_FRAME, "001D0A70BEEF"),
    ] {
        let claimed = codec(VP2_PLUS)
            .claimed_station(&Frame::new(frame.as_bytes().to_vec()))
            .expect("the frame carries a serial");
        assert_eq!(claimed.as_str(), did);
    }
    // A device error carries no serial, so there is nothing to claim and nothing to drop
    // a reading against.
    assert!(codec(VP2_PLUS)
        .claimed_station(&Frame::new(DEVICE_ERROR.as_bytes().to_vec()))
        .is_none());
}

/// The property that makes a golden test worth having: the same bytes decode to the same
/// reading every time, with nothing in the output that came from outside the frame.
#[test]
fn decoding_is_a_function_of_the_frame_alone() {
    let first = one(VP2_PLUS, CURRENT);
    let second = one(VP2_PLUS, CURRENT);
    assert_eq!(first, second);
    assert_ne!(first.values, Reading::default());
    assert_eq!(codec(VP2_PLUS).name(), "wll-conditions");
}
