//! Two nodes, one real mTLS socket, and an injected network partition.
//!
//! The plan's test strategy for M6-4 is "a two-node integration test with an injected
//! network partition", and its done-criterion is "**a satellite survives a collector outage
//! and resumes without duplicates**". This is both, end to end, over a loopback socket with
//! a real TLS handshake:
//!
//! ```text
//!   satellite (node A)                          collector (node C)
//!   spool ── admit ── uplink ──mTLS/h2──▶ ingest:grpc ── accept ── spool
//!                        ▲                     │
//!                        └──── IngestAck ──────┘
//! ```
//!
//! | Step | What is asserted |
//! |---|---|
//! | 1 | the first batch is acked, and the collector's spool holds it with **the satellite's** `(origin, seq)` |
//! | 2 | the collector is taken away mid-flight: `deliver` returns `Err`, so the supervisor would `Backoff` and the cursor does not move |
//! | 3 | the collector comes back: the same batch is delivered again and acked |
//! | 4 | a third delivery of the same batch is acked with `duplicates`, and the spool still holds each envelope **once** |
//!
//! # Why this test lives in the binary
//!
//! `ci/layers.toml`'s `only-mw-http-binds-a-socket` rule reserves every listener type and
//! every serve entry point to `mw-http`, and it scans `tests/` as well as `src/` — so a
//! test that needs a real socket has to go through `mw_http::bind`, and the crate that may
//! call it alongside `mw-collector` is this one. `crates/modernweather/tests/boot.rs` is in
//! the same position for the same reason, and says so.
//!
//! (That rule's scan is a substring match over the whole file, so it does not know what a
//! comment is — which is why this paragraph describes the reserved names instead of
//! spelling them. The note beside the rule says the same thing about itself.)
//!
//! # What "a partition" means here
//!
//! The collector's listener is dropped and re-bound on the same port. That is stronger than
//! pausing a socket: the TCP connection the uplink holds is destroyed, its `Ingest` stream
//! ends without an ack, and the uplink has to notice, fail, and later rebuild everything
//! from the cursor — which is the path a real link drop takes. The uplink is driven
//! directly rather than through the supervisor, so what is asserted is what the supervisor
//! would see: `Err` on the failed batch, `Ok` on the replayed one.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mw_collector::service::{Handlers, ServiceState};
use mw_collector::testing::{owned_station_registry, FakePeers};
use mw_collector::uplink::{Identity, Uplink, UplinkRow};
use mw_collector::{Gate, PeerAuthKind};
use mw_component::{ComponentId, Generation, OnReading as _, ReadingBatch};
use mw_http::{CertPaths, CertStore, HostTable, Listen, Mount, SniResolver};
use mw_types::{OriginId, Seq, StationKey};
use tokio_util::sync::CancellationToken;

/// The satellite's `NodeId`, which is the one `peer-a-cert.pem`'s SAN URI names.
///
/// Read from the certificate rather than written out twice: the whole point of the
/// `mtls-ca` path is that the certificate **is** the identity, so a test that declared the
/// id separately could pass with the two disagreeing.
fn satellite_node() -> OriginId {
    mw_collector::x509::node_from_san(&mw_collector::testing::cert("peer-a-cert.pem"))
        .expect("peer-a's SAN URI names a node")
}

/// The collector's own `NodeId`. Arbitrary: nothing authenticates it, because a satellite
/// verifies its collector by certificate and not by id.
fn collector_node() -> OriginId {
    OriginId::new([0xcc; 16])
}

const STATION: &str = "satellite-a:backyard";

fn data(name: &str) -> PathBuf {
    mw_collector::testing::data(name)
}

/// A scratch directory for one spool, unique per call.
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mw-two-node-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("a writable scratch directory");
    dir.join("spool")
}

fn spool(path: &std::path::Path, node: OriginId) -> Arc<mw_pipeline::Spool> {
    mw_pipeline::Spool::open(
        mw_pipeline::SpoolConfig::new(path),
        node,
        Arc::new(mw_pipeline::SystemClock) as Arc<dyn mw_pipeline::Clock>,
    )
    .expect("a spool opens in a writable directory")
}

/// Everything the collector end needs, minus the socket.
struct Collector {
    state: Arc<ServiceState>,
    spool: Arc<mw_pipeline::Spool>,
    tls: Arc<rustls::ServerConfig>,
}

fn build_collector(dir: &std::path::Path) -> Collector {
    let spool = spool(dir, collector_node());
    // The station is owned by **the satellite**, which is what W7 §4.4's step 5 checks
    // against: an envelope may only name a station some node on its inbound path owns. The
    // owner is the certificate's node id, not a shorthand -- see `owned_station_registry`.
    let registry = owned_station_registry(&[(STATION, satellite_node())]);
    let ingress: Arc<dyn mw_component::Ingress> = Arc::new(mw_pipeline::SpoolIngress::new(
        Arc::clone(&spool),
        Arc::clone(&registry),
    ));
    let peers =
        FakePeers::ca("satellite-a", satellite_node()) as Arc<dyn mw_component::PeerRegistry>;
    let state = Arc::new(ServiceState::new());
    state.install_for_test(Arc::new(Handlers::new(
        ComponentId::new(mw_collector::INGEST_ID).expect("a legal id"),
        Arc::clone(&peers),
        ingress,
        Arc::clone(&spool),
        Gate::new(registry, collector_node()),
        PeerAuthKind::MtlsCa,
    )));
    // **Layer 1**: the chain the peer CA signed, and the server certificate the satellite
    // verifies. `localhost` is the name on `collector-localhost-cert.pem`, so the uplink's
    // endpoint has to be `https://localhost:<port>` and not an address.
    let store = CertStore::load(
        &[CertPaths {
            hostname: "localhost".to_string(),
            cert: data("collector-localhost-cert.pem"),
            key: data("collector-localhost-key.pem"),
            is_default: true,
        }],
        None,
    )
    .expect("the checked-in server certificate loads");
    let tls = mw_collector::handshake::server_config(
        PeerAuthKind::MtlsCa,
        Some(&data("ca-cert.pem").to_string_lossy()),
        Arc::new(SniResolver::new(store)),
        peers,
    )
    .expect("the ingest socket's TLS is configurable");
    Collector {
        state,
        spool,
        tls: Arc::new(tls),
    }
}

/// The collector's listener, in a runtime of its own.
///
/// **A whole runtime, and that is what makes the partition real.** The serving machinery
/// spawns a task per connection, and dropping the serve future does not abort those tasks —
/// so cancelling a shutdown token leaves the uplink's existing HTTP/2 connection being
/// served by a task nobody is waiting for, and the "partitioned" batch is acknowledged as
/// though nothing had happened. That is exactly the shape of test that passes while proving
/// nothing. Dropping the runtime takes the tasks, the connection and the file descriptors
/// with it, which is what a link drop does.
struct Serving {
    runtime: Option<tokio::runtime::Runtime>,
    port: u16,
}

impl Serving {
    /// Bind and serve on `port` (0 asks the kernel to choose).
    ///
    /// Blocking on the channel, so a caller on a runtime must be inside `block_in_place`.
    fn start(collector: &Collector, port: u16) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime for the collector's listener");
        // A `grpc` row, which is what puts the three `collector.v1.Collector` methods on
        // the table (`mw_collector::service::manifest`).
        let table = mw_collector::service::route_table(
            Arc::clone(&collector.state),
            &[mw_collector::IngestRow {
                listen: mw_collector::IngestRow::GRPC.to_string(),
                peer_auth: "mtls-ca".to_string(),
                ca_path: None,
                allow_insecure: false,
            }],
        )
        .expect("the collector's route table");
        let (mount, _) = Mount::new(vec![table], HostTable::default()).expect("one mount");
        let listen = Listen {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            // The main port is not what this test is about, so it is plaintext and unused;
            // the **gRPC** port is the ingest socket and the one that completes a
            // client-certificate handshake.
            http_port: 0,
            https_port: None,
            grpc_port: port,
        };
        let tls = Arc::clone(&collector.tls);
        // Bound **inside** the new runtime and the port reported back over a channel, not
        // `Runtime::block_on`: this runs from inside the test's own runtime, and tokio
        // refuses to start one runtime from within another.
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let bound =
                mw_http::bind_with_hosts(&listen, mount.router(), None, mount.hosts(), Some(tls))
                    .await
                    .expect("both ports bind");
            assert!(
                bound.grpc_tls(),
                "the ingest socket has to complete a TLS handshake, or layer 1 is not \
                 under test"
            );
            let _ = tx.send(bound.grpc_addr().port());
            // Served until the runtime goes away: the shutdown this test injects is the
            // runtime's, not a token's.
            let _ = bound.serve(std::future::pending::<()>()).await;
        });
        let port = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the collector's listener binds and reports its port");
        Serving {
            runtime: Some(runtime),
            port,
        }
    }

    /// Take the collector away. Blocking, so the caller must be on a multi-threaded
    /// runtime and inside `block_in_place`.
    fn partition(mut self) {
        drop(self.runtime.take());
    }
}

/// Settle for one connect attempt's worth, so a delivery is not racing the accept loop's
/// first poll or a just-closed socket's `TIME_WAIT`.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// The satellite: a spool, an ingress that seals its own readings, and an uplink.
struct Satellite {
    spool: Arc<mw_pipeline::Spool>,
    ingress: Arc<dyn mw_component::Ingress>,
    cursor: ComponentId,
}

fn build_satellite(dir: &std::path::Path) -> Satellite {
    let spool = spool(dir, satellite_node());
    let registry = owned_station_registry(&[(STATION, satellite_node())]);
    let ingress: Arc<dyn mw_component::Ingress> =
        Arc::new(mw_pipeline::SpoolIngress::new(Arc::clone(&spool), registry));
    Satellite {
        spool,
        ingress,
        cursor: mw_collector::uplink::id("suncrest").expect("a legal id"),
    }
}

fn uplink(satellite: &Satellite, port: u16) -> Uplink {
    let row = UplinkRow {
        peer_name: "suncrest".to_string(),
        peer_node: collector_node(),
        endpoint: format!("https://localhost:{port}"),
        auth_kind: "mtls".to_string(),
        tls_cert_path: Some(data("peer-a-cert.pem").to_string_lossy().into_owned()),
        tls_key_path: Some(data("peer-a-key.pem").to_string_lossy().into_owned()),
        tls_ca_path: Some(data("ca-cert.pem").to_string_lossy().into_owned()),
        psk: None,
        require_ack: true,
    };
    Uplink::build(
        &row,
        Identity {
            node: satellite_node(),
            name: "satellite-a".to_string(),
            software: "modernweather test".to_string(),
            stations: vec![mw_collector::wire::station_meta(
                &StationKey::new(STATION),
                &mw_component::StationProjection {
                    name: "Back Yard".to_string(),
                    kind: "davis".to_string(),
                    timezone: Some("America/Denver".to_string()),
                    latitude: Some(39.64),
                    longitude: Some(-105.2),
                    altitude_m: Some(1800.0),
                    snow_base_distance_mm: None,
                },
                satellite_node(),
            )],
        },
        Arc::clone(&satellite.spool),
        satellite.cursor.clone(),
    )
    .expect("the checked-in client credentials are readable")
}

/// A `Cx` for a body driven without a supervisor.
fn cx(id: &ComponentId) -> mw_component::Cx {
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    mw_component::Cx::new(id.clone(), CancellationToken::new(), Generation::ZERO, rx)
}

/// Seal `n` readings into the satellite's own spool, and hand back the batch the
/// subscription would deliver.
async fn admit(satellite: &Satellite, n: usize, barometer: f32) -> ReadingBatch {
    let now = chrono::Utc::now();
    let readings: Vec<mw_types::PartialReading> = (0..n)
        .map(|i| {
            mw_types::PartialReading::at(
                now,
                mw_types::Reading {
                    time: now,
                    station_id: STATION.to_string(),
                    barometer: Some(barometer + i as f32),
                    ..mw_types::Reading::default()
                },
            )
        })
        .collect();
    let meta = mw_station::identity::StationIdentity::FromRegistration.frame_meta(
        &ComponentId::new("station:davis:backyard").expect("a legal id"),
        Some(&StationKey::new(STATION)),
        now,
    );
    let admitted = mw_component::Ingress::admit(satellite.ingress.as_ref(), &readings, &meta)
        .await
        .expect("the satellite seals its own readings");
    assert_eq!(admitted.accepted, n as u32);
    batch_from(satellite, Seq::new(admitted.seq_high.get() - n as u64))
}

/// The batch a `Durable` subscription would deliver from `after`.
fn batch_from(satellite: &Satellite, after: Seq) -> ReadingBatch {
    let read = satellite
        .spool
        .read_after(after, mw_pipeline::READ_AHEAD)
        .expect("a readable spool");
    assert!(
        !read.envelopes.is_empty(),
        "nothing to deliver after {after}"
    );
    ReadingBatch::at(
        read.envelopes
            .into_iter()
            .map(|(_, envelope)| envelope)
            .collect::<Vec<_>>(),
        read.through,
    )
}

/// What the collector spooled, as `(origin, origin seq)` pairs.
fn received(collector: &Collector) -> Vec<(OriginId, Seq)> {
    collector
        .spool
        .read_after(Seq::new(0), 1024)
        .expect("a readable spool")
        .envelopes
        .into_iter()
        .map(|(_, envelope)| (envelope.provenance.origin(), envelope.provenance.seq()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_satellite_survives_a_collector_outage_and_resumes_without_duplicates() {
    let collector = build_collector(&scratch("collector"));
    let satellite = build_satellite(&scratch("satellite"));
    let serving = tokio::task::block_in_place(|| Serving::start(&collector, 0));
    settle().await;
    let port = serving.port;
    let mut uplink = uplink(&satellite, port);
    let cx = cx(&satellite.cursor);

    // ---------------------------------------------------------------- 1. the happy path
    let first = admit(&satellite, 2, 29.90).await;
    uplink
        .readings(first, &cx)
        .await
        .expect("the collector acknowledges the first batch");
    assert_eq!(
        received(&collector),
        vec![
            (satellite_node(), Seq::new(1)),
            (satellite_node(), Seq::new(2))
        ],
        "`accept` preserves provenance: the collector's spool carries the SATELLITE's \
         (origin, seq), not its own -- which is what the ack and the dedup both depend on \
         (W7 D-26)"
    );

    // ------------------------------------------------------------- 2. the partition
    let second = admit(&satellite, 2, 30.10).await;
    // `block_in_place` because dropping a runtime blocks, and this test is itself on one.
    tokio::task::block_in_place(|| serving.partition());
    settle().await;
    let failed = uplink
        .readings(second, &cx)
        .await
        .expect_err("a batch cannot be delivered to a collector that is not there");
    assert_eq!(
        mw_types::Classify::class(&failed),
        mw_types::Class::RETRYABLE,
        "the supervisor has to read this as Backoff rather than as a dead component: the \
         readings are in the spool and the cursor did not move"
    );
    assert_eq!(
        received(&collector).len(),
        2,
        "nothing reached the collector during the partition"
    );

    // ------------------------------------------------------------- 3. resume
    let serving = tokio::task::block_in_place(|| Serving::start(&collector, port));
    settle().await;
    // **From the cursor, not from memory.** The satellite remembers nothing about the
    // stream: this is the same read a `Durable` subscription would make after the
    // supervisor's backoff, starting from the position the last successful ack left.
    let replay = batch_from(&satellite, Seq::new(2));
    uplink
        .readings(replay, &cx)
        .await
        .expect("the collector is back and acknowledges the replayed batch");
    assert_eq!(
        received(&collector),
        vec![
            (satellite_node(), Seq::new(1)),
            (satellite_node(), Seq::new(2)),
            (satellite_node(), Seq::new(3)),
            (satellite_node(), Seq::new(4)),
        ],
        "the two envelopes the partition swallowed arrive exactly once each"
    );

    // ------------------------------------------------- 4. an ack lost on the wire
    //
    // The collector spooled the batch and the ack never reached the satellite, so the
    // satellite's cursor did not advance and it sends the same envelopes again. The
    // collector must acknowledge them -- a sender left retrying is the defect -- and must
    // not spool them twice.
    let again = batch_from(&satellite, Seq::new(2));
    uplink
        .readings(again, &cx)
        .await
        .expect("a replay is acknowledged rather than refused");
    assert_eq!(
        received(&collector).len(),
        4,
        "effectively-once: at-least-once delivery plus an idempotent admission gate \
         (W7 D-29). A duplicate stopped at the spool gate never reached the downstream \
         peers, the snapshot or the database."
    );

    tokio::task::block_in_place(|| serving.partition());
}

/// **§9.3: a Rust satellite against a Go-era collector.**
///
/// The collector here serves `WeatherV1.SendWeatherReadings` and **not**
/// `collector.v1.Collector`, which is exactly what a Go daemon looks like on the wire. The
/// uplink's `Hello` gets `UNIMPLEMENTED`, it falls back, and the readings arrive.
///
/// `mw_grpc::grpcrx` is the server — the same compat shim a Go satellite would be talking
/// to, driven from the other direction. That is the closest thing to the real oracle
/// available without a Go daemon in the loop, and it tests the one thing a unit test cannot:
/// that the fallback's method path, codec and message shape are the ones something else
/// actually serves.
///
/// **At-most-once, and the test says so**: there is no ack on that wire, so what `Ok` from
/// `readings` means here is "the collector accepted the stream". The degradation is a number
/// rather than only a log line, which is what `require_ack = false` is for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_satellite_falls_back_to_the_go_method_when_hello_is_unimplemented() {
    let satellite = build_satellite(&scratch("satellite-compat"));
    // The Go-era collector: its own spool, its own ingress, and `station:grpcrx`'s route
    // table -- which serves `SendWeatherReadings` and nothing from `collector.v1`.
    let go_spool = spool(&scratch("collector-compat"), collector_node());
    let go_registry = owned_station_registry(&[(STATION, collector_node())]);
    let go_ingress: Arc<dyn mw_component::Ingress> = Arc::new(mw_pipeline::SpoolIngress::new(
        Arc::clone(&go_spool),
        Arc::clone(&go_registry),
    ));
    let go_state = Arc::new(mw_grpc::grpcrx::ServiceState::new());
    go_state.install_for_test(Arc::new(mw_grpc::grpcrx::Handlers::new(
        ComponentId::new(mw_grpc::grpcrx::ID).expect("a legal id"),
        go_ingress,
    )));
    let table =
        mw_grpc::grpcrx::route_table(Arc::clone(&go_state)).expect("the compat shim's route table");
    // **The table's own router with a catch-all, not a `Mount`.** Two reasons, and the
    // second cost one debugging round:
    //
    // 1. `grpc-go`'s server answers `Unimplemented` for any method it does not register,
    //    and `axum`'s own fallback answers an HTTP `404` with a `text/plain` body that
    //    `tonic` cannot read as a gRPC status at all — it reports a frame error about the
    //    compression flag. A test using the default fallback would exercise a shape no Go
    //    collector produces, and the first version of this test did.
    // 2. `Mount` dispatches **by host through its own fallback**
    //    (`mw_http::mount`'s diagram), so replacing that fallback does not add a catch-all,
    //    it deletes the routing. `into_router_for_test` is the accessor that exists for a
    //    test that wants one table's handlers without the listener's host dispatch.
    //
    // The uplink deliberately does **not** fall back on a bare `404`: that is
    // indistinguishable from an endpoint pointed at the wrong server, and reporting a wrong
    // address as a degraded link would hide a configuration mistake.
    // `mw_collector::uplink::Uplink::hello` says so at the branch.
    let router = mw_http::RouteTable::into_router_for_test(table).fallback(|| async {
        tonic::Status::unimplemented("this collector does not implement that method")
            .into_http::<axum::body::Body>()
    });
    let hosts = Arc::new(HostTable::default());

    // Plaintext, because a Go daemon's gRPC port is: `grpcreceiver` configures server TLS
    // only and this test is about the *protocol* fallback rather than about the handshake.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the Go-era listener");
    let (tx, rx) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        let bound = mw_http::bind_with_hosts(
            &Listen {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                http_port: 0,
                https_port: None,
                grpc_port: 0,
            },
            router,
            None,
            hosts,
            None,
        )
        .await
        .expect("both ports bind");
        let _ = tx.send(bound.grpc_addr().port());
        let _ = bound.serve(std::future::pending::<()>()).await;
    });
    let port = tokio::task::block_in_place(|| {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the Go-era listener binds")
    });
    settle().await;

    let row = UplinkRow {
        peer_name: "go-collector".to_string(),
        peer_node: collector_node(),
        // `http://`, which is what a plaintext gRPC endpoint is.
        endpoint: format!("http://127.0.0.1:{port}"),
        // No client certificate: `psk` with an empty secret is the shape of an uplink
        // pointed at a daemon that authenticates nobody, which is what a Go collector does.
        auth_kind: "psk".to_string(),
        tls_cert_path: None,
        tls_key_path: None,
        tls_ca_path: None,
        psk: None,
        // The transition default. `true` would make the fallback a hard failure, which is
        // the other half of §9.3 and is asserted below.
        require_ack: false,
    };
    let identity = Identity {
        node: satellite_node(),
        name: "satellite-a".to_string(),
        software: "modernweather test".to_string(),
        stations: Vec::new(),
    };
    let mut uplink = Uplink::build(
        &row,
        identity.clone(),
        Arc::clone(&satellite.spool),
        satellite.cursor.clone(),
    )
    .expect("a psk row needs no files");

    let first = admit(&satellite, 2, 29.90).await;
    uplink
        .readings(first, &cx(&satellite.cursor))
        .await
        .expect("the fallback delivers to a collector that serves only the Go method");
    // The Go method has no provenance, so the receiving node **re-originates** every
    // reading to itself through `admit` -- which is §9.2's first limitation seen from this
    // side, and is why the spool here carries the collector's own node and its own `seq`
    // rather than the satellite's.
    let received: Vec<(OriginId, Seq)> = go_spool
        .read_after(Seq::new(0), 1024)
        .expect("a readable spool")
        .envelopes
        .into_iter()
        .map(|(_, e)| (e.provenance.origin(), e.provenance.seq()))
        .collect();
    assert_eq!(
        received,
        vec![
            (collector_node(), Seq::new(1)),
            (collector_node(), Seq::new(2))
        ],
        "a reading forwarded through the Go method is re-originated to the receiver, so it          is not deduplicable against the same reading arriving later through the collector          -- §9.2's first limitation, which is why the shim is a transition and not a tier"
    );

    // `require_ack = true` turns the same fallback into a hard failure, which is what an
    // operator sets once the fleet is upgraded (§9.3).
    let mut strict = Uplink::build(
        &UplinkRow {
            require_ack: true,
            ..row
        },
        identity,
        Arc::clone(&satellite.spool),
        satellite.cursor.clone(),
    )
    .expect("a psk row needs no files");
    let batch = admit(&satellite, 1, 30.10).await;
    let refused = strict
        .readings(batch, &cx(&satellite.cursor))
        .await
        .expect_err("`require_ack` makes a collector with no acknowledgement a failure");
    let chain = mw_types::DisplayChain(&refused).to_string();
    assert!(
        chain.contains("Hello"),
        "the failure names the RPC that was not implemented: {chain}"
    );

    tokio::task::block_in_place(|| drop(runtime));
}

/// The other half of layer 1: a certificate the peer CA signed that names **no node** must
/// not resolve to the only peer there is.
///
/// Over a real handshake, because the chain *is* valid — `webpki` accepts it and the
/// connection is established. What refuses it is layer 2, reading the SAN and finding
/// nothing: so this is the test that says a valid certificate is not an identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_validated_certificate_that_names_no_node_is_not_a_peer() {
    let collector = build_collector(&scratch("collector-no-san"));
    let satellite = build_satellite(&scratch("satellite-no-san"));
    let serving = tokio::task::block_in_place(|| Serving::start(&collector, 0));
    settle().await;
    let port = serving.port;

    let row = UplinkRow {
        peer_name: "suncrest".to_string(),
        peer_node: collector_node(),
        endpoint: format!("https://localhost:{port}"),
        auth_kind: "mtls".to_string(),
        // The CA signed this one too, and its SAN is a DNS name.
        tls_cert_path: Some(
            data("peer-dns-only-cert.pem")
                .to_string_lossy()
                .into_owned(),
        ),
        tls_key_path: Some(data("peer-dns-only-key.pem").to_string_lossy().into_owned()),
        tls_ca_path: Some(data("ca-cert.pem").to_string_lossy().into_owned()),
        psk: None,
        require_ack: true,
    };
    let mut uplink = Uplink::build(
        &row,
        Identity {
            node: satellite_node(),
            name: "impostor".to_string(),
            software: "modernweather test".to_string(),
            stations: Vec::new(),
        },
        Arc::clone(&satellite.spool),
        satellite.cursor.clone(),
    )
    .expect("the credentials are readable");

    let batch = admit(&satellite, 1, 29.92).await;
    let refused = uplink
        .readings(batch, &cx(&satellite.cursor))
        .await
        .expect_err("a certificate that names no node is not a peer");
    assert!(
        format!("{}", mw_types::DisplayChain(&refused)).contains("Hello"),
        "the refusal is at `Hello`, which is the per-RPC check running before any method \
         body: {}",
        mw_types::DisplayChain(&refused)
    );
    assert!(
        received(&collector).is_empty(),
        "no stream was created, so no envelope was read"
    );
    tokio::task::block_in_place(|| serving.partition());
}
