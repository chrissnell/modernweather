//! The golden test: captured packets, compared byte for byte.
//!
//! `implementation-plan.md` M6-2's done-criterion is *"a golden test compares an encoded
//! packet against a captured one byte for byte"*, and this is it. The property it asserts
//! is a **round trip**: each captured information field is parsed into values, the values
//! are re-encoded, and the bytes are compared with the capture. That is stronger than
//! asserting against a literal somebody typed, because a literal only tests whoever typed
//! it — the two directions have to agree about every field width, every unit and every
//! rounding rule, or the bytes differ.
//!
//! # Where the captures come from
//!
//! `fixtures/*.aprs` is one information field per file, with a `#` comment block above it
//! naming the source. Three kinds:
//!
//! - **The APRS Protocol Reference's own worked examples** (v1.0.1 §12, §9). These are the
//!   normative bytes — the specification's statement of what a complete weather report
//!   looks like — which makes them the best possible capture for a conformance test.
//! - **CWOP traffic in the shape the live network carries**, including the two cases the Go
//!   builder gets wrong: a station whose minutes are under ten, and a station reporting no
//!   temperature.
//! - **A packet with an operator comment**, because that is the one field this program does
//!   not generate and still has to reproduce.
//!
//! This test links no database and no transport, which `ci/layers.toml` enforces over this
//! crate's dev-dependencies as well as its dependencies — W2a assumption 7. It reads no
//! file either: the captures are embedded at compile time, for the reason [`CAPTURES`]
//! records.

use mw_aprs::{parse_info, Position, Symbol, WeatherReport};

/// The captures, embedded at compile time.
///
/// Read at build time rather than at run time, and not because it is tidier:
/// `ci/layers.toml` forbids this crate's **source** the standard library's filesystem
/// module, dev-dependencies included, so a test that opened a file would be a gate failure.
/// That rule is W2a assumption 7 — a golden test for this encoder links without a database
/// or a transport — and embedding is what satisfies it. It also makes the captures part of
/// the binary that asserts against them, so a stale path is not a failure mode.
const CAPTURES: &[(&str, &str)] = &[
    (
        "spec-complete-weather-report",
        include_str!("fixtures/spec-complete-weather-report.aprs"),
    ),
    (
        "cwop-minutes-under-ten",
        include_str!("fixtures/cwop-minutes-under-ten.aprs"),
    ),
    (
        "cwop-no-temperature",
        include_str!("fixtures/cwop-no-temperature.aprs"),
    ),
    (
        "cwop-with-comment",
        include_str!("fixtures/cwop-with-comment.aprs"),
    ),
    (
        "cwop-high-solar",
        include_str!("fixtures/cwop-high-solar.aprs"),
    ),
    (
        "cwop-below-zero",
        include_str!("fixtures/cwop-below-zero.aprs"),
    ),
];

/// One fixture: the provenance block, and the information field.
struct Capture {
    name: &'static str,
    /// The `#`-prefixed provenance block, which the test asserts is present.
    provenance: String,
    /// The one information-field line.
    info: String,
}

fn captures() -> Vec<Capture> {
    CAPTURES
        .iter()
        .map(|(name, text)| {
            let provenance: String = text
                .lines()
                .filter(|l| l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            let info: Vec<&str> = text
                .lines()
                .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                .collect();
            assert_eq!(
                info.len(),
                1,
                "{name} holds one information field and {} were found",
                info.len()
            );
            Capture {
                name,
                provenance,
                info: info[0].to_string(),
            }
        })
        .collect()
}

/// **The done-criterion.** Every captured field re-encodes to itself.
#[test]
fn every_captured_packet_re_encodes_to_the_same_bytes() {
    let captures = captures();
    assert!(!captures.is_empty(), "the fixture directory is empty");
    for Capture { name, info, .. } in &captures {
        let parsed = parse_info(info).unwrap_or_else(|e| panic!("{name}: {e}"));
        let encoded = parsed
            .weather
            .encode_info(
                parsed.position,
                parsed.symbol,
                &parsed.trailer,
                // The trailer carries the software identifier *and* the comment as one
                // string, because that is how a capture holds them: the protocol has no
                // delimiter between them that a parser could rely on. So it goes back in
                // the software position and the comment is `None`.
                None,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            encoded.as_str(),
            info,
            "{name} did not round trip\n  captured: {info}\n  re-encoded: {encoded}"
        );
        assert_eq!(
            encoded.as_bytes(),
            info.as_bytes(),
            "{name} differs at the byte level despite comparing equal as text"
        );
    }
}

/// Every fixture says where it came from. A golden file whose provenance is unrecorded is
/// a literal somebody typed, which is the thing this test exists not to be.
#[test]
fn every_fixture_records_its_provenance() {
    for Capture {
        name, provenance, ..
    } in &captures()
    {
        assert!(
            provenance.len() > 40,
            "{name} has no provenance block: {provenance}"
        );
    }
}

/// The specification's own worked example, field by field, so a round trip that is
/// self-consistently wrong is still caught.
///
/// §12 of the protocol reference: *"`!4903.50N/07201.75W_220/004g005t077r000p000P000h50
/// b09900wRSW`"*, which it describes as a complete weather report from a station at
/// 49°03.50′N 072°01.75′W with a wind from 220° at 4 mph, gusting 5, 77 °F, no rain, 50%
/// humidity and 990.0 hPa.
#[test]
fn the_specifications_example_decodes_to_the_values_it_documents() {
    let parsed = parse_info("!4903.50N/07201.75W_220/004g005t077r000p000P000h50b09900wRSW")
        .expect("the reference's example parses");
    assert_eq!(parsed.symbol, Symbol::WEATHER);
    assert!((parsed.position.latitude() - 49.058_333).abs() < 1e-5);
    assert!((parsed.position.longitude() + 72.029_166).abs() < 1e-5);
    assert_eq!(parsed.weather.wind_direction_deg, Some(220.0));
    assert_eq!(parsed.weather.wind_speed_mph, Some(4.0));
    assert_eq!(parsed.weather.wind_gust_mph, Some(5.0));
    assert_eq!(parsed.weather.temperature_f, Some(77.0));
    assert_eq!(parsed.weather.rain_last_hour_in, Some(0.0));
    assert_eq!(parsed.weather.rain_last_24h_in, Some(0.0));
    assert_eq!(parsed.weather.rain_since_midnight_in, Some(0.0));
    assert_eq!(parsed.weather.humidity_percent, Some(50.0));
    let hpa = parsed.weather.barometer_in_hg.expect("a barometer") * 33.863_886_666_666_7;
    assert!((hpa - 990.0).abs() < 0.01, "{hpa} hPa");
    assert_eq!(parsed.weather.solar_watts_per_m2, None);
    assert_eq!(parsed.trailer, "wRSW");
}

/// **The defect the captures exist to pin.** A station between 45°00′ and 45°10′ renders a
/// seven-byte latitude under the Go format string, and the symbol-table byte that follows
/// is then read as part of the position.
#[test]
fn a_station_whose_minutes_are_under_ten_encodes_eight_bytes_of_latitude() {
    let position = Position::new(45.1, -122.05).expect("a position");
    let info = WeatherReport {
        wind_direction_deg: Some(10.0),
        wind_speed_mph: Some(3.0),
        wind_gust_mph: Some(7.0),
        temperature_f: Some(41.0),
        ..WeatherReport::default()
    }
    .encode_info(position, Symbol::WEATHER, "", None)
    .expect("encodes");
    assert!(info.as_str().starts_with("!4506.00N/12203.00W_"), "{info}");
    // What the Go builder produces for the same position, for the record: `%2d%2.2f%v`
    // renders `456.00N`, so the `/` lands where the hundredths of a minute belong.
    assert!(!info.as_str().starts_with("!456.00N"), "{info}");
    // A receiver that read seven bytes of latitude would find the symbol table at the
    // wrong offset, which is a position several degrees away rather than a parse failure.
    let parsed = parse_info(info.as_str()).expect("round trips");
    assert!((parsed.position.latitude() - 45.1).abs() < 0.001);
}
