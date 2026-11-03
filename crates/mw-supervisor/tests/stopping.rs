//! The task table's invariant, and the four ways a component used to be able to die
//! quietly.
//!
//! Every test here was written against a confirmed defect found in review, and every one
//! of them fails against the code as it was. They are in their own file because they share
//! one shape: a body that does not cooperate, and an assertion about what the supervisor
//! does about it.
//!
//! > **The invariant (§3.2): a `JoinHandle` is never owned by anything but `TaskTable`,
//! > and `TaskTable` has no removal path that does not await.**
//!
//! The thing that makes it easy to believe and easy to get wrong is that
//! `timeout(grace, handle)` *drops* the handle when the grace elapses — and dropping a
//! `JoinHandle` **detaches** the task rather than cancelling it. A `stop` written that way
//! reports `Aborted`, increments `stop_timeout_total`, and leaves the body running: past
//! shutdown, or alongside the replacement a plan diff has just started, still holding
//! whatever the component held.

#![cfg(not(loom))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mw_component::{
    BoxFuture, Builder, ComponentError, ComponentSpec, Cx, Generation, Outcome, Serve, State, Tick,
    Trigger,
};
use mw_supervisor::plan::{validate, Plan, PlanContext};
use mw_supervisor::testing::{id, roots, CooperativeServe};
use mw_supervisor::Supervisor;
use tokio_util::sync::CancellationToken;

async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

async fn advance(by: Duration) {
    tokio::time::advance(by).await;
    settle().await;
}

fn planned(specs: Vec<ComponentSpec>, generation: u64) -> Plan {
    validate(specs, &PlanContext::new(Generation::new(generation))).expect("a valid plan")
}

// --- 1. a stopped task is actually stopped -----------------------------------------

/// A `Serve` body that never returns and never looks at its token, and that records when
/// it is finally dropped.
///
/// The `Drop` is the instrument. A task that was **aborted** has had its future dropped,
/// so this runs; a task that was merely **detached** — which is what dropping a
/// `JoinHandle` does — is still parked on the same `pending()` and this never runs. The
/// difference is invisible to the `aborted` counter, which is why the counter is not the
/// assertion.
struct Parked {
    dropped: Arc<AtomicBool>,
}

impl Drop for Parked {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl Serve for Parked {
    fn serve(self: Box<Self>, _cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            // Held across the await, so the guard's lifetime is the task's.
            let _parked = self;
            std::future::pending::<()>().await;
            Ok(())
        })
    }
}

/// **The abort has to be real.** A body that ignores cancellation is aborted at its grace
/// and its future is *dropped* — which is what releases the serial port, the socket and
/// the pool handle (**D-24**: resource release is `Drop`, not a method).
///
/// `timeout(grace, handle)` on its own drops the `JoinHandle`, which **detaches** the task
/// rather than cancelling it: `stop` then reported `Aborted`, incremented
/// `stop_timeout_total`, and left the body alive — past shutdown, and alongside the
/// replacement a plan diff had just started.
#[tokio::test(start_paused = true)]
async fn a_body_that_ignores_cancellation_is_aborted_rather_than_detached() {
    let dropped = Arc::new(AtomicBool::new(false));
    let shared = Arc::clone(&dropped);
    let spec = ComponentSpec::new(
        id("station:davis:backyard"),
        Trigger::Service,
        Builder::serve(move |_| {
            let dropped = Arc::clone(&shared);
            async move { Ok(Parked { dropped }) }
        }),
    )
    .grace(Duration::from_secs(10));

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert!(
            matches!(
                control
                    .component(&id("station:davis:backyard"))
                    .unwrap()
                    .state,
                State::Running { .. }
            ),
            "it is running"
        );
        assert!(!dropped.load(Ordering::SeqCst));
        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(planned(vec![spec], 1)), driver);
    assert_eq!(report.aborted, 1, "{report:?}");
    assert_eq!(
        control
            .component(&id("station:davis:backyard"))
            .unwrap()
            .counters
            .stop_timeout_total,
        1
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the body was never dropped, so `stop` detached the task instead of aborting it \
         -- and whatever it was holding is still held"
    );
}

// --- 2. a panicking body is not a silent death -------------------------------------

/// A `Tick` body that panics on its first occurrence.
struct Panicky {
    runs: Arc<Mutex<usize>>,
}

impl Tick for Panicky {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        *self.runs.lock().expect("the counter is not poisoned") += 1;
        Box::pin(async { panic!("the body panicked") })
    }
}

/// A panic inside a body used to be invisible: the exit report was sent from inside the
/// future, which is exactly where a panic escapes it. The component stayed `Running`,
/// `in_flight` stayed true, and the only trace was a climbing `skipped_total` — a dead
/// component that the management API reported as healthy.
///
/// The report is in a `Drop` guard now, so the panic unwinds into it.
#[tokio::test(start_paused = true)]
async fn a_panicking_body_is_reported_and_restarted() {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let spec = ComponentSpec::new(
        id("wu:backyard"),
        Trigger::every(Duration::from_secs(60), Duration::from_secs(30)),
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(Panicky { runs }) }
        }),
    );

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        advance(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock().unwrap(), 1, "the occurrence ran and panicked");

        let status = control.component(&id("wu:backyard")).unwrap();
        assert!(
            matches!(status.state, State::Backoff { .. } | State::Starting { .. }),
            "a panicking body must not be reported as healthy: {:?}",
            status.state
        );
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("panicked")),
            "the panic has to reach the operator: {:?}",
            status.last_error
        );
        assert!(status.counters.restart_total >= 1);

        // And it comes back, rather than being dead with a climbing `skipped_total`.
        advance(Duration::from_secs(61)).await;
        advance(Duration::from_secs(60)).await;
        assert!(
            *runs.lock().unwrap() >= 2,
            "the component was never rebuilt after the panic"
        );
        // What the counters say about a panic, stated so it is not mistaken for a gap: a
        // panicking occurrence never reports an outcome, so `runs_total` and
        // `failed_total` stay at zero and the **restart** is the visible signal, alongside
        // `last_error`. Some skips are honest too — an occurrence comes due while the
        // component is in `Backoff` with no body, and that is recorded rather than queued.
        //
        // The defect this test exists against looked different: `runs` frozen at 1,
        // `restart_total` at 0, and `skipped_total` climbing forever against a component
        // the management API reported as `Running`.
        let status = control.component(&id("wu:backyard")).unwrap();
        assert!(status.counters.restart_total >= 1, "{:?}", status.counters);
        assert!(
            status.counters.skipped_total < 5,
            "skips should be incidental to the rebuild, not the only thing happening: {:?}",
            status.counters
        );

        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(planned(vec![spec], 1)), driver);
}

// --- 3. one task per component ------------------------------------------------------

/// A `Serve` body that counts how many instances are live at once.
struct Counted {
    live: Arc<AtomicU64>,
}

impl Serve for Counted {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            self.live.fetch_add(1, Ordering::SeqCst);
            cx.cancelled().await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// `ComponentControl::start` on a component that is already running used to spawn a second
/// body: `reconcile` called `start`, and `spawn`'s `insert` overwrote the entry — dropping
/// the previous token (a drop is not a cancel) and the previous handle (a drop is not an
/// abort). Two live bodies for one component, one of them unreachable.
#[tokio::test(start_paused = true)]
async fn starting_a_running_component_does_not_create_a_second_body() {
    let live = Arc::new(AtomicU64::new(0));
    let shared = Arc::clone(&live);
    let spec = ComponentSpec::new(
        id("http:listener"),
        Trigger::Service,
        Builder::serve(move |_| {
            let live = Arc::clone(&shared);
            async move { Ok(Counted { live }) }
        }),
    );

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert_eq!(live.load(Ordering::SeqCst), 1);

        control.start(&id("http:listener")).expect("accepted");
        settle().await;
        assert_eq!(
            live.load(Ordering::SeqCst),
            1,
            "a second body was spawned and the first one leaked"
        );
        assert!(matches!(
            control.component(&id("http:listener")).unwrap().state,
            State::Running { .. }
        ));

        stop.cancel();
    };

    let (report, ()) = tokio::join!(supervisor.run(planned(vec![spec], 1)), driver);
    assert_eq!(report.aborted, 0, "{report:?}");
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "a body outlived shutdown, so something was holding no token"
    );
}

// --- 4. a failed on-demand check can be run again ----------------------------------

struct FailingCheck {
    runs: Arc<Mutex<usize>>,
    fail: Arc<AtomicBool>,
}

impl Tick for FailingCheck {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        *self.runs.lock().expect("the counter is not poisoned") += 1;
        let fail = self.fail.load(Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(ComponentError::tick(
                    "authenticate to APRS-IS",
                    mw_supervisor::testing::Refused,
                ))
            } else {
                Ok(())
            }
        })
    }
}

/// Failing is a `/test/*` connectivity check's **normal** outcome, and one failure used to
/// disable it for the life of the process: a non-scheduled trigger went to the terminal
/// `State::Failed`, which is not fireable, nothing re-arms an `OnDemand` component, and
/// `reconcile` leaves `Failed` alone. `run_now` kept returning success and dropping the
/// request.
#[tokio::test(start_paused = true)]
async fn a_failed_on_demand_check_can_be_run_again() {
    let runs = Arc::new(Mutex::new(0));
    let fail = Arc::new(AtomicBool::new(true));
    let shared_runs = Arc::clone(&runs);
    let shared_fail = Arc::clone(&fail);
    let spec = ComponentSpec::new(
        id("check:aprs-is"),
        Trigger::OnDemand {
            deadline: Duration::from_secs(5),
        },
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared_runs);
            let fail = Arc::clone(&shared_fail);
            async move { Ok(FailingCheck { runs, fail }) }
        }),
    );

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        control.run_now(&id("check:aprs-is")).expect("accepted");
        advance(Duration::from_millis(1)).await;
        assert_eq!(*runs.lock().unwrap(), 1);

        let status = control.component(&id("check:aprs-is")).unwrap();
        assert_eq!(status.counters.failed_total, 1);
        assert!(status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("authenticate to APRS-IS")));
        assert!(
            matches!(status.state, State::Running { .. }),
            "a failed check must stay live: {:?}",
            status.state
        );

        // The operator fixes the credential and presses the button again.
        fail.store(false, Ordering::SeqCst);
        control
            .run_now(&id("check:aprs-is"))
            .expect("still accepted after a failure");
        advance(Duration::from_millis(1)).await;
        assert_eq!(
            *runs.lock().unwrap(),
            2,
            "the second request was accepted and then dropped"
        );
        let status = control.component(&id("check:aprs-is")).unwrap();
        assert_eq!(status.schedule.last_outcome, Some(Outcome::Ok));
        assert_eq!(status.counters.failed_total, 1);

        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(planned(vec![spec], 1)), driver);
}

/// `run_now` on a component that is not live is **refused**, rather than accepted and
/// dropped: a 200 for a check that never ran is worse than a refusal naming the state.
#[tokio::test(start_paused = true)]
async fn run_now_on_a_component_that_is_not_live_is_refused() {
    let check = ComponentSpec::new(
        id("check:database"),
        Trigger::OnDemand {
            deadline: Duration::from_secs(5),
        },
        Builder::tick(|bcx| {
            // Never satisfiable in this plan: the component stays `Waiting`.
            let resolved = bcx.hard::<mw_supervisor::testing::TestReadingStore>();
            async move {
                resolved?;
                Ok(mw_supervisor::testing::CountingTick::default())
            }
        }),
    )
    .requires([mw_component::Need::hard(
        mw_component::Capability::ReadingStore,
    )]);
    let db = ComponentSpec::new(
        id("db:postgres"),
        Trigger::Service,
        // Provides the capability in the plan but never publishes it, which is how a
        // database that is unreachable at boot looks.
        Builder::serve(|_| async { Ok(CooperativeServe) }),
    )
    .provides([mw_component::Provision::of(
        mw_component::Capability::ReadingStore,
    )]);

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        assert!(matches!(
            control.component(&id("check:database")).unwrap().state,
            State::Waiting { .. }
        ));
        let refused = control
            .run_now(&id("check:database"))
            .expect_err("it is waiting on a capability");
        assert!(
            matches!(
                refused,
                mw_component::ControlError::NotLive {
                    state: "waiting",
                    ..
                }
            ),
            "{refused}"
        );
        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(planned(vec![check, db], 1)), driver);
}

// --- 5. backoff escalates ------------------------------------------------------------

/// A `Serve` body that builds, reports ready, and returns at once.
struct Immediate;

impl Serve for Immediate {
    fn serve(self: Box<Self>, _cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async { Ok(()) })
    }
}

/// §3.4: backoff resets "after a run that lasted longer than the cap". Resetting it on
/// `Ready` instead means a component that builds successfully and then returns — a serial
/// port that opens and EOFs, a TCP uplink that connects and drops — restarts on the base
/// delay forever.
#[tokio::test(start_paused = true)]
async fn a_build_then_return_loop_escalates_rather_than_spinning() {
    let builds = Arc::new(AtomicU64::new(0));
    let shared = Arc::clone(&builds);
    let spec = ComponentSpec::new(
        id("uplink:collector:peer"),
        Trigger::Service,
        Builder::serve(move |_| {
            let builds = Arc::clone(&shared);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(Immediate)
            }
        }),
    );

    let shutdown = CancellationToken::new();
    let (supervisor, control, _plans) = Supervisor::new(roots(), shutdown.clone());
    let stop = shutdown;

    let driver = async {
        settle().await;
        // Enough cycles for the attempt count to climb, if it climbs at all.
        for _ in 0..20 {
            advance(Duration::from_secs(5)).await;
        }

        let status = control.component(&id("uplink:collector:peer")).unwrap();
        let attempt = match status.state {
            State::Backoff { attempt, .. } | State::Starting { attempt } => attempt,
            other => panic!("expected a retry state, got {other:?}"),
        };
        assert!(
            attempt >= 3,
            "the attempt count is not climbing ({attempt}), so the delay cannot either: \
             a component that builds and returns is restarting on the base delay forever. \
             {:?}",
            status.counters
        );
        assert!(
            builds.load(Ordering::SeqCst) > 1,
            "it has to keep trying at all"
        );
        assert!(status.counters.restart_total >= 3);

        stop.cancel();
    };

    let (_report, ()) = tokio::join!(supervisor.run(planned(vec![spec], 1)), driver);
}
