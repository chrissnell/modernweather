//! The two uplink properties that are about **not** sending: W7 **D-9**'s cursor advance,
//! and §4.2's fast-resume skip.
//!
//! Both are asserted with no collector anywhere — not even a listening socket — because
//! that is the condition under which the first one is easy to get wrong. A batch this node
//! will never send to this peer has to advance the cursor *whether or not the peer is
//! reachable*: a cursor that stalls on such an envelope stalls for ever, the spool never
//! reclaims, and `[spool].max_lag` eventually abandons the cursor — so a mutual uplink
//! presents as a disk filling up rather than as a counter rising.
//!
//! The first version of `deliver` dialled the collector before it staged the batch, so this
//! advance depended on a collector that was up. The bug was invisible to every test that
//! had a collector running, which is why these tests deliberately do not.

use std::sync::Arc;

use mw_collector::uplink::{Identity, Uplink, UplinkRow};
use mw_component::{ComponentId, Generation, OnReading as _, ReadingBatch};
use mw_types::{Envelope, OriginId, Provenance, Reading, Seq, StationKey};
use tokio_util::sync::CancellationToken;

/// This node.
const ME: u8 = 1;
/// The peer this uplink sends to.
const PEER: u8 = 9;

fn node(b: u8) -> OriginId {
    OriginId::new([b; 16])
}

/// An uplink pointed at a port nothing is listening on.
///
/// **Nothing is listening, deliberately.** Every assertion in this file is about a batch
/// that is never sent, so a reachable collector would make the tests pass for a reason that
/// has nothing to do with what they claim.
fn uplink(spool: &Arc<mw_pipeline::Spool>) -> Uplink {
    let data = |name: &str| {
        mw_collector::testing::data(name)
            .to_string_lossy()
            .into_owned()
    };
    let row = UplinkRow {
        peer_name: "suncrest".to_string(),
        peer_node: node(PEER),
        // Port 1 on loopback: a privileged port nothing in this container binds, so a
        // connect attempt is refused rather than hanging.
        endpoint: "https://127.0.0.1:1".to_string(),
        auth_kind: "mtls".to_string(),
        tls_cert_path: Some(data("peer-a-cert.pem")),
        tls_key_path: Some(data("peer-a-key.pem")),
        tls_ca_path: Some(data("ca-cert.pem")),
        psk: None,
        require_ack: true,
    };
    Uplink::build(
        &row,
        Identity {
            node: node(ME),
            name: "satellite-a".to_string(),
            software: "modernweather test".to_string(),
            stations: Vec::new(),
        },
        Arc::clone(spool),
        mw_collector::uplink::id("suncrest").expect("a legal id"),
    )
    .expect("the checked-in credentials are readable")
}

fn cx() -> mw_component::Cx {
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    mw_component::Cx::new(
        ComponentId::new("uplink:suncrest").expect("a legal id"),
        CancellationToken::new(),
        Generation::ZERO,
        rx,
    )
}

/// One envelope whose path already contains `hops`.
fn envelope(seq: u64, hops: &[u8]) -> Envelope {
    let now = chrono::Utc::now();
    let path: Vec<OriginId> = hops.iter().copied().map(node).collect();
    Envelope {
        station: StationKey::new("satellite-a:backyard"),
        observed_at: now,
        received_at: now,
        provenance: Provenance::sealed(path[0], Seq::new(seq), &path)
            .expect("the test's path is one an honest encoder produces"),
        reading: Arc::new(Reading {
            time: now,
            station_id: "satellite-a:backyard".to_string(),
            barometer: Some(29.92),
            ..Reading::default()
        }),
    }
}

fn batch(envelopes: Vec<Envelope>, through: u64) -> ReadingBatch {
    ReadingBatch::at(envelopes, Seq::new(through))
}

fn spool() -> Arc<mw_pipeline::Spool> {
    mw_collector::testing::spool_for(&mw_collector::testing::scratch("uplink"), node(ME))
}

/// **W7 D-9, with the collector down.**
///
/// Every envelope in the batch has already visited the peer, so there is nothing to send —
/// and `Ok` is what advances the supervisor's cursor over it. If `deliver` dialled before
/// it staged, this would be `Err` and the cursor would stay put on a batch that will never
/// go anywhere.
#[tokio::test]
async fn a_batch_already_seen_by_the_peer_advances_the_cursor_with_the_collector_down() {
    let spool = spool();
    let mut uplink = uplink(&spool);
    // `[1, 9]` — admitted here, and the peer is already on the path.
    let batch = batch(vec![envelope(1, &[ME, PEER]), envelope(2, &[ME, PEER])], 2);
    uplink
        .readings(batch, &cx())
        .await
        .expect("a batch with nothing to send must advance the cursor, reachable or not");
}

/// The same property for `forward`'s refusal rather than the filter's.
///
/// A path already at `PathSet::MAX_HOPS` cannot be forwarded another hop, so `forward`
/// refuses — and that refusal must advance the cursor too, for D-9's reason. It is also the
/// one that must **not** be counted as a loop refusal: `uplink_loop_refused_total` is
/// documented as "a bug in this code" (D-41), and an eight-hop topology is the bound
/// working.
#[tokio::test]
async fn a_batch_at_the_hop_bound_advances_the_cursor_with_the_collector_down() {
    let spool = spool();
    let mut uplink = uplink(&spool);
    // Eight hops, none of them the peer, so the filter passes it to `forward` and `forward`
    // has nowhere to put the ninth.
    let hops: Vec<u8> = (10..18).collect();
    assert_eq!(hops.len(), mw_types::PathSet::MAX_HOPS);
    assert!(!hops.contains(&PEER));
    uplink
        .readings(batch(vec![envelope(1, &hops)], 1), &cx())
        .await
        .expect("an envelope at the hop bound must advance the cursor, reachable or not");
}

/// A batch that **does** have something to send fails when the collector is down — which is
/// what says the two tests above are not passing because `deliver` returns `Ok` for
/// everything.
#[tokio::test]
async fn a_batch_with_something_to_send_fails_when_the_collector_is_down() {
    let spool = spool();
    let mut uplink = uplink(&spool);
    let e = uplink
        .readings(batch(vec![envelope(1, &[ME])], 1), &cx())
        .await
        .expect_err("there is no collector on port 1");
    assert_eq!(
        mw_types::Classify::class(&e),
        mw_types::Class::RETRYABLE,
        "the supervisor has to read an unreachable collector as Backoff: the readings are \
         in the spool and the cursor did not move"
    );
}

/// A batch with no spool position is a wiring mistake, not a runtime condition: an uplink
/// has to declare `ReadingPolicy::Durable`, because that is the only policy that carries
/// the cursor position `IngestBatch.through` is (W7 **D-22**).
///
/// **Asserted as a failure and not as a `Fatal` one**, and the difference is worth pinning:
/// `ComponentError::Tick` is `RETRYABLE` unconditionally by design (W2a Decision 29, and
/// that type's own documentation), so no error reached through `readings` can be fatal to
/// the supervisor however it classifies itself. An earlier version of this test asserted
/// `Fatal` here and failed — which is how the overclaim in `Classify for UplinkError`'s
/// comment was found. The classification earns its keep at build time, which
/// [`a_malformed_endpoint_is_refused_at_build_and_is_fatal`] covers.
#[tokio::test]
async fn a_batch_with_no_cursor_position_is_a_failure() {
    let spool = spool();
    let mut uplink = uplink(&spool);
    let e = uplink
        .readings(ReadingBatch::new(vec![envelope(1, &[ME])]), &cx())
        .await
        .expect_err("a `Latest` or `Lossy` batch carries no position");
    let chain = mw_types::DisplayChain(&e).to_string();
    assert!(
        chain.contains("no spool position"),
        "the message has to name the wiring mistake: {chain}"
    );
}

/// A URL `tonic` will not dial is refused at **build**, where a classification is read.
///
/// On the connect path it would have been one more `Tick` failure, indistinguishable from a
/// collector that was merely down — so an operator with a typo in `uplink.endpoint` would
/// have seen an uplink backing off for ever with no reason they could act on.
#[test]
fn a_malformed_endpoint_is_refused_at_build_and_is_fatal() {
    let spool = spool();
    let data = |name: &str| {
        mw_collector::testing::data(name)
            .to_string_lossy()
            .into_owned()
    };
    let row = UplinkRow {
        peer_name: "suncrest".to_string(),
        peer_node: node(PEER),
        endpoint: "not a url at all".to_string(),
        auth_kind: "mtls".to_string(),
        tls_cert_path: Some(data("peer-a-cert.pem")),
        tls_key_path: Some(data("peer-a-key.pem")),
        tls_ca_path: Some(data("ca-cert.pem")),
        psk: None,
        require_ack: true,
    };
    let e = Uplink::build(
        &row,
        Identity {
            node: node(ME),
            name: "satellite-a".to_string(),
            software: "modernweather test".to_string(),
            stations: Vec::new(),
        },
        spool,
        mw_collector::uplink::id("suncrest").expect("a legal id"),
    )
    .expect_err("`not a url at all` is not an address");
    assert_eq!(
        mw_types::Classify::class(&e),
        mw_types::Class::Fatal,
        "no retry makes a URL parse"
    );
    let chain = mw_types::DisplayChain(&e).to_string();
    assert!(chain.contains("not a url at all"), "{chain}");
}
