//! Level 3 of W3 **D-82**'s three, for the snow gauge: the whole driver against a Go
//! emulator of the device's HTTP API.
//!
//! `emulator_davis.rs`'s header has why these are `#[ignore]`d and what that is not.
//!
//! **This file used to be `crates/mw-grpc/tests/emulator_snowgauge.rs`**, and the move is the
//! whole change: the gauge spoke gRPC, so its driver lived in a crate that may name `tonic`
//! and the emulator needed `grpc-go`, a committed `go.sum` and the reference tree's
//! generated protobuf stubs vendored beside it. The firmware serves JSON over plain HTTP
//! now, so the driver is `mw_station::snowgauge`, the emulator is standard-library-only, and
//! what it exercises is the same four lines of `crate::http` the AirGradient poll does.
//!
//! What this level has that the two below it cannot: a real socket, a separate process, a
//! device whose counters move on their own, and — the one that matters here — **a gauge that
//! is still answering after its sensor has stopped**. That state is a response the driver
//! has to refuse, and it only exists over several requests to one device.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{Cx, Generation, Ingress, Tick};
use mw_station::snowgauge::{station_id, Poll, SILENT_OCCURRENCES};
use mw_station::testing::{Admission, GoEmulator, RecordingIngress};
use mw_station::Endpoint;
use mw_types::StationKey;
use tokio_util::sync::CancellationToken;

const EMULATOR: &str = "snowgauge";

/// The calibration the emulator reports, which is also what its snowpack model measures
/// against. The device owns this number now — the station is configured with nothing.
const BASE: u32 = 1500;

/// Start the emulator and read the one line it prints once its socket is up.
async fn start(extra: &[&str]) -> (GoEmulator, Endpoint) {
    let args: Vec<String> = ["-port", "0"]
        .into_iter()
        .chain(extra.iter().copied())
        .map(str::to_string)
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
            station_id("suncrest:gauge").expect("a legal component id"),
            token.clone(),
            Generation::ZERO,
            rx,
        ),
        token,
    )
}

fn driver(endpoint: Endpoint, ingress: Arc<dyn Ingress>) -> Poll {
    Poll::new(endpoint, StationKey::new("suncrest:gauge"), ingress)
}

/// Drive `occurrences` occurrences of the real `Tick` body against the emulator, requiring
/// every one of them to succeed.
async fn poll(endpoint: Endpoint, occurrences: usize) -> Vec<Admission> {
    let ingress = RecordingIngress::accepting();
    let mut body = driver(endpoint, Arc::clone(&ingress) as Arc<dyn Ingress>);
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
/// it is visible. `-trim-after 2` puts the gauge past its trailing window immediately, so
/// the number being asserted is the trimmed mean the firmware says to prefer.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn every_occurrence_produces_a_reading_and_the_values_move() {
    let (_emulator, endpoint) = start(&["-trim-after", "2"]).await;
    let admissions = poll(endpoint, 5).await;
    assert_eq!(admissions.len(), 5, "five occurrences, five readings");

    let distances: Vec<Option<f32>> = admissions
        .iter()
        .map(|a| a.batch[0].values.snow_distance)
        .collect();
    assert!(
        distances.windows(2).any(|pair| pair[0] != pair[1]),
        "every reading carried the same distance: {distances:?}"
    );
    for admission in &admissions {
        assert_eq!(admission.receiver, "station:snowgauge:suncrest:gauge");
        assert_eq!(admission.presented_key, None, "a Fixed identity");
        let r = &admission.batch[0].values;
        let mm = r.snow_distance.expect("a distance");
        assert!(
            (1_300.0..=BASE as f32).contains(&mm),
            "a settling snowpack under a {BASE} mm calibration: {mm}"
        );
        // The depth is the **device's** now, where it used to be the enrich stage's
        // subtraction of a configured base distance (DEV-9).
        assert!(r.snow_depth.is_some(), "a calibrated gauge reports one");
        // And a gauge still measures one thing: these two and nothing else.
        assert_eq!(r.out_temp, None);
    }
}

/// **The frozen-depth defect, against a device that actually behaves that way.** The
/// emulator's sensor produces five frames and then stops; the gauge keeps serving `/data`
/// with the same `trim_mean` and a growing `uptime_ms`, which is the firmware
/// documentation's own description of a dead UART link.
///
/// Nothing further is stored, and the station eventually **fails** rather than reporting a
/// flat line for ever.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_gauge_whose_sensor_stops_stores_nothing_further_and_then_fails_the_station() {
    let (_emulator, endpoint) = start(&[
        "-trim-after",
        "2",
        "-sensor",
        "stalled",
        "-stall-after",
        "3",
    ])
    .await;
    let ingress = RecordingIngress::accepting();
    let mut body = driver(endpoint, Arc::clone(&ingress) as Arc<dyn Ingress>);
    let (context, _token) = cx();

    // The three frames the sensor produces, then the window of counted staleness.
    let mut failure = None;
    for n in 0..(3 + SILENT_OCCURRENCES as usize) {
        match body.tick(&context).await {
            Ok(()) => {}
            Err(e) => {
                failure = Some((n, mw_types::DisplayChain(&e).to_string()));
                break;
            }
        }
    }
    let (at, rendered) = failure.expect("a gauge that stopped reporting fails its station");
    assert!(
        at >= 3,
        "the three frames the sensor did produce were readings, so the failure cannot be \
         before occurrence 3: it was at {at}"
    );
    assert!(
        rendered.contains("no measurement for"),
        "the operator is told what is wrong: {rendered}"
    );
    assert!(
        rendered.contains("`readings` counter did not advance"),
        "...and which of the three states it is: {rendered}"
    );
    // `/health` is read on this path, so the sentence carries the sensor cross-check the
    // firmware documentation sends an operator to.
    assert!(
        rendered.contains("bytes from the sensor"),
        "...with /health's own number in it: {rendered}"
    );

    let admissions = ingress.admissions();
    assert_eq!(
        admissions.len(),
        3,
        "three frames off the sensor, three readings, and nothing after it stopped"
    );
}

/// A gauge whose sensor has **never** spoken: `status` is `no_data` and `distance` is zero
/// on every response. Zero is not a distance, so nothing is stored — where storing it
/// under a 1500 mm base would be a 1500 mm snowpack.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_gauge_with_no_sensor_stores_no_zero_distance() {
    let (_emulator, endpoint) = start(&["-sensor", "dead"]).await;
    let ingress = RecordingIngress::accepting();
    let mut body = driver(endpoint, Arc::clone(&ingress) as Arc<dyn Ingress>);
    let (context, _token) = cx();
    for n in 0..(SILENT_OCCURRENCES as usize - 1) {
        body.tick(&context)
            .await
            .unwrap_or_else(|e| panic!("occurrence {n} is counted, not failed: {e}"));
    }
    assert!(
        ingress.admissions().is_empty(),
        "a `no_data` body is not a reading of zero millimetres"
    );
    let e = body
        .tick(&context)
        .await
        .expect_err("a minute of `no_data` fails the station");
    let rendered = mw_types::DisplayChain(&e).to_string();
    assert!(rendered.contains("no_data"), "{rendered}");
    // The emulator's `/health` reports zero bytes off the sensor for this mode, which is
    // the half of the diagnosis the reading cannot carry.
    assert!(
        rendered.contains("no bytes at all from the sensor"),
        "{rendered}"
    );
}

/// **The depth is the device's arithmetic, asserted exactly.**
///
/// The firmware computes `base as i32 - trim_mean.round() as i32`, in that order, and the
/// emulator does the same — so this is equality, not a tolerance. A tolerance here would
/// hide the millimetre a rounded subtraction differs by, which is the one thing an oracle
/// written beside the driver is for.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_calibrated_gauge_reports_a_depth_the_device_computed() {
    let (_emulator, endpoint) = start(&["-trim-after", "2", "-base", "1500"]).await;
    let admissions = poll(endpoint, 5).await;
    assert_eq!(admissions.len(), 5);
    let mut depths = Vec::new();
    for admission in &admissions {
        let r = &admission.batch[0].values;
        let distance = r.snow_distance.expect("a distance");
        let depth = r.snow_depth.expect("a calibrated gauge reports one");
        assert_eq!(
            depth,
            (BASE as i32 - distance.round() as i32) as f32,
            "distance {distance} against a {BASE} mm calibration"
        );
        depths.push(depth);
    }
    // And it moves, so a driver that admitted the first depth and replayed it is visible.
    assert!(
        depths.windows(2).any(|pair| pair[0] != pair[1]),
        "every reading carried the same depth: {depths:?}"
    );
}

/// An uncalibrated gauge, against a device that really is one: it measures normally and
/// reports `base_distance` and `snow_depth` as null. Every occurrence still produces a
/// reading, because the distance is the series `/snow`'s history is made of and the
/// missing depth is fixed at the device's own web interface.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn an_uncalibrated_gauge_reports_a_distance_and_no_depth() {
    let (_emulator, endpoint) = start(&["-trim-after", "2", "-calibrated=false"]).await;
    let admissions = poll(endpoint, 5).await;
    assert_eq!(admissions.len(), 5, "an uncalibrated gauge still reports");
    for admission in &admissions {
        let r = &admission.batch[0].values;
        assert!(r.snow_distance.is_some(), "it measures normally");
        assert_eq!(r.snow_depth, None, "and has nothing to subtract from");
    }
}

/// A gauge that **rebooted** between two occurrences: `uptime_ms` drops and `readings`
/// restarts at zero. A driver that tested the sensor before the uptime would call that a
/// dead sensor and store nothing from then on.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_gauge_that_reboots_keeps_reporting() {
    let (_emulator, endpoint) = start(&["-trim-after", "1", "-reboot", "3"]).await;
    let admissions = poll(endpoint, 9).await;
    assert_eq!(
        admissions.len(),
        9,
        "three reboots over nine occurrences, and every one of them a reading"
    );
}

/// **Firmware that predates `/data`.** A bare `404` would read as a wrong address; this one
/// has a documented meaning, so the driver reads `/health` and says which version is running
/// and what to do about it.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn old_firmware_is_named_as_an_upgrade_rather_than_a_not_found() {
    let (_emulator, endpoint) = start(&["-firmware", "old"]).await;
    let ingress = RecordingIngress::accepting();
    let mut body = driver(endpoint, Arc::clone(&ingress) as Arc<dyn Ingress>);
    let (context, _token) = cx();
    let e = body
        .tick(&context)
        .await
        .expect_err("a 404 on `/data` is not a measurement");
    let rendered = mw_types::DisplayChain(&e).to_string();
    assert!(rendered.contains("404 on `/data`"), "{rendered}");
    assert!(
        rendered.contains("firmware 0.0.9"),
        "the version comes off the gauge's own `/health`: {rendered}"
    );
    assert!(rendered.contains("Upgrade the gauge"), "{rendered}");
    assert!(ingress.admissions().is_empty());
}
