//! The task table, and the three component runners.
//!
//! > **The invariant: a `JoinHandle` is never owned by anything but [`TaskTable`], and
//! > `TaskTable` has no removal path that does not await.**
//!
//! Two halves, and both are needed (W3 §3.2):
//!
//! - **[`TaskTable::spawn`] returns `()`.** There is no handle to forget, no `let _ =`, no
//!   detached future. A component body cannot hold its own task.
//! - **There is no `remove`.** The only way out is [`TaskTable::stop`], whose signature is
//!   `async` — so "delete the map entry and log that it will stop on the next restart"
//!   (`RemoveController`, measured) is not a thing a caller can write, because the type
//!   offers nothing that does it.
//!
//! What this deletes, measured: the APRS controller's **private** `sync.WaitGroup`, which
//! makes its goroutines invisible to application shutdown while it receives no context at
//! all; and the three sites that call `wg.Add(1)` from *inside* a function already launched
//! with `go`, so `wg.Wait()` can return before they register. Neither is expressible here:
//! there is one table, registration happens in `spawn` before the future is handed to the
//! runtime, and the cancellation token is constructed by the table rather than by the
//! component.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{
    BuildCx, BuildFn, Capability, ComponentError, ComponentId, Cx, OnReading, Outcome,
    ReadingSubscription, Serve, Tick,
};
use mw_types::{Class, Classify, DisplayChain};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info_span, warn, Instrument};

use crate::clock::Occurrence;

/// Which attempt a task is, so the supervisor can tell a live task's report from the
/// trailing report of one it has already replaced.
///
/// Every event carries it, and the supervisor discards anything that is not the current
/// epoch. That is deliberately not a "we are expecting an exit" flag: a task that is
/// cancelled still reports, and that report arrives *after* `stop` has already awaited
/// the handle and decided the state -- so a flag has to be cleared by the arriving event,
/// and a flag that a lost event can leave set swallows the **next** real exit, leaving a
/// dead component reported as `Running`. An epoch cannot be poisoned: a stale report is
/// stale by construction.
pub type Epoch = u64;

/// What a running component task tells the supervisor.
///
/// One channel, because there is one owner of component state: the supervisor's single
/// `select!` loop. A component never sets its own state (W2a Decision 28); it reports an
/// outcome and the supervisor decides what that means.
#[derive(Debug)]
pub enum Event {
    /// The builder succeeded and the body is live.
    Ready {
        /// Whose.
        id: ComponentId,
        /// Which attempt.
        epoch: Epoch,
    },
    /// One occurrence finished.
    Occurred {
        /// Whose.
        id: ComponentId,
        /// Which attempt.
        epoch: Epoch,
        /// Which occurrence.
        occurrence: Occurrence,
        /// How it went.
        outcome: Outcome,
        /// The error, with its cause chain, when there was one.
        error: Option<String>,
        /// When it finished.
        at: DateTime<Utc>,
    },
    /// The task ended.
    Exited {
        /// Whose.
        id: ComponentId,
        /// Which attempt.
        epoch: Epoch,
        /// Why.
        exit: Exit,
    },
    /// A provider announced readiness with a handle.
    Publish {
        /// Who published.
        provider: ComponentId,
        /// What they published.
        capability: Capability,
        /// The type-erased handle.
        handle: Arc<dyn Any + Send + Sync>,
    },
    /// A provider withdrew a handle.
    Revoke {
        /// Who revoked.
        provider: ComponentId,
        /// What they withdrew.
        capability: Capability,
    },
}

/// Why a component task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// An `OnStartup` component completed its one occurrence. Terminal.
    Finished,
    /// The component was cancelled and returned.
    Cancelled,
    /// The builder failed. `Starting → Failed → Backoff → Starting`.
    BuildFailed(String),
    /// The body failed, or a `Serve` returned without being cancelled.
    Failed(String),
}

/// One spawned component.
#[derive(Debug)]
struct Task {
    token: CancellationToken,
    handle: JoinHandle<()>,
    /// The occurrence channel, for a `Tick` body. `None` for `Serve` and `OnReading`,
    /// which are not fired.
    fires: Option<mpsc::Sender<Fire>>,
    /// When this attempt started. §3.2 sketches this field and it earns its keep in one
    /// place: backoff resets after an uptime longer than the cap, and "how long was it up"
    /// is a fact about the task rather than about the component.
    started: Instant,
}

/// One occurrence, handed to a `Tick` body's task.
#[derive(Debug)]
pub struct Fire {
    /// Which occurrence.
    pub occurrence: Occurrence,
    /// How long it has.
    pub deadline: Duration,
}

/// What happened when a component was asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// It observed cancellation and returned inside its grace.
    Stopped,
    /// It did not, and was aborted. Counted as `stop_timeout_total`.
    Aborted,
    /// It had already unwound. The exit was still reported, because the report is in a
    /// `Drop` guard rather than in the body's own return path.
    Panicked,
    /// There was no task to stop.
    NotRunning,
}

/// Every running component task.
#[derive(Debug, Default)]
pub struct TaskTable {
    tasks: HashMap<ComponentId, Task>,
}

impl TaskTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn and retain. **Returns nothing** — there is no handle for a caller to drop.
    ///
    /// The future is instrumented with the component's id, which is what "a logger
    /// pre-tagged with its `ComponentId`" means in practice: every event a body emits
    /// carries the field, so per-component filtering needs no cooperation from the body
    /// (docs/service-architecture.md §5.4).
    pub fn spawn<F>(
        &mut self,
        id: ComponentId,
        token: CancellationToken,
        fires: Option<mpsc::Sender<Fire>>,
        body: F,
    ) where
        F: Future<Output = ()> + Send + 'static,
    {
        // `insert` would otherwise overwrite -- dropping the previous token (a drop is not
        // a cancel) and the previous handle (a drop is not an abort), leaving two live
        // bodies for one component. That is a removal path that does not await, which is
        // exactly what this type exists to make unexpressible, so it is asserted here as
        // well as guarded at every caller.
        debug_assert!(
            !self.tasks.contains_key(&id),
            "spawning `{id}` while it already has a task: stop it first"
        );
        if let Some(existing) = self.tasks.remove(&id) {
            // Release-build backstop. Cancelling and aborting is the best that can be done
            // without an await here; the caller is the bug.
            warn!(component = %id, "replacing a live task; the caller should have stopped it");
            existing.token.cancel();
            existing.handle.abort();
        }
        let span = info_span!("component", component = %id);
        // The one permitted spawn site, with `clock` (§3.3). `clippy.toml` forbids it
        // everywhere else, which is the lint half of this invariant.
        #[allow(clippy::disallowed_methods)]
        let handle = tokio::task::spawn(body.instrument(span));
        self.tasks.insert(
            id,
            Task {
                token,
                handle,
                fires,
                started: Instant::now(),
            },
        );
    }

    /// How long this component's current task has been running, if it has one.
    pub fn uptime(&self, id: &ComponentId) -> Option<Duration> {
        self.tasks.get(id).map(|t| t.started.elapsed())
    }

    /// The **only** way a task leaves the table.
    ///
    /// Cancel, await the handle within the grace deadline, then **abort and await the
    /// abort**. Resource release is `Drop` rather than a method, which is what makes
    /// "removing a controller leaks its listeners" structurally impossible rather than
    /// merely fixed (**D-24**).
    ///
    /// The second await is the part that is easy to leave out and the part that matters:
    /// `timeout(grace, handle)` on its own *drops* the handle when the grace elapses, and
    /// dropping a `JoinHandle` **detaches** the task rather than cancelling it. A body
    /// that ignores its token would then keep running -- past shutdown, or alongside the
    /// replacement a plan diff just started -- while `stop` reported `Aborted`.
    pub async fn stop(&mut self, id: &ComponentId, grace: Duration) -> StopOutcome {
        let Some(mut task) = self.tasks.remove(id) else {
            return StopOutcome::NotRunning;
        };
        task.token.cancel();
        // `&mut handle` rather than `handle`, so the handle survives the timeout and can
        // still be aborted.
        match tokio::time::timeout(grace, &mut task.handle).await {
            Ok(Ok(())) => {
                debug!(component = %id, "stopped");
                StopOutcome::Stopped
            }
            Ok(Err(e)) if e.is_panic() => {
                warn!(component = %id, "the body panicked");
                StopOutcome::Panicked
            }
            // Already cancelled by something else; the task is gone either way.
            Ok(Err(_)) => StopOutcome::Stopped,
            Err(_) => {
                task.handle.abort();
                // Awaiting the abort is what makes "the task is gone when this returns"
                // true rather than hopeful.
                let _ = task.handle.await;
                warn!(
                    component = %id,
                    grace_s = grace.as_secs(),
                    "aborted after grace"
                );
                StopOutcome::Aborted
            }
        }
    }

    /// Stop a whole rank, with **one** grace deadline for the rank rather than one each.
    ///
    /// §3.6 specifies siblings stopping concurrently, and the reason is a bound rather
    /// than speed: twenty components stopped in sequence is twenty times the grace, which
    /// outlives `TimeoutStopSec` and gets the daemon `SIGKILL`ed -- killing bodies
    /// mid-write, which is the thing the grace exists to prevent.
    ///
    /// Every token is cancelled first, so each body's grace runs concurrently with the
    /// others', and then each handle is awaited against the shared deadline.
    pub async fn stop_rank(
        &mut self,
        ids: &[ComponentId],
        grace: Duration,
    ) -> Vec<(ComponentId, StopOutcome)> {
        for id in ids {
            if let Some(task) = self.tasks.get(id) {
                task.token.cancel();
            }
        }
        let deadline = Instant::now() + grace;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let remaining = deadline.saturating_duration_since(Instant::now());
            out.push((id.clone(), self.stop(id, remaining).await));
        }
        out
    }

    /// Hand an occurrence to a running `Tick` body.
    ///
    /// `try_send` on a one-deep channel: if the body has not taken the previous occurrence
    /// the firing is dropped, which is `Skip` — the only overlap policy (W2a Decision 8).
    /// The supervisor's own `in_flight` check catches the ordinary case; this is the
    /// backstop, and both count the skip.
    pub fn fire(&self, id: &ComponentId, fire: Fire) -> bool {
        match self.tasks.get(id).and_then(|t| t.fires.as_ref()) {
            Some(tx) => tx.try_send(fire).is_ok(),
            None => false,
        }
    }

    /// Whether a component has a task.
    pub fn contains(&self, id: &ComponentId) -> bool {
        self.tasks.contains_key(id)
    }

    /// How many tasks are running.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether anything is running. Asserted after shutdown.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Every running component, for a shutdown that walks the plan in reverse.
    pub fn ids(&self) -> Vec<ComponentId> {
        self.tasks.keys().cloned().collect()
    }
}

/// Reports a task's events, and **guarantees exactly one exit event** for the task.
///
/// The guarantee is why this is a type rather than a function. §3.2 says "every spawned
/// future is wrapped so that on completion it sends `(id, Outcome)`" -- but a `send` at the
/// end of the body is *inside* the future, which is precisely where a panic escapes it. A
/// panicking body would then report nothing: no exit, no state change, `in_flight` stuck
/// true, and the component reported `Running` for the life of the process while its only
/// trace is a climbing `skipped_total`.
///
/// So the exit lives in `Drop`. A panic unwinds the future, which drops this, which
/// reports. An abort drops the future, which does the same. A body that returned normally
/// has already reported and `Drop` stays quiet.
struct Reporter {
    id: ComponentId,
    epoch: Epoch,
    events: mpsc::UnboundedSender<Event>,
    reported: bool,
}

impl Reporter {
    fn new(id: ComponentId, epoch: Epoch, events: mpsc::UnboundedSender<Event>) -> Self {
        Self {
            id,
            epoch,
            events,
            reported: false,
        }
    }

    /// A non-terminal event.
    fn send(&self, event: Event) {
        if self.events.send(event).is_err() {
            // The channel closes only once the supervisor's loop has finished, which is
            // after shutdown has already cancelled every task -- so this means "nobody is
            // listening any more", not "the event was lost".
            debug!(component = %self.id, "the supervisor is gone; event dropped");
        }
    }

    fn ready(&self) {
        self.send(Event::Ready {
            id: self.id.clone(),
            epoch: self.epoch,
        });
    }

    /// The task's one exit event.
    fn exit(&mut self, exit: Exit) {
        self.reported = true;
        self.send(Event::Exited {
            id: self.id.clone(),
            epoch: self.epoch,
            exit,
        });
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        if self.reported {
            return;
        }
        let exit = if std::thread::panicking() {
            Exit::Failed("the component body panicked".to_string())
        } else {
            // The future was dropped without returning: an abort, or a runtime shutting
            // down underneath it.
            Exit::Failed("the component task ended without reporting an outcome".to_string())
        };
        self.reported = true;
        let _ = self.events.send(Event::Exited {
            id: self.id.clone(),
            epoch: self.epoch,
            exit,
        });
    }
}

/// How deep a `Tick` body's occurrence channel is.
///
/// One. A deeper channel would be a queue of occurrences, and a queue that later fires as
/// a burst is exactly what the rate limits exist to prevent.
pub const FIRE_DEPTH: usize = 1;

/// Run a `Tick` body: build, then one occurrence per firing.
pub async fn run_tick(
    builder: BuildFn<Box<dyn Tick>>,
    bcx: BuildCx,
    cx: Cx,
    epoch: Epoch,
    once: bool,
    mut fires: mpsc::Receiver<Fire>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut reporter = Reporter::new(cx.id().clone(), epoch, events);
    let mut body = match builder(bcx).await {
        Ok(body) => body,
        Err(e) => {
            reporter.exit(Exit::BuildFailed(DisplayChain(&e).to_string()));
            return;
        }
    };
    reporter.ready();

    loop {
        // The firing is taken inside the `select!` and handled outside it, so the body's
        // mutable borrow does not overlap the receiver's.
        let fire = tokio::select! {
            _ = cx.cancelled() => None,
            fire = fires.recv() => fire,
        };
        let Some(fire) = fire else {
            reporter.exit(Exit::Cancelled);
            return;
        };

        let (occurrence, child) = occurrence_cx(&cx);
        let result = tokio::time::timeout(fire.deadline, body.tick(&occurrence)).await;
        let (outcome, error) = finish(result, &child);
        reporter.send(Event::Occurred {
            id: cx.id().clone(),
            epoch,
            occurrence: fire.occurrence,
            outcome,
            error,
            at: Utc::now(),
        });
        if once && outcome == Outcome::Ok {
            reporter.exit(Exit::Finished);
            return;
        }
    }
}

/// Run an `OnReading` body: build, then one delivery per batch.
///
/// A delivery failure ends the task, which is deliberate: the supervisor then applies
/// exponential backoff and starts it again, and because the subscription's cursor did not
/// advance, the backoff is a **retry of the same batch** (W3 §3.4) rather than a lost one.
pub async fn run_on_reading(
    builder: BuildFn<Box<dyn OnReading>>,
    bcx: BuildCx,
    cx: Cx,
    epoch: Epoch,
    mut subscription: Box<dyn ReadingSubscription>,
    deadline: Duration,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut reporter = Reporter::new(cx.id().clone(), epoch, events);
    let mut body = match builder(bcx).await {
        Ok(body) => body,
        Err(e) => {
            reporter.exit(Exit::BuildFailed(DisplayChain(&e).to_string()));
            return;
        }
    };
    reporter.ready();

    loop {
        let next = tokio::select! {
            _ = cx.cancelled() => None,
            batch = subscription.next_batch() => Some(batch),
        };
        let (Some(Some(batch)), false) = (next, cx.is_cancelled()) else {
            reporter.exit(Exit::Cancelled);
            return;
        };

        let (occurrence, child) = occurrence_cx(&cx);
        let result = tokio::time::timeout(deadline, body.readings(batch, &occurrence)).await;
        let (outcome, error) = finish(result, &child);
        reporter.send(Event::Occurred {
            id: cx.id().clone(),
            epoch,
            occurrence: Occurrence::Demand,
            outcome,
            error: error.clone(),
            at: Utc::now(),
        });
        match outcome {
            Outcome::Ok => {}
            // An `Ignorable` error is not a failure, and the same reasoning has to apply
            // here as in `run_serve`: a body that noticed cancellation mid-delivery is
            // stopping, not failing, and backing the component off for it would both
            // count a restart and replay a batch nobody is waiting for.
            Outcome::Skipped => {
                reporter.exit(Exit::Cancelled);
                return;
            }
            _ => {
                reporter.exit(Exit::Failed(error.unwrap_or_else(|| outcome.to_string())));
                return;
            }
        }
        // Acknowledged only on a handled batch, which is what makes a `Durable`
        // subscription's cursor mean "delivered" rather than "read".
        if let Err(e) = subscription.ack().await {
            reporter.exit(Exit::Failed(DisplayChain(&e).to_string()));
            return;
        }
    }
}

/// Run a `Serve` body: build, then its own loop until cancellation.
pub async fn run_serve(
    builder: BuildFn<Box<dyn Serve>>,
    bcx: BuildCx,
    cx: Cx,
    epoch: Epoch,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut reporter = Reporter::new(cx.id().clone(), epoch, events);
    let body = match builder(bcx).await {
        Ok(body) => body,
        Err(e) => {
            reporter.exit(Exit::BuildFailed(DisplayChain(&e).to_string()));
            return;
        }
    };
    reporter.ready();

    let cancelled = cx.token().clone();
    let exit = match body.serve(cx).await {
        Ok(()) if cancelled.is_cancelled() => Exit::Cancelled,
        // A service that returns on its own has stopped serving, which is a failure
        // however calmly it was spelled. Backoff restarts it.
        Ok(()) => Exit::Failed("the service loop returned without being cancelled".to_string()),
        Err(e) if e.class() == Class::Ignorable => Exit::Cancelled,
        Err(e) => Exit::Failed(DisplayChain(&e).to_string()),
    };
    reporter.exit(exit);
}

/// A context for one occurrence, with its own child token.
///
/// The token is a **child** of the component's: cancelling an occurrence on its deadline
/// must not kill the component, and cancelling the component must kill the occurrence (W3
/// §3.4).
fn occurrence_cx(cx: &Cx) -> (Cx, CancellationToken) {
    let child = cx.token().child_token();
    (
        Cx::new(
            cx.id().clone(),
            child.clone(),
            cx.generation(),
            cx.generation_watch(),
        ),
        child,
    )
}

/// Interpret one occurrence's result, cancelling its token if the deadline elapsed.
///
/// Dropping the body's future is what actually ends the work; `cancel()` is what a body
/// awaiting something cooperative -- a `tower` stack, a subscription, another component's
/// watch -- observes, and without it that call never learns the occurrence is over.
fn finish(
    result: Result<Result<(), ComponentError>, tokio::time::error::Elapsed>,
    child: &CancellationToken,
) -> (Outcome, Option<String>) {
    if result.is_err() {
        child.cancel();
    }
    interpret(result)
}

/// Map a body's result onto the five-word outcome vocabulary.
///
/// An `Ignorable` error is **not** a failure: `ComponentError::Cancelled` is what a body
/// returns when it noticed cancellation mid-occurrence, and recording that as `Failed`
/// would put a spurious failure in the last thing an operator sees in the log (W3 §3.6's
/// reason for walking shutdown in reverse order is the same concern).
fn interpret(
    result: Result<Result<(), ComponentError>, tokio::time::error::Elapsed>,
) -> (Outcome, Option<String>) {
    match result {
        Ok(Ok(())) => (Outcome::Ok, None),
        Ok(Err(e)) => {
            let rendered = DisplayChain(&e).to_string();
            match e.class() {
                Class::Ignorable => (Outcome::Skipped, Some(rendered)),
                _ => (Outcome::Failed, Some(rendered)),
            }
        }
        // `timeout` drops the body's future, which **is** the cancellation; the child
        // token exists so a body that spawned nothing but is waiting on something
        // cooperative can also notice.
        Err(_) => (
            Outcome::DeadlineExceeded,
            Some(ComponentError::DeadlineExceeded.to_string()),
        ),
    }
}
