//! The two concurrency tests W3 **D-85** specifies, and the jobs that exist to run them.
//!
//! Both the `race` (ThreadSanitizer) and `loom` jobs have been green since W8 landed while
//! finding nothing to run. That is the `docs/reconciliation.md` G-1 shape — a job that
//! reports green while doing nothing — and these are what make them mean something. The
//! `loom` half is in `loom_publish.rs`, because loom needs its own `cfg`.
//!
//! **1. Configuration reload racing live readers (D-28).** The measured failure is a
//! concurrent map read and write that kills the Go process when a website is saved while
//! serving traffic: `ReloadWebsiteConfig` replaces five unsynchronised fields wholesale
//! while in-flight requests read them. What replaces it is one immutable snapshot behind
//! one atomic store, and **the assertion is the absence of a torn read**, not the absence
//! of a crash — `ArcSwap` would not crash even if the design were wrong.
//!
//! **2. Shutdown with a stalled sink (D-29).** The measured failure is an unguarded
//! blocking send per engine into a 20-deep channel, so a stalled TimescaleDB
//! back-pressures the serial read loops and can hang shutdown forever. Run under
//! `tokio::time::pause()`, so it is deterministic and finishes in milliseconds rather than
//! sleeping through a 10-second grace. M3-2 re-points this test at a real spool; what is
//! assertable here is the half the supervisor owns — a stalled sink blocks neither the
//! source nor its siblings, and shutdown completes anyway.
//!
//! The test double's per-subscriber queue is **bounded** at the same depth as the Go
//! channel the defect is in, and the fan-out offers with `try_send`. That is what makes
//! "the source is never blocked" falsifiable here: the stalled sink's queue really does
//! fill, and the assertion is that the offer is refused and counted rather than waited on.

#![cfg(not(loom))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use mw_component::{
    Builder, ComponentSpec, ComponentStatus, Counters, Generation, ReadingBatch, ReadingFilter,
    ReadingPolicy, ScheduleState, State, Trigger,
};
use mw_supervisor::plan::{validate, PlanContext};
use mw_supervisor::publish::{Publication, StateTable};
use mw_supervisor::testing::{
    id, roots_with_stream, ChannelStream, CountingOnReading, StalledOnReading, SUBSCRIBER_DEPTH,
};
use mw_supervisor::Supervisor;
use mw_types::{Envelope, OriginId, Provenance, Reading, Seq, StationKey};
use tokio_util::sync::CancellationToken;

// --- 1. configuration reload racing 64 readers ------------------------------------

/// How many readers. W3 §8.4 says 64, which is well past the number of cores on any
/// release target and therefore past the point where adding more changes what is
/// observable.
const READERS: usize = 64;

/// How many commits the writer makes. 10,000 per W3 §8.4.
const COMMITS: u64 = 10_000;

fn status(id_text: &str, applied: u64) -> ComponentStatus {
    ComponentStatus {
        id: id(id_text),
        trigger: "every",
        state: State::Running { degraded: vec![] },
        since: Utc::now(),
        applied_generation: Generation::new(applied),
        schedule: ScheduleState::default(),
        counters: Counters::default(),
        last_error: None,
    }
}

/// One commit: the generation, and rows that have all applied exactly it.
///
/// `StateTable::is_coherent` is the sentinel, and the equality is the point: the
/// production predicate `is_fully_applied` is `>=`, which cannot see a reader that
/// observed a stale `generation` beside fresh rows — one of the two directions a split
/// publication tears in. A sentinel that is blind to half the failure is worth less than
/// the confidence it produces.
fn commit(generation: u64) -> Arc<StateTable> {
    Arc::new(StateTable {
        generation: Generation::new(generation),
        components: vec![
            status("db:postgres", generation),
            status("wu:backyard", generation),
            status("aprs:backyard", generation),
            status("web:site:1", generation),
        ],
    })
}

#[test]
fn a_configuration_reload_racing_sixty_four_readers_never_tears() {
    let published = Arc::new(Publication::new(commit(0)));
    let stop = Arc::new(AtomicBool::new(false));

    // Scoped threads rather than `std::thread::spawn`: the handles are joined when the
    // scope ends, so the test cannot leak a thread -- which is the same property
    // `TaskTable` gives the daemon, and the reason `clippy.toml` forbids the free
    // function.
    let total_reads: u64 = std::thread::scope(|scope| {
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let published = Arc::clone(&published);
                let stop = Arc::clone(&stop);
                scope.spawn(move || {
                    // Counted per reader and summed after the join, rather than through one
                    // shared atomic: 64 threads incrementing one counter in a tight loop is a
                    // contended cache line that dominates the loop the test is actually
                    // about, and every one of those operations is instrumented under
                    // ThreadSanitizer.
                    let mut reads = 0u64;
                    let mut highest = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        // One load, and the `Arc` keeps the snapshot alive for as long as this
                        // reader needs it -- the shape of a request handler holding a
                        // configuration for the duration of the request.
                        let table = published.load();
                        let generation = table.generation.get();

                        assert!(
                            table.is_coherent(),
                            "a reader observed a mixture of two commits: table generation \
                         {generation}, rows {:?}",
                            table
                                .components
                                .iter()
                                .map(|c| c.applied_generation.get())
                                .collect::<Vec<_>>()
                        );
                        assert_eq!(
                            table.components.len(),
                            4,
                            "a reader observed a table that was never published"
                        );
                        // Coherence on one location: a reader's successive loads can never go
                        // backwards, so "the configuration I am serving from" is monotonic for
                        // the life of a reader.
                        assert!(
                            generation >= highest,
                            "a reader saw generation {generation} after {highest}"
                        );
                        highest = generation;
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();

        let writer_published = Arc::clone(&published);
        scope.spawn(move || {
            for generation in 1..=COMMITS {
                writer_published.store(commit(generation));
            }
            stop.store(true, Ordering::Release);
        });

        readers
            .into_iter()
            .map(|r| r.join().expect("a reader panicked"))
            .sum()
    });

    assert_eq!(published.load().generation, Generation::new(COMMITS));
    assert!(
        total_reads > 0,
        "the readers must actually have read something for this to mean anything"
    );
}

// --- 2. shutdown with a stalled sink ----------------------------------------------

fn envelope(seq: u64) -> Envelope {
    Envelope {
        station: StationKey::new("backyard"),
        observed_at: Utc::now(),
        received_at: Utc::now(),
        provenance: Provenance::admitted(OriginId::new([1; 16]), Seq::new(seq)),
        reading: Arc::new(Reading::default()),
    }
}

fn on_reading_trigger(deadline: Duration) -> Trigger {
    Trigger::OnReading {
        filter: ReadingFilter::All,
        policy: ReadingPolicy::Durable,
        deadline,
    }
}

/// A sink that takes a batch and never returns.
fn stalled_sink(name: &str, deadline: Duration) -> ComponentSpec {
    ComponentSpec::new(
        id(name),
        on_reading_trigger(deadline),
        Builder::on_reading(|_| async { Ok(StalledOnReading) }),
    )
}

/// A sink that handles every batch, with a counter the test can read.
fn counting_sink(name: &str, deadline: Duration) -> (ComponentSpec, Arc<Mutex<usize>>) {
    let batches = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&batches);
    let spec = ComponentSpec::new(
        id(name),
        on_reading_trigger(deadline),
        Builder::on_reading(move |_| {
            let batches = Arc::clone(&shared);
            async move {
                Ok(CountingOnReading {
                    batches,
                    readings: Arc::new(Mutex::new(0)),
                })
            }
        }),
    );
    (spec, batches)
}

#[tokio::test(start_paused = true)]
async fn a_stalled_sink_blocks_neither_the_source_nor_shutdown() {
    let stream = Arc::new(ChannelStream::new());
    // A deliberately long deadline, so the stall is a stall rather than a deadline hit:
    // what is under test is the grace, not the timeout.
    let stalled = stalled_sink("writer:timescale", Duration::from_secs(3_600));
    let (healthy, healthy_batches) = counting_sink("uplink:collector", Duration::from_secs(5));
    let stalled_id = id("writer:timescale");
    let healthy_id = id("uplink:collector");

    let grace = Duration::from_secs(10);
    let plan = validate(
        vec![stalled.grace(grace), healthy.grace(grace)],
        &PlanContext::new(Generation::new(1)),
    )
    .expect("a valid plan");

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) =
        Supervisor::new(roots_with_stream(Arc::clone(&stream)), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        // Offer 1,000 envelopes to each sink. The stalled one takes its first batch and
        // never returns, so its 20-deep queue fills and every further offer is **refused
        // rather than waited on** — which is the invariant no reading policy may break
        // (W2a Decision 61). The Go defect is the other branch: an unguarded blocking send
        // into a 20-deep channel, which back-pressures the serial read loops.
        let mut accepted_by_healthy = 0usize;
        for seq in 1..=1_000u64 {
            let batch = ReadingBatch::new(vec![envelope(seq)]);
            stream.deliver(&stalled_id, batch.clone());
            if stream.deliver(&healthy_id, batch) {
                accepted_by_healthy += 1;
            }
            // The healthy sink drains as it goes, so yielding keeps its queue moving
            // while the stalled one's stays full.
            tokio::task::yield_now().await;
        }
        assert!(
            stream.refused(&stalled_id) > 0,
            "the stalled sink's queue must actually have filled, or this test is \
             asserting a property of an unbounded channel"
        );
        assert_eq!(
            stream.refused(&healthy_id),
            0,
            "a draining sink is never refused"
        );
        assert!(accepted_by_healthy > SUBSCRIBER_DEPTH);

        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        // The healthy sibling kept receiving and acknowledging while the other stalled.
        assert!(
            *healthy_batches.lock().unwrap() > 1,
            "a stalled sink must not stop its siblings"
        );
        assert!(
            stream.acks(&healthy_id) > 1,
            "the healthy sink's cursor advanced"
        );
        assert_eq!(
            stream.acks(&stalled_id),
            0,
            "the stalled sink acknowledged nothing, so its cursor did not advance -- \
             which is what makes the backoff a retry of the same batch rather than a loss"
        );

        let at_shutdown = tokio::time::Instant::now();
        stop.cancel();
        (at_shutdown, control)
    };

    let (report, (at_shutdown, control)) = tokio::join!(supervisor.run(plan), driver);
    let took = at_shutdown.elapsed();

    assert!(
        took <= grace + Duration::from_millis(500),
        "shutdown took {took:?}, which is past the grace plus a margin"
    );
    assert_eq!(report.aborted, 1, "exactly the stalled one was aborted");
    assert_eq!(report.stopped, 1, "and the healthy one stopped cleanly");
    assert_eq!(
        control
            .component(&stalled_id)
            .expect("still in the table")
            .counters
            .stop_timeout_total,
        1,
        "the abort is counted against the component that caused it"
    );
    assert_eq!(
        control
            .component(&healthy_id)
            .expect("still in the table")
            .counters
            .stop_timeout_total,
        0
    );
}

/// The other half of the same invariant, stated separately because it is the one that
/// would have caught the Go defect: a sink that is slow enough to hit its **deadline**
/// does not stop the stream either, and the component survives to take the next batch.
#[tokio::test(start_paused = true)]
async fn a_sink_that_misses_its_deadline_is_restarted_and_replays() {
    let stream = Arc::new(ChannelStream::new());
    let stalled = stalled_sink("writer:timescale", Duration::from_secs(5));
    let stalled_id = id("writer:timescale");
    let plan =
        validate(vec![stalled], &PlanContext::new(Generation::new(1))).expect("a valid plan");

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) =
        Supervisor::new(roots_with_stream(Arc::clone(&stream)), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert!(stream.deliver(&stalled_id, ReadingBatch::new(vec![envelope(1)])));
        // Let the body take the batch before the clock moves: the deadline is armed when
        // the delivery starts, not when the batch is queued.
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        // The deadline fires, the delivery is counted as a miss, and the component is
        // backed off rather than left holding a batch forever.
        tokio::time::advance(Duration::from_secs(6)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        let status = control.component(&stalled_id).expect("in the table");
        assert_eq!(status.counters.deadline_exceeded_total, 1);
        assert_eq!(
            stream.acks(&stalled_id),
            0,
            "nothing was acknowledged, so nothing was lost"
        );
        assert!(
            matches!(status.state, State::Backoff { .. } | State::Starting { .. }),
            "{:?}",
            status.state
        );

        // And it comes back: no permanent give-up (W2a Decision 30).
        tokio::time::advance(Duration::from_secs(61)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        let status = control.component(&stalled_id).expect("in the table");
        assert!(
            matches!(status.state, State::Running { .. }),
            "{:?}",
            status.state
        );
        assert!(status.counters.restart_total >= 1);

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    // Nothing had to be aborted: by shutdown the restarted sink is idle in its receive
    // loop, where it observes cancellation at its next await. The abort in the test above
    // happens only because that one is stopped *mid-delivery*, which is the distinction
    // between "slow" and "stalled" that the grace exists to make.
    assert_eq!(report.aborted, 0, "{report:?}");
    assert_eq!(report.stopped, 1, "{report:?}");
}
