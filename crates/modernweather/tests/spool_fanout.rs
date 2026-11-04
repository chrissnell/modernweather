//! The stalled-sink test of W3 **D-85**, **re-pointed at a real spool** — which is
//! implementation-plan.md M3-2's third test-strategy item.
//!
//! **Why it lives in the binary's tests.** `mw-supervisor` and `mw-pipeline` are the same
//! layer in `ci/layers.toml`, and the layout gate rejects a same-layer dependency in either
//! direction, dev-dependencies included. So the supervisor cannot be driven against the
//! real fan-out anywhere except here, which is §1.3 rule 1 working as designed: the binary
//! is the only place a capability consumer and its provider meet.
//!
//! **What changes by using the real spool.** `mw_supervisor::testing::ChannelStream` is a
//! bounded 20-deep queue per subscriber, which makes "a source is never blocked by a sink"
//! falsifiable — the stalled sink's queue really fills and the offer is refused rather than
//! waited on. That is the half the supervisor owns. The half it could not assert is the one
//! **D-30a** is about: with a real spool the stalled sink does not *lose* the refused
//! envelopes, it falls behind **on disk**, and when it comes back the records are still
//! there. So the assertions here are the ones the double could not make:
//!
//! 1. `admit` never waits on a sink — a thousand readings are admitted while the sink holds
//!    its first batch, and every one is in the spool.
//! 2. The stalled sink's cursor does not advance, so nothing is acknowledged and nothing is
//!    lost.
//! 3. A healthy sibling on the same spool keeps draining, at its own position.
//! 4. Shutdown still completes inside the grace, with the stalled one aborted.
//! 5. After the restart the records the stalled sink never handled are **still readable**.
//!
//! Not run under ThreadSanitizer for the same reason `boot.rs` is not: the `race` job sets
//! `--cfg mw_sanitizer`, and the in-process concurrency it exists for is covered where TSan
//! can see it.
#![cfg(not(mw_sanitizer))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mw_component::{
    Builder, ComponentSpec, Generation, ReadingFilter, ReadingPolicy, ReadingStream, State, Trigger,
};
use mw_pipeline::testing::{config, id, node, reading, Scratch};
use mw_pipeline::{Clock, Draft, Spool, SpoolStream, TestClock};
use mw_supervisor::plan::{validate, PlanContext};
use mw_supervisor::testing::{
    CountingOnReading, MemoryLogControl, MemoryScheduleStore, StalledOnReading,
};
use mw_supervisor::{Roots, Supervisor};
use mw_types::{Seq, StationKey};
use tokio_util::sync::CancellationToken;

/// §3.6's default grace, which is what "shutdown completes" is measured against.
const GRACE: Duration = Duration::from_secs(10);

fn draft(clock: &TestClock) -> Draft {
    Draft {
        station: StationKey::new("backyard"),
        observed_at: clock.now(),
        received_at: clock.now(),
        reading: Arc::new(reading(29.92)),
    }
}

fn on_reading(deadline: Duration) -> Trigger {
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
        on_reading(deadline),
        Builder::on_reading(|_| async { Ok(StalledOnReading) }),
    )
}

/// A sink that handles every batch, with counters the test can read.
fn counting_sink(
    name: &str,
    deadline: Duration,
) -> (ComponentSpec, Arc<Mutex<usize>>, Arc<Mutex<usize>>) {
    let batches = Arc::new(Mutex::new(0));
    let readings = Arc::new(Mutex::new(0));
    let (b, r) = (Arc::clone(&batches), Arc::clone(&readings));
    let spec = ComponentSpec::new(
        id(name),
        on_reading(deadline),
        Builder::on_reading(move |_| {
            let (batches, readings) = (Arc::clone(&b), Arc::clone(&r));
            async move { Ok(CountingOnReading { batches, readings }) }
        }),
    );
    (spec, batches, readings)
}

fn roots(stream: Arc<SpoolStream>) -> Roots {
    Roots {
        schedule: Arc::new(MemoryScheduleStore::new()),
        stream: Some(stream as Arc<dyn ReadingStream>),
        log: Arc::new(MemoryLogControl::default()),
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_sink_blocks_neither_the_source_nor_shutdown_and_loses_nothing() {
    const ADMITTED: u64 = 1_000;

    let scratch = Scratch::new("stalled-sink");
    let clock = TestClock::fixed();
    let spool = Spool::open(
        config(scratch.spool()),
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let shutdown = CancellationToken::new();
    let stream = Arc::new(SpoolStream::new(Arc::clone(&spool)));

    // A deliberately long deadline, so the stall is a stall rather than a deadline hit:
    // what is under test is the grace, not the timeout.
    let stalled_id = id("writer:timescale");
    let healthy_id = id("uplink:suncrest");
    let stalled = stalled_sink(stalled_id.as_str(), Duration::from_secs(3_600));
    let (healthy, healthy_batches, healthy_readings) =
        counting_sink(healthy_id.as_str(), Duration::from_secs(5));

    let plan = validate(
        vec![stalled.grace(GRACE), healthy.grace(GRACE)],
        &PlanContext::new(Generation::new(1)),
    )
    .expect("a valid plan");

    let (supervisor, control, _plans) =
        Supervisor::new(roots(Arc::clone(&stream)), shutdown.clone());
    let stop = shutdown.clone();

    let driver = {
        let spool = Arc::clone(&spool);
        let clock = clock.clone();
        let stalled_id = stalled_id.clone();
        let healthy_id = healthy_id.clone();
        async move {
            // Let both components build and subscribe before anything is admitted, so
            // their cursors start at the tail rather than skipping the batch.
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }

            // **1. `admit` never waits on a sink.** A thousand readings while the stalled
            // sink holds its first batch; the Go defect is the other branch -- an
            // unguarded blocking send into a 20-deep channel, which back-pressures the
            // serial read loops of a console that drops packets when it is not drained.
            for _ in 0..ADMITTED {
                spool
                    .admit_local(&[draft(&clock)])
                    .await
                    .expect("admit waits on nothing but its own fsync");
                tokio::task::yield_now().await;
            }
            assert_eq!(
                spool.tail_seq(),
                Seq::new(ADMITTED),
                "every reading reached the spool while a sink was stalled"
            );

            for _ in 0..256 {
                tokio::task::yield_now().await;
            }

            // **2. The stalled sink's cursor did not advance**, so nothing is
            // acknowledged -- which is what makes the backoff a retry of the same batch
            // rather than a loss.
            assert_eq!(
                spool
                    .cursor_position(&stalled_id)
                    .expect("registered")
                    .acked_seq,
                Seq::new(0),
                "the stalled sink acknowledged nothing"
            );

            // **3. The healthy sibling kept draining**, at its own position, on the same
            // spool. This is the assertion the in-memory double could not make: the two
            // cursors are independent positions in one log rather than two queues.
            let healthy_at = spool
                .cursor_position(&healthy_id)
                .expect("registered")
                .acked_seq;
            assert!(
                healthy_at.get() > 0,
                "a stalled sink must not stop its siblings; the healthy one is at \
                 {healthy_at}"
            );
            assert!(
                *healthy_readings.lock().unwrap() > 0,
                "and it really did receive readings"
            );
            assert!(*healthy_batches.lock().unwrap() > 0);

            let at_shutdown = tokio::time::Instant::now();
            stop.cancel();
            (at_shutdown, control, healthy_at)
        }
    };

    let (report, (at_shutdown, control, healthy_at)) = tokio::join!(supervisor.run(plan), driver);
    let took = at_shutdown.elapsed();

    // **4. Shutdown completed anyway**, with exactly the stalled one aborted at the end of
    // its grace.
    assert!(
        took <= GRACE + Duration::from_millis(500),
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

    // **5. Nothing was lost.** The records the stalled sink never handled are still in the
    // spool, at its position, and the healthy sibling's position is independent of them.
    // That is **D-30a**: a slow sink falls behind on disk, never upstream.
    assert_eq!(
        spool
            .cursor_position(&stalled_id)
            .expect("registered")
            .acked_seq,
        Seq::new(0)
    );
    let unhandled = spool.read_after(Seq::new(0), 1_024).expect("readable");
    assert_eq!(
        unhandled.envelopes.len() as u64,
        ADMITTED,
        "every admitted reading is still there to be replayed"
    );
    assert_eq!(
        spool
            .cursor_position(&healthy_id)
            .expect("registered")
            .acked_seq,
        healthy_at,
        "and the healthy sink's position is its own"
    );
}

/// The other half of the same invariant, against a real spool: a sink that is slow enough
/// to hit its **deadline** does not stop the stream either, and the component survives to
/// take the next batch — from exactly where it was.
#[tokio::test(start_paused = true)]
async fn a_sink_that_misses_its_deadline_replays_the_same_records_from_the_spool() {
    let scratch = Scratch::new("deadline");
    let clock = TestClock::fixed();
    let spool = Spool::open(
        config(scratch.spool()),
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let shutdown = CancellationToken::new();
    let stream = Arc::new(SpoolStream::new(Arc::clone(&spool)));

    let stalled_id = id("writer:timescale");
    let plan = validate(
        vec![stalled_sink(stalled_id.as_str(), Duration::from_secs(5))],
        &PlanContext::new(Generation::new(1)),
    )
    .expect("a valid plan");

    let (supervisor, control, _plans) =
        Supervisor::new(roots(Arc::clone(&stream)), shutdown.clone());
    let stop = shutdown.clone();

    let driver = {
        let spool = Arc::clone(&spool);
        let clock = clock.clone();
        let stalled_id = stalled_id.clone();
        async move {
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            spool.admit_local(&[draft(&clock)]).await.expect("accepted");
            // Let the body take the batch before the clock moves: the deadline is armed
            // when the delivery starts, not when the record is appended.
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }

            tokio::time::advance(Duration::from_secs(6)).await;
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            let status = control.component(&stalled_id).expect("in the table");
            assert_eq!(status.counters.deadline_exceeded_total, 1);
            assert_eq!(
                spool
                    .cursor_position(&stalled_id)
                    .expect("registered")
                    .acked_seq,
                Seq::new(0),
                "nothing was acknowledged, so nothing was lost"
            );

            // And it comes back -- no permanent give-up (W2a Decision 30) -- and the
            // record is still there for it.
            tokio::time::advance(Duration::from_secs(61)).await;
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            let status = control.component(&stalled_id).expect("in the table");
            assert!(
                matches!(status.state, State::Running { .. }),
                "{:?}",
                status.state
            );
            assert!(status.counters.restart_total >= 1);
            assert_eq!(
                spool
                    .read_after(Seq::new(0), 16)
                    .expect("readable")
                    .envelopes
                    .len(),
                1,
                "the same record, replayed to the restarted component"
            );

            stop.cancel();
        }
    };

    let (_report, ()) = tokio::join!(supervisor.run(plan), driver);
}
