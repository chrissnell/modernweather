//! Level 3 of W3 **D-82**'s three, for the snow gauge: the whole driver against a Go gRPC
//! emulator whose **generated stubs are the reference tree's own**.
//!
//! `crates/mw-station/tests/emulator_davis.rs`'s header has why these are `#[ignore]`d and
//! what that is not. `tests/emulators/snowgauge/README.md` has what the emulator is and the
//! one property it has that no other emulator in this tree does: it is the only one that is
//! not standard-library-only, because it speaks gRPC.
//!
//! # What this level proves that the two below it cannot
//!
//! | Property | Where it comes from |
//! |---|---|
//! | the **protocol** is independently defined | `snowgauge.pb.go` and `snowgauge_grpc.pb.go` are verbatim from the pinned commit, so the service name, the method name and the field numbers are a real gauge's |
//! | a separate process, a different language | the emulator is Go, over `grpc-go` |
//! | real HTTP/2 over a real socket | `tonic` dials it |
//! | **more than one message** | a server stream that keeps sending |
//! | the DEV-9 discard, against a device that actually does it | `-fault`, which sends a distance past the configured base |
//!
//! The first row is the one worth the sentence. `mw_codec::SnowGaugeProto` declares two
//! fields of a four-field message **by hand**, because `ci/layers.toml` puts `mw-proto` in
//! `mw-codec`'s own layer — and nothing until this test checked that hand-written
//! declaration against the schema the device was generated from. It does now: a wrong tag
//! would be a distance of zero.

use std::sync::Arc;
use std::time::Duration;

use mw_codec::SnowGaugeProto;
use mw_component::{ComponentId, Cx, Generation, Ingress};
use mw_grpc::snowgauge::SnowGauge;
use mw_station::testing::{Admission, GoEmulator, RecordingIngress};
use mw_station::Endpoint;
use mw_types::StationKey;
use tokio_util::sync::CancellationToken;

const EMULATOR: &str = "snowgauge";

/// The base distance both ends are configured with, in millimetres.
const BASE: f32 = 1500.0;

const DEADLINE: Duration = Duration::from_secs(90);

fn cx() -> (Cx, CancellationToken) {
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    (
        Cx::new(
            ComponentId::new("station:snowgauge:suncrest:gauge").expect("a legal id"),
            token.clone(),
            Generation::ZERO,
            rx,
        ),
        token,
    )
}

/// Start the emulator and read the one line it prints once its socket is up.
async fn start(extra: &[&str]) -> (GoEmulator, Endpoint) {
    let args: Vec<String> = ["-port", "0", "-base", "1500"]
        .iter()
        .chain(extra.iter())
        .map(|a| (*a).to_string())
        .collect();
    let mut emulator = GoEmulator::spawn(EMULATOR, &args).await;
    let ready = emulator
        .next_line()
        .await
        .expect("the emulator printed its ready line");
    let port: u16 = ready
        .split_whitespace()
        .find_map(|field| field.strip_prefix("grpc=")?.parse().ok())
        .unwrap_or_else(|| panic!("no gRPC port in `{ready}`"));
    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}")).expect("a literal address");
    (emulator, endpoint)
}

/// Run the real driver against the emulator until `want` readings have been admitted.
async fn collect(endpoint: &Endpoint, base: Option<f32>, want: usize) -> Vec<Admission> {
    let ingress = RecordingIngress::accepting();
    let gauge = Box::new(
        SnowGauge::connect(
            endpoint,
            SnowGaugeProto::new(base),
            StationKey::new("suncrest:gauge"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the driver dials the emulator and opens its stream"),
    );
    let (context, token) = cx();
    let enough = {
        let ingress = Arc::clone(&ingress);
        let token = token.clone();
        async move {
            while ingress.admissions().len() < want {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            token.cancel();
        }
    };
    // No `spawn`: the body is driven against a poll of the ingress in the same task, so
    // "enough readings arrived" cancels the component rather than racing a handle.
    tokio::select! {
        outcome = gauge.run(context) => {
            assert!(
                outcome.is_ok() || ingress.admissions().len() >= want,
                "the stream ended before {want} readings: {outcome:?}"
            );
        }
        () = enough => {}
        () = tokio::time::sleep(DEADLINE) => {
            panic!(
                "only {} of {want} readings arrived in {DEADLINE:?}",
                ingress.admissions().len()
            );
        }
    }
    ingress.admissions()
}

/// **Every message is a reading**, and the distances are the ones the emulator sent — which
/// is what says the two field tags `mw_codec::SnowGaugeProto` declares by hand are the ones
/// the reference's own generated schema uses.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn every_message_becomes_a_reading_with_the_distance_the_gauge_sent() {
    let (_emulator, endpoint) = start(&[]).await;
    let admissions = collect(&endpoint, Some(BASE), 5).await;
    assert!(admissions.len() >= 5);

    let distances: Vec<f32> = admissions
        .iter()
        .map(|a| {
            a.batch[0]
                .values
                .snow_distance
                .expect("every message carries a distance")
        })
        .collect();
    for distance in &distances {
        // The emulator's own range: `base - (100 + 50*sin(n/3))`, so 1350..=1450 mm. A
        // wrong field tag would be zero, and a wrong wire type would not decode at all.
        assert!(
            (1340.0..=1460.0).contains(distance),
            "{distance} mm is not what the emulator sends; check the field tags"
        );
    }
    assert!(
        distances.windows(2).any(|pair| pair[0] != pair[1]),
        "every reading carried the same distance: {distances:?}"
    );
    for admission in &admissions {
        assert_eq!(admission.receiver, "station:snowgauge:suncrest:gauge");
        assert_eq!(
            admission.presented_key, None,
            "a Fixed identity presents nothing: we dialled the gauge"
        );
        // `snowdepth` is the enrich stage's subtraction and not the codec's (DEV-9).
        assert_eq!(admission.batch[0].values.snow_depth, None);
        // And nothing else: a gauge measures one thing.
        assert_eq!(admission.batch[0].values.out_temp, None);
    }
}

/// **DEV-9's constraint, against a device that actually produces the fault.** Every third
/// reading is a distance past the configured base, which Go discards at the device — and
/// which, stored, would be a negative depth in a column with `min`/`mean`/`max` over it.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_distance_past_the_base_is_discarded_and_the_stream_continues() {
    let (_emulator, endpoint) = start(&["-fault", "3"]).await;
    let admissions = collect(&endpoint, Some(BASE), 6).await;
    assert!(admissions.len() >= 6);
    // Every admitted reading is inside the base distance: the faults were dropped, and the
    // stream kept going -- which is what makes it a counted drop rather than an outage.
    for admission in &admissions {
        let distance = admission.batch[0].values.snow_distance.expect("a distance");
        assert!(
            distance <= BASE,
            "{distance} mm is past the base distance and was stored"
        );
    }
    // The assertion above is not vacuous: the SAME emulator flags with no base distance
    // configured do admit the faults, which is the test below. The two together are the
    // discard.
}

/// A gauge with no configured base distance discards nothing — the one departure from Go,
/// where an unconfigured gauge has a base of zero and reports nothing at all.
///
/// This is also what makes the test above non-vacuous: the same emulator, the same
/// `-fault 3`, and the faults now reach the ingress.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn an_unconfigured_gauge_still_reports_its_distance() {
    let (_emulator, endpoint) = start(&["-fault", "2"]).await;
    let admissions = collect(&endpoint, None, 4).await;
    assert!(admissions.len() >= 4);
    let distances: Vec<f32> = admissions
        .iter()
        .map(|a| a.batch[0].values.snow_distance.expect("a distance"))
        .collect();
    assert!(
        distances.iter().any(|d| *d > BASE),
        "a gauge with no base distance discards nothing: {distances:?}"
    );
}

/// A gauge that closes its stream has **stopped reporting**, which is a failure the
/// supervisor reconnects from — not a component that has finished.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_gauge_that_closes_its_stream_is_a_retryable_failure() {
    let (_emulator, endpoint) = start(&["-count", "2", "-hangup"]).await;
    let ingress = RecordingIngress::accepting();
    let gauge = Box::new(
        SnowGauge::connect(
            &endpoint,
            SnowGaugeProto::new(Some(BASE)),
            StationKey::new("suncrest:gauge"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the driver dials the emulator"),
    );
    let (context, _token) = cx();
    let e = tokio::time::timeout(DEADLINE, gauge.run(context))
        .await
        .expect("the driver noticed the stream had ended")
        .expect_err("a gauge that stops reporting is not a gauge that has finished");
    assert_eq!(
        mw_types::Classify::class(&e),
        mw_types::Class::RETRYABLE,
        "a reconnect, not a give-up (D-27): {}",
        mw_types::DisplayChain(&e)
    );
    assert_eq!(ingress.admissions().len(), 2, "both readings were admitted");
}

/// A gauge that is not there at all is a **build** failure naming the station, not a
/// startup abort — which is what `logger.Fatalf` is in the Go driver.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_gauge_that_is_not_listening_is_a_build_failure() {
    let (emulator, endpoint) = start(&[]).await;
    // The port the emulator had, after it is gone: a gauge that has been unplugged.
    drop(emulator);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let e = SnowGauge::connect(
        &endpoint,
        SnowGaugeProto::new(Some(BASE)),
        StationKey::new("suncrest:gauge"),
        RecordingIngress::accepting() as Arc<dyn Ingress>,
    )
    .await
    .expect_err("nothing is listening");
    let rendered = mw_types::DisplayChain(&e.build(&StationKey::new("suncrest:gauge"))).to_string();
    assert!(rendered.contains("suncrest:gauge"), "{rendered}");
    assert!(
        rendered.contains(&endpoint.authority()),
        "the message names the address that was tried: {rendered}"
    );
}
