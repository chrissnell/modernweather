//! `Ingress::admit` and `Ingress::accept`, including **D-60**'s whole point: that running
//! `admit` on a forwarded envelope would destroy the identity the ack and the dedup both
//! depend on, so there are two entry points and the second one never re-seals.
//!
//! Not run under `--cfg loom`. Every test here drives a real `tokio` runtime, and under
//! that flag tokio's time/IO driver refuses to be enabled (`assertion failed: !enabled`),
//! so the whole file panics rather than modelling anything. The `loom` job builds
//! `mw-pipeline` for the scope guard's sake; what it has to run is
//! `mw-supervisor`'s `loom_publish.rs`, and `crates/mw-supervisor/tests/` carries this
//! same `cfg` for this same reason.

#![cfg(not(loom))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mw_component::{
    AcceptError, AdmitError, FrameMeta, IdentitySource, Ingress, Reject, StationMeta,
    StationRegistry,
};
use mw_pipeline::testing::{forwarded, frame, id, node, partial, reading, spool, Scratch};
use mw_pipeline::{Clock, Spool, SpoolIngress, TestClock, MAX_BATCH_ENVELOPES};
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
    /// How many times the enrich stage asked for station metadata. Asserted rather than
    /// ignored: W3 **D-87** allows no I/O on this path, and "one lookup per batch, not one
    /// per reading" is the property that keeps a four-observation frame from four reads
    /// of a published snapshot.
    metadata_calls: AtomicUsize,
    /// What `metadata` answers. `EMPTY` by default, which is a station with no configured
    /// coordinates -- the shape every test but the enrich one wants.
    meta: StationMeta,
}

impl CountingRegistry {
    fn resolving(station: &str) -> Arc<Self> {
        Arc::new(Self {
            station: Some(StationKey::new(station)),
            ..Self::default()
        })
    }

    /// A registry that resolves to `station` and answers `meta` for it.
    fn with_metadata(station: &str, meta: StationMeta) -> Arc<Self> {
        Arc::new(Self {
            station: Some(StationKey::new(station)),
            meta,
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

    fn metadata_calls(&self) -> usize {
        self.metadata_calls.load(Ordering::Relaxed)
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

    fn metadata(&self, _station: &StationKey) -> StationMeta {
        self.metadata_calls.fetch_add(1, Ordering::Relaxed);
        self.meta
    }

    fn projection(&self, _station: &StationKey) -> Option<mw_component::StationProjection> {
        None
    }

    /// Only the one station this double was built around, so that W7 §4.4 step 5's
    /// refusal is reachable from here and not papered over.
    fn station_owner(&self, station: &StationKey) -> Option<mw_types::OriginId> {
        (self.station.as_ref() == Some(station)).then(|| mw_types::OriginId::new([1; 16]))
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
    assert_eq!(
        stats.commits, 1,
        "and one group commit, for all four readings"
    );
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
        // A reading whose only content is a text metric. This used to be dropped as
        // "empty": `Reading::get` is `None` for the eleven text metrics, and the
        // hand-written list of exceptions named `tz` and four timestamps but none of
        // `extratext1..10`.
        PartialReading::new(mw_types::Reading {
            extra_text1: Some("a note from the station".into()),
            ..mw_types::Reading::default()
        }),
    ];
    let admitted = ingress
        .admit(&batch, &frame("station:davis", clock.now()))
        .await
        .expect("the frame resolved");
    assert_eq!(
        admitted.accepted, 3,
        "two numeric readings and the text-only one"
    );
    assert_eq!(admitted.rejected, 1, "only the genuinely empty one");
    assert_eq!(s.tail_seq(), Seq::new(3));
    assert_eq!(
        ingress.stats().empty_readings,
        1,
        "and the rejection is counted by reason"
    );
}

/// **The ceiling policy, counted** — the half of the plan's "counted and alarmed" that is
/// assertable without a process-global metrics recorder.
///
/// `mw_readings_dropped_total` is defined by §5.10 as a **reading** count, so a refused
/// 256-envelope batch must move it by 256 and not by 1.
#[tokio::test]
async fn at_the_ceiling_every_lost_reading_is_counted() {
    let scratch = Scratch::new("ceiling-counted");
    let clock = TestClock::fixed();
    let s = Spool::open(
        mw_pipeline::SpoolConfig {
            ceiling_bytes: 4 * 1024,
            segment_bytes: 4 * 1024,
            ..mw_pipeline::testing::config(scratch.spool())
        },
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    // Fill it, then offer a batch of four and have it refused whole.
    let four: Vec<PartialReading> = (0..4)
        .map(|i| partial(clock.now(), 29.0 + i as f32))
        .collect();
    loop {
        match ingress
            .admit(&four, &frame("station:davis", clock.now()))
            .await
        {
            Ok(_) => assert!(s.tail_seq().get() < 10_000, "the ceiling is reachable"),
            Err(AdmitError::SpoolFull { depth_bytes }) => {
                assert!(depth_bytes > 0, "the error names what is on disk");
                break;
            }
            Err(other) => panic!("unexpected: {other}"),
        }
    }

    assert_eq!(
        ingress.stats().dropped_spool_full,
        4,
        "four readings were in the refused batch, so four were lost -- not one 'batch'"
    );
    let before = s.tail_seq();
    ingress
        .admit(&four, &frame("station:davis", clock.now()))
        .await
        .expect_err("still full");
    assert_eq!(
        ingress.stats().dropped_spool_full,
        8,
        "and the second refusal adds its four"
    );
    assert_eq!(s.tail_seq(), before, "with nothing written either time");
}

/// §4.1 and §4.5 bound a batch by encoded size as well as by count, and §4.5 owes a `413`
/// on it. `MAX_BATCH_BYTES` was defined and exported and never read, which reads as a
/// discharged requirement.
#[tokio::test]
async fn a_batch_over_the_encoded_size_limit_is_refused_whole() {
    let scratch = Scratch::new("accept-bytes");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );

    // Well inside the 256-envelope limit, well outside the 1 MiB one: a long station key
    // is the cheapest way for a peer to make a big envelope.
    let fat = "s".repeat(8_000);
    let batch: Vec<Envelope> = (1..=200)
        .map(|seq| forwarded(node(7), seq, &fat, clock.now(), &[node(1)]))
        .collect();
    assert!(batch.len() <= mw_pipeline::MAX_BATCH_ENVELOPES);

    let err = ingress
        .accept(&batch, node(7))
        .await
        .expect_err("over the encoded-size limit");
    assert!(
        matches!(err, AcceptError::Rejected(Reject::BatchTooLarge)),
        "{err}"
    );
    assert_eq!(
        s.tail_seq(),
        Seq::new(0),
        "trimming it would make the ack a claim about envelopes never seen"
    );
}

/// **Regression: the envelope's instants and the reading's timestamp must agree after a
/// spool round trip.**
///
/// `observed_at`/`received_at` are `int64` milliseconds on the wire while `reading.time` is
/// a nanosecond `Timestamp`, so a reading sealed from a nanosecond clock came back with the
/// three disagreeing — on every envelope in production, because **D-11** makes the spool
/// record that same message. The whole suite was blind to it because the test clock sat on
/// a whole second.
#[tokio::test]
async fn the_envelopes_instants_survive_the_spool_on_a_nanosecond_clock() {
    let scratch = Scratch::new("nanos");
    let clock = TestClock::new(
        chrono::DateTime::from_timestamp(1_762_171_200, 123_456_789).expect("a real instant"),
    );
    assert_ne!(clock.now().timestamp_subsec_nanos() % 1_000_000, 0);

    let s = spool(scratch.spool(), node(1), &clock);
    let ingress = SpoolIngress::new(
        Arc::clone(&s),
        CountingRegistry::resolving("backyard") as Arc<_>,
    );
    ingress
        .admit(
            &[PartialReading::new(reading(29.92))],
            &frame("station:davis", clock.now()),
        )
        .await
        .expect("resolved");

    let stored = s.read_after(Seq::new(0), 10).expect("readable");
    let e = &stored.envelopes[0].1;
    assert_eq!(
        e.observed_at, e.reading.time,
        "the envelope and the stored column agree about when it was measured"
    );
    assert_eq!(e.received_at, e.observed_at);
    assert_eq!(
        e.observed_at.timestamp_subsec_nanos() % 1_000_000,
        0,
        "truncated to the millisecond the wire can carry, rather than silently rounded \
         in one of the three places"
    );
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
            commits: 1
        },
        "256 records, one append call, one group commit"
    );
}

/// **The enrich stage is on `admit`'s path, before the spool** (W3 §2.5 consequence 2):
/// storage, the snapshot and every peer see the same enriched reading, so what is in the
/// spool is what is enriched.
///
/// Also the one performance property the stage has to hold: the station metadata is read
/// **once per batch**, not once per reading. W3 **D-87** allows nothing on this path but
/// the spool's `fsync`, and a four-observation frame doing four snapshot reads would be
/// four pointer loads rather than one — cheap, but it is the kind of per-reading work the
/// Go collector's `LoadConfig()` call is the extreme of, so it is asserted rather than
/// assumed.
#[tokio::test]
async fn admit_enriches_before_the_spool_and_reads_the_station_metadata_once_per_batch() {
    let scratch = Scratch::new("enrich-admit");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let registry = CountingRegistry::with_metadata(
        "backyard",
        StationMeta {
            latitude: Some(39.7392),
            longitude: Some(-104.9903),
            altitude_m: Some(1609.0),
            snow_base_distance_mm: Some(1500.0),
        },
    );
    let ingress = SpoolIngress::new(Arc::clone(&s), Arc::clone(&registry) as Arc<_>);
    // Opened **before** the append: a new cursor starts at the tail and not at the oldest
    // record, which is the spool's own documented behaviour (`a_new_cursor_starts_at_the_
    // tail_and_not_at_the_oldest_record`).
    let mut cursor = s
        .open_cursor(id("test:reader"))
        .expect("a fresh cursor on an unused id");

    // A winter morning in Denver: cold enough and windy enough for wind chill, and the
    // coordinates make the clear-sky figure computable.
    let at = clock.now();
    let batch: Vec<PartialReading> = (0..4)
        .map(|_| {
            PartialReading::at(
                at,
                mw_types::Reading {
                    out_temp: Some(20.0),
                    out_humidity: Some(60.0),
                    wind_speed: Some(15.0),
                    pm25: Some(30.0),
                    snow_distance: Some(1200.0),
                    ..mw_types::Reading::default()
                },
            )
        })
        .collect();
    let admitted = ingress
        .admit(&batch, &frame("station:davis", at))
        .await
        .expect("a resolvable frame");
    assert_eq!(admitted.accepted, 4);
    assert_eq!(
        registry.metadata_calls(),
        1,
        "one metadata read for the whole batch"
    );

    // Read it back out of the spool rather than off the return value: the claim is about
    // what is *durable*, which is the half that matters for "every peer sees the same
    // enriched reading".
    let read = cursor.read().expect("the records are readable");
    assert_eq!(read.envelopes.len(), 4);
    for (_, envelope) in &read.envelopes {
        let r = &envelope.reading;
        assert_eq!(
            r.wind_chill,
            mw_pipeline::enrich::wind_chill(20.0, 15.0),
            "windchill is stored, not recomputed at read time"
        );
        assert_eq!(r.heat_index, None, "20 degF is below the heat-index range");
        assert_eq!(r.aqi_pm25_aqin, mw_pipeline::enrich::aqi_pm25(30.0));
        assert_eq!(r.snow_depth, Some(300.0), "1500 mm base less 1200 mm");
        assert!(
            r.potential_solar_watts.is_some(),
            "the station has coordinates, so the clear-sky figure is computable"
        );
    }
}

/// A station with no configured coordinates gets the columns that need none and `NULL` for
/// the three that do — **not** a zero. That is **D-26** one level up from a sensor, and it
/// is the shape the `accept` path is in until M6-4 resolves a peer's station key.
#[tokio::test]
async fn an_unplaced_station_stores_null_for_the_metadata_derived_columns() {
    let scratch = Scratch::new("enrich-unplaced");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(1), &clock);
    let registry = CountingRegistry::resolving("backyard");
    let ingress = SpoolIngress::new(Arc::clone(&s), Arc::clone(&registry) as Arc<_>);
    let mut cursor = s
        .open_cursor(id("test:reader"))
        .expect("a fresh cursor on an unused id");

    let at = clock.now();
    let batch = vec![PartialReading::at(
        at,
        mw_types::Reading {
            out_temp: Some(20.0),
            out_humidity: Some(60.0),
            wind_speed: Some(15.0),
            snow_distance: Some(1200.0),
            ..mw_types::Reading::default()
        },
    )];
    ingress
        .admit(&batch, &frame("station:davis", at))
        .await
        .expect("a resolvable frame");

    let read = cursor.read().expect("readable");
    let r = &read.envelopes[0].1.reading;
    assert!(r.wind_chill.is_some(), "wind chill needs no metadata");
    assert_eq!(r.potential_solar_watts, None, "no coordinates, no figure");
    assert_eq!(r.snow_depth, None, "no base distance, no depth");
}

/// **`accept` enriches too** (W7 §4.4 step 8), and only where a column is `None`.
///
/// Two envelopes from the same peer: one the satellite already enriched, one it did not.
/// The first must come back byte-identical — that is what makes a collector agree with its
/// satellite rather than recompute from a different formula version — and the second must
/// come back filled, which is what covers a satellite older than this collector.
#[tokio::test]
async fn accept_fills_a_column_a_satellite_left_empty_and_never_rewrites_one_it_filled() {
    let scratch = Scratch::new("enrich-accept");
    let clock = TestClock::fixed();
    let s = spool(scratch.spool(), node(3), &clock);
    let registry = CountingRegistry::resolving("backyard");
    let ingress = SpoolIngress::new(Arc::clone(&s), Arc::clone(&registry) as Arc<_>);
    let mut cursor = s
        .open_cursor(id("test:reader"))
        .expect("a fresh cursor on an unused id");

    let (a, c) = (node(1), node(3));
    let at = clock.now();
    let unenriched = peer_envelope(a, 1, at, c, None);
    // A deliberately wrong value, so that "it was not recomputed" is distinguishable from
    // "it was recomputed and happened to agree".
    let already = peer_envelope(a, 2, at + chrono::Duration::seconds(1), c, Some(-99.0));

    let accepted = ingress
        .accept(&[unenriched, already], a)
        .await
        .expect("a well-formed batch");
    assert_eq!(accepted.accepted, 2);

    let read = cursor.read().expect("readable");
    assert_eq!(read.envelopes.len(), 2);
    assert_eq!(
        read.envelopes[0].1.reading.wind_chill,
        mw_pipeline::enrich::wind_chill(20.0, 15.0),
        "the collector filled the column the satellite left empty"
    );
    assert_eq!(
        read.envelopes[1].1.reading.wind_chill,
        Some(-99.0),
        "and left the one it had filled alone, wrong or not"
    );
    // The provenance is untouched by any of it (**D-60**).
    for (_, envelope) in &read.envelopes {
        assert_eq!(envelope.provenance.origin(), a);
        assert_eq!(envelope.provenance.path().hops(), &[a, c]);
    }
}

/// An envelope as a satellite would send it: admitted at `origin`, forwarded to `me`, and
/// carrying the inputs wind chill is a function of.
fn peer_envelope(
    origin: mw_types::OriginId,
    seq: u64,
    at: chrono::DateTime<chrono::Utc>,
    me: mw_types::OriginId,
    wind_chill: Option<f32>,
) -> Envelope {
    let base = forwarded(origin, seq, "backyard", at, &[me]);
    Envelope {
        reading: Arc::new(mw_types::Reading {
            time: at,
            station_id: "backyard".into(),
            out_temp: Some(20.0),
            wind_speed: Some(15.0),
            wind_chill,
            ..mw_types::Reading::default()
        }),
        ..base
    }
}

/// The `Arc` property `accept` depends on, asserted at the boundary that has the `Arc`.
///
/// `enrich::derive` returning an empty `Derived` is what keeps `Arc::make_mut` off the hot
/// path, and "off the hot path" means the allocation is never un-shared. A refcount and a
/// pointer are the only things that can say so, and neither is visible from inside the
/// enrich stage.
#[test]
fn an_already_enriched_envelope_is_not_deep_copied_on_the_accept_path() {
    use std::sync::Arc;

    let meta = StationMeta {
        latitude: Some(39.7392),
        longitude: Some(-104.9903),
        altitude_m: Some(1609.0),
        snow_base_distance_mm: Some(1500.0),
    };
    let at = chrono::Utc::now();
    let mut reading = mw_types::Reading {
        time: at,
        station_id: "backyard".into(),
        out_temp: Some(20.0),
        out_humidity: Some(60.0),
        wind_speed: Some(15.0),
        pm25: Some(30.0),
        pm10_in_aqin: Some(80.0),
        snow_distance: Some(1200.0),
        ..mw_types::Reading::default()
    };
    // Enriched the way a satellite running this build would have left it.
    assert!(mw_pipeline::enrich::enrich(&mut reading, &meta));
    let enriched = Arc::new(reading);

    // Two holders, exactly as `accept` has after cloning the envelope out of the batch.
    let mut mine = Arc::clone(&enriched);
    assert_eq!(Arc::strong_count(&enriched), 2);
    assert!(
        mw_pipeline::enrich::derive(&mine, &meta).is_empty(),
        "there is nothing left for the collector to add"
    );
    assert!(
        Arc::ptr_eq(&mine, &enriched),
        "no deep copy happened: this is the whole of the claim"
    );

    // The converse, so the test cannot pass by never exercising the copy: clear a column
    // the collector *can* fill, and the same path does un-share.
    Arc::make_mut(&mut mine).wind_chill = None;
    assert!(!mw_pipeline::enrich::derive(&mine, &meta).is_empty());
    assert!(!Arc::ptr_eq(&mine, &enriched));
}
