//! The supervisor: `plan()`, the hard-edge digraph, one timer heap, the task table,
//! capability watches, the `Spool` handle and the fan-out.
//!
//! It does not know its components (docs/service-architecture.md §1.3, rule 2). Each
//! component crate contributes one `fn register(cfg, out)` and the binary assembles the
//! list, so adding a component crate never edits this one and no `match` arm per
//! component kind can grow here -- which is the shape `managers/controller.go` took, down
//! to its `if con.Type == "management" { continue }`.
//!
//! One timer heap replaces 21 hand-rolled clocks, and `tokio::spawn` is reachable only
//! from this crate's `task` and `clock` modules (§1.4, §3.2).
//!
//! # The loop
//!
//! ```text
//! loop {
//!     select! {
//!         event   = events.recv()    => on_event(..)   // exits, outcomes, publications
//!         _       = sleep_until(due) => on_due(..)     // the one clock
//!         command = commands.recv()  => on_command(..) // ComponentControl
//!         plan    = plans.recv()     => apply_plan(..) // a configuration change
//!         _       = shutdown         => break
//!     }
//! }
//! ```
//!
//! One loop, one owner of state, no locks on component state at all.
//!
//! # What is **not** here yet
//!
//! The reading fan-out and the spool. `Trigger::OnReading` delivery goes through the
//! [`mw_component::ReadingStream`] handle, which this crate consumes and does not
//! implement: `Durable` requires a durable spool at the fan-out point (W2a Decision 62),
//! and that is implementation-plan.md M3-2's deliverable together with `Ingress` and the
//! ack protocol. What is fixed here is the boundary — the supervisor subscribes on the
//! component's behalf, applies the deadline, and acknowledges only a handled batch.

mod bus;
pub mod clock;
mod control;
pub mod plan;
pub mod publish;
pub mod task;
pub mod testing;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{
    stable_hash, BuildCx, Builder, Capability, CapabilityAccess, ComponentControl, ComponentId,
    ComponentSpec, ComponentStatus, Counters, Cx, Generation, LogControl, Need, OccurrenceKey,
    OccurrenceRecord, Outcome, ReadingStream, ScheduleState, ScheduleStore, State, Strength,
    Trigger, WaitingOn,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::bus::{Bus, SPINE};
use crate::clock::{Clock, Due, Occurrence};
use crate::plan::Plan;
use crate::task::{Event, Exit, Fire, StopOutcome, TaskTable, FIRE_DEPTH};

pub use crate::control::{Command, Control};
pub use crate::publish::{Publication, StateTable};

/// The root resources the supervisor is handed by `main`.
///
/// Exactly three, and the narrowness is the point (W3 §3.1): the SQLite pool is a root
/// resource owned by `main`, which hands a `ScheduleStore` over `component_schedule`
/// **alone** to the supervisor and the full handle to `config:sqlite`. Routing the
/// supervisor's own durable state through `ConfigStore` would mean the supervisor
/// depending on a component it supervises, which is the `AppReloader` cycle in a new coat.
pub struct Roots {
    /// The supervisor's own durable state: the latest `Calendar` occurrence per component.
    pub schedule: Arc<dyn ScheduleStore>,
    /// The reading fan-out, when this process has one. `None` is a legitimate
    /// configuration — a node that serves but does not ingest — and `plan()` then rejects
    /// any `OnReading` component rather than letting it start and never be delivered to.
    pub stream: Option<Arc<dyn ReadingStream>>,
    /// The runtime log-level handle (**D-52**), provided by `mw-obs`.
    pub log: Arc<dyn LogControl>,
}

impl std::fmt::Debug for Roots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Roots")
            .field("stream", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

impl Roots {
    /// Which root capabilities this process provides, for `plan()`.
    ///
    /// Derived rather than declared separately, so the set `plan()` validates against
    /// cannot drift from the set the supervisor actually publishes.
    pub fn capabilities(&self) -> plan::RootSet {
        let mut caps = vec![Capability::ComponentControl];
        if self.stream.is_some() {
            caps.push(Capability::ReadingStream);
        }
        plan::RootSet::new(caps)
    }
}

/// What `READY=1` and `STATUS=` are derived from (W3 §3.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    /// Whether the first full reconciliation pass has completed — every component has
    /// reached `Running`, `Waiting`, `Finished`, `Backoff` or `Stopped` at least once.
    ///
    /// Deliberately **not** "every component is `Running`": an uploader waiting on a dead
    /// database would then block readiness forever, and systemd would kill a daemon that
    /// is working exactly as D-27 specifies.
    pub converged: bool,
    /// The `STATUS=` line — "12 running, 2 waiting (db:postgres), 0 failed".
    pub status: String,
}

/// What a completed [`Supervisor::run`] reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    /// How many components stopped inside their grace.
    pub stopped: usize,
    /// How many had to be aborted. Each is also `stop_timeout_total` on its own record.
    pub aborted: usize,
    /// How many times the wall clock was observed stepping.
    pub clock_steps: u64,
}

/// One component's runtime record. The supervisor's state, and the only copy of it.
#[derive(Debug)]
struct Record {
    spec: ComponentSpec,
    state: State,
    since: DateTime<Utc>,
    applied_generation: Generation,
    counters: Counters,
    schedule: ScheduleState,
    last_error: Option<String>,
    attempt: u32,
    in_flight: bool,
    /// Set when the component should not be started again until asked — a `stop` through
    /// `ComponentControl`, or an `OnStartup` that has finished.
    held: bool,
    /// Set while a stop this supervisor asked for is in flight.
    ///
    /// A task that is cancelled still reports its exit, and that report arrives *after*
    /// `stop` has already awaited the handle and set the state. Without this flag the
    /// stale `Cancelled` would be read as "the body returned on its own" and the component
    /// would be backed off and restarted — which is what a revoked capability looked like
    /// before this field existed: `Backoff` where `Waiting` belonged.
    expect_exit: bool,
}

impl Record {
    fn new(spec: ComponentSpec) -> Self {
        let schedule = ScheduleState {
            period: match &spec.trigger {
                Trigger::Every { period, .. } => Some(*period),
                _ => None,
            },
            ..ScheduleState::default()
        };
        Self {
            spec,
            state: State::Planned,
            since: Utc::now(),
            applied_generation: Generation::ZERO,
            counters: Counters::default(),
            schedule,
            last_error: None,
            attempt: 0,
            in_flight: false,
            held: false,
            expect_exit: false,
        }
    }

    fn status(&self) -> ComponentStatus {
        ComponentStatus {
            id: self.spec.id.clone(),
            trigger: self.spec.trigger.kind(),
            state: self.state.clone(),
            since: self.since,
            applied_generation: self.applied_generation,
            schedule: self.schedule.clone(),
            counters: self.counters,
            last_error: self.last_error.clone(),
        }
    }

    /// Whether an occurrence may be delivered.
    ///
    /// `Backoff` is fireable, and that is a deliberate reading of W3 §3.4, whose
    /// pseudocode is self-contradictory on exactly this point: it skips an occurrence when
    /// `state != Running` *and* transitions a failed scheduled occurrence to
    /// `Backoff { next_occurrence }`. Taken literally, a 60-second uploader that failed
    /// once would never run again. W2a Decision 29 settles it — for `Every` and `Calendar`
    /// **the period is the backoff** — so a scheduled component in `Backoff` fires on its
    /// next occurrence, and the state is what an operator reads rather than a gate.
    fn is_fireable(&self) -> bool {
        matches!(self.state, State::Running { .. } | State::Backoff { .. })
    }
}

/// The supervisor.
#[derive(Debug)]
pub struct Supervisor {
    core: Core,
    events_rx: mpsc::UnboundedReceiver<Event>,
    commands_rx: mpsc::UnboundedReceiver<Command>,
    plans_rx: mpsc::UnboundedReceiver<Plan>,
    shutdown: CancellationToken,
}

/// Everything the loop's handlers mutate.
///
/// Split from [`Supervisor`] so the `select!` arms can borrow the receivers while the
/// handlers borrow the state — which is also an honest description of the design: the
/// channels are the only way in.
#[derive(Debug)]
struct Core {
    records: BTreeMap<ComponentId, Record>,
    order: Vec<Vec<ComponentId>>,
    providers: BTreeMap<Capability, Vec<ComponentId>>,
    bus: Bus,
    clock: Clock,
    tasks: TaskTable,
    events_tx: mpsc::UnboundedSender<Event>,
    published: Arc<Publication<StateTable>>,
    generation: Generation,
    generation_tx: watch::Sender<Generation>,
    roots: Roots,
    readiness: watch::Sender<Readiness>,
    occurrences: BTreeMap<ComponentId, OccurrenceRecord>,
    report: RunReport,
}

/// How a new plan reaches a running supervisor.
///
/// **Reconfiguration is a plan diff** (W2a Decision 25) and the supervisor is the thing
/// that applies it. What it cannot do is *produce* it: `plan()` is generic over the
/// configuration type, so re-planning happens where that type is known — the binary — and
/// the result arrives here. Nothing calls "reload": a `ConfigAdmin` write bumps the
/// generation, the binary re-plans, and this is the channel the new plan comes down.
#[derive(Debug, Clone)]
pub struct PlanSink(mpsc::UnboundedSender<Plan>);

impl PlanSink {
    /// Hand the supervisor a new plan. `Err` means the supervisor has stopped.
    pub fn apply(&self, plan: Plan) -> Result<(), Plan> {
        self.0.send(plan).map_err(|e| e.0)
    }
}

impl Supervisor {
    /// Assemble a supervisor over its roots.
    ///
    /// Returns the supervisor, the `ComponentControl` handle it publishes as a root
    /// capability, and the sink a new plan arrives through.
    pub fn new(
        roots: Roots,
        shutdown: CancellationToken,
    ) -> (Self, Arc<dyn ComponentControl>, PlanSink) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (plans_tx, plans_rx) = mpsc::unbounded_channel();
        let (generation_tx, _) = watch::channel(Generation::ZERO);
        let (readiness, _) = watch::channel(Readiness::default());

        let published = Arc::new(Publication::new(Arc::new(StateTable::default())));
        let control: Arc<dyn ComponentControl> = Arc::new(Control::new(
            Arc::clone(&published),
            commands_tx,
            Arc::clone(&roots.log),
        ));

        let mut bus = Bus::new(events_tx.clone());
        let spine = ComponentId::new(SPINE).expect("the spine's id is a literal");
        // The two roots, published before any component exists -- which is what makes the
        // supervisor incapable of participating in a cycle (W2a §2.5).
        bus.publish(
            spine.clone(),
            Capability::ComponentControl,
            Arc::new(Arc::clone(&control)),
        );
        if let Some(stream) = &roots.stream {
            bus.publish(
                spine,
                Capability::ReadingStream,
                Arc::new(Arc::clone(stream)),
            );
        }

        let core = Core {
            records: BTreeMap::new(),
            order: Vec::new(),
            providers: BTreeMap::new(),
            bus,
            clock: Clock::new(),
            tasks: TaskTable::new(),
            events_tx,
            published,
            generation: Generation::ZERO,
            generation_tx,
            roots,
            readiness,
            occurrences: BTreeMap::new(),
            report: RunReport::default(),
        };

        (
            Self {
                core,
                events_rx,
                commands_rx,
                plans_rx,
                shutdown,
            },
            control,
            PlanSink(plans_tx),
        )
    }

    /// A receiver for the readiness signal, for `main`'s `sd-notify` wiring.
    pub fn readiness(&self) -> watch::Receiver<Readiness> {
        self.core.readiness.subscribe()
    }

    /// The published state table, for a test or a diagnostic that wants to read it
    /// without going through the capability.
    pub fn published(&self) -> Arc<Publication<StateTable>> {
        Arc::clone(&self.core.published)
    }

    /// Run until the shutdown token is cancelled, then stop everything and report.
    ///
    /// Never returns before shutdown completes: the last thing it does is walk the plan in
    /// reverse topological order, so a provider outlives its consumers and a consumer
    /// never observes a revoked capability while it is trying to stop.
    pub async fn run(mut self, plan: Plan) -> RunReport {
        // The supervisor's own durable state, read once. A failure here is logged and not
        // fatal: the worst consequence is that a missed `Calendar` occurrence is made up
        // twice or not at all, and every `Calendar` user in this system is idempotent
        // (W2a Decision 14).
        match self.core.roots.schedule.all().await {
            Ok(records) => {
                self.core.occurrences = records
                    .into_iter()
                    .map(|r| (r.key.component.clone(), r))
                    .collect();
            }
            Err(e) => warn!(
                err = %mw_types::DisplayChain(&e),
                "the component schedule could not be read; missed occurrences will not be made up"
            ),
        }

        self.core.apply_plan(plan).await;

        loop {
            let next_due = self.core.clock.peek();
            let timer = async move {
                match next_due {
                    Some(at) => tokio::time::sleep_until(at).await,
                    // Nothing is scheduled. A `Service`-only plan is the ordinary case for
                    // a node that serves and does not upload.
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                () = self.shutdown.cancelled() => break,
                Some(event) = self.events_rx.recv() => self.core.on_event(event).await,
                () = timer => self.core.on_timer().await,
                Some(command) = self.commands_rx.recv() => self.core.on_command(command).await,
                Some(plan) = self.plans_rx.recv() => self.core.apply_plan(plan).await,
            }
        }

        self.core.shutdown().await;
        self.core.report.clock_steps = self.core.clock.clock_step_total;
        self.core.report
    }
}

impl Core {
    /// Install a plan: diff it against what is running, stop what is gone or changed,
    /// start what is new, and leave the rest alone.
    async fn apply_plan(&mut self, plan: Plan) {
        self.generation = plan.generation();
        self.generation_tx.send_replace(self.generation);

        let incoming: BTreeMap<ComponentId, ComponentSpec> = plan
            .specs()
            .iter()
            .map(|s| (s.id.clone(), s.clone()))
            .collect();
        self.order = plan.ranks().to_vec();
        self.providers = Capability::ALL
            .iter()
            .filter_map(|c| {
                let providers = plan.providers_of(*c);
                (!providers.is_empty()).then(|| (*c, providers.to_vec()))
            })
            .collect();

        // Removed, and changed, in reverse order so a provider is stopped after its
        // consumers.
        let removed: Vec<ComponentId> = self
            .records
            .keys()
            .filter(|id| !incoming.contains_key(*id))
            .cloned()
            .collect();
        let changed: Vec<ComponentId> = self
            .records
            .iter()
            .filter(|(id, record)| {
                incoming
                    .get(*id)
                    .is_some_and(|spec| spec.fingerprint() != record.spec.fingerprint())
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in removed.iter().chain(changed.iter()) {
            self.stop_component(id, "the plan changed").await;
        }
        for id in &removed {
            self.records.remove(id);
            self.clock.forget(id);
        }

        for (id, spec) in incoming {
            match self.records.get_mut(&id) {
                // Unchanged: not touched. This is the whole of "saving a website
                // configuration no longer disturbs the APRS schedule".
                Some(record) if record.spec.fingerprint() == spec.fingerprint() => {}
                Some(record) => {
                    record.spec = spec;
                    record.state = State::Planned;
                    record.attempt = 0;
                    record.held = false;
                    record.since = Utc::now();
                }
                None => {
                    self.records.insert(id, Record::new(spec));
                }
            }
        }

        self.reconcile().await;
    }

    /// Bring every record into the state its requirements allow.
    ///
    /// Walked in rank order, which is an optimisation rather than a correctness
    /// requirement: it avoids a flurry of `Waiting` transitions at boot, and a supervisor
    /// that started everything in random order would converge to the same state.
    async fn reconcile(&mut self) {
        let order: Vec<ComponentId> = if self.order.is_empty() {
            self.records.keys().cloned().collect()
        } else {
            self.order.iter().flatten().cloned().collect()
        };

        for id in order {
            let Some(record) = self.records.get(&id) else {
                continue;
            };
            if record.held || record.state.is_terminal() {
                continue;
            }
            let missing = self.missing_hard(&id);
            match (&record.state, missing.is_empty()) {
                // Nothing has been constructed and a hard requirement is unavailable.
                (State::Planned | State::Waiting { .. }, false) => {
                    self.transition(&id, State::Waiting { on: missing });
                }
                (State::Planned | State::Waiting { .. }, true) => self.start(&id).await,
                // A capability was revoked under a running component.
                (State::Running { .. } | State::Backoff { .. } | State::Starting { .. }, false) => {
                    self.stop_component(&id, "a hard requirement was revoked")
                        .await;
                    self.transition(&id, State::Waiting { on: missing });
                }
                (State::Running { .. }, true) => {
                    // Refresh `degraded`, because a soft requirement may have arrived or
                    // gone since the component started.
                    let degraded = self.degraded(&id);
                    if let Some(record) = self.records.get(&id) {
                        if record.state
                            != (State::Running {
                                degraded: degraded.clone(),
                            })
                        {
                            self.transition(&id, State::Running { degraded });
                        }
                    }
                }
                _ => {}
            }
        }
        self.publish_state();
    }

    /// Which of a component's hard requirements are unavailable, with their providers.
    fn missing_hard(&self, id: &ComponentId) -> Vec<WaitingOn> {
        let Some(record) = self.records.get(id) else {
            return Vec::new();
        };
        record
            .spec
            .hard_needs()
            .filter(|need| !self.bus.is_available(need.capability))
            .map(|need| WaitingOn {
                capability: need.capability,
                provider: self
                    .providers
                    .get(&need.capability)
                    .and_then(|p| p.first())
                    .cloned(),
            })
            .collect()
    }

    /// Which of a component's soft requirements are unavailable.
    fn degraded(&self, id: &ComponentId) -> Vec<Capability> {
        let Some(record) = self.records.get(id) else {
            return Vec::new();
        };
        record
            .spec
            .requires
            .iter()
            .filter(|need| {
                need.strength == Strength::Soft && !self.bus.is_available(need.capability)
            })
            .map(|need| need.capability)
            .collect()
    }

    /// Build and spawn one component.
    async fn start(&mut self, id: &ComponentId) {
        let Some(record) = self.records.get_mut(id) else {
            return;
        };
        let attempt = record.attempt.saturating_add(1);
        record.attempt = attempt;
        let spec = record.spec.clone();
        self.transition(id, State::Starting { attempt });

        let token = CancellationToken::new();
        let cx = Cx::new(
            id.clone(),
            token.clone(),
            self.generation,
            self.generation_tx.subscribe(),
        );

        let slots: Vec<_> = spec
            .requires
            .iter()
            .map(|need: &Need| (need.capability, self.bus.receiver(need.capability)))
            .chain(
                spec.provides
                    .iter()
                    .map(|p| (p.capability, self.bus.receiver(p.capability))),
            )
            .collect();
        let access = CapabilityAccess::new(
            spec.requires.clone().into(),
            spec.provides
                .iter()
                .map(|p| p.capability)
                .collect::<Vec<_>>()
                .into(),
            slots,
            self.bus.publisher(),
        );
        let bcx = BuildCx::new(cx.clone(), access);
        let events = self.events_tx.clone();

        match spec.build.clone() {
            Builder::Tick(builder) => {
                let (fires_tx, fires_rx) = mpsc::channel(FIRE_DEPTH);
                let once = matches!(spec.trigger, Trigger::OnStartup { .. });
                self.tasks.spawn(
                    id.clone(),
                    token,
                    Some(fires_tx),
                    task::run_tick(builder, bcx, cx, once, fires_rx, events),
                );
            }
            Builder::OnReading(builder) => {
                let Trigger::OnReading {
                    filter,
                    policy,
                    deadline,
                } = spec.trigger.clone()
                else {
                    // `plan()` has already rejected this, so reaching it would mean the
                    // check and the start path disagree.
                    error!(component = %id, "an OnReading body with another trigger");
                    return;
                };
                let Some(stream) = self.roots.stream.clone() else {
                    error!(component = %id, "no reading stream; plan() should have refused");
                    return;
                };
                match stream.subscribe(id, &filter, policy) {
                    Ok(subscription) => self.tasks.spawn(
                        id.clone(),
                        token,
                        None,
                        task::run_on_reading(builder, bcx, cx, subscription, deadline, events),
                    ),
                    Err(e) => {
                        let rendered = mw_types::DisplayChain(&e).to_string();
                        self.fail(id, rendered);
                    }
                }
            }
            Builder::Serve(builder) => {
                self.tasks.spawn(
                    id.clone(),
                    token,
                    None,
                    task::run_serve(builder, bcx, cx, events),
                );
            }
        }
    }

    /// Cancel a component, await its task inside the grace, and withdraw what it provided.
    async fn stop_component(&mut self, id: &ComponentId, why: &str) {
        let grace = self
            .records
            .get(id)
            .map_or(mw_component::DEFAULT_GRACE, |r| r.spec.grace);
        if self.tasks.contains(id) {
            debug!(component = %id, reason = why, "stopping");
            if let Some(record) = self.records.get_mut(id) {
                record.expect_exit = true;
            }
            self.transition(id, State::Stopping { since: Utc::now() });
            match self.tasks.stop(id, grace).await {
                StopOutcome::Stopped => self.report.stopped += 1,
                StopOutcome::Aborted => {
                    self.report.aborted += 1;
                    if let Some(record) = self.records.get_mut(id) {
                        record.counters.stop_timeout_total += 1;
                    }
                }
                StopOutcome::NotRunning => {}
            }
        }
        self.clock.forget(id);
        // A stopped provider's capabilities go with it, which is what moves its consumers
        // to `Waiting` rather than leaving them holding a handle to a dead thing.
        let revoked = self.bus.revoke_all(id);
        if let Some(record) = self.records.get_mut(id) {
            record.in_flight = false;
            record.state = State::Stopped;
            record.since = Utc::now();
        }
        if !revoked.is_empty() {
            debug!(component = %id, capabilities = revoked.len(), "capabilities withdrawn");
        }
    }

    async fn on_timer(&mut self) {
        // A stepped wall clock invalidates exactly the `Calendar` entries: `Every` is
        // monotonic by construction (W3 §3.3).
        if let Some(divergence) = self.clock.wall_step() {
            warn!(
                divergence_s = divergence.as_secs(),
                "the wall clock was stepped; recomputing calendar occurrences"
            );
            self.clock.forget_calendar();
            let now = Utc::now();
            let calendars: Vec<ComponentId> = self
                .records
                .iter()
                .filter(|(_, r)| matches!(r.spec.trigger, Trigger::Calendar { .. }))
                .map(|(id, _)| id.clone())
                .collect();
            for id in calendars {
                if let Some(record) = self.records.get(&id) {
                    let trigger = record.spec.trigger.clone();
                    self.clock.arm(&id, &trigger, now);
                }
            }
        }

        let due = self.clock.take_due(tokio::time::Instant::now());
        for item in due {
            self.on_due(item).await;
        }
        self.publish_state();
    }

    /// One firing.
    async fn on_due(&mut self, due: Due) {
        // **Rescheduled before the outcome is interpreted**, and before the state is even
        // consulted, so no branch below can leave a component without a next occurrence
        // (W3 §3.4).
        if let Some(record) = self.records.get(&due.id) {
            let trigger = record.spec.trigger.clone();
            self.clock.rearm_after(&due.id, &trigger, &due);
        }

        let Some(record) = self.records.get_mut(&due.id) else {
            return;
        };
        let deadline = record
            .spec
            .trigger
            .deadline()
            .unwrap_or(mw_component::DEFAULT_GRACE);

        if matches!(due.occurrence, Occurrence::Restart) {
            if !record.held {
                let id = due.id.clone();
                self.start(&id).await;
            }
            return;
        }

        if !record.is_fireable() || record.in_flight {
            record.counters.skipped_total += 1;
            record.schedule.last_outcome = Some(Outcome::Skipped);
            count(&due.id, "skipped_total", record.counters.skipped_total);
            return;
        }

        record.in_flight = true;
        let id = due.id.clone();
        let occurrence = due.occurrence.clone();
        if !self.tasks.fire(
            &id,
            Fire {
                occurrence,
                deadline,
            },
        ) {
            // The body has not taken the previous occurrence: `Skip`, counted.
            if let Some(record) = self.records.get_mut(&id) {
                record.in_flight = false;
                record.counters.skipped_total += 1;
                record.schedule.last_outcome = Some(Outcome::Skipped);
                count(&id, "skipped_total", record.counters.skipped_total);
            }
        }
    }

    async fn on_event(&mut self, event: Event) {
        match event {
            Event::Ready { id } => {
                if let Some(record) = self.records.get_mut(&id) {
                    record.attempt = 0;
                    record.applied_generation = self.generation;
                }
                let degraded = self.degraded(&id);
                self.transition(&id, State::Running { degraded });
                // Armed only once the body exists, so "next run at" is never a promise
                // about a component that has not been built.
                let now = Utc::now();
                let (trigger, misfire_needed) = match self.records.get(&id) {
                    Some(record) => (
                        record.spec.trigger.clone(),
                        matches!(record.spec.trigger, Trigger::Calendar { .. }),
                    ),
                    None => return,
                };
                if misfire_needed {
                    self.handle_misfire(&id, &trigger, now).await;
                }
                self.clock.arm(&id, &trigger, now);
                self.publish_state();
            }

            Event::Occurred {
                id,
                occurrence,
                outcome,
                error,
                at,
            } => {
                let next_run = self.next_run_of(&id);
                let Some(record) = self.records.get_mut(&id) else {
                    return;
                };
                record.in_flight = false;
                record.counters.runs_total += 1;
                record.schedule.last_run = Some(at);
                record.schedule.last_outcome = Some(outcome);
                record.schedule.next_run = next_run;
                record.last_error = error.clone();
                count(&id, "ticks_total", record.counters.runs_total);
                match outcome {
                    Outcome::Ok => {
                        let degraded = self.degraded(&id);
                        self.transition(&id, State::Running { degraded });
                    }
                    Outcome::Skipped => {
                        record.counters.skipped_total += 1;
                        count(&id, "skipped_total", record.counters.skipped_total);
                    }
                    Outcome::DeadlineExceeded => {
                        record.counters.deadline_exceeded_total += 1;
                        count(
                            &id,
                            "deadline_exceeded_total",
                            record.counters.deadline_exceeded_total,
                        );
                        self.fail_scheduled(&id, error);
                    }
                    Outcome::Failed => {
                        record.counters.failed_total += 1;
                        count(&id, "failed_total", record.counters.failed_total);
                        self.fail_scheduled(&id, error);
                    }
                    Outcome::Misfired => {}
                }
                self.record_occurrence(&id, &occurrence, outcome, at).await;
                self.publish_state();
            }

            Event::Exited { id, exit } => {
                self.on_exit(&id, exit).await;
                self.publish_state();
            }

            Event::Publish {
                provider,
                capability,
                handle,
            } => {
                let became_available = self.bus.publish(provider.clone(), capability, handle);
                debug!(%provider, %capability, became_available, "capability published");
                if became_available {
                    self.reconcile().await;
                } else {
                    self.publish_state();
                }
            }

            Event::Revoke {
                provider,
                capability,
            } => {
                let became_unavailable = self.bus.revoke(&provider, capability);
                if became_unavailable {
                    warn!(%provider, %capability, "capability revoked");
                    self.reconcile().await;
                } else {
                    self.publish_state();
                }
            }
        }
    }

    async fn on_exit(&mut self, id: &ComponentId, exit: Exit) {
        // A stop this supervisor asked for has already decided what the state is -- the
        // component may be `Stopped`, or `Waiting` on a capability that was revoked under
        // it, or `Starting` again after a plan diff -- and `stop` has already awaited the
        // handle and withdrawn what it provided. The exit report is then only an
        // acknowledgement, and it is checked **first**: by the time it arrives the table
        // may already hold the component's *next* task, and touching it here would stop
        // the replacement.
        if let Some(record) = self.records.get_mut(id) {
            if record.expect_exit {
                record.expect_exit = false;
                debug!(component = %id, ?exit, "expected exit");
                return;
            }
        }

        // An unexpected exit: withdraw what it provided before deciding what to do about
        // it, so a consumer is not left holding a handle to a dead provider.
        let revoked = self.bus.revoke_all(id);
        let _ = self.tasks.stop(id, Duration::ZERO).await;

        match exit {
            Exit::Finished => {
                self.clock.forget(id);
                if let Some(record) = self.records.get_mut(id) {
                    record.held = true;
                }
                self.transition(id, State::Finished { at: Utc::now() });
            }
            Exit::Cancelled => {
                if let Some(record) = self.records.get(id) {
                    // An expected stop has already set `Stopped`; anything else means the
                    // body returned on its own and the component should come back.
                    if matches!(record.state, State::Stopping { .. } | State::Stopped) {
                        self.transition(id, State::Stopped);
                    } else {
                        self.backoff(id, "the component returned".to_string());
                    }
                }
            }
            Exit::BuildFailed(error) | Exit::Failed(error) => self.backoff(id, error),
        }

        if !revoked.is_empty() {
            // Its consumers now have an unavailable hard need. One reconciliation pass is
            // enough because it walks the plan in rank order -- a provider is visited
            // before its consumers, so a cascade settles in that one pass rather than
            // needing `reconcile` to call itself.
            self.reconcile().await;
        }
    }

    /// A failed occurrence on a scheduled trigger: the period **is** the backoff (W2a
    /// Decision 29), so this records the failure and points `Backoff { until }` at the
    /// next occurrence rather than inventing a second clock.
    fn fail_scheduled(&mut self, id: &ComponentId, error: Option<String>) {
        let next = self.next_run_of(id);
        let Some(record) = self.records.get_mut(id) else {
            return;
        };
        let attempt = record.attempt.saturating_add(1);
        record.attempt = attempt;
        record.last_error = error.clone();
        match (next, &record.spec.trigger) {
            (Some(until), Trigger::Every { .. } | Trigger::Calendar { .. }) => {
                self.transition(id, State::Backoff { until, attempt });
            }
            _ => {
                self.transition(
                    id,
                    State::Failed {
                        error: error.unwrap_or_else(|| "the occurrence failed".into()),
                        at: Utc::now(),
                        attempt,
                    },
                );
            }
        }
    }

    /// A build failure, a `Serve` exit or an `OnReading` delivery failure: exponential
    /// backoff with full jitter, base 1 s, cap 60 s, and **no permanent give-up**.
    ///
    /// What makes retrying forever acceptable rather than a silent infinite loop is that
    /// `restart_total`, `attempt`, `last_error` and `Backoff { until }` are all state — a
    /// component restarting every minute for a day is *visible*, which is the thing that
    /// was missing, not the retrying (W2a Decision 30).
    fn backoff(&mut self, id: &ComponentId, error: String) {
        let Some(record) = self.records.get_mut(id) else {
            return;
        };
        if record.held {
            record.state = State::Stopped;
            return;
        }
        let attempt = record.attempt.max(1);
        record.counters.restart_total += 1;
        record.last_error = Some(error.clone());
        count(id, "restart_total", record.counters.restart_total);
        let delay = backoff_delay(id, attempt);
        let until = Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
        self.transition(
            id,
            State::Failed {
                error,
                at: Utc::now(),
                attempt,
            },
        );
        self.transition(id, State::Backoff { until, attempt });
        self.clock.push(Due {
            at: tokio::time::Instant::now() + delay,
            id: id.clone(),
            occurrence: Occurrence::Restart,
        });
    }

    /// A failure before a task existed — a subscription that could not be opened.
    fn fail(&mut self, id: &ComponentId, error: String) {
        self.backoff(id, error);
    }

    /// Make up, or deliberately do not make up, an occurrence the process was down for.
    async fn handle_misfire(&mut self, id: &ComponentId, trigger: &Trigger, now: DateTime<Utc>) {
        let Trigger::Calendar {
            spec, tz, misfire, ..
        } = trigger
        else {
            return;
        };
        let Some(zone) = tz.zone() else { return };
        let Some(previous) = clock::previous_calendar(spec, zone, now) else {
            return;
        };
        let key = OccurrenceKey::new(id.clone(), previous.local_date, previous.slot);
        if self.occurrences.get(id).is_some_and(|r| r.key == key) {
            // Already run -- which is also how fall-back's second pass is recognised,
            // because the key is the local date and slot rather than the instant.
            return;
        }
        let age = now
            .signed_duration_since(previous.at)
            .to_std()
            .unwrap_or_default();
        let within_grace = match misfire {
            mw_component::Misfire::RunLatest { grace } => age <= *grace,
            mw_component::Misfire::Skip => false,
        };
        if within_grace {
            info!(component = %id, age_s = age.as_secs(), "making up a missed occurrence");
            self.clock.push_now(id.clone(), Occurrence::Calendar(key));
        } else {
            if let Some(record) = self.records.get_mut(id) {
                record.counters.misfired_total += 1;
                record.schedule.last_outcome = Some(Outcome::Misfired);
                count(id, "misfired_total", record.counters.misfired_total);
            }
            warn!(
                component = %id,
                age_s = age.as_secs(),
                "an occurrence was missed and is past its grace"
            );
            self.record_occurrence(id, &Occurrence::Calendar(key), Outcome::Misfired, now)
                .await;
        }
    }

    /// Persist the latest occurrence, for the occurrences that have a key.
    async fn record_occurrence(
        &mut self,
        id: &ComponentId,
        occurrence: &Occurrence,
        outcome: Outcome,
        at: DateTime<Utc>,
    ) {
        let Some(key) = occurrence.key() else { return };
        let record = OccurrenceRecord {
            key: key.clone(),
            completed_at: at,
            outcome,
        };
        if let Err(e) = self.roots.schedule.record(&record).await {
            warn!(
                component = %id,
                err = %mw_types::DisplayChain(&e),
                "the occurrence could not be recorded; it may be made up again after a restart"
            );
        }
        self.occurrences.insert(id.clone(), record);
    }

    async fn on_command(&mut self, command: Command) {
        match command {
            Command::RunNow(id) => {
                let fireable = self.records.get(&id).is_some_and(Record::is_fireable);
                if fireable {
                    self.clock.push_now(id, Occurrence::Demand);
                } else {
                    debug!(component = %id, "run-now ignored: the component is not live");
                }
            }
            Command::Start(id) => {
                if let Some(record) = self.records.get_mut(&id) {
                    record.held = false;
                    record.attempt = 0;
                    record.state = State::Planned;
                }
                self.reconcile().await;
            }
            Command::Stop(id) => {
                if let Some(record) = self.records.get_mut(&id) {
                    record.held = true;
                }
                self.stop_component(&id, "asked through ComponentControl")
                    .await;
                self.publish_state();
            }
        }
    }

    /// Cancel everything in reverse topological order and await each inside its grace.
    async fn shutdown(&mut self) {
        info!(components = self.tasks.len(), "shutting down");
        let mut ranks = self.order.clone();
        ranks.reverse();
        if ranks.is_empty() {
            ranks = vec![self.tasks.ids()];
        }
        for rank in ranks {
            // Reverse order matters for one specific reason: a consumer must never observe
            // a *revoked* capability during shutdown, because that would move it to
            // `Waiting` while it is trying to stop and produce a spurious transition in
            // the last thing an operator sees in the log.
            for id in rank {
                if self.tasks.contains(&id) {
                    if let Some(record) = self.records.get_mut(&id) {
                        record.held = true;
                    }
                    self.stop_component(&id, "shutdown").await;
                }
            }
        }
        // Anything the plan's ranks did not name — a component added and not yet ranked.
        for id in self.tasks.ids() {
            self.stop_component(&id, "shutdown").await;
        }
        debug_assert!(self.tasks.is_empty(), "the task table outlived shutdown");
        self.publish_state();
    }

    fn transition(&mut self, id: &ComponentId, state: State) {
        let Some(record) = self.records.get_mut(id) else {
            return;
        };
        if record.state == state {
            return;
        }
        debug!(component = %id, from = %record.state, to = %state, "transition");
        record.state = state;
        record.since = Utc::now();
        gauge_state(id, &record.state);
    }

    fn next_run_of(&self, id: &ComponentId) -> Option<DateTime<Utc>> {
        let next = self.clock.next_for(id)?;
        let ahead = next.saturating_duration_since(tokio::time::Instant::now());
        chrono::Duration::from_std(ahead)
            .ok()
            .map(|d| Utc::now() + d)
    }

    /// Recompute and store the one published table, and refresh the readiness signal.
    fn publish_state(&mut self) {
        let mut components: Vec<ComponentStatus> = Vec::with_capacity(self.records.len());
        let ordered: Vec<ComponentId> = if self.order.is_empty() {
            self.records.keys().cloned().collect()
        } else {
            self.order.iter().flatten().cloned().collect()
        };
        for id in &ordered {
            if let Some(record) = self.records.get(id) {
                let mut status = record.status();
                status.schedule.next_run = self.next_run_of(id);
                components.push(status);
            }
        }
        // Anything not in the ranks yet, so the table never silently omits a component.
        for (id, record) in &self.records {
            if !ordered.contains(id) {
                components.push(record.status());
            }
        }

        // **One commit is one store.** The generation and the rows that describe it are
        // published together, so a reader cannot observe a component whose
        // `applied_generation` is ahead of the committed generation (W5 §2.2 depends on
        // that comparison being answerable from one load).
        self.published.store(Arc::new(StateTable {
            generation: self.generation,
            components,
        }));

        let readiness = self.readiness_now();
        self.readiness.send_replace(readiness);
    }

    fn readiness_now(&self) -> Readiness {
        let mut running = 0usize;
        let mut waiting: Vec<&ComponentId> = Vec::new();
        let mut failed = 0usize;
        let mut settled = 0usize;
        for record in self.records.values() {
            match &record.state {
                State::Running { .. } => running += 1,
                State::Waiting { on } => {
                    waiting.extend(on.iter().filter_map(|w| w.provider.as_ref()))
                }
                State::Failed { .. } | State::Backoff { .. } => failed += 1,
                _ => {}
            }
            if record.state.is_settled() {
                settled += 1;
            }
        }
        let waiting_names: Vec<&str> = {
            let mut names: Vec<&str> = waiting.iter().map(|id| id.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            names
        };
        let status = if waiting_names.is_empty() {
            format!("{running} running, 0 waiting, {failed} failed")
        } else {
            format!(
                "{running} running, {} waiting ({}), {failed} failed",
                waiting_names.len(),
                waiting_names.join(", ")
            )
        };
        Readiness {
            converged: settled == self.records.len(),
            status,
        }
    }
}

/// Exponential backoff with full jitter: base 1 s, cap 60 s.
///
/// The jitter is derived from the component id and the attempt rather than from a random
/// number generator, and that is a dependency decision as much as a design one: **D-32**'s
/// crate table has no RNG in it, and what jitter is for here is decorrelating two
/// components that failed at the same instant — which a hash of two things that differ
/// between them does perfectly well. It is also reproducible, which a test can use.
fn backoff_delay(id: &ComponentId, attempt: u32) -> Duration {
    const BASE_MS: u64 = 1_000;
    const CAP_MS: u64 = 60_000;
    let exponential = BASE_MS.saturating_mul(1u64 << attempt.min(6)).min(CAP_MS);
    let entropy = stable_hash(id.as_str().as_bytes()) ^ u64::from(attempt).wrapping_mul(0x9E37);
    Duration::from_millis(entropy % exponential.max(1))
}

/// `mw_component_state` — one series per state, so "how long has it been Waiting" is a
/// query rather than a guess (docs/service-architecture.md §5.10).
fn gauge_state(id: &ComponentId, state: &State) {
    for name in State::ALL {
        let value = f64::from(u8::from(*name == state.as_str()));
        metrics::gauge!("mw_component_state", "component" => id.as_str().to_string(), "state" => (*name).to_string())
            .set(value);
    }
}

/// A per-component counter, set absolutely from the record so the metric and the state
/// cannot drift apart.
fn count(id: &ComponentId, name: &'static str, value: u64) {
    let metric = match name {
        "ticks_total" => "mw_component_ticks_total",
        "failed_total" => "mw_component_failed_total",
        "skipped_total" => "mw_component_skipped_total",
        "deadline_exceeded_total" => "mw_component_deadline_exceeded_total",
        "misfired_total" => "mw_component_misfired_total",
        "restart_total" => "mw_component_restart_total",
        other => {
            debug_assert!(false, "unknown counter {other}");
            return;
        }
    };
    metrics::counter!(metric, "component" => id.as_str().to_string()).absolute(value);
}
