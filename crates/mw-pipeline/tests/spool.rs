//! The spool's durability properties, as implementation-plan.md M3-2's test strategy
//! names them.
//!
//! - **power-loss simulation by truncating a segment at every byte offset of the last
//!   record** — `a_kill_minus_nine_mid_append_loses_nothing_acknowledged`, which does it
//!   through `Spool::open`'s recovery rather than only through the framing functions (the
//!   format-level version is in `spool::format`'s own tests).
//! - **a property test that a replayed prefix is idempotent** —
//!   `replaying_any_prefix_of_a_peers_stream_is_idempotent`.
//! - **the ceiling policy, which needs its own test** — `at_the_ceiling_*`, three of them:
//!   refused at the edge, counted, and the spool still usable afterwards.
//!
//! The stalled-sink test re-pointed at a real spool is in `crates/modernweather/tests/`,
//! because `mw-supervisor` and `mw-pipeline` are the same layer and `ci/layers.toml`
//! forbids either naming the other — the binary is the only place they meet.

use std::sync::Arc;

use chrono::Duration as ChronoDuration;
use mw_pipeline::spool::format::{scan_records, SEGMENT_HEADER_BYTES};
use mw_pipeline::spool::{Spool, SpoolConfig, Verdict};
use mw_pipeline::testing::{config, forwarded, id, node, reading, spool, Scratch};
use mw_pipeline::{Clock, CursorState, Draft, SpoolError, TestClock};
use mw_types::{Seq, StationKey};

fn drafts(n: usize, clock: &TestClock) -> Vec<Draft> {
    (0..n)
        .map(|i| Draft {
            station: StationKey::new("backyard"),
            observed_at: clock.now(),
            received_at: clock.now(),
            reading: Arc::new(reading(29.0 + i as f32 / 100.0)),
        })
        .collect()
}

fn segments(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .expect("the spool directory is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("seg"))
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_fresh_spool_starts_empty_and_mints_from_one() {
    let scratch = Scratch::new("fresh");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    assert_eq!(s.tail_seq(), Seq::new(0), "nothing appended yet");

    let committed = s
        .admit_local(&drafts(3, &clock))
        .await
        .expect("a fresh spool accepts");
    assert_eq!(committed.seq_high, Seq::new(3));
    assert_eq!(committed.envelopes.len(), 3);
    assert_eq!(
        committed.envelopes[0].provenance.seq(),
        Seq::FIRST,
        "the first local position is 1, and it is the envelope's own seq (W7 D-4)"
    );
    assert_eq!(committed.envelopes[0].provenance.origin(), node(1));
    assert_eq!(
        committed.envelopes[0].provenance.path().hops(),
        &[node(1)],
        "a locally admitted envelope has traversed one node"
    );
}

#[tokio::test]
async fn the_tail_and_the_origin_table_survive_a_restart() {
    let scratch = Scratch::new("restart");
    let clock = TestClock::fixed();
    {
        let s = spool(scratch.spool(), node(1), &clock);
        s.admit_local(&drafts(5, &clock)).await.expect("accepted");
        for seq in 1..=3 {
            assert_eq!(
                s.record_accepted(node(7), Seq::new(seq)),
                Verdict::Fresh,
                "a peer's envelope, interned and recorded in the manifest's high water"
            );
            s.append_sealed(&[forwarded(node(7), seq, "remote", clock.now(), &[node(1)])])
                .await
                .expect("accepted");
        }
    }
    let s = spool(scratch.spool(), node(1), &clock);
    assert_eq!(s.tail_seq(), Seq::new(8), "five local plus three relayed");
    assert_eq!(
        s.high_water(node(7)),
        Seq::new(3),
        "the peer's high water came back from the manifest"
    );
    assert!(
        s.is_duplicate(node(7), Seq::new(3)),
        "so a resend after the restart is still a duplicate"
    );
    let committed = s.admit_local(&drafts(1, &clock)).await.expect("accepted");
    assert_eq!(
        committed.seq_high,
        Seq::new(9),
        "and the next position continues the log rather than restarting it"
    );
}

/// The dedup window holds the sequence numbers accepted **above** the high water, and it
/// is in memory only (W7 §4.4). So the trade W7 states out loud is worth asserting rather
/// than trusting: a gap in a peer's stream pins its *persisted* high water, the window
/// covers the gap while the process lives, and a restart costs at most
/// [`DEDUP_WINDOW`](mw_pipeline::spool::DEDUP_WINDOW) deduplicated replays per origin —
/// which the writer's `ON CONFLICT … DO UPDATE` absorbs.
///
/// The alternative — treating an unrecognised old sequence number as a duplicate — would
/// lose a genuinely late envelope, and losing one is worse than storing one twice.
#[tokio::test]
async fn a_gap_in_a_peers_stream_pins_the_persisted_high_water_and_the_window_covers_it() {
    let scratch = Scratch::new("window-above-hw");
    let clock = TestClock::fixed();
    {
        let s = spool(scratch.spool(), node(1), &clock);
        // 901 arrives with nothing before it, which is what the diamond of W7 §2.3 looks
        // like from the receiver's side.
        assert_eq!(s.record_accepted(node(7), Seq::new(901)), Verdict::Fresh);
        s.append_sealed(&[forwarded(node(7), 901, "remote", clock.now(), &[node(1)])])
            .await
            .expect("accepted");
        assert_eq!(
            s.high_water(node(7)),
            Seq::new(0),
            "the high water is the contiguous prefix, and there is none"
        );
        assert!(
            s.is_duplicate(node(7), Seq::new(901)),
            "but the window remembers it, so a resend in this process is a duplicate"
        );
    }
    // The newest segment still holds the record, so §3.6 step 5 rebuilds the window from
    // it and the memory survives this particular restart.
    let s = spool(scratch.spool(), node(1), &clock);
    assert!(
        s.is_duplicate(node(7), Seq::new(901)),
        "§3.6 step 5 rebuilt the window by scanning the newest segment"
    );
    assert_eq!(s.high_water(node(7)), Seq::new(0));
}

/// A spool directory belonging to another node is refused rather than adopted: its origin
/// table indexes that node's peers, and its `seq` numbers are that node's positions.
#[tokio::test]
async fn a_spool_belonging_to_another_node_is_refused() {
    let scratch = Scratch::new("wrong-node");
    let clock = TestClock::fixed();
    drop(spool(scratch.spool(), node(1), &clock));
    let err = Spool::open(
        config(scratch.spool()),
        node(2),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect_err("node 2 must not adopt node 1's spool");
    assert!(
        matches!(err, SpoolError::WrongNode { .. }),
        "{}",
        mw_types::DisplayChain(&err)
    );
}

/// §3.6 step 1: a lost origin table reads every `hw_seq` as zero and makes every envelope
/// a peer ever sent look new, so the start is refused rather than silently starting over.
#[tokio::test]
async fn a_corrupt_manifest_refuses_the_start() {
    let scratch = Scratch::new("bad-manifest");
    let clock = TestClock::fixed();
    {
        let s = spool(scratch.spool(), node(1), &clock);
        s.admit_local(&drafts(1, &clock)).await.expect("accepted");
    }
    let manifest = scratch.spool().join("MANIFEST");
    let mut bytes = std::fs::read(&manifest).expect("readable");
    bytes[12] ^= 0xff;
    std::fs::write(&manifest, &bytes).expect("writable");

    let err = Spool::open(
        config(scratch.spool()),
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect_err("a corrupt manifest is not recoverable by truncation");
    assert!(
        matches!(
            err,
            SpoolError::Manifest { .. } | SpoolError::WrongNode { .. }
        ),
        "{}",
        mw_types::DisplayChain(&err)
    );
}

/// **The power-loss simulation, through recovery.**
///
/// Truncate the newest segment at *every* byte offset of its last record, re-open the
/// spool, and assert three things each time: the records before the torn one are all still
/// readable, the tail is the last intact record, and the cursor that had acknowledged a
/// prefix still has that prefix. "Loses nothing acknowledged" is the done-criterion, so it
/// is the assertion rather than a consequence.
///
/// The state it reconstructs is the one a `kill -9` mid-append actually leaves, and getting
/// that right is most of the test: §3.3 writes the records' `fsync` **before** the
/// manifest, so a crash that lost the eighth record also lost the manifest update that
/// mentioned it. So the manifest comes from the spool as it was after seven committed
/// records, and the segment comes from after the eighth was written — which is exactly the
/// pair the disk holds when the power goes out between the `write_all` and the `fsync`.
#[tokio::test]
async fn a_kill_minus_nine_mid_append_loses_nothing_acknowledged() {
    let clock = TestClock::fixed();
    let cursor_id = id("writer:timescale");
    let cursor_file = "writer-timescale.cur";
    // One segment for the whole test, so "the newest segment" is unambiguous.
    let big_segment = 1024 * 1024;

    // The committed state: seven records, five acknowledged.
    let reference = Scratch::new("power-loss-ref");
    let (manifest, cursor_bytes, segment_name) = {
        let s = Spool::open(
            SpoolConfig {
                segment_bytes: big_segment,
                ..config(reference.spool())
            },
            node(1),
            Arc::new(clock.clone()) as Arc<dyn Clock>,
        )
        .expect("a fresh spool");
        // The cursor is opened before the records it must not have acknowledged: a new
        // cursor starts at the current tail (§3.4), so opening it after all seven would
        // make `ack(5)` a no-op and the test would assert nothing.
        s.admit_local(&drafts(3, &clock)).await.expect("accepted");
        let mut cursor = s.open_cursor(cursor_id.clone()).expect("a free cursor");
        assert_eq!(
            cursor.acked_seq(),
            Seq::new(3),
            "a new cursor starts at the tail"
        );
        s.admit_local(&drafts(4, &clock)).await.expect("accepted");
        cursor.ack(Seq::new(5)).expect("acknowledged");
        drop(cursor);
        let files = segments(&reference.spool());
        assert_eq!(files.len(), 1, "one segment, as configured");
        (
            std::fs::read(reference.spool().join("MANIFEST")).expect("readable"),
            std::fs::read(reference.spool().join("cursors").join(cursor_file)).expect("readable"),
            files[0].file_name().expect("a file name").to_owned(),
        )
    };

    // ...and the segment after an eighth record was written but not committed.
    let torn_segment = {
        let s = Spool::open(
            SpoolConfig {
                segment_bytes: big_segment,
                ..config(reference.spool())
            },
            node(1),
            Arc::new(clock.clone()) as Arc<dyn Clock>,
        )
        .expect("the reference spool reopens");
        s.admit_local(&drafts(1, &clock)).await.expect("accepted");
        std::fs::read(&segments(&reference.spool())[0]).expect("readable")
    };

    // Where the eighth record starts, taken from the framing rather than counted by hand.
    let last_record_at = {
        let scan = scan_records(&torn_segment).expect("intact");
        assert_eq!(scan.records.len(), 8);
        scan.valid_bytes - (16 + scan.records[7].payload.len())
    };
    assert!(last_record_at > SEGMENT_HEADER_BYTES);

    for cut in last_record_at..torn_segment.len() {
        let scratch = Scratch::new("power-loss-cut");
        let dir = scratch.spool();
        std::fs::create_dir_all(dir.join("cursors")).expect("creatable");
        std::fs::write(dir.join("MANIFEST"), &manifest).expect("writable");
        std::fs::write(dir.join("cursors").join(cursor_file), &cursor_bytes).expect("writable");
        std::fs::write(dir.join(&segment_name), &torn_segment[..cut]).expect("writable");

        let s = Spool::open(
            SpoolConfig {
                segment_bytes: big_segment,
                ..config(&dir)
            },
            node(1),
            Arc::new(clock.clone()) as Arc<dyn Clock>,
        )
        .expect("a torn tail is recoverable; only a torn manifest is not");

        assert_eq!(
            s.tail_seq(),
            Seq::new(7),
            "cut at {cut}: the seven intact records are the log and the torn eighth is gone"
        );
        let mut cursor = s.open_cursor(cursor_id.clone()).expect("a free cursor");
        assert_eq!(
            cursor.acked_seq(),
            Seq::new(5),
            "cut at {cut}: nothing acknowledged was lost"
        );
        let read = cursor.read().expect("the surviving records are readable");
        assert_eq!(
            read.envelopes.len(),
            2,
            "cut at {cut}: records 6 and 7 are still there to be delivered"
        );
        assert_eq!(read.envelopes[0].0, Seq::new(6), "cut at {cut}");
        assert_eq!(read.through, Seq::new(7), "cut at {cut}");

        // And the spool is usable afterwards: the next append continues from the intact
        // tail rather than overwriting the torn bytes or leaving the file unreadable.
        let committed = s.admit_local(&drafts(1, &clock)).await.expect("accepted");
        assert_eq!(committed.seq_high, Seq::new(8), "cut at {cut}");
        let read = cursor.read().expect("readable");
        assert_eq!(
            read.envelopes.len(),
            1,
            "cut at {cut}: and it is deliverable"
        );
    }
}

/// A replayed prefix is idempotent: the dedup gate's whole point, and the property the
/// collector's at-least-once delivery rests on.
///
/// A property test over the prefix length rather than one example, because the interesting
/// cases are the boundaries — nothing replayed, everything replayed — and an example picks
/// neither.
#[tokio::test]
async fn replaying_any_prefix_of_a_peers_stream_is_idempotent() {
    const STREAM: u64 = 40;
    for prefix in 0..=STREAM {
        let scratch = Scratch::new("replay");
        let clock = TestClock::fixed();
        let s = spool(scratch.spool(), node(1), &clock);

        let envelopes: Vec<_> = (1..=STREAM)
            .map(|seq| forwarded(node(7), seq, "remote", clock.now(), &[node(1)]))
            .collect();

        // The peer's first delivery.
        let mut accepted = 0usize;
        for e in &envelopes {
            if s.record_accepted(node(7), e.provenance.seq()) != Verdict::Duplicate {
                s.append_sealed(std::slice::from_ref(e)).await.expect("ok");
                accepted += 1;
            }
        }
        assert_eq!(accepted, STREAM as usize);
        let tail_after_first = s.tail_seq();

        // Its reconnect, replaying `prefix` of what it already sent.
        let mut duplicates = 0usize;
        for e in envelopes.iter().take(prefix as usize) {
            if s.record_accepted(node(7), e.provenance.seq()) == Verdict::Duplicate {
                duplicates += 1;
            } else {
                s.append_sealed(std::slice::from_ref(e)).await.expect("ok");
            }
        }
        assert_eq!(
            duplicates, prefix as usize,
            "prefix {prefix}: every replayed envelope is recognised, not merely absorbed"
        );
        assert_eq!(
            s.tail_seq(),
            tail_after_first,
            "prefix {prefix}: and nothing was appended a second time"
        );
    }
}

/// **The ceiling policy** (W3 §2.7, W7 **D-17**): refused at the edge, with nothing
/// written.
#[tokio::test]
async fn at_the_ceiling_a_batch_is_refused_at_the_edge_and_nothing_is_written() {
    let scratch = Scratch::new("ceiling");
    let clock = TestClock::fixed();
    let s = Spool::open(
        SpoolConfig {
            // Room for a few records and no more.
            ceiling_bytes: 4 * 1024,
            segment_bytes: 4 * 1024,
            ..config(scratch.spool())
        },
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");

    // Fill it.
    let mut appended = 0u64;
    loop {
        match s.admit_local(&drafts(4, &clock)).await {
            Ok(c) => appended += c.envelopes.len() as u64,
            Err(SpoolError::Full { depth_bytes, .. }) => {
                assert!(depth_bytes > 0, "the error names what is on disk");
                break;
            }
            Err(other) => panic!("unexpected: {}", mw_types::DisplayChain(&other)),
        }
        assert!(appended < 10_000, "the ceiling must actually be reachable");
    }
    assert!(appended > 0, "some records got in before the ceiling");
    let at_ceiling = s.tail_seq();
    let depth_at_ceiling = s.depth_bytes();

    // Refused again, and still nothing written: `drop-oldest` is rejected as a policy
    // (W7 D-17), so the refusal must not have made room by discarding history.
    let err = s
        .admit_local(&drafts(4, &clock))
        .await
        .expect_err("still full");
    assert!(matches!(err, SpoolError::Full { .. }));
    assert_eq!(s.tail_seq(), at_ceiling, "the tail did not move");
    assert_eq!(
        s.depth_bytes(),
        depth_at_ceiling,
        "and neither did the depth"
    );
}

/// The other half of "drop at the edge": the spool recovers once a cursor drains it, so
/// the ceiling is a stall and not a wedge.
#[tokio::test]
async fn the_ceiling_clears_when_a_cursor_catches_up() {
    let scratch = Scratch::new("ceiling-clears");
    let clock = TestClock::fixed();
    let s = Spool::open(
        SpoolConfig {
            ceiling_bytes: 8 * 1024,
            segment_bytes: 1024,
            ..config(scratch.spool())
        },
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let mut cursor = s
        .open_cursor(id("writer:timescale"))
        .expect("a free cursor");

    while s.admit_local(&drafts(4, &clock)).await.is_ok() {
        assert!(s.tail_seq().get() < 10_000, "the ceiling is reachable");
    }
    let full_at = s.depth_bytes();

    // Drain it: the cursor acknowledges everything, segments are reclaimed, and the depth
    // falls.
    cursor.ack(s.tail_seq()).expect("acknowledged");
    assert!(
        s.depth_bytes() < full_at,
        "reclamation freed something: {} -> {}",
        full_at,
        s.depth_bytes()
    );
    s.admit_local(&drafts(1, &clock))
        .await
        .expect("and the spool accepts again");
}

#[tokio::test]
async fn segments_rotate_and_are_reclaimed_once_every_active_cursor_is_past_them() {
    let scratch = Scratch::new("reclaim");
    let clock = TestClock::fixed();
    let s = Spool::open(
        SpoolConfig {
            segment_bytes: 512,
            ..config(scratch.spool())
        },
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let mut cursor = s
        .open_cursor(id("writer:timescale"))
        .expect("a free cursor");

    for _ in 0..20 {
        s.admit_local(&drafts(2, &clock)).await.expect("accepted");
    }
    let before = segments(&scratch.spool()).len();
    assert!(before > 2, "rotation happened: {before} segments");

    cursor.ack(s.tail_seq()).expect("acknowledged");
    let after = segments(&scratch.spool()).len();
    assert!(
        after < before,
        "segments are reclaimed once acked past: {before} -> {after}"
    );
    assert!(after >= 1, "the open segment is never reclaimed");
}

/// A cursor that is behind and stays behind is abandoned, **not** left to fill the spool:
/// W7 **D-16** makes `max_lag` fire before the ceiling, which is what makes "drop at the
/// edge" the right answer rather than a coin flip.
#[tokio::test]
async fn a_cursor_behind_longer_than_max_lag_is_abandoned_and_stops_holding_segments() {
    let scratch = Scratch::new("max-lag");
    let clock = TestClock::fixed();
    let s = Spool::open(
        SpoolConfig {
            segment_bytes: 512,
            ..config(scratch.spool())
        },
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let stuck = id("uplink:suncrest");
    let cursor = s.open_cursor(stuck.clone()).expect("a free cursor");

    for _ in 0..20 {
        s.admit_local(&drafts(2, &clock)).await.expect("accepted");
    }
    let held = segments(&scratch.spool()).len();
    assert!(held > 2, "the stuck cursor is holding {held} segments");
    assert_eq!(
        s.cursor_position(&stuck).expect("registered").state,
        CursorState::Active
    );

    // Eight days: past the seven-day default.
    clock.advance(ChronoDuration::days(8));
    s.admit_local(&drafts(1, &clock)).await.expect("accepted");

    assert_eq!(
        s.cursor_position(&stuck).expect("registered").state,
        CursorState::Abandoned,
        "max_lag fired"
    );
    assert!(cursor.is_abandoned(), "and the handle says so");
    let after = segments(&scratch.spool()).len();
    assert!(
        after < held,
        "an abandoned cursor is ignored for reclamation: {held} -> {after}"
    );
}

/// W3 Decision 20: a cursor is owned by exactly one task. Two holders would mean two
/// tasks advancing one offset, which is the Go `s.Engines` defect in a new shape.
#[tokio::test]
async fn a_cursor_cannot_be_opened_twice_at_once_and_can_be_reopened_after_a_drop() {
    let scratch = Scratch::new("one-owner");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    s.admit_local(&drafts(4, &clock)).await.expect("accepted");

    let mut first = s
        .open_cursor(id("writer:timescale"))
        .expect("a free cursor");
    let err = s
        .open_cursor(id("writer:timescale"))
        .expect_err("the second holder is refused");
    assert!(matches!(err, SpoolError::CursorBusy { .. }));

    // Four more records, so there is somewhere ahead of the start position to ack to: an
    // ack only ever advances, so acking behind the start is a no-op by design.
    s.admit_local(&drafts(4, &clock)).await.expect("accepted");
    first.ack(Seq::new(6)).expect("acknowledged");
    drop(first);

    // A component in `Backoff` gets a new handle on its next start attempt and must resume
    // where it was, because the cursor not advancing is what makes the backoff a retry of
    // the same batch rather than a loss.
    let second = s
        .open_cursor(id("writer:timescale"))
        .expect("the cursor is free again after a drop");
    assert_eq!(
        second.acked_seq(),
        Seq::new(6),
        "and it resumed its position"
    );
}

/// A brand-new cursor starts at the tail, not at the oldest retained record: a peer
/// configured today has no claim on history it was never promised.
#[tokio::test]
async fn a_new_cursor_starts_at_the_tail_and_not_at_the_oldest_record() {
    let scratch = Scratch::new("new-cursor");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    s.admit_local(&drafts(10, &clock)).await.expect("accepted");

    let mut cursor = s
        .open_cursor(id("uplink:relay-west"))
        .expect("a free cursor");
    assert_eq!(cursor.acked_seq(), Seq::new(10));
    assert!(
        cursor.read().expect("readable").envelopes.is_empty(),
        "there is nothing after the tail yet"
    );

    s.admit_local(&drafts(2, &clock)).await.expect("accepted");
    let read = cursor.read().expect("readable");
    assert_eq!(read.envelopes.len(), 2, "and it sees what arrives after it");
    assert_eq!(read.envelopes[0].0, Seq::new(11));
}

/// The two-slot cursor file, as the only thing it has to do: a torn slot costs a replay
/// and never a loss.
#[tokio::test]
async fn a_torn_cursor_slot_costs_a_replay_and_not_a_loss() {
    let scratch = Scratch::new("torn-cursor");
    let clock = TestClock::fixed();
    let cursor_id = id("writer:timescale");
    {
        let s = spool(scratch.spool(), node(1), &clock);
        let mut c = s.open_cursor(cursor_id.clone()).expect("a free cursor");
        s.admit_local(&drafts(20, &clock)).await.expect("accepted");
        // Two acks, so both slots hold a real position and the generations differ by one.
        c.ack(Seq::new(14)).expect("acknowledged");
        c.ack(Seq::new(18)).expect("acknowledged");
    }
    // Tear the live slot. The other one holds the position before it.
    let path = scratch.spool().join("cursors").join("writer-timescale.cur");
    let mut bytes = std::fs::read(&path).expect("readable");
    // Generations alternate slots, so the newer of the two is whichever has the higher
    // generation; tear both candidates' payloads one at a time and assert the surviving
    // position is a real earlier one.
    let live = if u64::from_le_bytes(bytes[0..8].try_into().unwrap())
        > u64::from_le_bytes(bytes[64..72].try_into().unwrap())
    {
        0
    } else {
        64
    };
    bytes[live + 9] ^= 0xff;
    std::fs::write(&path, &bytes).expect("writable");

    let s = spool(scratch.spool(), node(1), &clock);
    let cursor = s.open_cursor(cursor_id).expect("a free cursor");
    assert_eq!(
        cursor.acked_seq(),
        Seq::new(14),
        "the intact slot is the earlier position, which costs a replay of 15..=18"
    );
}
