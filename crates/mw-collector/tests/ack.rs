//! **What an ack means** (W7 **D-21**), demonstrated rather than claimed.
//!
//! > An `IngestAck` means "durably appended to the collector's spool". It does not mean
//! > "accepted" and it does not mean "written to PostgreSQL".
//!
//! Those are three different claims, and a sender that treats an ack as the third will lose
//! data it believes is safe. So the boundary is tested from both sides:
//!
//! | Test | What it shows |
//! |---|---|
//! | [`an_ack_survives_sigkill`] | the ack is **stronger** than "accepted": the process is killed with no chance to flush and the reading is still there |
//! | [`nothing_in_this_file_has_a_database`] | the ack is **weaker** than "stored": there is no PostgreSQL in any of this, and the acks still happen |
//! | [`a_replayed_batch_is_acked_again_and_spooled_once`] | the resume path: an ack lost on the wire costs a deduplicated resend and never a duplicate row |
//!
//! # Why this kills a real process
//!
//! The cheap version of the first test drops the `Spool` and reopens it, and it would pass
//! for the wrong reason: `Drop` runs, buffers flush, and what is asserted is that a clean
//! shutdown works. The claim is about a collector that **stops without warning** between
//! acking and being asked again — so the child is sent `SIGKILL`, which no destructor runs
//! after, and the parent reopens the spool from the directory the child left behind.
//!
//! The child is this same test binary, re-executed with `MW_ACK_CHILD_SPOOL` set and the
//! child's own test selected by name. That is the standard arrangement for a test that
//! needs a process to die, and it is why [`child_acks_then_waits`] is a no-op without the
//! variable: in an ordinary run it is one of the tests and does nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mw_collector::service::Handlers;
use mw_collector::testing::{station_registry, FakePeers};
use mw_collector::{Gate, PeerAuthKind};
use mw_component::{PeerEntry, PeerName, PeerRegistry as _};
use mw_proto::wire::collector::v1 as pb;
use mw_types::{OriginId, Seq};

/// The environment variable the child is re-executed with.
const CHILD_SPOOL: &str = "MW_ACK_CHILD_SPOOL";

/// How long the parent waits for the child to produce an ack before giving up.
const CHILD_DEADLINE: Duration = Duration::from_secs(30);

/// This node is 9, the collector. The satellite is 1.
fn node(b: u8) -> OriginId {
    OriginId::new([b; 16])
}

const STATION: &str = "satellite-a:backyard";

/// One sealed observation, as the satellite would send it: `origin = 1`, addressed to 9.
///
/// **`observed_at` is `now`**, not a fixed literal, and that is not laziness: §4.6's window
/// is +5 minutes / −30 days against the *collector's* clock, so a test pinned to a literal
/// instant passes until thirty days after it was written and then starts failing for a
/// reason that has nothing to do with the code. The window's own edges are asserted with a
/// controlled clock in `crate::admit`'s unit tests, where `now` is an argument.
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

/// A batch of envelopes at the given origin sequence numbers.
///
/// **The numbers start at 1 rather than at §2.1's `4711`**, and the reason is a property of
/// the dedup gate worth knowing: the window is 4096 wide and sits on the high water, so an
/// origin's **first** envelope at `seq = 4711` lands past the window's reach, is accepted
/// as `FreshAfterOverrun`, and leaves the high water at 0 — correctly, because nothing in
/// `1..=4710` was ever accepted (`mw_pipeline::spool::dedup`'s note on `hw`). Starting at 1
/// is the steady state these tests are about: a contiguous run, so the high water advances
/// and `known_through` means something.
fn batch(seqs: &[u64], through: u64) -> pb::IngestBatch {
    pb::IngestBatch {
        envelopes: seqs.iter().copied().map(envelope).collect(),
        through,
        sender_depth: 0,
    }
}

fn peer() -> PeerEntry {
    PeerEntry {
        node: node(1),
        name: PeerName::new("satellite-a"),
        auth: mw_component::PeerAuth::MtlsCa,
        relay: false,
        origins: [node(1)].into_iter().collect(),
        revoked_at: None,
        software: Some("modernweather 0.1.0".to_string()),
    }
}

/// The collector's handlers over a real spool at `path`, with **no database anywhere**.
fn collector(path: &Path) -> (Arc<Handlers>, Arc<mw_pipeline::Spool>) {
    let spool = mw_collector::testing::spool_for(path, node(9));
    let registry = station_registry(&[(STATION, 1)]);
    let ingress: Arc<dyn mw_component::Ingress> = Arc::new(mw_pipeline::SpoolIngress::new(
        Arc::clone(&spool),
        Arc::clone(&registry),
    ));
    let handlers = Arc::new(Handlers::new(
        mw_component::ComponentId::new(mw_collector::INGEST_ID).expect("a legal id"),
        FakePeers::ca("satellite-a", node(1)) as Arc<dyn mw_component::PeerRegistry>,
        ingress,
        Arc::clone(&spool),
        Gate::new(registry, node(9)),
        PeerAuthKind::MtlsCa,
    ));
    (handlers, spool)
}

/// What is in the spool, read back as the records themselves.
fn spooled(spool: &mw_pipeline::Spool) -> Vec<(Seq, mw_types::Envelope)> {
    spool
        .read_after(Seq::new(0), 1024)
        .expect("a readable spool")
        .envelopes
}

// ---------------------------------------------------------------------------------------
// The child: ack, record that it happened, and wait to be killed
// ---------------------------------------------------------------------------------------

/// Half of [`an_ack_survives_sigkill`], running in the child process.
///
/// A no-op without [`CHILD_SPOOL`], which is every ordinary run. With it: open the spool in
/// the directory the parent chose, acknowledge one batch, write the marker that says the
/// ack was issued, and then **block for ever**. The parent kills it here, so nothing below
/// the marker ever runs — no flush, no close, no `Drop`.
#[test]
fn child_acks_then_waits() {
    let Ok(dir) = std::env::var(CHILD_SPOOL) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async move {
        let (handlers, _spool) = collector(&dir.join("spool"));
        let mut acked_through = 0u64;
        let ack = handlers
            .ingest_batch(&peer(), &batch(&[1], 1), true, &mut acked_through)
            .await
            .expect("the batch is acknowledged");
        assert_eq!(ack.through, 1);
        assert_eq!(ack.status, pb::AckStatus::AckOk as i32);
        // **The marker is written after the ack and is itself fsynced**, so the parent
        // cannot see it before the promise was made — and cannot fail to see it because of
        // its own buffering.
        let marker = std::fs::File::create(dir.join("acked")).expect("a writable marker");
        marker.sync_all().expect("the marker is durable");
        // Nothing below this line runs. The parent sends SIGKILL.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
}

// ---------------------------------------------------------------------------------------
// The three assertions
// ---------------------------------------------------------------------------------------

/// **An ack issued, the process killed, and the reading still present on restart.**
#[test]
fn an_ack_survives_sigkill() {
    let dir = mw_collector::testing::scratch("ack-sigkill");
    // `scratch` hands back the spool path; the marker goes beside it so that the directory
    // the child writes the spool into is the one the parent reopens.
    let root = dir
        .parent()
        .expect("the scratch path has a parent")
        .to_path_buf();

    let mut child =
        std::process::Command::new(std::env::current_exe().expect("this test binary's own path"))
            .args(["--exact", "child_acks_then_waits", "--nocapture"])
            .env(CHILD_SPOOL, &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the test binary can re-execute itself");

    let marker = root.join("acked");
    let started = Instant::now();
    while !marker.exists() {
        assert!(
            started.elapsed() < CHILD_DEADLINE,
            "the child did not acknowledge a batch within {CHILD_DEADLINE:?}; it exited \
             with {:?}",
            child.try_wait()
        );
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the child exited before acknowledging anything: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // **SIGKILL**, which no destructor runs after: whatever is on disk now is what the
    // `fsync` inside the ack put there.
    child.kill().expect("the child can be killed");
    let status = child.wait().expect("the child can be reaped");
    assert!(
        !status.success(),
        "the child was supposed to be killed, not to exit cleanly: {status}"
    );

    // Restart: a different process, the same directory.
    let (_handlers, spool) = collector(&root.join("spool"));
    let records = spooled(&spool);
    assert_eq!(
        records.len(),
        1,
        "the envelope the collector acknowledged is not in the spool after a SIGKILL, so \
         the ack was a promise it could not keep"
    );
    let (position, envelope) = &records[0];
    assert_eq!(*position, Seq::FIRST, "the collector's own first position");
    assert_eq!(
        envelope.provenance.origin(),
        node(1),
        "`accept` preserves provenance: the origin is still the satellite's"
    );
    assert_eq!(
        envelope.provenance.seq(),
        Seq::FIRST,
        "…and so is the origin's own sequence number, which is what the ack and the dedup \
         both depend on (W7 D-26)"
    );
    assert_eq!(envelope.station.as_str(), STATION);
    assert_eq!(envelope.reading.barometer, Some(29.92));
    // Field presence across the hop and across the restart, which is **D-89**: an
    // unreported humidity must not read as 0 %.
    assert_eq!(
        envelope.reading.out_humidity, None,
        "the satellite reported no humidity, and the record says so"
    );
    assert_eq!(
        spool.high_water(node(1)),
        Seq::FIRST,
        "the dedup gate's high water is in the same group commit as the record (§1.3), so \
         it survives the kill too -- which is what makes the resume deduplicated"
    );
}

/// **The ack is weaker than "stored", and this file is the proof.**
///
/// There is no `ReadingStore`, no `db:postgres`, no connection string and no PostgreSQL
/// anywhere in these tests, and every ack above still happens. That is W7 **D-21** as an
/// executable fact: acking on the database would make a stalled database back-pressure
/// every satellite in the fleet, and the collector's own database write is a *cursor* on
/// this same spool rather than a precondition for the ack.
#[tokio::test]
async fn nothing_in_this_file_has_a_database() {
    let dir = mw_collector::testing::scratch("ack-no-database");
    let (handlers, spool) = collector(&dir);
    let mut acked_through = 0u64;
    let ack = handlers
        .ingest_batch(&peer(), &batch(&[1], 1), true, &mut acked_through)
        .await
        .expect("an ack needs no database");
    assert_eq!(ack.status, pb::AckStatus::AckOk as i32);
    assert_eq!(spooled(&spool).len(), 1);
    // The spool has a `writer:timescale` cursor's worth of records waiting and no writer.
    // That is the collector of W7 §8.4 -- a relay with no database of its own -- and it is
    // a legitimate node rather than a degraded one.
    assert_eq!(spool.tail_seq(), Seq::FIRST);
}

/// **Resume without duplicates** (W7 **D-27**, **D-29**), which is the other half of the
/// done-criterion: a satellite that survives a collector outage and resumes.
///
/// The ack that was lost on the wire is modelled exactly: the batch is delivered, the
/// collector acks, the ack is discarded, and the satellite sends the same batch again. The
/// second delivery is acknowledged — a sender must not be left retrying — and it is
/// **spooled once**.
#[tokio::test]
async fn a_replayed_batch_is_acked_again_and_spooled_once() {
    let dir = mw_collector::testing::scratch("ack-replay");
    let (handlers, spool) = collector(&dir);
    let mut acked_through = 0u64;

    let first = handlers
        .ingest_batch(&peer(), &batch(&[1, 2], 2), true, &mut acked_through)
        .await
        .expect("the first delivery is acknowledged");
    assert_eq!(first.through, 2);
    assert_eq!(first.duplicates, 0);
    assert_eq!(
        first.known_through.len(),
        1,
        "the first ack of a stream carries the per-origin high water (W7 D-23)"
    );
    assert_eq!(spooled(&spool).len(), 2);

    // The link dropped between the collector's `fsync` and the satellite reading the ack.
    // The satellite's cursor did not advance, so it sends the same two envelopes.
    let second = handlers
        .ingest_batch(&peer(), &batch(&[1, 2], 2), false, &mut acked_through)
        .await
        .expect("a replay is acknowledged rather than refused");
    assert_eq!(
        second.through, 2,
        "the sender must be able to advance, or it retries for ever"
    );
    assert_eq!(
        second.duplicates, 2,
        "the dedup gate in front of the spool recognised both (W7 D-27)"
    );
    assert!(
        second.known_through.is_empty(),
        "`known_through` is sent once per stream, not per batch"
    );
    assert_eq!(
        spooled(&spool).len(),
        2,
        "a replayed envelope must not reach the spool a second time: a constraint in the \
         database would fire after fan-out, by which time the duplicate has already \
         reached the downstream peers and the live stream"
    );

    // A genuinely new envelope after the replay still lands, which is what says the gate
    // deduplicates rather than stops.
    let third = handlers
        .ingest_batch(&peer(), &batch(&[3], 3), false, &mut acked_through)
        .await
        .expect("the stream carries on");
    assert_eq!(third.duplicates, 0);
    assert_eq!(spooled(&spool).len(), 3);
    assert_eq!(spool.high_water(node(1)), Seq::new(3));
}

/// `HelloAck.known_through` is the collector's per-origin high water **for this peer's
/// origins only** — the advisory fast resume of §4.2.
///
/// Asserted because the alternative is a leak: handing a satellite every origin this node
/// has ever seen would tell it how far along every other node in the fleet is.
#[tokio::test]
async fn known_through_covers_this_peer_s_origins_and_no_others() {
    let dir = mw_collector::testing::scratch("ack-known-through");
    let (handlers, _spool) = collector(&dir);
    let mut acked_through = 0u64;
    handlers
        .ingest_batch(&peer(), &batch(&[1], 1), true, &mut acked_through)
        .await
        .expect("acknowledged");

    let peers = FakePeers::ca("satellite-a", node(1));
    let entry = peers.peer(node(1)).expect("the one peer");
    let ack = handlers
        .hello(
            &entry,
            &pb::NodeIdentity {
                node_id: node(1).as_bytes().to_vec(),
                node_name: "satellite-a".to_string(),
                software: "modernweather 0.1.0".to_string(),
                stations: vec![pb::StationMeta {
                    station_key: STATION.to_string(),
                    name: "Back Yard".to_string(),
                    station_type: "davis".to_string(),
                    ..Default::default()
                }],
            },
        )
        .await
        .expect("a resolved peer says hello");
    assert_eq!(
        ack.known_through.len(),
        1,
        "one origin: this peer, which has no relay grant"
    );
    assert_eq!(ack.known_through[0].origin, node(1).as_bytes().to_vec());
    assert_eq!(ack.known_through[0].seq, 1);
    assert_eq!(ack.collector_node_id, node(9).as_bytes().to_vec());
    assert_eq!(
        ack.max_batch_envelopes, 256,
        "what the ack advertises has to be what `accept` enforces"
    );
    assert!(
        (ack.server_time - chrono::Utc::now().timestamp_millis()).abs() < 60_000,
        "`server_time` is unix MILLISECONDS, which is what the satellite's \
         `clock_skew_seconds` is computed from -- in seconds it would read as 1970"
    );
}
