//! `Ingress::admit` and `Ingress::accept`, including **D-60**'s whole point: that running
//! `admit` on a forwarded envelope would destroy the identity the ack and the dedup both
//! depend on, so there are two entry points and the second one never re-seals.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mw_component::{
    AcceptError, AdmitError, FrameMeta, IdentitySource, Ingress, Reject, StationRegistry,
};
use mw_pipeline::testing::{forwarded, frame, id, node, partial, reading, spool, Scratch};
use mw_pipeline::{Clock, SpoolIngress, TestClock, MAX_BATCH_ENVELOPES};
use mw_types::{Envelope, PartialReading, Seq, StationKey};

/// A registry that answers `Fixed` frames and counts how often it was asked.
///
/// The count is what makes "identity resolution happens once per frame, not once per
/// reading" assertable, which is the first line of W3 §2.6's order.
#[derive(Debug, Default)]
struct CountingRegistry {
    station: Option<StationKey>,
    refuse: Option<Reject>,
    calls: AtomicUsize,
}

impl CountingRegistry {
    fn resolving(station: &str) -> Arc<Self> {
        Arc::new(Self {
            station: Some(StationKey::new(station)),
            ..Self::default()
        })
    }

    fn refusing(reason: Reject) -> Arc<Self> {
        Arc::new(Self {
            refuse: Some(reason),
            ..Self::default()
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl StationRegistry for CountingRegistry {
    fn resolve(&self, _meta: &FrameMeta) -> Result<StationKey, Reject> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(reason) = self.refuse {
            return Err(reason);
        }
        self.station.clone().ok_or(Reject::UnknownStation)
    }
}

#[tokio::test]
async fn admit_seals_every_reading_in_a_frame_against_one_identity_resolution() {
    let scratch = Scratch::new("admit");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let registry = CountingRegistry::resolving("backyard");
    let ingress = SpoolIngress::new(Arc::clone(&s), Arc::clone(&registry) as Arc<_>);

    let batch: Vec<PartialReading> = (0..4)
        .map(|i| partial(clock.now(), 29.0 + i as f32))
        .collect();
    let admitted = ingress
        .admit(&batch, &frame("station:davis", clock.now()))
        .await
        .expect("a resolvable frame");

    assert_eq!(admitted.accepted, 4);
    assert_eq!(admitted.rejected, 0);
    assert_eq!(admitted.seq_high, Seq::new(4));
    assert_eq!(
        registry.calls(),
        1,
        "identity is a property of the FRAME, so one resolution covers its four readings"
    );

    // **The append is one call and one fsync** (W7 D-13's "counts calls, not records").
    // This is the observable content of W3 D-87: the only I/O `admit` did was the spool
    // append, and the append was one group commit.
    let stats = s.stats();
    assert_eq!(stats.appends, 1, "one admit is one append call");
    assert_eq!(stats.fsyncs, 1, "and one fsync, for all four readings");
}

/// W2a §4a.6: there is no `Envelope` without a `StationKey`, and the only producer of one
/// is `resolve`. So a refused frame leaves **nothing** behind — the Go gRPC receiver
/// stores the reading at `station.go:174` and consults the registry at `:194`.
#[tokio::test]
async fn a_refused_frame_leaves_nothing_in_the_spool() {
    let scratch = Scratch::new("refused");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let registry = CountingRegistry::refusing(Reject::BadSecret);
    let ingress = SpoolIngress::new(Arc::clone(&s), registry as Arc<_>);

    let err = ingress
        .admit(
            &[partial(clock.now(), 29.92)],
            &FrameMeta::presenting(
                id("station:ambient"),
                IdentitySource::FromQuery,
                "backyard",
                clock.now(),
            ),
        )
        .await
        .expect_err("a bad secret is refused");

    assert!(
        matches!(err, AdmitError::Rejected(Reject::BadSecret)),
        "{err}"
    );
    assert_eq!(s.tail_seq(), Seq::new(0), "nothing was appended");
    assert_eq!(s.stats().appends, 0, "and no append was even attempted");
    assert_eq!(
        mw_types::Classify::class(&err),
        mw_types::Class::Ignorable,
        "a rejected reading is a signal, not a failure of the component that received it"
    );
}

/// W7 §4.6's last check: an all-`None` reading would store as an all-`NULL` row the
/// aggregates still have to process. The rest of the batch still commits, because one
/// empty frame among four is not a reason to lose the other three.
#[tokio::test]
async fn an_empty_reading_is_rejected_per_reading_and_the_batch_still_commits() {
    let scratch = Scratch::new("empty");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    let batch = vec![
        partial(clock.now(), 29.92),
        PartialReading::new(mw_types::Reading::default()),
        partial(clock.now(), 29.93),
    ];
    let admitted = ingress
        .admit(&batch, &frame("station:davis", clock.now()))
        .await
        .expect("the frame resolved");
    assert_eq!(admitted.accepted, 2);
    assert_eq!(admitted.rejected, 1);
    assert_eq!(s.tail_seq(), Seq::new(2));
}

/// A frame with no station-reported timestamp takes the frame's arrival time, in exactly
/// one place. The codec cannot read a clock (`ci/layers.toml` forbids it), so the
/// substitution has to happen here or nowhere.
#[tokio::test]
async fn a_reading_with_no_timestamp_takes_the_frames_arrival_time() {
    let scratch = Scratch::new("no-timestamp");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    let arrived = clock.now();
    let admitted = ingress
        .admit(
            &[PartialReading::new(reading(29.92))],
            &frame("station:davis", arrived),
        )
        .await
        .expect("resolved");
    assert_eq!(admitted.accepted, 1);

    // Read the record back out of the segment, so what is asserted is what was stored
    // rather than what was passed around in memory.
    let stored = s.read_after(Seq::new(0), 10).expect("readable");
    let envelope = &stored.envelopes[0].1;
    assert_eq!(envelope.observed_at, arrived);
    assert_eq!(envelope.received_at, arrived);
    assert_eq!(
        envelope.reading.time, arrived,
        "and the stored column agrees with the envelope"
    );
    assert_eq!(envelope.reading.station_id, "backyard");
}

/// **D-60.** `accept` preserves `origin`, `seq` and `path` exactly. Running `admit`'s
/// `seq` mint on a forwarded envelope would renumber it in this node's coordinate space
/// and destroy the `(origin, seq)` identity the cumulative ack and the dedup gate both
/// depend on.
#[tokio::test]
async fn accept_preserves_the_provenance_a_peer_sealed() {
    let scratch = Scratch::new("accept");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    // Four local readings first, so this node's own positions are 1..4 and a reassignment
    // would be visible as 5..6 rather than as 4711..4712.
    ingress
        .admit(
            &(0..4)
                .map(|i| partial(clock.now(), 29.0 + i as f32))
                .collect::<Vec<_>>(),
            &frame("station:davis", clock.now()),
        )
        .await
        .expect("resolved");

    let batch: Vec<Envelope> = [4711u64, 4712]
        .into_iter()
        .map(|seq| forwarded(node(7), seq, "remote", clock.now(), &[node(1)]))
        .collect();
    let accepted = ingress
        .accept(&batch, node(7))
        .await
        .expect("a well-formed batch from a known peer");
    assert_eq!(accepted.accepted, 2);
    assert_eq!(accepted.duplicates, 0);
    assert_eq!(accepted.rejected, 0);

    // The local positions advanced -- the records occupy 5 and 6 of this node's log -- and
    // the envelopes' own provenance did not change at all.
    assert_eq!(s.tail_seq(), Seq::new(6));
    let read = s.read_after(Seq::new(4), 10).expect("readable");
    assert_eq!(read.envelopes.len(), 2);
    for (expected_seq, (local, envelope)) in [4711u64, 4712].into_iter().zip(read.envelopes) {
        assert_eq!(
            envelope.provenance.origin(),
            node(7),
            "origin is never rewritten"
        );
        assert_eq!(
            envelope.provenance.seq(),
            Seq::new(expected_seq),
            "and seq is never renumbered (D-60)"
        );
        assert_ne!(
            local,
            envelope.provenance.seq(),
            "the record's local position and the origin's seq are different numbers for a \
             relayed envelope, which is exactly why the ack is in the sender's space"
        );
    }
}

/// The dedup gate in front of the spool (W7 **D-27**): a duplicate never reaches the
/// append, so it never reaches the fan-out, the downstream peers or the live stream.
#[tokio::test]
async fn accept_deduplicates_a_resend_before_it_reaches_the_spool() {
    let scratch = Scratch::new("accept-dedup");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    let batch: Vec<Envelope> = (1..=3)
        .map(|seq| forwarded(node(7), seq, "remote", clock.now(), &[node(1)]))
        .collect();
    assert_eq!(
        ingress.accept(&batch, node(7)).await.expect("ok").accepted,
        3
    );
    let tail = s.tail_seq();

    let again = ingress
        .accept(&batch, node(7))
        .await
        .expect("a resend is a successful accept, not an error");
    assert_eq!(again.accepted, 0);
    assert_eq!(again.duplicates, 3);
    assert_eq!(s.tail_seq(), tail, "and nothing was appended a second time");
}

/// The three positional clauses of W7 **D-7**, through `accept` rather than through the
/// function: an envelope that fails one is dropped and counted, and **the batch still
/// succeeds** — a batch retried forever over one malformed member is a satellite that
/// never drains (W7 **D-8**).
#[tokio::test]
async fn a_malformed_path_is_dropped_and_counted_with_the_batch_still_accepted() {
    let scratch = Scratch::new("accept-path");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    let batch = vec![
        // Good: A admitted it, A forwarded it here.
        forwarded(node(7), 1, "remote", clock.now(), &[node(1)]),
        // Addressed to node 9, not to us.
        forwarded(node(7), 2, "remote", clock.now(), &[node(9)]),
        // Addressed here, but the hop before us is node 9 and our peer on this stream is
        // node 7 -- so the path does not record the route it took.
        forwarded(node(7), 3, "remote", clock.now(), &[node(9), node(1)]),
    ];
    let accepted = ingress.accept(&batch, node(7)).await.expect("ok");
    assert_eq!(accepted.accepted, 1);
    assert_eq!(accepted.rejected, 2);
    assert_eq!(s.tail_seq(), Seq::new(1), "only the well-formed one landed");
}

/// A relayed envelope is checked against the **sender**, not the origin: `B.forward(C)`
/// appended C, so `path[len - 2]` is B.
#[tokio::test]
async fn a_relayed_envelope_is_accepted_from_the_node_that_forwarded_it() {
    let scratch = Scratch::new("accept-relay");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(3), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    // A -> B -> C(=us). A admitted it, B forwarded to C.
    let envelope = forwarded(node(1), 500, "remote", clock.now(), &[node(2), node(3)]);
    assert_eq!(
        ingress
            .accept(std::slice::from_ref(&envelope), node(2))
            .await
            .expect("ok")
            .accepted,
        1,
        "B is the sender"
    );
    let accepted = ingress
        .accept(
            &[forwarded(
                node(1),
                501,
                "remote",
                clock.now(),
                &[node(2), node(3)],
            )],
            node(1),
        )
        .await
        .expect("ok");
    assert_eq!(
        accepted.rejected, 1,
        "A is the origin, not the sender; the stream this arrived on is B's"
    );
}

#[tokio::test]
async fn a_batch_over_the_envelope_limit_is_refused_whole() {
    let scratch = Scratch::new("accept-big");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    let batch: Vec<Envelope> = (1..=(MAX_BATCH_ENVELOPES as u64 + 1))
        .map(|seq| forwarded(node(7), seq, "remote", clock.now(), &[node(1)]))
        .collect();
    let err = ingress
        .accept(&batch, node(7))
        .await
        .expect_err("over the limit");
    assert!(
        matches!(err, AcceptError::Rejected(Reject::BatchTooLarge)),
        "{err}"
    );
    assert_eq!(
        s.tail_seq(),
        Seq::new(0),
        "trimming it would make the ack a claim about envelopes we never saw"
    );
}

/// A 256-envelope `accept` is one append call and one `fsync` — W7 **D-13** in the shape
/// that matters most, because this is the replay burst the cap exists for.
#[tokio::test]
async fn a_full_batch_is_one_append_call_and_one_fsync() {
    let scratch = Scratch::new("accept-one-fsync");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    let batch: Vec<Envelope> = (1..=MAX_BATCH_ENVELOPES as u64)
        .map(|seq| forwarded(node(7), seq, "remote", clock.now(), &[node(1)]))
        .collect();
    assert_eq!(
        ingress.accept(&batch, node(7)).await.expect("ok").accepted,
        MAX_BATCH_ENVELOPES as u32
    );
    assert_eq!(
        s.stats(),
        mw_pipeline::SpoolStats {
            appends: 1,
            fsyncs: 1
        },
        "256 records, one append call, one disk sync"
    );
}
