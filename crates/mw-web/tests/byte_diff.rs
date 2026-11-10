//! **The acceptance test for the website port: a byte diff against W1's recorded bodies.**
//!
//! `docs/service-architecture.md` §7.1 takes the stronger of the two assertions W1 left
//! available:
//!
//! > The website port is verified by byte-diffing the rendered output against W1's recorded
//! > HTML bodies for the six captured websites. W1 Decision 14 records the full rendered
//! > bodies *as a reference for W3* while asserting only at substring level; this document
//! > takes the stronger assertion.
//!
//! So this test reads the fixtures themselves -- `contract/fixtures/html-weather-station.json`
//! and `html-portal.json` -- rather than a copy of them, and compares every byte. The
//! conformance suite asserts the same two responses at substring level, which is a different
//! and weaker thing; a port that dropped a `<div>` would pass there and fail here.
//!
//! # The one expected delta, as a patch
//!
//! `weather-station.html` gains the `<script id="mw-config" type="application/json">` block
//! (**D-79**, **D-80**) immediately before the module that reads it. The patch is applied to
//! the **recording**, so any *other* difference is a failure -- a tolerance would have made
//! this test say nothing.
//!
//! §7.4 predicted a second delta, "a changed `src` attribute on the weather-app script tag",
//! and it does not happen: `/js/weather-app.js` keeps its path because §7.2's mime table
//! freezes that exact path's `Content-Type` and `contract/fixtures/asset-weather-app-js`
//! requests it.
//!
//! # Two values the recording pins that the daemon does not
//!
//! `Version` was `5.10.22` at capture and is the daemon's own version now, so it is passed
//! in. `AppleAppID` was the Go schema's column default (`6755874087`), which the new schema
//! does not have -- `ci/conformance/rust-seed/01-config.sql` sets it explicitly, and the
//! `SiteView` below carries the same value.

use std::path::PathBuf;

use mw_types::{StationKey, WebsiteId};
use mw_web::{page, SiteView};

/// Where `contract/fixtures/` is, from this crate.
fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("contract")
        .join("fixtures")
}

/// One fixture's recorded response body.
fn recorded(name: &str) -> String {
    let path = fixtures().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let json: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let body = json["response"]["body"]
        .as_str()
        .unwrap_or_else(|| panic!("{} records no body", path.display()))
        .to_string();
    let declared = json["response"]["body_bytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("{} declares no body_bytes", path.display()));
    assert_eq!(
        body.len() as u64,
        declared,
        "{}: the recorded body is not the recorded length, so the fixture itself is wrong",
        path.display()
    );
    body
}

/// The `localhost` website of `ci/conformance/rust-seed/01-config.sql`, which is the one
/// `html-weather-station` was captured against.
fn suncrest() -> SiteView {
    SiteView {
        id: WebsiteId::new(1),
        hostname: "localhost".into(),
        name: "Suncrest Live Weather".into(),
        page_title: "Suncrest Live Weather".into(),
        apple_app_id: "6755874087".into(),
        is_portal: false,
        radar_enabled: true,
        radar_token: "contract-radar-token".into(),
        snow_enabled: true,
        snow_device: "snowgauge".into(),
        air_quality_enabled: true,
        air_quality_device: "aqmon".into(),
        air_quality_device_id: 4,
        primary_station: Some(StationKey::new("davis-full")),
        primary_station_name: "davis-full".into(),
        primary_station_id: 1,
        latitude: Some(39.900),
        longitude: Some(-105.500),
        aeris_enabled: true,
    }
}

/// The version the Go daemon reported when W1 captured the page (`constants.Version`).
const CAPTURED_VERSION: &str = "5.10.22";

/// The anchor the configuration block is inserted before, which is the module that reads it.
const ANCHOR: &str = "  <script src=\"js/weather-app.js\"></script>\n";

/// The block, as the template emits it for [`suncrest`].
const MW_CONFIG: &str = concat!(
    "  <script id=\"mw-config\" type=\"application/json\">\n",
    "    {\"pullFromDevice\": \"davis-full\", \"stationID\": 1, ",
    "\"stationName\": \"Suncrest Live Weather\", \"snowEnabled\": true, ",
    "\"snowDevice\": \"snowgauge\", \"airQualityEnabled\": true, ",
    "\"airQualityDevice\": \"aqmon\", \"airQualityDeviceID\": 4, ",
    "\"aerisWeatherEnabled\": true}\n",
    "  </script>\n"
);

/// Report the first difference as a line number and the two lines, because a 40 kB diff of
/// one byte is unreadable.
fn assert_same_bytes(what: &str, expected: &str, got: &str) {
    if expected == got {
        return;
    }
    let mut line = 1;
    let mut e = expected.lines();
    let mut g = got.lines();
    loop {
        match (e.next(), g.next()) {
            (None, None) => break,
            (a, b) if a == b => line += 1,
            (a, b) => panic!(
                "{what}: line {line} differs\n  recorded: {a:?}\n  rendered: {b:?}\n\
                 (recorded {} bytes, rendered {} bytes)",
                expected.len(),
                got.len()
            ),
        }
    }
    // Every line matched, so the difference is a trailing newline.
    panic!(
        "{what}: the lines are identical and the bytes are not -- a trailing newline \
         differs (recorded {} bytes, rendered {} bytes)",
        expected.len(),
        got.len()
    );
}

/// The headline done-criterion of implementation-plan.md M4-3.
#[test]
fn the_weather_page_is_byte_identical_to_the_recording_plus_one_block() {
    let recorded = recorded("html-weather-station");
    assert!(
        recorded.contains(ANCHOR),
        "the recording must contain the anchor the block is inserted before"
    );
    let expected = recorded.replacen(ANCHOR, &format!("{MW_CONFIG}{ANCHOR}"), 1);
    let rendered = page::weather_station(&suncrest(), CAPTURED_VERSION).expect("the page renders");
    assert_same_bytes("html-weather-station", &expected, &rendered);
}

/// **D-75**: the portal template consumes nothing, so the port is the file itself and the
/// diff has no delta at all.
#[test]
fn the_portal_page_is_byte_identical_to_the_recording() {
    let portal = recorded("html-portal");
    let rendered = String::from_utf8(page::portal().to_vec()).expect("the portal page is utf-8");
    assert_same_bytes("html-portal", &portal, &rendered);
    // And `/` on a portal website is the same bytes, which is what `html-portal-root`
    // records.
    assert_same_bytes("html-portal-root", &recorded("html-portal-root"), &rendered);
}

/// The two static pages, which were already static in the Go tree.
#[test]
fn the_privacy_and_support_pages_are_byte_identical_to_their_recordings() {
    assert_same_bytes(
        "html-privacy",
        &recorded("html-privacy"),
        str::from_utf8(page::privacy()).expect("utf-8"),
    );
    assert_same_bytes(
        "html-support",
        &recorded("html-support"),
        str::from_utf8(page::support()).expect("utf-8"),
    );
}

/// **The negative control.** A byte diff that cannot fail is worth nothing, so this asserts
/// that a one-character change to the page is caught -- which is the property the three
/// tests above rest on.
#[test]
#[should_panic(expected = "line")]
fn the_byte_diff_catches_a_single_changed_character() {
    let mut site = suncrest();
    site.name = "Suncrest Live Weathe".into(); // one byte shorter
    let recorded = recorded("html-weather-station");
    let expected = recorded.replacen(ANCHOR, &format!("{MW_CONFIG}{ANCHOR}"), 1);
    let rendered = page::weather_station(&site, CAPTURED_VERSION).expect("renders");
    assert_same_bytes("negative control", &expected, &rendered);
}

/// And that the **delta itself** is load-bearing: comparing against the unpatched recording
/// must fail, or the block would not be being emitted at all.
#[test]
#[should_panic(expected = "line")]
fn the_rendered_page_is_not_identical_to_the_unpatched_recording() {
    let rendered = page::weather_station(&suncrest(), CAPTURED_VERSION).expect("renders");
    assert_same_bytes("unpatched", &recorded("html-weather-station"), &rendered);
}
