//! Level 1 of W3 **D-82**'s three, for Ambient: the pure codec against a **captured** query
//! string.
//!
//! The fixture is one `updateraw` upload exactly as a console sends it — percent-encoding
//! and all — captured off a socket from `tests/emulators/ambient`. That emulator is written
//! in this repository, so what this pins is the decoder against the vendor's documented
//! vocabulary rather than against an independent authority;
//! `tests/emulators/manifest.toml` records that and the gate prints it.
//!
//! What the *encoding* half pins is real either way: `+` is a space and `%3A` is a colon in
//! any console's URL, and a decoder that got either wrong would fail here.

use mw_codec::{
    ambient_observed_at, ambient_params, rain_since, total_rain, AmbientQuery, Codec, CodecError,
    Frame, QueryParams, WindDirCorrection, ACTION_PARAM, ACTION_UPDATERAW, ID_PARAM,
    PASSWORD_PARAM,
};

/// One captured upload. `include_str!` leaves the trailing newline the capture wrote, which
/// is trimmed here rather than in the fixture: a query string with a newline in it is not
/// one a console sends, and the decoder should not have to tolerate it.
const UPLOAD: &str = include_str!("frames/ambient-upload.txt");

fn query() -> &'static str {
    UPLOAD.trim_end()
}

fn frame() -> Frame {
    Frame::new(query().as_bytes().to_vec())
}

fn decode() -> mw_types::PartialReading {
    let batch = AmbientQuery::default()
        .decode(&frame())
        .expect("a captured upload decodes");
    assert_eq!(batch.len(), 1, "one upload is one observation");
    batch[0].clone()
}

/// The capture is still percent-encoded, which is what makes the rest of this file a test
/// of the decoding rather than of a pre-decoded string.
#[test]
fn the_fixture_is_a_console_url_and_not_a_decoded_one() {
    assert!(
        query().contains("dateutc=2026-11-10+22%3A49%3A09"),
        "the timestamp arrives `+`-separated and colon-escaped: {}",
        query()
    );
    assert!(
        query().contains("tz=America%2FDenver"),
        "and the zone's slash is escaped: {}",
        query()
    );
    let params = QueryParams::parse(query());
    assert_eq!(params.skipped(), 0, "every pair decoded");
    assert!(params.len() >= 30, "{} parameters", params.len());
    assert_eq!(params.get("dateutc"), Some("2026-11-10 22:49:09"));
    assert_eq!(params.get("tz"), Some("America/Denver"));
}

/// **The upload's own instant**, which is the one thing a console sends that the arrival
/// time cannot substitute for: an Ambient console buffers and retries.
#[test]
fn the_uploads_dateutc_is_the_observation_instant() {
    let decoded = decode();
    assert_eq!(
        decoded
            .observed_at
            .expect("`dateutc` is required")
            .to_rfc3339(),
        "2026-11-10T22:49:09+00:00"
    );
}

#[test]
fn the_captured_upload_fills_the_metrics_the_console_sent() {
    let r = decode().values;
    assert_eq!(r.out_temp, Some(68.4));
    assert_eq!(r.out_humidity, Some(41.0));
    assert_eq!(r.extra_temp1, Some(44.2), "`dewptf`, as Go files it");
    assert_eq!(r.wind_dir, Some(183.0));
    assert_eq!(r.wind_speed, Some(7.5));
    assert_eq!(r.wind_speed10, Some(12.1), "`windgustmph`, as Go files it");
    assert_eq!(r.barometer, Some(30.12));
    assert_eq!(r.barom_rel_in, Some(30.12));
    assert_eq!(r.barom_abs_in, Some(29.88));
    assert_eq!(r.solar_watts, Some(512.5));
    assert_eq!(r.uv, Some(3.0));
    assert_eq!(r.in_temp, Some(70.1));
    assert_eq!(r.in_humidity, Some(38.0));
    assert_eq!(
        r.rain_incremental,
        Some(0.02),
        "`rainin`, the hourly figure"
    );
    assert_eq!(r.day_rain, Some(0.31));
    assert_eq!(r.month_rain, Some(2.40));
    assert_eq!(r.extra_float1, Some(1.10), "`weeklyrainin`, as Go files it");
    assert_eq!(r.pm25, Some(8.1));
    assert_eq!(r.co2, Some(612.0));
    assert_eq!(r.aqi_pm25_aqin, Some(34));
    assert_eq!(r.lightning_day, Some(3));
    assert_eq!(r.lightning_distance, Some(8.5));
    assert_eq!(
        r.lightning_time.map(|at| at.to_rfc3339()),
        Some("2026-11-10T22:49:09+00:00".to_string()),
        "`lightning_time` is RFC 3339 and `dateutc` is not: two formats in one upload"
    );
    assert_eq!(r.tz.as_deref(), Some("America/Denver"));
    assert_eq!(r.batt_out, Some(1));
    assert_eq!(r.tx_battery_status, Some(0), "`lowbatt`");

    // The enrich stage's two (DEV-9), and the column Go leaves at zero because its own
    // `parseInt64("dateutc")` never succeeds against the protocol's format.
    assert_eq!(r.wind_chill, None);
    assert_eq!(r.heat_index, None);
    assert_eq!(r.date_utc, None);
    // A hundred-odd parameters this console does not have.
    assert_eq!(r.soil_temp1, None);
    assert_eq!(r.leaf_wetness1, None);
    assert_eq!(r.snow_distance, None);
}

/// The driver reads `?ID=`, `?PASSWORD=`, `?action=` and the rainfall counter out of the
/// **same parse** this codec maps, which is the one implementation of
/// "`+` means space" there is.
#[test]
fn the_driver_reads_its_four_parameters_out_of_the_same_parse() {
    let params = ambient_params(&frame()).expect("a query string");
    assert_eq!(params.get(ID_PARAM), Some("roof"));
    assert_eq!(params.get(PASSWORD_PARAM), Some("hunter2"));
    assert_eq!(params.get(ACTION_PARAM), Some(ACTION_UPDATERAW));
    assert_eq!(total_rain(&params), Some(118.70));
    assert_eq!(
        ambient_observed_at(&params)
            .expect("an instant")
            .to_rfc3339(),
        "2026-11-10T22:49:09+00:00"
    );
    // And the counter becomes an increment only across two uploads, which is the driver's
    // one piece of state.
    assert_eq!(rain_since(None, 118.70), None, "the first upload");
    assert!(matches!(rain_since(Some(118.70), 118.75), Some(v) if (0.049..=0.051).contains(&v)));
}

#[test]
fn the_vane_calibration_moves_the_captured_direction_and_no_other_field() {
    let plain = decode().values;
    let corrected = AmbientQuery::new(WindDirCorrection::new(10))
        .decode(&frame())
        .expect("decodes")[0]
        .values
        .clone();
    assert_eq!(corrected.wind_dir, Some(193.0));
    assert_eq!(corrected.wind_speed, plain.wind_speed);
    assert_eq!(corrected.out_temp, plain.out_temp);
}

/// `dateutc` is required, and the captured upload with it removed says which parameter.
#[test]
fn the_captured_upload_without_its_timestamp_is_refused_by_name() {
    let without: String = query()
        .split('&')
        .filter(|field| !field.starts_with("dateutc="))
        .collect::<Vec<_>>()
        .join("&");
    let e = AmbientQuery::default()
        .decode(&Frame::new(without.into_bytes()))
        .expect_err("no instant");
    let CodecError::Incomplete { what } = e else {
        panic!("expected an incomplete upload, got {e}");
    };
    assert!(what.contains("dateutc"), "{what}");
}

#[test]
fn decoding_is_a_function_of_the_query_alone() {
    assert_eq!(decode(), decode());
    assert_eq!(AmbientQuery::default().name(), "ambient-query");
}
