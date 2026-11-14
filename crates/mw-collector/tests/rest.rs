//! `POST /v1/ingest` — §4.5's **same contract** over a different codec (W7 **D-30**).
//!
//! This file exists because the REST handler is the collector's **second** authorisation
//! entry point and nothing exercised it. `rest.rs`'s own module documentation claims the
//! same admission, the same `accept`, the same dedup gate and the same idempotency as the
//! gRPC path, and the only test that mentioned those limits asserted two constants against
//! each other — a test that would still pass with the handler deleted.
//!
//! What makes it worth a file of its own rather than a line in `ack.rs` is the first case
//! below. `mw_collector::service::route_table` mounts `/v1/ingest` with `Scope::Any`, so the
//! path is routable on the **website's** socket as well as the ingest one, and the only
//! thing between a request there and the spool is `presented`'s
//! `connection.ok_or(Reject::UnauthorizedPeer)`. That line is layer 1 of three (**D-46**),
//! and a test that never sends a request without a connection cannot see it go.
//!
//! **`psk` rather than `mtls`** for the authorised cases, because a PSK peer authenticates
//! from two headers and needs no certificate chain — `PeerCerts` has no public constructor
//! that fills one in, and a real chain is `collector_two_node.rs`'s job.

use std::sync::Arc;

use http::{header, StatusCode};
use mw_collector::service::{Handlers, ServiceState};
use mw_collector::testing::{station_registry, FakePeers};
use mw_collector::{Gate, PeerAuthKind};
use mw_proto::wire::collector::v1 as pb;
use mw_types::OriginId;
use prost::Message as _;

const STATION: &str = "satellite-a:backyard";
const SECRET: &str = "a-pre-shared-secret-of-adequate-length";

fn node(b: u8) -> OriginId {
    OriginId::new([b; 16])
}

/// A state with the handlers installed, over a real spool and no database.
fn collector(tag: &str) -> (Arc<ServiceState>, Arc<mw_pipeline::Spool>) {
    let dir = mw_collector::testing::scratch(tag);
    let spool = mw_collector::testing::spool_for(&dir, node(9));
    let registry = station_registry(&[(STATION, 1)]);
    let ingress: Arc<dyn mw_component::Ingress> = Arc::new(mw_pipeline::SpoolIngress::new(
        Arc::clone(&spool),
        Arc::clone(&registry),
    ));
    let state = Arc::new(ServiceState::new());
    state.install_for_test(Arc::new(Handlers::new(
        mw_component::ComponentId::new(mw_collector::INGEST_ID).expect("a legal id"),
        FakePeers::psk("satellite-a", node(1), SECRET) as Arc<dyn mw_component::PeerRegistry>,
        ingress,
        Arc::clone(&spool),
        Gate::new(registry, node(9)),
        PeerAuthKind::Psk,
    )));
    (state, spool)
}

/// One sealed observation from satellite 1, addressed to 9.
fn envelope(seq: u64) -> pb::Envelope {
    let now = chrono::Utc::now();
    pb::Envelope {
        origin: prost::bytes::Bytes::copy_from_slice(node(1).as_bytes()),
        seq,
        path: vec![
            prost::bytes::Bytes::copy_from_slice(node(1).as_bytes()),
            prost::bytes::Bytes::copy_from_slice(node(9).as_bytes()),
        ],
        station_key: STATION.to_string(),
        observed_at: now.timestamp_millis(),
        received_at: now.timestamp_millis(),
        reading: Some(mw_proto::wire::Reading {
            station_id: STATION.to_string(),
            reading_timestamp: Some(mw_proto::wire::Timestamp {
                seconds: now.timestamp(),
                nanos: 0,
            }),
            barometer: Some(29.92),
            outside_temperature: Some(41.5),
            ..Default::default()
        }),
    }
}

fn batch(seqs: &[u64], through: u64) -> pb::IngestBatch {
    pb::IngestBatch {
        envelopes: seqs.iter().copied().map(envelope).collect(),
        through,
        sender_depth: 0,
    }
}

/// How a request is built: `authenticated` decides whether it carries the two PSK headers,
/// and `connection` whether it looks like it arrived over the ingest socket at all.
struct Req {
    connection: bool,
    authenticated: bool,
    content_type: Option<&'static str>,
    body: Vec<u8>,
}

impl Req {
    fn good(batch: &pb::IngestBatch) -> Self {
        Self {
            connection: true,
            authenticated: true,
            content_type: Some(mw_collector::rest::CONTENT_TYPE),
            body: batch.encode_to_vec(),
        }
    }

    fn build(self) -> axum::extract::Request {
        let mut request = http::Request::builder()
            .method(http::Method::POST)
            .uri("/v1/ingest");
        if let Some(content_type) = self.content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        if self.authenticated {
            request = request
                .header("x-mw-node-id", hex(node(1)))
                .header(header::AUTHORIZATION, format!("Bearer {SECRET}"));
        }
        let mut request = request
            .body(axum::body::Body::from(self.body))
            .expect("a legal request");
        if self.connection {
            // `PeerCerts::default()` is the no-certificate connection its own documentation
            // offers for a test. It is what a `psk` socket hands over for real, which is
            // why the authorised cases below are `psk` ones.
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(mw_http::PeerCerts::default()));
        }
        request
    }
}

fn hex(node: OriginId) -> String {
    node.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

async fn post(state: &Arc<ServiceState>, request: Req) -> (StatusCode, Vec<u8>) {
    let response = mw_collector::rest::ingest(Arc::clone(state), request.build()).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the response body is readable")
        .to_vec();
    (status, body)
}

fn ack(body: &[u8]) -> pb::IngestAck {
    pb::IngestAck::decode(body).expect("the body is an encoded IngestAck")
}

/// **Layer 1 is not optional on this path.**
///
/// `/v1/ingest` is mounted `Scope::Any`, so the same handler answers on the website's
/// socket — where no `ConnectInfo<PeerCerts>` exists. A request with no connection has
/// presented nothing, whatever its headers say, and the two PSK headers here are the ones
/// that would authenticate it on the ingest socket.
#[tokio::test]
async fn a_request_with_no_connection_is_refused_however_well_it_authenticates() {
    let (state, spool) = collector("rest-no-connection");
    let (status, _) = post(
        &state,
        Req {
            connection: false,
            ..Req::good(&batch(&[1], 1))
        },
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a request that did not arrive over the ingest socket is not a peer's"
    );
    assert_eq!(
        spool.stats().appends,
        0,
        "and nothing it carried reached the spool"
    );
}

/// The credentials themselves are checked, not merely present.
#[tokio::test]
async fn a_request_with_no_credentials_is_refused() {
    let (state, spool) = collector("rest-no-credentials");
    let (status, _) = post(
        &state,
        Req {
            authenticated: false,
            ..Req::good(&batch(&[1], 1))
        },
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(spool.stats().appends, 0);
}

/// §4.5's `413`, over the **envelope** limit rather than the byte one.
///
/// Asserted through the handler rather than against the constant: an earlier test compared
/// `MAX_BATCH_ENVELOPES` with itself, which would have passed with this check deleted.
#[tokio::test]
async fn a_batch_over_the_envelope_limit_is_refused_before_it_is_admitted() {
    let (state, spool) = collector("rest-too-many");
    let seqs: Vec<u64> = (1..=(mw_pipeline::MAX_BATCH_ENVELOPES as u64 + 1)).collect();
    let (status, _) = post(&state, Req::good(&batch(&seqs, seqs.len() as u64))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        spool.stats().appends,
        0,
        "the limit is enforced before the append, or it is not a limit"
    );
}

/// One codec, named in the refusal (§4.5: the JSON mapping is deliberately absent).
#[tokio::test]
async fn another_content_type_is_refused_naming_the_one_that_works() {
    let (state, _spool) = collector("rest-content-type");
    let (status, body) = post(
        &state,
        Req {
            content_type: Some("application/json"),
            ..Req::good(&batch(&[1], 1))
        },
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(
        String::from_utf8_lossy(&body).contains(mw_collector::rest::CONTENT_TYPE),
        "the refusal has to name the content type that works: {}",
        String::from_utf8_lossy(&body)
    );
}

/// **The same idempotency as the stream**: a retried POST re-acks and does not duplicate.
///
/// This is the REST half of `ack.rs`'s resume property, and it is the one that says the
/// handler really does go through `ingest_batch` rather than a path of its own.
#[tokio::test]
async fn a_retried_post_is_acked_again_and_spooled_once() {
    let (state, spool) = collector("rest-retry");
    // Opened before anything arrives, because a cursor starts at the tail.
    let mut cursor = spool
        .open_cursor(mw_component::ComponentId::new("writer:timescale").expect("a legal id"))
        .expect("a free cursor");
    let batch = batch(&[1, 2], 2);
    let (status, body) = post(&state, Req::good(&batch)).await;
    assert_eq!(status, StatusCode::OK);
    let first = ack(&body);
    assert_eq!(first.status, pb::AckStatus::AckOk as i32);
    assert_eq!(first.through, 2);
    assert_eq!(first.duplicates, 0);
    assert_eq!(first.rejected, 0);
    // `known_through` is on every response, because there is no stream to put it on once.
    assert_eq!(
        first.known_through.len(),
        1,
        "one origin, this peer's: {:?}",
        first.known_through
    );

    let (status, body) = post(&state, Req::good(&batch)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a sender whose response was lost must not be left retrying"
    );
    let second = ack(&body);
    assert_eq!(second.status, pb::AckStatus::AckOk as i32);
    assert_eq!(second.duplicates, 2, "the dedup gate is the same gate");
    assert_eq!(second.rejected, 0);
    assert_eq!(
        spool.stats().appends,
        1,
        "the second request appended nothing: `accept` commits a batch whose every \
         envelope the dedup gate refused without touching the log"
    );
    assert_eq!(
        cursor.read().expect("readable").envelopes.len(),
        2,
        "and the second request stored nothing"
    );
}

/// §4.5's `422`: every envelope failed a check, so there is an ack with a status and no
/// append — and `through` still advances, or the sender stalls on it for ever.
///
/// **The refusal has to be a permanent one**, which is a distinction this test learned the
/// expensive way: an unknown station is *transient* (`Reject::is_transient` — an operator
/// may add the row, and advancing over it would lose the readings for good), so it answers
/// `200` with `ACK_RESOURCE_EXHAUSTED` and an unchanged `through`. A reading carrying no
/// metric at all is permanent: nothing a later configuration does makes it storable.
#[tokio::test]
async fn a_batch_every_envelope_of_which_is_refused_is_a_422_that_still_advances() {
    let (state, spool) = collector("rest-rejected");
    let mut batch = batch(&[1], 1);
    let reading = batch.envelopes[0]
        .reading
        .as_mut()
        .expect("the fixture sets a reading");
    reading.barometer = None;
    reading.outside_temperature = None;
    let (status, body) = post(&state, Req::good(&batch)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let ack = ack(&body);
    assert_eq!(ack.status, pb::AckStatus::AckRejected as i32);
    assert_eq!(ack.rejected, 1);
    assert_eq!(
        ack.through, 1,
        "a permanent refusal advances the sender's cursor (D-26)"
    );
    assert_eq!(spool.stats().appends, 0);
}

/// §4.5's `503` + `Retry-After`: the route exists and the component is not running.
#[tokio::test]
async fn a_route_whose_component_is_not_running_answers_503() {
    let state = Arc::new(ServiceState::new());
    let response =
        mw_collector::rest::ingest(Arc::clone(&state), Req::good(&batch(&[1], 1)).build()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().expect("ascii")),
        Some(mw_collector::rest::RETRY_AFTER_SECONDS.to_string().as_str()),
        "a sender needs to be told how long to wait"
    );
}
