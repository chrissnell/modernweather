//! Level 3 of W3 **D-82**'s three: the whole driver against an emulator, over real
//! sockets.
//!
//! **Ignored by default, and that is not a skip.** `cargo test` reports these as `ignored`,
//! which is visible; `cargo xtask emulator-tests` — the `driver-emulators` gate's command —
//! runs them with `--ignored` and **asserts that exactly the declared number of them
//! passed**, so a gate that ran nothing fails. That assertion is the whole reason the
//! manifest declares a count: `docs/reconciliation.md` G-1 is a gate reporting green while
//! running nothing, and a filter that matched no tests exits zero.
//!
//! They are ignored because they need a Go toolchain, which the `test` job does not have
//! and the `driver-emulators` job does (`.github/workflows/pr.yml`).
//!
//! What this level adds over the two below it: a separate process, a different language,
//! real sockets, the real bind, and **more than one batch** — the three assertions that a
//! golden test and a transcript cannot make between them. What it does not add, and the
//! manifest says so in the same words, is an independently-written oracle: the reference
//! tree has no WeatherLink Live emulator at the pinned commit, so this one lives in this
//! repository and was written alongside the driver.

use std::sync::Arc;
use std::time::Duration;

use mw_codec::{SensorMapping, WllConditions};
use mw_component::{ComponentId, Cx, Generation, Ingress, Serve, Tick};
use mw_station::testing::RecordingIngress;
use mw_station::wll::{BroadcastListener, BroadcastUnit, Poll, Rearm};
use mw_station::{Endpoint, HttpMethod};
use mw_types::StationKey;
use tokio::io::AsyncBufReadExt;
use tokio_util::sync::CancellationToken;

/// The emulator's own default serial.
const SERIAL: &str = "001D0A700002";

/// Everything a Vantage Pro2 Plus reports through this unit.
const MAPPING: &str = "th:1, rain:1, wind:1, uv:1, solar:1, windchill:1, thw:1, \
                       thsw:1:appTemp, th_indoor, baro, battery:1";

/// How long any one wait may take. Generous, because this is a process start on a shared
/// CI runner; the assertions are on facts and never on durations.
const DEADLINE: Duration = Duration::from_secs(60);

/// A running emulator, killed on drop.
struct Emulator {
    child: tokio::process::Child,
    http: u16,
    udp: u16,
}

/// Build the emulator once per test binary, and hand back the executable.
///
/// **Built rather than `go run`**, and the reason is a hang rather than a preference.
/// `go run` compiles to a cache directory and then execs the result as a **child of its
/// own**, so `kill_on_drop` kills the wrapper and the emulator keeps the file descriptors
/// it inherited -- which means a harness that reads this test's output through a pipe never
/// sees end-of-file and waits forever. `cargo xtask emulator-tests` reads exactly that
/// pipe, so the gate would hang rather than fail. Spawning the binary directly makes
/// `kill_on_drop` kill the emulator itself.
fn emulator_binary() -> &'static std::path::Path {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/emulators/wll")
                .canonicalize()
                .expect("the emulator directory is in the tree");
            let out = std::env::temp_dir().join(format!("mw-wll-emulator-{}", std::process::id()));
            let status = std::process::Command::new("go")
                .arg("build")
                .arg("-o")
                .arg(&out)
                .arg(".")
                .current_dir(&dir)
                .status()
                .unwrap_or_else(|e| {
                    panic!(
                        "the emulator needs a Go toolchain on PATH (this test is `--ignored` \
                     for that reason): {e}"
                    )
                });
            assert!(status.success(), "the emulator did not build");
            out
        })
        .as_path()
}

impl Emulator {
    /// Start the emulator and wait for the line it prints once both sockets are up.
    async fn start(extra: &[&str]) -> Self {
        let mut child = tokio::process::Command::new(emulator_binary())
            .args(["-interval", "100ms"])
            .args(extra)
            .stdout(std::process::Stdio::piped())
            // Not inherited: an inherited descriptor outlives a killed child's siblings and
            // is what made the harness's pipe never close. The emulator writes to stderr
            // only when it cannot open a socket, and that surfaces as the ready line never
            // arriving.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("the emulator starts");
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(DEADLINE, lines.next_line())
            .await
            .expect("the emulator started")
            .expect("its stdout is readable")
            .expect("it printed its ready line");
        // `ready http=<port> udp=<port>`
        let mut http = None;
        let mut udp = None;
        for field in ready.split_whitespace() {
            if let Some(value) = field.strip_prefix("http=") {
                http = value.parse().ok();
            }
            if let Some(value) = field.strip_prefix("udp=") {
                udp = value.parse().ok();
            }
        }
        Self {
            child,
            http: http.unwrap_or_else(|| panic!("no http port in `{ready}`")),
            udp: udp.unwrap_or_else(|| panic!("no udp port in `{ready}`")),
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::parse(&format!("http://127.0.0.1:{}", self.http)).expect("a literal address")
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        // `kill_on_drop` covers the usual path; this makes it immediate rather than at the
        // next runtime poll, which matters when a test fails and the next one wants the
        // port.
        let _ = self.child.start_kill();
    }
}

fn codec() -> WllConditions {
    WllConditions::new(SensorMapping::parse(MAPPING).expect("a literal mapping"))
}

fn cx(component: &ComponentId) -> (Cx, CancellationToken) {
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    (
        Cx::new(component.clone(), token.clone(), Generation::ZERO, rx),
        token,
    )
}

/// Wait until `f` is true, or fail. Never a bare sleep: a fixed wait is either a flake or
/// wasted time, and what is being asserted is the fact and not the latency.
async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(DEADLINE, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

/// **The broadcast path, end to end: arm the unit, then keep receiving.**
///
/// The "stops after one batch" assertion for an inbound driver: twelve datagrams at 100 ms
/// is more than a second of continuous collection, and the values move, so a driver that
/// admitted the first and stalled is a failing test rather than a slow one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a Go toolchain; run through `cargo xtask emulator-tests`"]
async fn the_broadcast_driver_arms_the_unit_and_keeps_receiving() {
    let emulator = Emulator::start(&[]).await;
    let ingress = RecordingIngress::accepting();
    let component = ComponentId::new(format!("station:wll:udp:{}", emulator.udp))
        .expect("a port makes a valid id");

    // Nothing arrives before the unit is armed, which is the half of broadcast mode the
    // Go driver's `startBroadcastMode` exists for and the reason the re-arm is a component.
    let listener = Box::new(BroadcastListener::new(
        emulator.udp,
        None,
        [BroadcastUnit {
            serial: SERIAL.to_string(),
            station: StationKey::new("suncrest:roof"),
            codec: codec(),
        }],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    let (context, token) = cx(&component);
    // `spawn` is forbidden in the daemon (`clippy.toml`, §3.2) and this test is the
    // supervisor: a `Serve` body returns only on cancellation.
    #[allow(clippy::disallowed_methods)]
    let serving = tokio::task::spawn(listener.serve(context));
    // Give the listener time to bind, then confirm silence: not a race, because the
    // emulator broadcasts nothing at all until it is armed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        ingress.admissions().len(),
        0,
        "an unarmed unit broadcasts nothing"
    );

    // The re-arm component's own body, against the real endpoint.
    let rearm = Rearm::new(
        emulator.endpoint(),
        emulator.udp,
        StationKey::new("suncrest:roof"),
    );
    let armed = rearm.arm().await.expect("the unit arms");
    assert_eq!(armed.port, emulator.udp, "the unit agrees about the port");
    assert_eq!(armed.duration_seconds, 3600);

    until("twelve datagrams", || ingress.readings() >= 12).await;

    let admissions = ingress.admissions();
    for admission in &admissions {
        assert_eq!(admission.batch.len(), 1, "one datagram is one observation");
        assert_eq!(
            admission.presented_key.as_deref(),
            Some(SERIAL),
            "the claim is the serial the unit broadcast"
        );
        let reading = &admission.batch[0].values;
        // A broadcast datagram carries wind and rain and nothing else, which is the
        // asymmetry between the two transports (W2a D-44).
        assert!(reading.wind_speed.is_some(), "wind is in every datagram");
        assert!(reading.day_rain.is_some(), "rainfall, converted from tips");
        assert_eq!(reading.out_temp, None, "a datagram carries no temperature");
    }
    // The values move, so the driver is reading each datagram rather than replaying one.
    // `f32` is not `Ord`, so the set is over the rendered value -- which is also what makes
    // the failure message readable.
    let winds: std::collections::BTreeSet<String> = admissions
        .iter()
        .map(|a| format!("{:?}", a.batch[0].values.wind_speed))
        .collect();
    assert!(
        winds.len() > 1,
        "every datagram decoded to the same wind speed: {winds:?}"
    );

    token.cancel();
    let outcome = tokio::time::timeout(DEADLINE, serving)
        .await
        .expect("the listener returns on cancellation")
        .expect("the task did not panic");
    assert!(
        outcome.is_ok(),
        "cancellation is not a failure: {outcome:?}"
    );
}

/// **The poll path, end to end: every occurrence produces a reading.**
///
/// This is the Davis defect's own shape — "stops polling after one batch" — on the
/// transport it was actually on. Four occurrences, four readings, with the values moving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a Go toolchain; run through `cargo xtask emulator-tests`"]
async fn the_poll_driver_produces_a_reading_on_every_occurrence() {
    let emulator = Emulator::start(&[]).await;
    let ingress = RecordingIngress::accepting();
    let component = ComponentId::new("station:wll:suncrest:field").expect("a literal id");
    let mut poll = Poll::new(
        emulator.endpoint(),
        HttpMethod::Get,
        None,
        StationKey::new("suncrest:field"),
        codec(),
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    );
    let (context, _token) = cx(&component);

    for occurrence in 1..=4 {
        // The real `Tick` body: the HTTP request, the decode and the admission.
        Tick::tick(&mut poll, &context)
            .await
            .unwrap_or_else(|e| panic!("occurrence {occurrence} failed: {e}"));
        assert_eq!(
            ingress.admissions().len(),
            occurrence,
            "occurrence {occurrence} produced no reading"
        );
        // The emulator's values move on a ten-second period, so a pause makes the next
        // reading different rather than merely later.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let admissions = ingress.admissions();
    let first = &admissions[0].batch[0].values;
    // A polled document carries everything the unit has, which is the other half of D-44.
    assert!(first.out_temp.is_some(), "the ISS record");
    assert!(first.in_temp.is_some(), "the unit's own temp/humidity");
    assert!(first.barometer.is_some(), "the unit's barometer");
    assert!(first.radiation.is_some(), "solar radiation");
    assert_eq!(first.batt1, Some(0), "transmitter 1's battery flag");
    assert!(
        admissions[0].presented_key.is_none(),
        "a Fixed identity presents nothing"
    );
    // Four readings, and not four copies of one.
    let temps: std::collections::BTreeSet<String> = admissions
        .iter()
        .map(|a| format!("{:?}", a.batch[0].values.out_temp))
        .collect();
    assert!(temps.len() > 1, "every poll decoded the same temperature");
}

/// The unit refuses an arming with `200 OK` and a populated `error`, and the driver has to
/// read the body.
///
/// A client that trusted the status line would treat the refusal as success and then listen
/// to a port nothing sends to — which is a station that silently stops reporting. The
/// emulator refuses every fifth arming on purpose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a Go toolchain; run through `cargo xtask emulator-tests`"]
async fn a_refusal_carrying_two_hundred_fails_the_re_arm() {
    let emulator = Emulator::start(&[]).await;
    let rearm = Rearm::new(
        emulator.endpoint(),
        emulator.udp,
        StationKey::new("suncrest:roof"),
    );
    for attempt in 1..=4 {
        rearm
            .arm()
            .await
            .unwrap_or_else(|e| panic!("arming {attempt} should succeed: {e}"));
    }
    let e = rearm
        .arm()
        .await
        .expect_err("the fifth arming is refused in the body");
    let chain = mw_types::DisplayChain(&e).to_string();
    assert!(chain.contains("409"), "{chain}");
    assert!(chain.contains("Request Rejected"), "{chain}");
    // ... and the component's next occurrence is the retry, so the failure is retryable
    // rather than fatal (W2a Decision 29).
    assert_eq!(
        mw_types::Classify::class(&e),
        mw_types::Class::RETRYABLE,
        "a unit that is busy now is not a unit that is broken"
    );
    // The sixth succeeds, which is what makes the refusal a transient rather than a wall.
    rearm.arm().await.expect("the sixth arming succeeds");
}

/// A unit broadcasting on a port this node is not listening on, and a node listening for a
/// unit that is not broadcasting: both are configurations an operator can get wrong, and
/// neither may look like a working station.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a Go toolchain; run through `cargo xtask emulator-tests`"]
async fn a_port_disagreement_is_reported_rather_than_followed() {
    let emulator = Emulator::start(&["-always-broadcast"]).await;
    // The listener is on a different port from the one the unit broadcasts to.
    let wrong = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port");
    let wrong_port = wrong.local_addr().expect("bound").port();
    drop(wrong);
    assert_ne!(wrong_port, emulator.udp);

    let rearm = Rearm::new(
        emulator.endpoint(),
        wrong_port,
        StationKey::new("suncrest:roof"),
    );
    // The re-arm succeeds -- the unit armed -- and reports the port it actually chose,
    // which is **not** the configured one. The driver logs and counts the disagreement and
    // does not move the listener: the listener is a different component and not this one's
    // to reconfigure (`mw_station::wll`, decision 3).
    let armed = rearm.arm().await.expect("the unit arms");
    assert_eq!(
        armed.port, emulator.udp,
        "the unit reported its own port, not the configured one"
    );
    assert_ne!(armed.port, wrong_port);

    // And a listener on the wrong port receives nothing, which is the observable cost.
    let ingress = RecordingIngress::accepting();
    let component = ComponentId::new("station:wll:udp:1").expect("a literal id");
    let listener = Box::new(BroadcastListener::new(
        wrong_port,
        None,
        [BroadcastUnit {
            serial: SERIAL.to_string(),
            station: StationKey::new("suncrest:roof"),
            codec: codec(),
        }],
        Arc::clone(&ingress) as Arc<dyn Ingress>,
    ));
    let (context, token) = cx(&component);
    #[allow(clippy::disallowed_methods)]
    let serving = tokio::task::spawn(listener.serve(context));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        ingress.admissions().len(),
        0,
        "a listener on the wrong port is a station that reports nothing"
    );
    token.cancel();
    let _ = tokio::time::timeout(DEADLINE, serving).await;
}
