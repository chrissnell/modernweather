//! The Ambient decoder, over query strings.
//!
//! The upload below is a WS-2902's — the console the Go driver was written against — with
//! the extra-sensor, particulate and lightning groups appended so that one fixture exercises
//! every group in the module's own table.

use super::*;

/// What a console with the full sensor complement sends. One line per group, in the order
/// the module documents them.
const UPLOAD: &str = concat!(
    "ID=roof&PASSWORD=hunter2&action=updateraw&dateutc=2026-11-10+22%3A49%3A09",
    "&tempf=68.4&humidity=41&dewptf=44.2&winddir=183&windspeedmph=7.5&windgustmph=12.1",
    "&baromin=30.12&baromrelin=30.12&baromabsin=29.88&solarradiation=512.5&UV=3",
    "&indoortempf=70.1&indoorhumidity=38",
    "&rainin=0.02&dailyrainin=0.31&weeklyrainin=1.10&monthlyrainin=2.40&totalrainin=118.75",
    "&temp1f=61.2&temp10f=59.9&humidity1=55&humidity10=57",
    "&soiltemp1f=52.3&soiltemp10f=51.1&soilhum1=31&soilhum10=29",
    "&leafwetness1=4&leafwetness8=0&soiltens1=12.5&soiltens4=14.0",
    "&gdd=1180&etos=0.21&etrs=0.26",
    "&pm25=8.1&pm25_24h=9.4&pm25_in=5.2&pm25_in_24h=6.0",
    "&pm25_in_aqin=5.5&pm25_in_24h_aqin=6.1&pm10_in_aqin=11.2&pm10_in_24h_aqin=12.8",
    "&co2=612&co2_in_aqin=640&co2_in_24h_aqin=655&pm_in_temp_aqin=70.5",
    "&pm_in_humidity_aqin=39&aqi_pm25_aqin=34&aqi_pm25_24h_aqin=39&aqi_pm10_aqin=10",
    "&aqi_pm10_24h_aqin=12&aqi_pm25_in=22&aqi_pm25_in_24h=25",
    "&lightning_day=3&lightning_hour=1&lightning_time=2026-11-10T22%3A40%3A00Z",
    "&lightning_distance=8.5",
    "&leak1=0&leak4=1&relay1=1&relay10=0",
    "&battout=1&battin=1&batt1=1&batt10=0&batt_25=1&batt_lightning=1",
    "&batleak1=1&batleak4=0&battsm1=1&battsm4=0&batt_co2=1&batt_cellgateway=1",
    "&lowbatt=0&tz=America%2FDenver",
);

fn frame(query: &str) -> Frame {
    Frame::new(query.as_bytes().to_vec())
}

fn decode(codec: &AmbientQuery, query: &str) -> PartialReading {
    let batch = codec.decode(&frame(query)).expect("the upload decodes");
    assert_eq!(batch.len(), 1, "one upload is one observation");
    batch[0].clone()
}

/// The instant the console declares, not the instant we received it.
#[test]
fn the_uploads_own_timestamp_is_the_observation_instant() {
    let decoded = decode(&AmbientQuery::default(), UPLOAD);
    let at = decoded.observed_at.expect("`dateutc` is required");
    assert_eq!(at.to_rfc3339(), "2026-11-10T22:49:09+00:00");
}

#[test]
fn every_group_in_the_table_lands_on_its_metrics() {
    let r = decode(&AmbientQuery::default(), UPLOAD).values;

    // The console.
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

    // Rain. `rainin` is what the codec puts in `rainincremental`; `totalrainin` is the
    // driver's to override, and this test is the codec's.
    assert_eq!(r.rain_incremental, Some(0.02));
    assert_eq!(r.day_rain, Some(0.31));
    assert_eq!(r.extra_float1, Some(1.10), "`weeklyrainin`, as Go files it");
    assert_eq!(r.month_rain, Some(2.40));

    // Extra sensors, at both ends of each range.
    assert_eq!(r.temp1, Some(61.2));
    assert_eq!(r.temp10, Some(59.9));
    assert_eq!(r.humidity1, Some(55.0));
    assert_eq!(r.humidity10, Some(57.0));
    assert_eq!(r.soil_temp1, Some(52.3));
    assert_eq!(r.soil_temp10, Some(51.1));
    assert_eq!(r.soil_hum1, Some(31.0));
    assert_eq!(r.soil_hum10, Some(29.0));
    assert_eq!(r.leaf_wetness1, Some(4.0));
    assert_eq!(r.leaf_wetness8, Some(0.0));
    assert_eq!(r.soil_tens1, Some(12.5));
    assert_eq!(r.soil_tens4, Some(14.0));

    // Agriculture.
    assert_eq!(r.gdd, Some(1180));
    assert_eq!(r.etos, Some(0.21));
    assert_eq!(r.etrs, Some(0.26));

    // Particulates.
    assert_eq!(r.pm25, Some(8.1));
    assert_eq!(r.pm25_24h, Some(9.4));
    assert_eq!(r.pm25_in, Some(5.2));
    assert_eq!(r.pm25_in_24h, Some(6.0));
    assert_eq!(r.pm25_in_aqin, Some(5.5));
    assert_eq!(r.pm25_in_24h_aqin, Some(6.1));
    assert_eq!(r.pm10_in_aqin, Some(11.2));
    assert_eq!(r.pm10_in_24h_aqin, Some(12.8));
    assert_eq!(r.co2, Some(612.0));
    assert_eq!(r.co2_in_aqin, Some(640));
    assert_eq!(r.co2_in_24h_aqin, Some(655));
    assert_eq!(r.pm_in_temp_aqin, Some(70.5));
    assert_eq!(r.pm_in_humidity_aqin, Some(39));
    assert_eq!(r.aqi_pm25_aqin, Some(34));
    assert_eq!(r.aqi_pm25_24h_aqin, Some(39));
    assert_eq!(r.aqi_pm10_aqin, Some(10));
    assert_eq!(r.aqi_pm10_24h_aqin, Some(12));
    assert_eq!(r.aqi_pm25_in, Some(22));
    assert_eq!(r.aqi_pm25_in_24h, Some(25));

    // Lightning.
    assert_eq!(r.lightning_day, Some(3));
    assert_eq!(r.lightning_hour, Some(1));
    assert_eq!(
        r.lightning_time.map(|at| at.to_rfc3339()),
        Some("2026-11-10T22:40:00+00:00".to_string())
    );
    assert_eq!(r.lightning_distance, Some(8.5));

    // Hardware.
    assert_eq!(r.leak1, Some(0));
    assert_eq!(r.leak4, Some(1));
    assert_eq!(r.relay1, Some(1));
    assert_eq!(r.relay10, Some(0));
    assert_eq!(r.batt_out, Some(1));
    assert_eq!(r.batt_in, Some(1));
    assert_eq!(r.batt1, Some(1));
    assert_eq!(r.batt10, Some(0));
    assert_eq!(r.batt_25, Some(1));
    assert_eq!(r.batt_lightning, Some(1));
    assert_eq!(r.bat_leak1, Some(1));
    assert_eq!(r.bat_leak4, Some(0));
    assert_eq!(r.batt_sm1, Some(1));
    assert_eq!(r.batt_sm4, Some(0));
    assert_eq!(r.batt_co2, Some(1));
    assert_eq!(r.batt_cellgateway, Some(1));
    assert_eq!(r.tx_battery_status, Some(0), "`lowbatt`");

    // Metadata.
    assert_eq!(r.tz.as_deref(), Some("America/Denver"));

    // The enrich stage's, and the one column Go leaves at zero.
    assert_eq!(r.wind_chill, None);
    assert_eq!(r.heat_index, None);
    assert_eq!(r.date_utc, None);
    // Nothing an Ambient console reports.
    assert_eq!(r.radiation, None);
    assert_eq!(r.snow_distance, None);
}

/// The first and second disagreements, on the console that is actually in the field.
#[test]
fn a_console_that_sends_twenty_parameters_leaves_the_other_hundred_and_ten_null() {
    let r = decode(
        &AmbientQuery::default(),
        "ID=roof&PASSWORD=p&action=updateraw&dateutc=2026-11-10+22%3A49%3A09\
         &tempf=68.4&humidity=41&windspeedmph=7.5&winddir=183&baromin=30.12\
         &indoortempf=&indoorhumidity=",
    )
    .values;
    assert_eq!(r.out_temp, Some(68.4));
    assert_eq!(r.wind_speed, Some(7.5));
    // Absent, where Go stores 0 degF for an indoor sensor that is not installed...
    assert_eq!(r.in_temp, None);
    assert_eq!(r.soil_temp1, None);
    assert_eq!(r.pm25, None);
    assert_eq!(r.batt_out, None);
    assert_eq!(r.tz, None);
    // ... and absent for a key that arrived empty, which is a sensor the console lost.
    assert_eq!(r.in_humidity, None);
}

/// The third disagreement.
#[test]
fn a_value_that_is_not_a_finite_number_is_refused() {
    for bad in [
        "NaN", "nan", "inf", "Inf", "+Inf", "-inf", "banana", "1.2.3",
    ] {
        let r = decode(
            &AmbientQuery::default(),
            &format!("dateutc=2026-11-10+22%3A49%3A09&tempf={bad}&humidity=41"),
        )
        .values;
        assert_eq!(r.out_temp, None, "`tempf={bad}` decoded to a value");
        assert_eq!(
            r.out_humidity,
            Some(41.0),
            "`tempf={bad}` cost its neighbour"
        );
    }
}

/// An integer parameter that arrives as `612.0`, which Go's `parseInt` rejects.
#[test]
fn an_integer_parameter_that_arrives_with_a_decimal_point_still_decodes() {
    let r = decode(
        &AmbientQuery::default(),
        "dateutc=2026-11-10+22%3A49%3A09&co2_in_aqin=640.0&gdd=1180&aqi_pm25_in=22.0",
    )
    .values;
    assert_eq!(r.co2_in_aqin, Some(640));
    assert_eq!(r.gdd, Some(1180));
    assert_eq!(r.aqi_pm25_in, Some(22));

    // And one outside the column's range is not an integer this column can hold, where
    // `as` would saturate it to a plausible wrong one.
    let r = decode(
        &AmbientQuery::default(),
        "dateutc=2026-11-10+22%3A49%3A09&gdd=99999999999",
    )
    .values;
    assert_eq!(r.gdd, None);
}

#[test]
fn the_vane_calibration_is_applied_to_the_direction_and_to_nothing_else() {
    let codec = AmbientQuery::new(WindDirCorrection::new(10));
    assert_eq!(codec.correction().degrees(), 10);
    let r = decode(
        &codec,
        "dateutc=2026-11-10+22%3A49%3A09&winddir=355.5&windspeedmph=7.5",
    )
    .values;
    assert_eq!(r.wind_dir, Some(5.5), "Go truncates the half degree");
    assert_eq!(r.wind_speed, Some(7.5));
}

/// `dateutc` is required, and the two ways it can be wrong say which.
#[test]
fn an_upload_with_no_usable_timestamp_is_refused_by_name() {
    let codec = AmbientQuery::default();
    for query in ["tempf=68.4", "tempf=68.4&dateutc=", "dateutc=now"] {
        let e = codec.decode(&frame(query)).expect_err("no instant");
        let CodecError::Incomplete { what } = e else {
            panic!("`{query}`: expected an incomplete upload, got {e}");
        };
        assert!(what.contains("dateutc"), "`{query}`: {what}");
    }
    // The distinction: missing names the requirement, unparseable names the format.
    let missing = codec.decode(&frame("tempf=1")).expect_err("missing");
    assert!(missing.to_string().contains("every `updateraw` upload"));
    let malformed = codec
        .decode(&frame("dateutc=10/11/2026+22:49"))
        .expect_err("malformed");
    assert!(malformed.to_string().contains("YYYY-MM-DD"));
}

/// The parse the driver and the codec share.
#[test]
fn the_driver_reads_the_identity_out_of_the_same_parse_the_codec_maps() {
    let q = params(&frame(UPLOAD)).expect("a query string");
    assert_eq!(q.get(ID_PARAM), Some("roof"));
    assert_eq!(q.get(PASSWORD_PARAM), Some("hunter2"));
    assert_eq!(q.get(ACTION_PARAM), Some(ACTION_UPDATERAW));
    assert_eq!(total_rain(&q), Some(118.75));
    assert_eq!(
        observed_at(&q).expect("an instant").to_rfc3339(),
        "2026-11-10T22:49:09+00:00"
    );
    assert_eq!(q.non_empty(WIND_DIR_PARAM), Some("183"));

    // And a frame that is not text is refused before any of it.
    let e = params(&Frame::new(vec![0xff, 0xfe])).expect_err("not text");
    assert!(matches!(e, CodecError::NotText), "{e}");
}

/// The stateful half, as a pure function. Go stores `0` for all three absences.
#[test]
fn rainfall_since_the_previous_upload_is_absent_rather_than_zero_where_it_cannot_be_derived() {
    // `f32` subtraction of two large totals, which is what the column holds: the exact
    // value is the one the arithmetic produces and not 0.05, and asserting the rounded
    // number would be asserting a different function.
    assert_eq!(rain_since(Some(118.70), 118.75), Some(0.050_003_05));
    assert_eq!(rain_since(Some(118.75), 118.75), Some(0.0), "no rain fell");
    assert_eq!(rain_since(None, 118.75), None, "the first upload");
    assert_eq!(
        rain_since(Some(0.0), 0.31),
        None,
        "Go's `previous > 0` test"
    );
    assert_eq!(rain_since(Some(200.0), 0.31), None, "a console that reset");
    assert_eq!(rain_since(Some(f32::NAN), 1.0), None);
    assert_eq!(rain_since(Some(1.0), f32::INFINITY), None);
}

#[test]
fn the_codec_names_itself_and_does_not_claim_a_station_through_the_codec_trait() {
    let codec = AmbientQuery::default();
    assert_eq!(codec.name(), "ambient-query");
    // `?ID=` is a `FromQuery` claim, which the DRIVER builds from `params`: a codec that
    // answered `claimed_station` would be a second place identity comes from, and
    // `FrameMeta::source` would then disagree with how the claim was found.
    assert_eq!(codec.claimed_station(&frame(UPLOAD)), None);
}
