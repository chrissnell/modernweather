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
use tokio_util::sync::CancellationToken;
use tracing::{debug, info_span, warn, Instrument};

use crate::clock::Occurrence;

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
    },
    /// One occurrence finished.
    Occurred {
        /// Whose.
        id: ComponentId,
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
            },
        );
    }

    /// The **only** way a task leaves the table.
    ///
    /// Cancel, then await the handle within the grace deadline, then abort. Resource
    /// release is `Drop` rather than a method, which is what makes "removing a controller
    /// leaks its listeners" structurally impossible rather than merely fixed (**D-24**).
    pub async fn stop(&mut self, id: &ComponentId, grace: Duration) -> StopOutcome {
        let Some(task) = self.tasks.remove(id) else {
            return StopOutcome::NotRunning;
        };
        task.token.cancel();
        match tokio::time::timeout(grace, task.handle).await {
            Ok(_) => {
                debug!(component = %id, "stopped");
                StopOutcome::Stopped
            }
            Err(_) => {
                warn!(
                    component = %id,
                    grace_s = grace.as_secs(),
                    "aborted after grace"
                );
                StopOutcome::Aborted
            }
        }
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
    once: bool,
    mut fires: mpsc::Receiver<Fire>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut body = match builder(bcx).await {
        Ok(body) => body,
        Err(e) => {
            send(&events, build_failed(cx.id(), &e));
            return;
        }
    };
    send(
        &events,
        Event::Ready {
            id: cx.id().clone(),
        },
    );

    loop {
        // The firing is taken inside the `select!` and handled outside it, so the body's
        // mutable borrow does not overlap the receiver's.
        let fire = tokio::select! {
            _ = cx.cancelled() => None,
            fire = fires.recv() => fire,
        };
        let Some(fire) = fire else {
            send(
                &events,
                Event::Exited {
                    id: cx.id().clone(),
                    exit: Exit::Cancelled,
                },
            );
            return;
        };

        // A child of the component's token: cancelling an occurrence on its deadline must
        // not kill the component, and cancelling the component must kill the occurrence
        // (W3 §3.4).
        let child = cx.token().child_token();
        let occurrence_cx = Cx::new(
            cx.id().clone(),
            child,
            cx.generation(),
            cx.generation_watch(),
        );
        let result = tokio::time::timeout(fire.deadline, body.tick(&occurrence_cx)).await;
        let (outcome, error) = interpret(result);
        send(
            &events,
            Event::Occurred {
                id: cx.id().clone(),
                occurrence: fire.occurrence,
                outcome,
                error,
                at: Utc::now(),
            },
        );
        if once && outcome == Outcome::Ok {
            send(
                &events,
                Event::Exited {
                    id: cx.id().clone(),
                    exit: Exit::Finished,
                },
            );
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
    mut subscription: Box<dyn ReadingSubscription>,
    deadline: Duration,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut body = match builder(bcx).await {
        Ok(body) => body,
        Err(e) => {
            send(&events, build_failed(cx.id(), &e));
            return;
        }
    };
    send(
        &events,
        Event::Ready {
            id: cx.id().clone(),
        },
    );

    loop {
        let next = tokio::select! {
            _ = cx.cancelled() => None,
            batch = subscription.next_batch() => Some(batch),
        };
        let (Some(Some(batch)), false) = (next, cx.is_cancelled()) else {
            send(
                &events,
                Event::Exited {
                    id: cx.id().clone(),
                    exit: Exit::Cancelled,
                },
            );
            return;
        };

        let child = cx.token().child_token();
        let occurrence_cx = Cx::new(
            cx.id().clone(),
            child,
            cx.generation(),
            cx.generation_watch(),
        );
        let result = tokio::time::timeout(deadline, body.readings(batch, &occurrence_cx)).await;
        let (outcome, error) = interpret(result);
        send(
            &events,
            Event::Occurred {
                id: cx.id().clone(),
                occurrence: Occurrence::Demand,
                outcome,
                error: error.clone(),
                at: Utc::now(),
            },
        );
        if outcome != Outcome::Ok {
            send(
                &events,
                Event::Exited {
                    id: cx.id().clone(),
                    exit: Exit::Failed(error.unwrap_or_else(|| outcome.to_string())),
                },
            );
            return;
        }
        // Acknowledged only on a handled batch, which is what makes a `Durable`
        // subscription's cursor mean "delivered" rather than "read".
        if let Err(e) = subscription.ack().await {
            send(
                &events,
                Event::Exited {
                    id: cx.id().clone(),
                    exit: Exit::Failed(DisplayChain(&e).to_string()),
                },
            );
            return;
        }
    }
}

/// Run a `Serve` body: build, then its own loop until cancellation.
pub async fn run_serve(
    builder: BuildFn<Box<dyn Serve>>,
    bcx: BuildCx,
    cx: Cx,
    events: mpsc::UnboundedSender<Event>,
) {
    let body = match builder(bcx).await {
        Ok(body) => body,
        Err(e) => {
            send(&events, build_failed(cx.id(), &e));
            return;
        }
    };
    send(
        &events,
        Event::Ready {
            id: cx.id().clone(),
        },
    );

    let id = cx.id().clone();
    let cancelled = cx.token().clone();
    let exit = match body.serve(cx).await {
        Ok(()) if cancelled.is_cancelled() => Exit::Cancelled,
        // A service that returns on its own has stopped serving, which is a failure
        // however calmly it was spelled. Backoff restarts it.
        Ok(()) => Exit::Failed("the service loop returned without being cancelled".to_string()),
        Err(e) if e.class() == Class::Ignorable => Exit::Cancelled,
        Err(e) => Exit::Failed(DisplayChain(&e).to_string()),
    };
    send(&events, Event::Exited { id, exit });
}

fn build_failed(id: &ComponentId, e: &mw_component::BuildError) -> Event {
    Event::Exited {
        id: id.clone(),
        exit: Exit::BuildFailed(DisplayChain(e).to_string()),
    }
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

/// Send an event, tolerating a closed channel.
///
/// The channel closes only when the supervisor has finished its loop, which happens after
/// shutdown has already cancelled every task — so a failed send here means "nobody is
/// listening any more", not "the event was lost".
fn send(events: &mpsc::UnboundedSender<Event>, event: Event) {
    if events.send(event).is_err() {
        debug!("the supervisor is gone; event dropped");
    }
}
