//! Level 3 of W3 **D-82**'s three, for AirGradient: the whole driver against a Go emulator
//! of the device's local API.
//!
//! `emulator_davis.rs`'s header has why these are `#[ignore]`d and what that is not.
//!
//! **`origin = "this-repository"`.** The reference tree has no AirGradient emulator at the
//! pinned commit and is read-only to this programme, so this one is written here —
//! `tests/emulators/manifest.toml` records that, and records what it costs: independent
//! authorship. What it keeps is a separate process, a different language, a real socket, a
//! real bind, and a device that answers **more than once** with values that move, which is
//! the property the two levels below cannot have.
//!
//! The two cases worth a test of their own are the firmware ones, because they are where
//! the Go driver stores a plausible wrong number: a device that reports `atmp` and not
//! `atmpCompensated` makes `celsiusToFahrenheit(0.0)` = **32 °F** there, every three
//! seconds, for a monitor that is working.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{Cx, Generation, Ingress, Tick};
use mw_station::airgradient::{station_id, Poll};
use mw_station::testing::{Admission, GoEmulator, RecordingIngress};
use mw_station::Endpoint;
use mw_types::StationKey;
use tokio_util::sync::CancellationToken;

const EMULATOR: &str = "airgradient";

/// Start the emulator and read the one line it prints once its socket is up.
async fn start(extra: &[&str]) -> (GoEmulator, Endpoint) {
    let args: Vec<String> = std::iter::once("-port".to_string())
        .chain(std::iter::once("0".to_string()))
        .chain(extra.iter().map(|a| (*a).to_string()))
        .collect();
    let mut emulator = GoEmulator::spawn(EMULATOR, &args).await;
    let ready = emulator
        .next_line()
        .await
        .expect("the emulator printed its ready line");
    let port: u16 = ready
        .split_whitespace()
        .find_map(|field| field.strip_prefix("http=")?.parse().ok())
        .unwrap_or_else(|| panic!("no http port in `{ready}`"));
    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}")).expect("a literal address");
    (emulator, endpoint)
}

fn cx() -> (Cx, CancellationToken) {
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    (
        Cx::new(
            station_id("suncrest:aqmon").expect("a legal component id"),
            token.clone(),
            Generation::ZERO,
            rx,
        ),
        token,
    )
}

/// Drive `occurrences` occurrences of the real `Tick` body against the emulator.
async fn poll(endpoint: Endpoint, occurrences: usize) -> Vec<Admission> {
    let ingress = RecordingIngress::accepting();
    let mut body = Poll::new(
        endpoint,
        StationKey::new("suncrest:aqmon"),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    let (context, _token) = cx();
    for n in 0..occurrences {
        tokio::time::timeout(Duration::from_secs(30), body.tick(&context))
            .await
            .unwrap_or_else(|_| panic!("occurrence {n} finished"))
            .unwrap_or_else(|e| panic!("occurrence {n} failed: {}", mw_types::DisplayChain(&e)));
    }
    ingress.admissions()
}

/// **Every occurrence produces a reading**, which is the D-82 assertion for a polling
/// driver, and the values move — so a driver that answered the first body and then replayed
/// it is visible.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn every_occurrence_produces_a_reading_and_the_values_move() {
    let (_emulator, endpoint) = start(&[]).await;
    let admissions = poll(endpoint, 5).await;
    assert_eq!(admissions.len(), 5, "five occurrences, five readings");

    let co2: Vec<Option<f32>> = admissions.iter().map(|a| a.batch[0].values.co2).collect();
    assert!(
        co2.windows(2).any(|pair| pair[0] != pair[1]),
        "every reading carried the same CO2: {co2:?}"
    );
    for admission in &admissions {
        assert_eq!(admission.receiver, "station:airgradient:suncrest:aqmon");
        assert_eq!(admission.presented_key, None, "a Fixed identity");
        let r = &admission.batch[0].values;
        // °C on the wire, °F in the column, and the compensated field is the one read.
        assert!(
            matches!(r.out_temp, Some(t) if (60.0..=75.0).contains(&t)),
            "about 20 C in degF: {:?}",
            r.out_temp
        );
        assert!(
            matches!(r.out_humidity, Some(h) if (40.0..=55.0).contains(&h)),
            "humidity: {:?}",
            r.out_humidity
        );
        assert!(
            matches!(r.co2, Some(c) if (500.0..=700.0).contains(&c)),
            "CO2: {:?}",
            r.co2
        );
        assert!(r.pm25.is_some(), "the compensated particulate");
        // The nine `extrafloat` and three `extratext` slots the Go driver files these in.
        assert!(r.extra_float1.is_some(), "pm01");
        assert!(r.extra_float5.is_some(), "the WiFi signal");
        assert!(r.extra_float9.is_some(), "the 2.5 um particle count");
        assert_eq!(r.extra_text1.as_deref(), Some("ecda3bbd1f2c"));
        assert_eq!(r.extra_text2.as_deref(), Some("I-9PSL"));
        assert_eq!(r.extra_text3.as_deref(), Some("3.1.9"));
        // The AQI columns are the enrich stage's (DEV-9), not this codec's.
        assert_eq!(r.aqi_pm25_in, None);
    }
}

/// **The 32 °F defect**, against a device that actually behaves that way: an older firmware
/// reports `atmp` and `rhum` and no `*Compensated` fields at all.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn an_older_firmware_reports_its_raw_temperature_rather_than_thirty_two_degrees() {
    let (_emulator, endpoint) = start(&["-firmware", "old"]).await;
    let admissions = poll(endpoint, 2).await;
    assert_eq!(admissions.len(), 2);
    for admission in &admissions {
        let r = &admission.batch[0].values;
        let temp = r
            .out_temp
            .expect("the raw field is read when the other is absent");
        assert!(
            (68.0..=72.0).contains(&temp),
            "about 21 C in degF, and certainly not 32: {temp}"
        );
        assert_ne!(temp, 32.0, "this is the number the Go driver stores");
        assert!(r.out_humidity.is_some(), "`rhum`");
        assert!(r.pm25.is_some(), "`pm02`");
    }
}

/// A monitor with no temperature or humidity sensor reporting at all: absent, not zero.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_monitor_with_no_temperature_sensor_stores_no_temperature() {
    let (_emulator, endpoint) = start(&["-sensor", "none"]).await;
    let admissions = poll(endpoint, 2).await;
    for admission in &admissions {
        let r = &admission.batch[0].values;
        assert_eq!(r.out_temp, None, "and NOT 32 degF");
        assert_eq!(r.out_humidity, None);
        // The particulate and CO2 sensors are separate devices inside the monitor, so what
        // the SHT module's absence costs is two metrics and not the reading.
        assert!(r.pm25.is_some(), "the particulate sensor still reports");
        assert!(r.co2.is_some(), "and so does the CO2 one");
    }
}

/// A device answering something other than `200` is a **failed occurrence**, where the Go
/// driver logs and returns — three times a second, with nothing an operator can alert on.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_device_that_answers_an_error_fails_the_occurrence() {
    let (_emulator, endpoint) = start(&["-status", "503"]).await;
    let ingress = RecordingIngress::accepting();
    let mut body = Poll::new(
        endpoint,
        StationKey::new("suncrest:aqmon"),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    let (context, _token) = cx();
    let e = body
        .tick(&context)
        .await
        .expect_err("a 503 is not a measurement");
    let rendered = mw_types::DisplayChain(&e).to_string();
    assert!(
        rendered.contains("polling an AirGradient monitor"),
        "{rendered}"
    );
    assert!(rendered.contains("503"), "{rendered}");
    assert!(ingress.admissions().is_empty());
}
