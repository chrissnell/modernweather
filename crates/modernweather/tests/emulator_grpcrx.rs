//! Level 3 of W3 **D-82**'s three, for the gRPC receiver — and the test that checks the
//! registry's own founding claim.
//!
//! `metrics/registry.toml` says its field numbers were *"bootstrapped from the Go
//! implementation's own definition (215 columns in `internal/types/weather.go` and
//! `internal/storage/timescaledb/sql.go` at remoteweather `c3ec9f7`)"*, and **D-21** is the
//! decision that a number is allocated once and never reused — because *"a reused number
//! silently reinterprets historical data on every client that has not been rebuilt"*.
//! Nothing in the programme had checked that claim on the wire. This does:
//!
//! ```text
//!   the Go satellite ──WeatherV1.SendWeatherReadings──▶ mw_http::bind   (a real socket)
//!   (the REFERENCE tree's own generated client,            │
//!    at the pinned commit)                                 │
//!                       mw_grpc::grpcrx::route_table ───────┘
//!                                                           │
//!                        mw_proto::wire::Reading  ◀──────────┘  (the registry's schema)
//!                                                           │
//!                       mw_pipeline::SpoolIngress ◀──────────┘  (the real admit path)
//!                                                           │
//!                       mw_store::ConfigRegistry ◀───────────┘  (THE one gate)
//!                                      the spool ◀──────────┘
//! ```
//!
//! A Go encoder writes `WeatherReading`; a Rust decoder reads `modernweather.v1.Reading`;
//! the values have to come out where they went in. A drifted field number is a reading whose
//! barometer is in the humidity column.
//!
//! It lives in the binary's test crate for `emulator_ambient.rs`'s reason: it needs a
//! socket, which is `mw-http`'s, and a service, which is `mw-grpc`'s. The binary is the one
//! place they meet.
//!
//! **Ignored by default, and that is not a skip** — see
//! `crates/mw-station/tests/emulator_davis.rs`'s header.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{ComponentId, Ingress};
use mw_grpc::grpcrx::{Handlers, ServiceState};
use mw_pipeline::testing::{spool, Scratch};
use mw_pipeline::{SpoolIngress, TestClock};
use mw_station::testing::GoEmulator;
use mw_store::config_db::StationRow;
use mw_types::{OriginId, Seq};
use tokio_util::sync::CancellationToken;

const EMULATOR: &str = "grpcrx";

/// The key the satellite was registered under, which it presents as `station_id`.
const REGISTERED: &str = "satellite-a:backyard";

/// A received station's row: `from_registration`, no transport (the schema's own `CHECK`
/// ties the two together), and the key the satellite presents in `presented_key`.
fn row(key: &str, claim: &str) -> StationRow {
    StationRow {
        id: 0,
        station_key: key.to_string(),
        owning_node_id: vec![1; 16],
        name: key.to_string(),
        kind: "davis".to_string(),
        enabled: true,
        identity_kind: "from_registration".to_string(),
        latitude: None,
        longitude: None,
        altitude_m: None,
        timezone: None,
        snow_base_distance_mm: None,
        wind_dir_correction_degrees: 0,
        presented_key: Some(claim.to_string()),
        ingest_secret_hash: None,
        transport_kind: None,
        transport_dev: None,
        transport_baud: None,
        transport_host: None,
        transport_port: None,
        transport_url: None,
        transport_method: None,
        transport_auth: None,
        transport_streaming: None,
        transport_mount: None,
        transport_group: None,
        transport_topic: None,
        transport_tls_ca_path: None,
        transport_tls_cert_path: None,
        transport_tls_key_path: None,
        wll_sensor_mapping: None,
        wll_poll_interval_seconds: None,
    }
}

/// The production path, assembled: a spool, the gate, the ingress, the service's handlers,
/// the route and a bound socket.
struct Collector {
    authority: String,
    spool: Arc<mw_pipeline::Spool>,
    cursor: mw_pipeline::Cursor,
    shutdown: CancellationToken,
    serving: tokio::task::JoinHandle<()>,
    _scratch: Scratch,
    _state: Arc<ServiceState>,
}

impl Collector {
    async fn start(tag: &str, rows: Vec<StationRow>) -> Self {
        let registry = Arc::new(mw_store::ConfigRegistry::new(
            mw_store::RegistrySnapshot::new(&rows, []),
        ));
        let scratch = Scratch::new(tag);
        let clock = TestClock::fixed();
        let spool = spool(scratch.spool(), OriginId::new([9u8; 16]), &clock);
        // Before anything arrives, because a cursor starts at the tail.
        let cursor = spool
            .open_cursor(ComponentId::new("writer:timescale").expect("a legal id"))
            .expect("a free cursor");
        let ingress = Arc::new(SpoolIngress::new(
            Arc::clone(&spool),
            Arc::clone(&registry) as Arc<dyn mw_component::StationRegistry>,
        )) as Arc<dyn Ingress>;

        let receiver = ComponentId::new(mw_grpc::grpcrx::ID).expect("a legal component id");
        let state = Arc::new(ServiceState::new());
        // The seam `station:grpcrx`'s own `Serve` body uses in production.
        state.install_for_test(Arc::new(Handlers::new(receiver, ingress)));

        let table = mw_grpc::grpcrx::route_table(Arc::clone(&state)).expect("a legal id");
        let (mount, _) = mw_http::Mount::new(vec![table], mw_http::HostTable::default())
            .expect("the table mounts");
        // Port 0 on both sockets: `ci/layers.toml` reserves binding for `mw-http`, so this
        // goes through the real `bind`.
        let listen = mw_http::Listen {
            addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            http_port: 0,
            https_port: None,
            grpc_port: 0,
        };
        let bound = mw_http::bind(&listen, mount.router(), None)
            .await
            .expect("the ephemeral ports bind");
        // **The gRPC port**, which is the one a satellite is configured with: §6.1 gives it
        // its own socket serving the same router (W3 D-92).
        let authority = bound.grpc_addr().to_string();
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        #[allow(clippy::disallowed_methods)]
        let serving = tokio::task::spawn(async move {
            let _ = bound.serve(async move { token.cancelled().await }).await;
        });

        Self {
            authority,
            spool,
            cursor,
            shutdown,
            serving,
            _scratch: scratch,
            _state: state,
        }
    }

    /// Run the Go satellite against this collector and hand back every line it printed.
    async fn satellite(&self, args: &[&str]) -> Vec<String> {
        let mut argv = vec!["-url".to_string(), self.authority.clone()];
        argv.extend(args.iter().map(|a| (*a).to_string()));
        let mut emulator = GoEmulator::spawn(EMULATOR, &argv).await;
        let mut lines = Vec::new();
        while let Some(line) = emulator.next_line().await {
            lines.push(line);
        }
        emulator.finished().await;
        lines
    }

    fn appended(&mut self) -> Vec<(Seq, mw_types::Envelope)> {
        self.cursor.read().expect("the spool is readable").envelopes
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(20), self.serving).await;
    }
}

/// **The registry's founding claim, on the wire.** The Go client encodes
/// `remoteweather.v1.WeatherReading`; the Rust receiver decodes
/// `modernweather.v1.Reading`; every value comes out where it went in.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_go_satellites_readings_decode_into_the_columns_they_were_sent_in() {
    let mut collector = Collector::start(
        "grpcrx-fields",
        vec![row("satellite-a:backyard", REGISTERED)],
    )
    .await;
    let lines = collector
        .satellite(&["-id", REGISTERED, "-count", "3"])
        .await;
    assert!(
        lines.iter().any(|l| l == "accepted"),
        "the collector refused the stream: {lines:?}"
    );

    let appended = collector.appended();
    assert_eq!(appended.len(), 3, "three readings, three durable envelopes");
    for (n, (_, envelope)) in appended.iter().enumerate() {
        let phase = n as f32;
        let r = &envelope.reading;
        assert_eq!(
            r.station_id, "satellite-a:backyard",
            "the station is the registry's answer to `station_id`"
        );
        // One assertion per field the satellite set, which is the whole point: a drifted
        // number would put the barometer in another column and this is what notices.
        assert_eq!(r.barometer, Some(29.92 + 0.01 * phase), "barometer (4)");
        assert_eq!(
            r.out_temp,
            Some(68.4 + 0.1 * phase),
            "outsideTemperature (7)"
        );
        assert_eq!(r.wind_speed, Some(7.5 + 0.2 * phase), "windSpeed (9)");
        assert_eq!(r.wind_dir, Some(183.0 + phase), "windDirection (11)");
        assert_eq!(r.rain_rate, Some(0.02), "rainRate (36)");
        assert_eq!(r.solar_watts, Some(512.5 + phase), "solarWatts (43)");
        assert_eq!(r.snow_distance, Some(1200.0), "snowDistance (82)");
        assert!(
            matches!(r.out_humidity, Some(h) if (40.0..=42.0).contains(&h)),
            "outsideHumidity (8): {:?}",
            r.out_humidity
        );
        // **The presence bit**: every metric field is `optional` (D-89), so a field the
        // satellite did not set is `None` rather than a stored zero.
        assert_eq!(r.in_temp, None, "a field the satellite did not set");
        assert_eq!(r.day_rain, None);
    }

    // **The satellite's own clock**, which the wire message carries and the emulator steps
    // by a minute per reading -- so a receiver storing the arrival time would show three
    // instants a few milliseconds apart.
    let instants: Vec<String> = appended
        .iter()
        .map(|(_, e)| e.reading.time.to_rfc3339())
        .collect();
    assert_eq!(
        instants,
        vec![
            "2026-11-10T22:49:09+00:00",
            "2026-11-10T22:50:09+00:00",
            "2026-11-10T22:51:09+00:00",
        ]
    );
    collector.stop().await;
}

/// **The defect §4a.6 names.** A satellite presenting a key this node has no row for is
/// refused and **nothing reaches the spool** — where `station.go:174` pushes the reading
/// into the distributor and `:194` consults the registry afterwards, logging a `Debugf`.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn an_unregistered_satellite_is_refused_and_stores_nothing() {
    let mut collector = Collector::start(
        "grpcrx-unknown",
        vec![row("satellite-a:backyard", REGISTERED)],
    )
    .await;
    let lines = collector
        .satellite(&["-id", "somebody-elses-satellite", "-count", "2"])
        .await;
    assert!(
        lines.iter().any(|l| l == "refused status=PermissionDenied"),
        "the satellite should have been told: {lines:?}"
    );
    assert!(collector.appended().is_empty());
    assert_eq!(collector.spool.stats().appends, 0);
    collector.stop().await;
}

/// A Go-era satellite that sets only `stationName`, which is the compatibility this
/// method exists for.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_satellite_that_sends_only_a_name_is_resolved_by_it() {
    let mut collector =
        Collector::start("grpcrx-name", vec![row("satellite-a:backyard", "backyard")]).await;
    let lines = collector
        .satellite(&["-id", "", "-name", "backyard", "-count", "2"])
        .await;
    assert!(
        lines.iter().any(|l| l == "accepted"),
        "the collector refused the stream: {lines:?}"
    );
    let appended = collector.appended();
    assert_eq!(appended.len(), 2);
    assert_eq!(appended[0].1.reading.station_id, "satellite-a:backyard");
    collector.stop().await;
}

/// A station that is configured and switched off: `enabled` is about collection, and a
/// satellite streaming at a disabled station is told rather than silently stored.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_disabled_station_is_refused() {
    let mut off = row("satellite-a:backyard", REGISTERED);
    off.enabled = false;
    let mut collector = Collector::start("grpcrx-disabled", vec![off]).await;
    let lines = collector
        .satellite(&["-id", REGISTERED, "-count", "1"])
        .await;
    assert!(
        lines.iter().any(|l| l == "refused status=PermissionDenied"),
        "{lines:?}"
    );
    assert!(collector.appended().is_empty());
    collector.stop().await;
}

/// **`RegisterRemoteStation` is not implemented and will not be**, and the message says
/// which path to use instead — not a `404` that looks like the wrong address.
///
/// The message used to name implementation-plan.md M6-4 as the item that would land it.
/// M6-4 landed and did not: that method's own schema carries the four plaintext secrets
/// docs/collector.md §5.2 and **D-67** delete, so decoding it would put them back into this
/// tree. What replaced it is `collector.v1.Collector/Hello`, which registers a peer's
/// stations carrying identity only — so that is what the message has to name, because a
/// refusal an operator cannot act on is the same as a 404 to them.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn enrolment_answers_unimplemented_and_names_the_path_that_works() {
    let collector = Collector::start(
        "grpcrx-enrol",
        vec![row("satellite-a:backyard", REGISTERED)],
    )
    .await;
    // Through the route rather than through the service type: what a satellite sees is the
    // status and the message, which is what this asserts.
    let url = format!(
        "http://{}{}",
        collector.authority,
        mw_grpc::grpcrx::REGISTER_REMOTE_STATION
    );
    let response = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-D",
            "-",
            "--http2-prior-knowledge",
        ])
        .args(["-H", "content-type: application/grpc", "-X", "POST"])
        .arg(&url)
        .output()
        .await
        .expect("curl is on PATH");
    let headers = String::from_utf8_lossy(&response.stdout);
    assert!(
        headers.contains("grpc-status: 12"),
        "`Unimplemented` is code 12: {headers}"
    );
    assert!(
        headers.contains("Hello"),
        "the message names the RPC that replaced it: {headers}"
    );
    collector.stop().await;
}
