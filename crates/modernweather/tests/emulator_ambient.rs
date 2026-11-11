//! Level 3 of W3 **D-82**'s three, for Ambient — and the only one of the six that is a
//! whole **path** rather than a driver.
//!
//! The Ambient station is the **client**, so there is no device to connect to: the test is
//! a request against the route the driver mounts. `tests/emulators/manifest.toml`'s pending
//! entry said so before this existed. What that means is that this test cannot live in
//! `mw-station` — it needs a socket, which is `mw-http`'s, and the two crates are in the
//! same layer — so it lives here, in the one crate that may name both. That is the same
//! `§1.3 rule 1` seam `crates/modernweather/src/boot.rs` assembles in production, and this
//! test assembles it the same way:
//!
//! ```text
//!   the Go console ──GET /ingest/ambient?ID=…&PASSWORD=…──▶ mw_http::bind  (a real socket)
//!                                                             │
//!                            mw_http::upload_table ───────────┘
//!                                                             │
//!                   mw_station::ambient::Handler ◀────────────┘   (the driver)
//!                                                             │
//!                      mw_pipeline::SpoolIngress ◀────────────┘   (the real admit path)
//!                                                             │
//!                      mw_store::ConfigRegistry ◀─────────────┘   (THE one gate: ?ID=,
//!                                                             │    and Argon2 over
//!                                                             │    ?PASSWORD=)
//!                                   the spool ◀───────────────┘   (durably appended)
//! ```
//!
//! Every box is the production implementation. The only things stood in for are the
//! configuration rows and the clock.
//!
//! **Ignored by default, and that is not a skip** — see
//! `crates/mw-station/tests/emulator_davis.rs`'s header.

use std::sync::Arc;
use std::time::Duration;

use mw_codec::WindDirCorrection;
use mw_component::{ComponentId, Ingress};
use mw_pipeline::testing::{spool, Scratch};
use mw_pipeline::{SpoolIngress, TestClock};
use mw_station::ambient::{AmbientUnit, Handler, UploadState, ACCEPTED_BODY, DEFAULT_MOUNT, ID};
use mw_station::testing::GoEmulator;
use mw_store::config_db::StationRow;
use mw_types::{OriginId, Seq, StationKey};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

const EMULATOR: &str = "ambient";

/// The secret both consoles present, and the one the registry stores a hash of.
const SECRET: &str = "hunter2";

/// A configured Ambient station row, as `config.stations` holds it.
fn row(key: &str, claim: &str, hash: &str) -> StationRow {
    StationRow {
        id: 0,
        station_key: key.to_string(),
        name: key.to_string(),
        kind: "ambient-customized".to_string(),
        enabled: true,
        identity_kind: "from_query".to_string(),
        latitude: None,
        longitude: None,
        altitude_m: None,
        timezone: None,
        snow_base_distance_mm: None,
        wind_dir_correction_degrees: 0,
        presented_key: Some(claim.to_string()),
        ingest_secret_hash: Some(hash.to_string()),
        transport_kind: Some("http_in".to_string()),
        transport_dev: None,
        transport_baud: None,
        transport_host: None,
        transport_port: None,
        transport_url: None,
        transport_method: None,
        transport_auth: None,
        transport_streaming: None,
        transport_mount: Some(DEFAULT_MOUNT.to_string()),
        transport_group: None,
        transport_topic: None,
        transport_tls_ca_path: None,
        transport_tls_cert_path: None,
        transport_tls_key_path: None,
        wll_sensor_mapping: None,
        wll_poll_interval_seconds: None,
    }
}

/// Everything the production path is, assembled: a spool, the gate, the ingress, the
/// driver's handler, the route, and a bound socket.
struct Endpoint {
    url: String,
    spool: Arc<mw_pipeline::Spool>,
    cursor: mw_pipeline::Cursor,
    shutdown: CancellationToken,
    serving: tokio::task::JoinHandle<()>,
    /// Held so that the scratch directory outlives the spool.
    _scratch: Scratch,
    /// Held so that the installed handler outlives the request.
    _state: Arc<UploadState>,
}

impl Endpoint {
    async fn start(tag: &str) -> Self {
        let hash = mw_store::hash_ingest_secret(&SecretString::from(SECRET.to_string()))
            .expect("hashing a literal");
        let rows = vec![
            row("suncrest:roof", "roof", &hash),
            row("suncrest:meadow", "meadow", &hash),
        ];
        let registry = Arc::new(mw_store::ConfigRegistry::new(
            mw_store::RegistrySnapshot::new(&rows, []),
        ));
        assert!(
            registry.snapshot().secret_problems().is_empty(),
            "the fixture's own secrets are usable"
        );

        let scratch = Scratch::new(tag);
        let clock = TestClock::fixed();
        let spool = spool(scratch.spool(), OriginId::new([7u8; 16]), &clock);
        // **Before any upload**, because a cursor starts at the tail: opened later it would
        // see nothing and the test would pass for the wrong reason.
        let cursor = spool
            .open_cursor(ComponentId::new("writer:timescale").expect("a legal id"))
            .expect("a free cursor");
        let ingress = Arc::new(SpoolIngress::new(
            Arc::clone(&spool),
            Arc::clone(&registry) as Arc<dyn mw_component::StationRegistry>,
        )) as Arc<dyn Ingress>;

        let receiver = ComponentId::new(ID).expect("a legal component id");
        let handler = Arc::new(Handler::new(
            receiver.clone(),
            vec![
                AmbientUnit {
                    claim: "roof".to_string(),
                    station: StationKey::new("suncrest:roof"),
                    codec: mw_codec::AmbientQuery::new(WindDirCorrection::NONE),
                },
                AmbientUnit {
                    claim: "meadow".to_string(),
                    station: StationKey::new("suncrest:meadow"),
                    // A calibrated vane on one of the two, so that "each console decodes
                    // with its own configuration" is observable from the far end.
                    codec: mw_codec::AmbientQuery::new(WindDirCorrection::new(10)),
                },
            ],
            Arc::clone(&ingress),
        ));
        let state = Arc::new(UploadState::new());
        // The seam `station:ambient`'s own `Serve` body uses in production; the component
        // would install the same handler into the same slot.
        state.install_for_test(Arc::clone(&handler));

        let table = mw_http::upload_table(receiver, DEFAULT_MOUNT, state.endpoint());
        let (mount, _) = mw_http::Mount::new(vec![table], mw_http::HostTable::default())
            .expect("the table mounts");
        // Port 0 on both sockets: the kernel chooses, and `ci/layers.toml` reserves binding
        // for `mw-http` -- so this goes through the real `bind` rather than opening one.
        let listen = mw_http::Listen {
            addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            http_port: 0,
            https_port: None,
            grpc_port: 0,
        };
        let bound = mw_http::bind(&listen, mount.router(), None)
            .await
            .expect("the ephemeral ports bind");
        let url = format!("http://{}{DEFAULT_MOUNT}", bound.local_addr());
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        // `clippy.toml` forbids `spawn` in the daemon and this test is `main`: the listener
        // serves until it is told to stop, and the console has to be able to reach it while
        // it does.
        #[allow(clippy::disallowed_methods)]
        let serving = tokio::task::spawn(async move {
            let _ = bound.serve(async move { token.cancelled().await }).await;
        });

        Self {
            url,
            spool,
            cursor,
            shutdown,
            serving,
            _scratch: scratch,
            _state: state,
        }
    }

    /// Run the Go console against this endpoint and hand back the status of every upload.
    async fn console(&self, args: &[&str]) -> Vec<u16> {
        let mut argv = vec!["-url".to_string(), self.url.clone()];
        argv.extend(args.iter().map(|a| (*a).to_string()));
        let mut emulator = GoEmulator::spawn(EMULATOR, &argv).await;
        let mut statuses = Vec::new();
        while let Some(line) = emulator.next_line().await {
            let status = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("status=")?.parse().ok())
                .unwrap_or_else(|| panic!("no status in `{line}`"));
            statuses.push(status);
        }
        emulator.finished().await;
        statuses
    }

    /// Everything the spool durably accepted, in order.
    fn appended(&mut self) -> Vec<(Seq, mw_types::Envelope)> {
        self.cursor.read().expect("the spool is readable").envelopes
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(20), self.serving).await;
    }
}

/// **The structural property, end to end.** Two consoles, one route, one component, two
/// stations — and the station each upload resolves to comes from `?ID=` through the single
/// gate, not from the port it arrived on.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn two_consoles_on_one_route_resolve_to_two_stations() {
    let mut endpoint = Endpoint::start("ambient-two").await;

    let roof = endpoint
        .console(&["-id", "roof", "-password", SECRET, "-count", "3"])
        .await;
    let meadow = endpoint
        .console(&["-id", "meadow", "-password", SECRET, "-count", "2"])
        .await;
    assert_eq!(roof, vec![200, 200, 200]);
    assert_eq!(meadow, vec![200, 200]);

    let appended = endpoint.appended();
    assert_eq!(appended.len(), 5, "five uploads, five durable envelopes");
    let stations: Vec<&str> = appended
        .iter()
        .map(|(_, envelope)| envelope.reading.station_id.as_str())
        .collect();
    assert_eq!(
        stations,
        vec![
            "suncrest:roof",
            "suncrest:roof",
            "suncrest:roof",
            "suncrest:meadow",
            "suncrest:meadow",
        ],
        "the station is `?ID=` through the registry, and nothing else"
    );

    // The console's own values, decoded: a temperature that moves, and the calibrated vane
    // on the second station only.
    let temps: Vec<Option<f32>> = appended.iter().map(|(_, e)| e.reading.out_temp).collect();
    assert_eq!(
        temps,
        vec![Some(68.4), Some(68.5), Some(68.6), Some(68.4), Some(68.5)]
    );
    assert_eq!(
        appended[0].1.reading.wind_dir,
        Some(183.0),
        "the roof's vane is not calibrated"
    );
    assert_eq!(
        appended[3].1.reading.wind_dir,
        Some(193.0),
        "the meadow's is, by ten degrees"
    );

    // **The console's clock, not ours.** `dateutc` is the observation instant and the
    // emulator steps it by a minute per upload, so a driver that stored the arrival time
    // would show five instants a few milliseconds apart.
    let instants: Vec<String> = appended
        .iter()
        .take(3)
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

    // **The rainfall counter**, which is the one piece of state the driver holds: the first
    // upload has no previous total so it derives nothing, and each later one is the
    // console's 0.05 in step.
    let rain: Vec<Option<f32>> = appended
        .iter()
        .take(3)
        .map(|(_, e)| e.reading.rain_incremental)
        .collect();
    assert_eq!(rain[0], None, "nothing to difference against");
    // A range rather than an equality: the increment is the `f32` difference of two
    // three-figure totals, so the exact value is the arithmetic's and not 0.05 -- and
    // asserting the rounded number would be asserting a different function.
    assert!(
        matches!(rain[1], Some(v) if (0.049..=0.051).contains(&v)),
        "{rain:?}"
    );
    assert!(
        matches!(rain[2], Some(v) if (0.049..=0.051).contains(&v)),
        "{rain:?}"
    );

    // One append per upload, which is what makes `admit`'s single group commit observable
    // (W3 D-87).
    assert_eq!(endpoint.spool.stats().appends, 5);
    endpoint.stop().await;
}

/// **The Go defect.** A console presenting the right identifier and the wrong secret is
/// refused, and nothing reaches the spool. In the Go driver any request arriving on that
/// station's port is trusted as that station, whatever `?PASSWORD=` says.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_console_with_the_wrong_secret_is_refused_and_stores_nothing() {
    let mut endpoint = Endpoint::start("ambient-secret").await;
    let statuses = endpoint
        .console(&[
            "-id",
            "roof",
            "-password",
            SECRET,
            "-mode",
            "bad-password",
            "-count",
            "2",
        ])
        .await;
    assert_eq!(statuses, vec![401, 401]);
    assert!(endpoint.appended().is_empty());
    assert_eq!(endpoint.spool.stats().appends, 0);
    endpoint.stop().await;
}

/// A console presenting an identifier nothing is configured for. The Ambient endpoint on a
/// public address legitimately receives these.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn a_console_this_node_has_no_row_for_is_refused() {
    let mut endpoint = Endpoint::start("ambient-unknown").await;
    let statuses = endpoint
        .console(&[
            "-id",
            "somebody-elses-console",
            "-password",
            SECRET,
            "-count",
            "1",
        ])
        .await;
    assert_eq!(statuses, vec![401]);
    assert!(endpoint.appended().is_empty());
    endpoint.stop().await;
}

/// The protocol's two required parameters a console can lose.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn an_upload_missing_a_required_parameter_is_a_bad_request() {
    let mut endpoint = Endpoint::start("ambient-malformed").await;
    for mode in ["no-dateutc", "no-action"] {
        let statuses = endpoint
            .console(&[
                "-id",
                "roof",
                "-password",
                SECRET,
                "-mode",
                mode,
                "-count",
                "1",
            ])
            .await;
        assert_eq!(statuses, vec![400], "{mode}");
    }
    assert!(endpoint.appended().is_empty());
    endpoint.stop().await;
}

/// A console whose upload is accepted gets the body its firmware looks for.
#[tokio::test]
#[ignore = "needs a Go toolchain; `cargo xtask emulator-tests` runs it"]
async fn an_accepted_upload_answers_the_body_the_protocol_expects() {
    let endpoint = Endpoint::start("ambient-body").await;
    // Through the same socket the console uses, so this is the response bytes rather than
    // the handler's return value.
    let url = format!(
        "{}?ID=roof&PASSWORD={SECRET}&action=updateraw\
         &dateutc=2026-11-10+22%3A49%3A09&tempf=68.4",
        endpoint.url
    );
    let mut emulator = GoEmulator::spawn(
        EMULATOR,
        &[
            "-url".to_string(),
            endpoint.url.clone(),
            "-id".to_string(),
            "roof".to_string(),
            "-password".to_string(),
            SECRET.to_string(),
            "-count".to_string(),
            "1".to_string(),
        ],
    )
    .await;
    let line = emulator.next_line().await.expect("one upload");
    assert!(line.contains("status=200"), "{line}");
    emulator.finished().await;
    assert_eq!(ACCEPTED_BODY, "success", "{url}");
    endpoint.stop().await;
}
