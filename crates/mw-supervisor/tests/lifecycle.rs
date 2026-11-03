//! The lifecycle, driven through a real supervisor.
//!
//! Every test here runs the supervisor's own loop under `tokio::time::pause()`, so a
//! 60-second period costs no wall-clock time and the assertions are about ordering rather
//! than about luck. The supervisor is driven with `join!` rather than on a spawned task,
//! which keeps `clippy.toml`'s one-spawn-site rule true in the tests as well as in the
//! daemon.
//!
//! One artefact of paused time is worth knowing: the supervisor compares elapsed
//! `SystemTime` against elapsed `Instant` to notice a stepped wall clock (W3 §3.3), and
//! advancing virtual time by a minute while the real clock stands still looks exactly like
//! that. The detector is right and the virtual clock is the thing lying, so `clock_steps`
//! in a test's report is expected rather than a finding.

#![cfg(not(loom))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mw_component::{
    BoxFuture, Builder, Capability, ComponentControl, ComponentError, ComponentId, ComponentSpec,
    ConfigDigest, Cx, Generation, Need, Outcome, Provision, Serve, State, Trigger,
};
use mw_supervisor::plan::{validate, Plan, PlanContext};
use mw_supervisor::testing::{
    id, roots, CooperativeServe, CountingTick, FailingTick, StalledTick, TestReadingStore,
};
use mw_supervisor::{Readiness, RunReport, Supervisor};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

// --- harness ------------------------------------------------------------------------

/// Let the supervisor's loop and every component task make progress.
async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

/// Move virtual time forward and let everything that became due be handled.
async fn advance(by: Duration) {
    tokio::time::advance(by).await;
    settle().await;
}

fn planned(specs: Vec<ComponentSpec>, generation: u64) -> Plan {
    validate(specs, &PlanContext::new(Generation::new(generation))).expect("a valid plan")
}

fn tick(name: &str, period_s: u64, deadline_s: u64) -> (ComponentSpec, Arc<Mutex<usize>>) {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let spec = ComponentSpec::new(
        id(name),
        Trigger::every(
            Duration::from_secs(period_s),
            Duration::from_secs(deadline_s),
        ),
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(CountingTick { runs }) }
        }),
    );
    (spec, runs)
}

/// A `Serve` body that publishes `ReadingStore` and revokes it when the test says so.
///
/// This is `db:postgres` in miniature: it is `Running` from the moment its task starts and
/// its capability is usable only once it has published, which is W2a Decision 22's
/// distinction between the two.
struct Database {
    publisher: mw_component::Publisher<TestReadingStore>,
    revoke: Arc<Notify>,
}

impl Serve for Database {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            self.publisher.publish(TestReadingStore("pool"));
            tokio::select! {
                () = cx.cancelled() => {}
                () = self.revoke.notified() => {
                    // A sustained failure, not one failed query (W2a §3.4).
                    self.publisher.revoke();
                    cx.cancelled().await;
                }
            }
            Ok(())
        })
    }
}

fn database(revoke: Arc<Notify>) -> ComponentSpec {
    ComponentSpec::new(
        id("db:postgres"),
        Trigger::Service,
        Builder::serve(move |bcx| {
            let publisher = bcx.publisher::<TestReadingStore>();
            let revoke = Arc::clone(&revoke);
            async move {
                Ok(Database {
                    publisher: publisher?,
                    revoke,
                })
            }
        }),
    )
    .provides([Provision::of(Capability::ReadingStore)])
}

/// A consumer that resolves `ReadingStore` in `build` and keeps it.
fn consumer(name: &str) -> ComponentSpec {
    ComponentSpec::new(
        id(name),
        Trigger::every(Duration::from_secs(60), Duration::from_secs(10)),
        Builder::tick(|bcx| {
            let resolved = bcx.hard::<TestReadingStore>();
            async move {
                resolved?;
                Ok(CountingTick::default())
            }
        }),
    )
    .requires([Need::hard(Capability::ReadingStore)])
}

fn state_of(control: &Arc<dyn ComponentControl>, name: &str) -> State {
    control
        .component(&id(name))
        .map(|s| s.state)
        .unwrap_or_else(|| panic!("{name} is not in the published table"))
}

// --- tests --------------------------------------------------------------------------

/// A hard requirement that is not yet available means **waiting**, not refusal — and
/// `Waiting` carries the capability *and its provider*, which is what lets W5 render a
/// sentence an operator can act on.
///
/// This is D-27, which is the defect the whole model exists to remove: four Go controllers
/// call `ValidateTimescaleDBConfig` in their constructors and refuse to exist without a
/// database, so a transient problem at boot disables the uploaders until the process is
/// restarted by hand.
#[tokio::test(start_paused = true)]
async fn a_missing_hard_requirement_means_waiting_and_resolves_itself() {
    let revoke = Arc::new(Notify::new());
    let plan = planned(
        vec![consumer("wu:backyard"), database(Arc::clone(&revoke))],
        1,
    );
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let supervisor_shutdown = shutdown;
    let mut readiness = supervisor.readiness();

    let driver = async {
        settle().await;

        // The database is `Running` as soon as its task starts; the consumer is `Waiting`
        // only until the publication arrives, which has already happened by now.
        assert!(matches!(
            state_of(&control, "db:postgres"),
            State::Running { .. }
        ));
        assert!(
            matches!(state_of(&control, "wu:backyard"), State::Running { .. }),
            "the consumer started once the capability was published, not before: {:?}",
            state_of(&control, "wu:backyard")
        );

        // Readiness is "the supervisor has converged", and it has.
        readiness.changed().await.ok();
        let Readiness { converged, status } = readiness.borrow().clone();
        assert!(converged, "{status}");
        assert!(status.contains("2 running"), "{status}");

        // A sustained failure revokes the capability, and the consumer returns to
        // `Waiting` with the provider named rather than dying.
        revoke.notify_one();
        settle().await;
        match state_of(&control, "wu:backyard") {
            State::Waiting { on } => {
                assert_eq!(on.len(), 1);
                assert_eq!(on[0].capability, Capability::ReadingStore);
                assert_eq!(
                    on[0].provider.as_ref().map(ComponentId::as_str),
                    Some("db:postgres"),
                    "the state carries the edge, not just the capability"
                );
            }
            other => panic!("expected Waiting, got {other:?}"),
        }

        supervisor_shutdown.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert!(report.aborted == 0, "{report:?}");
}

/// The supervisor owns the clock: a periodic component's occurrences, `last_run`,
/// `last_outcome` and `next_run` are state rather than local variables inside a
/// `for { select { case <-ticker.C: } }` loop.
#[tokio::test(start_paused = true)]
async fn a_periodic_component_fires_on_its_period_and_the_schedule_is_state() {
    let (spec, runs) = tick("pws:backyard", 60, 30);
    let plan = planned(vec![spec], 1);
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert_eq!(
            *runs.lock().unwrap(),
            0,
            "nothing fires before its phase slot"
        );
        let status = control.component(&id("pws:backyard")).unwrap();
        assert_eq!(status.trigger, "every");
        assert_eq!(status.schedule.period, Some(Duration::from_secs(60)));
        assert!(
            status.schedule.next_run.is_some(),
            "next_run is a heap peek, available before the first occurrence"
        );

        // Within one period, whatever the phase offset is.
        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 1);

        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 2, "and again, one per period");

        let status = control.component(&id("pws:backyard")).unwrap();
        assert_eq!(status.counters.runs_total, 2);
        assert_eq!(status.schedule.last_outcome, Some(Outcome::Ok));
        assert!(status.schedule.last_run.is_some());
        assert_eq!(status.counters.failed_total, 0);
        assert_eq!(status.counters.skipped_total, 0);
        assert_eq!(status.applied_generation, Generation::new(1));

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert_eq!(report.aborted, 0);
}

/// **For `Every` the period is the backoff** (W2a Decision 29). A failing uploader retries
/// on its own schedule — what the operator configured and what the remote service's rate
/// limit expects — while `failed_total` and `last_error` make the condition visible.
///
/// The `Backoff` state must not suppress the next occurrence, which is the one place W3
/// §3.4's pseudocode contradicts itself; this test is what pins the resolution.
#[tokio::test(start_paused = true)]
async fn a_failing_periodic_component_keeps_retrying_on_its_period() {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let spec = ComponentSpec::new(
        id("wu:backyard"),
        Trigger::every(Duration::from_secs(60), Duration::from_secs(30)),
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(FailingTick { runs }) }
        }),
    );
    let plan = planned(vec![spec], 1);
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 1);

        let status = control.component(&id("wu:backyard")).unwrap();
        assert_eq!(status.counters.failed_total, 1);
        assert_eq!(status.schedule.last_outcome, Some(Outcome::Failed));
        let error = status.last_error.expect("the error is recorded");
        assert!(
            error.contains("upload to the test service") && error.contains("os error 111"),
            "the whole cause chain is recorded, walked once: {error}"
        );
        assert!(
            matches!(status.state, State::Backoff { .. }),
            "{:?}",
            status.state
        );

        // The period is the backoff: it runs again without a second clock.
        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 2, "a failed component keeps trying");
        assert_eq!(
            control
                .component(&id("wu:backyard"))
                .unwrap()
                .counters
                .failed_total,
            2
        );

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert_eq!(report.aborted, 0);
}

/// W3 §8.4's fifth concurrency case, which needs no concurrency to assert: a tick that
/// ignores its cancellation token is dropped on its deadline, `deadline_exceeded_total`
/// increments, **the component survives** — because the per-occurrence token is a child —
/// and the next occurrence is not skipped.
#[tokio::test(start_paused = true)]
async fn a_tick_that_overruns_its_deadline_is_cancelled_and_the_component_survives() {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let spec = ComponentSpec::new(
        id("aprs:backyard"),
        Trigger::every(Duration::from_secs(60), Duration::from_secs(10)),
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(StalledTick { runs }) }
        }),
    );
    let plan = planned(vec![spec], 1);
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 1, "it started");

        // The deadline has not passed yet, so the occurrence is still in flight.
        let status = control.component(&id("aprs:backyard")).unwrap();
        assert_eq!(status.counters.deadline_exceeded_total, 0);

        advance(Duration::from_secs(10)).await;
        let status = control.component(&id("aprs:backyard")).unwrap();
        assert_eq!(status.counters.deadline_exceeded_total, 1);
        assert_eq!(
            status.schedule.last_outcome,
            Some(Outcome::DeadlineExceeded)
        );

        // The next occurrence still happens: the component was not killed with its tick.
        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 2, "the component survived its tick");

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    // The component was mid-occurrence in a body that ignores its token when shutdown
    // arrived, so the task table aborted it after the grace and **counted it**. That is
    // the other half of the same mechanism: a body that cannot be asked to stop is
    // stopped anyway, visibly, rather than hanging shutdown as the Go version's unguarded
    // blocking send does (D-29).
    assert_eq!(report.aborted, 1, "{report:?}");
    assert_eq!(
        control
            .component(&id("aprs:backyard"))
            .unwrap()
            .counters
            .stop_timeout_total,
        1
    );
}

/// A build failure is a **lifecycle transition**, not a startup abort: `Starting → Failed →
/// Backoff → Starting`, with no permanent give-up. That single change is what replaces
/// "four components refuse to construct and stay dead until the process is restarted".
#[tokio::test(start_paused = true)]
async fn a_build_failure_backs_off_and_tries_again() {
    let attempts = Arc::new(Mutex::new(0usize));
    let shared = Arc::clone(&attempts);
    let spec = ComponentSpec::new(
        id("station:davis:backyard"),
        Trigger::Service,
        Builder::serve(move |_| {
            let attempts = Arc::clone(&shared);
            async move {
                let mut n = attempts.lock().unwrap();
                *n += 1;
                if *n < 3 {
                    Err(mw_component::BuildError::Configuration {
                        reason: "the serial port is not there yet".into(),
                    })
                } else {
                    Ok(CooperativeServe)
                }
            }
        }),
    );
    let plan = planned(vec![spec], 1);
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert_eq!(*attempts.lock().unwrap(), 1);
        let status = control.component(&id("station:davis:backyard")).unwrap();
        assert!(matches!(status.state, State::Backoff { .. }), "{status:?}");
        assert!(status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("the serial port is not there yet")));

        // The cap is 60 s and it keeps trying; two more attempts get it running.
        advance(Duration::from_secs(61)).await;
        advance(Duration::from_secs(61)).await;
        assert_eq!(*attempts.lock().unwrap(), 3);
        let status = control.component(&id("station:davis:backyard")).unwrap();
        assert!(matches!(status.state, State::Running { .. }), "{status:?}");
        assert!(
            status.counters.restart_total >= 2,
            "the restarts are visible state, which is what makes retrying forever safe"
        );

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert_eq!(report.aborted, 0);
}

/// `OnStartup` runs once and is terminal.
#[tokio::test(start_paused = true)]
async fn an_on_startup_component_runs_once_and_finishes() {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let spec = ComponentSpec::new(
        id("precompute:once"),
        Trigger::OnStartup {
            deadline: Duration::from_secs(5),
        },
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(CountingTick { runs }) }
        }),
    );
    let plan = planned(vec![spec], 1);
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        advance(Duration::from_secs(1)).await;
        assert_eq!(*runs.lock().unwrap(), 1);
        let status = control.component(&id("precompute:once")).unwrap();
        assert!(matches!(status.state, State::Finished { .. }), "{status:?}");

        advance(Duration::from_secs(600)).await;
        assert_eq!(*runs.lock().unwrap(), 1, "once means once");

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert_eq!(report.aborted, 0);
}

/// `ComponentControl`: `run_now` on an `OnDemand` component — which is what the seven
/// `/test/*` connectivity checks are — and a refusal, by name, for a `Service` that has no
/// occurrence to run.
#[tokio::test(start_paused = true)]
async fn run_now_fires_an_on_demand_component_and_refuses_a_service() {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let check = ComponentSpec::new(
        id("check:aprs-is"),
        Trigger::OnDemand {
            deadline: Duration::from_secs(5),
        },
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(CountingTick { runs }) }
        }),
    );
    let listener = ComponentSpec::new(
        id("http:listener"),
        Trigger::Service,
        Builder::serve(|_| async { Ok(CooperativeServe) }),
    );
    let plan = planned(vec![check, listener], 1);
    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert_eq!(*runs.lock().unwrap(), 0, "on demand means on demand");

        control.run_now(&id("check:aprs-is")).expect("asked");
        // Twice, which a double-click in the UI is: the occurrence key deduplicates it.
        control.run_now(&id("check:aprs-is")).expect("asked again");
        advance(Duration::from_millis(1)).await;
        assert_eq!(*runs.lock().unwrap(), 1, "a double-click runs once");

        let refused = control
            .run_now(&id("http:listener"))
            .expect_err("a service has no occurrence");
        assert!(
            matches!(
                refused,
                mw_component::ControlError::NotRunnable {
                    trigger: "service",
                    ..
                }
            ),
            "{refused}"
        );

        let missing = control
            .run_now(&id("nothing:here"))
            .expect_err("not in the plan");
        assert!(matches!(
            missing,
            mw_component::ControlError::NoSuchComponent { .. }
        ));

        // Stopping is cancellation plus `Drop`, and it is observable.
        control.stop(&id("http:listener")).expect("asked to stop");
        settle().await;
        assert_eq!(state_of(&control, "http:listener"), State::Stopped);

        control.start(&id("http:listener")).expect("asked to start");
        settle().await;
        assert!(matches!(
            state_of(&control, "http:listener"),
            State::Running { .. }
        ));

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert_eq!(report.aborted, 0);
}

/// **Reconfiguration is a plan diff, not a restart.** An unchanged component is not
/// touched — so its `applied_generation` stays where it was, which is exactly how W5 §2.2
/// tells an operator that a save did not affect it.
#[tokio::test(start_paused = true)]
async fn a_plan_diff_restarts_only_what_changed() {
    let untouched_runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&untouched_runs);
    let untouched = || {
        let runs = Arc::clone(&shared);
        ComponentSpec::new(
            id("aprs:backyard"),
            Trigger::every(Duration::from_secs(600), Duration::from_secs(60)),
            Builder::tick(move |_| {
                let runs = Arc::clone(&runs);
                async move { Ok(CountingTick { runs }) }
            }),
        )
    };
    let changing = |digest: &str| {
        let (spec, _) = tick("wu:backyard", 60, 30);
        spec.config_digest(ConfigDigest::of(digest))
    };

    let first = planned(vec![untouched(), changing("KE7XYZ")], 41);
    let second = planned(vec![untouched(), changing("N0CALL")], 42);

    let shutdown = CancellationToken::new();
    let (supervisor, control, plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        let before = control.component(&id("aprs:backyard")).unwrap();
        assert_eq!(before.applied_generation, Generation::new(41));
        assert_eq!(control.generation(), Generation::new(41));

        plans.apply(second).expect("the supervisor is running");
        settle().await;

        assert_eq!(control.generation(), Generation::new(42));
        let untouched_now = control.component(&id("aprs:backyard")).unwrap();
        assert_eq!(
            untouched_now.applied_generation,
            Generation::new(41),
            "an untouched component is visibly still on the old generation"
        );
        assert_eq!(
            untouched_now.since, before.since,
            "and it was not restarted: its state has not changed since"
        );

        let changed = control.component(&id("wu:backyard")).unwrap();
        assert_eq!(
            changed.applied_generation,
            Generation::new(42),
            "the component whose configuration changed was rebuilt"
        );
        assert_eq!(
            changed.schedule.period,
            Some(Duration::from_secs(60)),
            "the schedule is state (§1.3), so every field of it has to follow the spec"
        );
        assert!(
            matches!(changed.state, State::Running { .. }),
            "{:?}",
            changed.state
        );

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(first), driver);
    assert!(
        report.stopped >= 1,
        "the changed component was stopped first"
    );
}

/// A changed spec's **schedule** follows it. The clock fires on the new period either
/// way; `schedule.period` is what W5 renders, and it used to be set once at construction
/// and never updated — so after a reconfiguration the UI showed the old interval forever.
#[tokio::test(start_paused = true)]
async fn a_changed_period_is_reflected_in_the_published_schedule() {
    let at = |period_s: u64, digest: &str| {
        let (spec, _) = tick("pws:backyard", period_s, 30);
        spec.config_digest(ConfigDigest::of(digest))
    };
    let first = planned(vec![at(60, "a")], 1);
    let second = planned(vec![at(300, "b")], 2);

    let shutdown = CancellationToken::new();
    let (supervisor, control, plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert_eq!(
            control
                .component(&id("pws:backyard"))
                .unwrap()
                .schedule
                .period,
            Some(Duration::from_secs(60))
        );

        plans.apply(second).expect("running");
        settle().await;
        assert_eq!(
            control
                .component(&id("pws:backyard"))
                .unwrap()
                .schedule
                .period,
            Some(Duration::from_secs(300))
        );
        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(first), driver);
}

/// A successful occurrence clears the attempt count, so `Backoff { attempt }` means "this
/// many consecutive failures" rather than "this many failures since the process started".
/// An intermittently failing uploader used to accumulate it for the life of the process.
#[tokio::test(start_paused = true)]
async fn a_successful_occurrence_clears_the_attempt_count() {
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let shared = Arc::clone(&fail);
    let spec = ComponentSpec::new(
        id("wu:backyard"),
        Trigger::every(Duration::from_secs(60), Duration::from_secs(30)),
        Builder::tick(move |_| {
            let fail = Arc::clone(&shared);
            async move { Ok(Flaky { fail }) }
        }),
    );

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        advance(Duration::from_secs(60)).await;
        advance(Duration::from_secs(60)).await;
        let attempt = match control.component(&id("wu:backyard")).unwrap().state {
            State::Backoff { attempt, .. } => attempt,
            other => panic!("expected Backoff, got {other:?}"),
        };
        assert!(attempt >= 2, "two failures in a row: {attempt}");

        // The remote end comes back.
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        advance(Duration::from_secs(60)).await;
        let status = control.component(&id("wu:backyard")).unwrap();
        assert!(
            matches!(status.state, State::Running { .. }),
            "{:?}",
            status.state
        );

        // ...and it fails once more. The attempt count starts again rather than carrying
        // the earlier failures.
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        advance(Duration::from_secs(60)).await;
        let attempt = match control.component(&id("wu:backyard")).unwrap().state {
            State::Backoff { attempt, .. } => attempt,
            other => panic!("expected Backoff, got {other:?}"),
        };
        assert_eq!(attempt, 1, "the count did not reset on the successful run");

        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(planned(vec![spec], 1)), driver);
}

/// A `Tick` body that fails while a flag is set.
struct Flaky {
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl mw_component::Tick for Flaky {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        let fail = self.fail.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(ComponentError::tick(
                    "upload to Weather Underground",
                    mw_supervisor::testing::Refused,
                ))
            } else {
                Ok(())
            }
        })
    }
}

/// `STATUS=` counts **components**, not distinct providers. Twelve uploaders waiting on
/// one database is "12 waiting", and reporting "1 waiting (db:postgres)" would say the
/// daemon is nearly fine when nothing is working.
#[tokio::test(start_paused = true)]
async fn the_status_line_counts_waiting_components_not_providers() {
    let db = ComponentSpec::new(
        id("db:postgres"),
        Trigger::Service,
        // Provides the capability in the plan and never publishes it: a database that is
        // unreachable at boot.
        Builder::serve(|_| async { Ok(CooperativeServe) }),
    )
    .provides([Provision::of(Capability::ReadingStore)]);
    let consumers: Vec<ComponentSpec> = (1..=3)
        .map(|n| consumer(&format!("wu:station{n}")))
        .collect();
    let mut specs = vec![db];
    specs.extend(consumers);

    let shutdown = CancellationToken::new();
    let (supervisor, _control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let readiness = supervisor.readiness();
    let stop = shutdown;

    let driver = async {
        settle().await;
        let status = readiness.borrow().status.clone();
        assert!(
            status.contains("3 waiting (db:postgres)"),
            "three components are waiting on one provider: {status}"
        );
        assert!(
            readiness.borrow().converged,
            "waiting is a settled state: an uploader waiting on a dead database must not \
             block readiness forever (§3.5)"
        );
        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(planned(specs, 1)), driver);
}

/// A removed component is stopped and leaves the table.
#[tokio::test(start_paused = true)]
async fn a_removed_component_is_stopped_and_gone() {
    let keep = ComponentSpec::new(
        id("http:listener"),
        Trigger::Service,
        Builder::serve(|_| async { Ok(CooperativeServe) }),
    );
    let remove = ComponentSpec::new(
        id("station:ambient"),
        Trigger::Service,
        Builder::serve(|_| async { Ok(CooperativeServe) }),
    );
    let first = planned(vec![keep, remove], 1);
    let keep_only = planned(
        vec![ComponentSpec::new(
            id("http:listener"),
            Trigger::Service,
            Builder::serve(|_| async { Ok(CooperativeServe) }),
        )],
        2,
    );

    let shutdown = CancellationToken::new();
    let (supervisor, control, plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert_eq!(control.components().len(), 2);
        plans.apply(keep_only).expect("running");
        settle().await;
        assert_eq!(control.components().len(), 1);
        assert!(control.component(&id("station:ambient")).is_none());
        stop.cancel();
    };

    let (report, ()): (RunReport, ()) = tokio::join!(supervisor.run(first), driver);
    assert_eq!(
        report.aborted, 0,
        "a cooperative service stops in its grace"
    );
    assert!(report.stopped >= 2);
}
