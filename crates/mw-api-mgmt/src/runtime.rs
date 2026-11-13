//! §2.3's runtime status resource and §2.5's root-cause rollup — **configured versus
//! running**, which is the most valuable thing this UI can gain and the one thing it is
//! incapable of today.
//!
//! # The three lies this replaces
//!
//! W5 §2.1 names them and W5 §2.2 names the mechanism: one integer, carried end to end.
//! `ConfigAdmin` assigns a monotonic `generation` to every committed write; every component
//! reports the `applied_generation` its **running instance was built from**; and the client
//! computes one derived fact — *a change is live when every affected component's
//! `applied_generation >= generation`.* Until then it is pending and the UI says so.
//!
//! That is small and it is sufficient, because it turns "did my change take effect?" into an
//! integer comparison **the server cannot fake**. A component that was not restarted keeps
//! its old `applied_generation` and is therefore visibly stale, which is exactly the
//! condition that is invisible today.
//!
//! # The server does the graph traversal
//!
//! §2.5: when the database goes away, `writer:timescale`, `window:rollup`, `aprs:*`, `pws:*`
//! and `wu:*` all move to `Waiting`, and an operator must see **one problem, not five**. The
//! traversal is here rather than in the client because a client doing it would be a second
//! implementation of the supervisor's own topology — and it is possible at all only because
//! W2a §3.1 put the **provider** on `WaitingOn` for this document.
//!
//! # What is deliberately not in this resource
//!
//! **`freshness` and `spool`.** §2.3's worked example shows both as `null`, and they are not
//! here at all rather than present and permanently null. Neither is computable from a
//! `ComponentStatus`: a freshness declaration needs the instant its capability was last
//! written, and a spool depth lives in `mw-pipeline`, which `ci/layers.toml` forbids this
//! crate from naming. [`crate::freshness`] is the renderer waiting for the first of them.
//! §4.4 makes every resource an object with named fields precisely so that adding them is
//! additive and needs no version bump.
//!
//! **Start and stop.** Not on this resource as a general capability, which is W5
//! **Decision 22**: `run-now` and `restart` are safe and reversible, and a general "stop this
//! component" button creates a running state that no configuration describes — a second
//! source of truth, which is the thing **D-28** and W2a §2.5 exist to remove. Turning
//! something off is a *configuration* change, which persists and survives a restart.

use std::collections::{BTreeMap, BTreeSet};

use mw_component::{ComponentId, ComponentStatus, State};

use crate::admin::Awaiting;
use crate::display::{self, Badge, Display, Vocabulary};

/// What is blocking a component, named as the thing it is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Blocker {
    /// The provider's internal id, when the plan names one. For keying and deep links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// What it is, on screen: "the weather database", not `ReadingStore`.
    pub provider_display: String,
    /// Where to go to look at it, when it has a page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
}

/// One component's schedule state.
///
/// **Real, not inferred.** W2a §1.3 puts the clock in the supervisor, so these are state;
/// today they are local variables inside `for { select { case <-ticker.C: } }` loops and are
/// not observable at all.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ScheduleView {
    /// The trigger's kind: `Every`, `Calendar`, `OnReading`, `OnStartup`, `Service`,
    /// `OnDemand`. Internal vocabulary, carried for the Advanced view and for keying; the
    /// sentence beside it is what a screen renders (**D-36**).
    pub kind: String,
    /// The sentence: "Runs every 10 minutes." `None` for `OnDemand`, which is a button
    /// rather than a schedule (§0.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sentence: Option<String>,
    /// The period in seconds, where the trigger has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_s: Option<u64>,
    /// When it last ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<chrono::DateTime<chrono::Utc>>,
    /// How that went: `ok`, `failed`, `skipped` or `deadline_exceeded`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    /// When it runs next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run: Option<chrono::DateTime<chrono::Utc>>,
}

/// §2.3's per-component counters.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct CountersView {
    /// Occurrences that ran, whatever the outcome.
    pub runs_total: u64,
    /// Occurrences that failed.
    pub failed_total: u64,
    /// Occurrences skipped because the previous one was still going.
    pub skipped_total: u64,
    /// Occurrences cancelled at their deadline.
    pub deadline_exceeded_total: u64,
    /// Scheduled occurrences missed while the daemon was not running.
    pub misfired_total: u64,
    /// Times this component has been rebuilt.
    pub restart_total: u64,
    /// Readings this component was too slow to receive.
    pub lagged_total: u64,
    /// Stops that had to be forced.
    pub stop_timeout_total: u64,
}

/// One component, as §2.3 returns it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ComponentView {
    /// The internal id. **Never rendered raw** outside `System → Advanced`; it travels for
    /// keying, deep links and support.
    pub id: String,
    /// What it is called on screen — the server does the translation, once (**D-36**).
    pub display: Display,
    /// The lifecycle state's one word: `planned`, `waiting`, `starting`, `running`,
    /// `failed`, `backoff`, `finished`, `stopping`, `stopped`.
    pub state: String,
    /// The badge §0.4 gives that state. Note that it is not the state's own word: `failed`
    /// is badged **Stopped** and `stopped` is badged **Off**.
    pub badge: Badge,
    /// The sentence §0.4 gives that state, in the operator's words.
    pub sentence: String,
    /// What it is waiting for, as things rather than capabilities.
    pub waiting_on: Vec<Blocker>,
    /// Which soft requirements are unavailable, as things. Empty is the ordinary case — and
    /// a degraded component **is running and serving** (W2a Decision 27).
    pub degraded: Vec<String>,
    /// Since when it has been in this state.
    pub since: chrono::DateTime<chrono::Utc>,
    /// When the next retry happens, for a `backoff` state.
    ///
    /// An **absolute timestamp**, because W5 **D-46** makes the only client-side timer a
    /// countdown rendering one of these: a server-supplied "12 seconds" is wrong by the time
    /// it is painted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The generation the **running instance** was built from (**D-53**).
    ///
    /// Per component, not global: a global "configuration version applied" would be wrong
    /// the moment one component restarts and another does not, which is the common case,
    /// since unchanged components are deliberately not touched.
    pub applied_generation: u64,
    /// Whether this component is still catching up with the change that was just saved.
    ///
    /// §2.2's derived fact, computed here as well as being computable by the client: the
    /// comparison is the whole mechanism, and a server that reports the integers but not
    /// their relation invites five clients to re-derive it five ways.
    ///
    /// **Scoped to the components the newest change affects** ([`Awaiting`]), not to every
    /// component below the newest generation. An unchanged component is deliberately not
    /// touched, so it is not behind -- it was never asked to move.
    pub pending: bool,
    /// Its schedule.
    pub schedule: ScheduleView,
    /// The last error, with its full cause chain (**D-34**), for the Details disclosure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Its counters.
    pub counters: CountersView,
    /// §0.4's counter sentences, each already filtered by its own "shown when" condition.
    pub notes: Vec<String>,
}

/// §2.3's resource: every component, and the newest committed generation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ComponentsView {
    /// The newest committed configuration generation.
    pub generation: u64,
    /// Every component in the plan, in id order.
    pub components: Vec<ComponentView>,
}

/// One thing a root cause is holding up.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Blocked {
    /// What it is on screen.
    pub display: String,
    /// Where to look at it.
    pub href: String,
}

/// One underlying problem, and everything waiting on it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RootCause {
    /// The internal id.
    pub id: String,
    /// What it is on screen.
    pub display: String,
    /// Its state's one word.
    pub state: String,
    /// One plain sentence: "Can't connect to the weather database. Retrying in 12 seconds."
    pub message: String,
    /// The technical half, for the Details disclosure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Since when.
    pub since: chrono::DateTime<chrono::Utc>,
    /// When the next retry happens, for the client's countdown (**D-46**).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Everything that is waiting on this, directly or through something else.
    pub blocking: Vec<Blocked>,
    /// What is **still working**, when anything is.
    ///
    /// Not reassurance padding: W3 §2a.6's measured result is that `/latest` is served from
    /// the in-memory snapshot and is untouched by a database outage, and this is exactly the
    /// thing an operator cannot currently determine during an incident. Computed from the
    /// listener's and the frozen surface's actual states rather than asserted, so it cannot
    /// claim the website is up while it is down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unaffected_note: Option<String>,
}

/// How many components are in each state.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Counts {
    /// Live.
    pub running: u64,
    /// Waiting on something.
    pub waiting: u64,
    /// Failed.
    pub failed: u64,
    /// Waiting out a retry delay.
    pub backoff: u64,
    /// Stopped, finished, or not started.
    pub off: u64,
}

/// §2.5's resource: the root-cause rollup Home renders.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SummaryView {
    /// The newest committed configuration generation.
    pub generation: u64,
    /// `ok`, `degraded` or `broken`.
    ///
    /// **`broken` means the public website is not serving**, and nothing else does: that is
    /// the one distinction an operator makes a different decision on. A failed uploader with
    /// a healthy site is `degraded`, because the thing the owner's visitors see is up.
    pub health: String,
    /// One entry per underlying problem — not one per symptom.
    pub root_causes: Vec<RootCause>,
    /// How many components are in each state.
    pub counts: Counts,
    /// How many components have not yet caught up with [`SummaryView::generation`].
    ///
    /// §2.4's `ApplyBanner { count }`: "Saved. 1 item is restarting…". Zero is "Saved and
    /// live."
    pub pending: u64,
}

/// Build §2.3's resource.
pub fn components(
    statuses: &[ComponentStatus],
    newest: u64,
    awaiting: &Awaiting,
    vocab: &Vocabulary,
    now: chrono::DateTime<chrono::Utc>,
) -> ComponentsView {
    let mut components: Vec<ComponentView> = statuses
        .iter()
        .map(|s| component(s, awaiting, vocab, now))
        .collect();
    components.sort_by(|a, b| a.id.cmp(&b.id));
    ComponentsView {
        generation: newest,
        components,
    }
}

/// Build one component's view.
pub fn component(
    status: &ComponentStatus,
    awaiting: &Awaiting,
    vocab: &Vocabulary,
    now: chrono::DateTime<chrono::Utc>,
) -> ComponentView {
    let waiting_on = match &status.state {
        State::Waiting { on } => on
            .iter()
            .map(|w| {
                let display = w.provider.as_ref().map(|p| vocab.describe(p));
                Blocker {
                    provider: w.provider.as_ref().map(|p| p.as_str().to_string()),
                    provider_display: display.as_ref().map_or_else(
                        || display::capability_as_a_thing(w.capability).to_string(),
                        Display::one_line,
                    ),
                    href: display.map(|d| d.href),
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    let degraded = match &status.state {
        State::Running { degraded } => degraded
            .iter()
            .map(|c| display::capability_as_a_thing(*c).to_string())
            .collect(),
        _ => Vec::new(),
    };
    let retry_at = match &status.state {
        State::Backoff { until, .. } => Some(*until),
        _ => None,
    };
    let applied = status.applied_generation.get();
    ComponentView {
        id: status.id.as_str().to_string(),
        display: vocab.describe(&status.id),
        state: status.state.as_str().to_string(),
        badge: display::badge_for(&status.state),
        sentence: display::sentence_for(&status.state, vocab, now),
        waiting_on,
        degraded,
        since: status.since,
        retry_at,
        applied_generation: applied,
        // Three conditions, and each one removes a way for the banner to spin for ever: the
        // change has to have asked for this component, the component has to be one the
        // supervisor is still trying to get to `Running` (a stopped one has no instance to
        // report an applied generation), and it has to not have caught up yet.
        pending: is_pending(status, awaiting),
        schedule: ScheduleView {
            kind: status.trigger.to_string(),
            sentence: display::schedule_sentence(status.trigger, status.schedule.period),
            period_s: status.schedule.period.map(|p| p.as_secs()),
            last_run: status.schedule.last_run,
            last_outcome: status.schedule.last_outcome.map(|o| o.as_str().to_string()),
            next_run: status.schedule.next_run,
        },
        last_error: status.last_error.clone(),
        counters: CountersView {
            runs_total: status.counters.runs_total,
            failed_total: status.counters.failed_total,
            skipped_total: status.counters.skipped_total,
            deadline_exceeded_total: status.counters.deadline_exceeded_total,
            misfired_total: status.counters.misfired_total,
            restart_total: status.counters.restart_total,
            lagged_total: status.counters.lagged_total,
            stop_timeout_total: status.counters.stop_timeout_total,
        },
        notes: display::counter_notes(status),
    }
}

/// Whether this component has not yet caught up with the change that was just saved.
fn is_pending(status: &ComponentStatus, awaiting: &Awaiting) -> bool {
    awaiting.wants(status.id.as_str())
        && status.applied_generation.get() < awaiting.generation
        && is_meant_to_run(&status.state)
}

/// Whether a state is one the supervisor is still trying to get to `Running`.
///
/// `Stopped` and `Finished` are terminal and `Planned` has nothing built, so none of the
/// three can report an applied generation — which is why none of them counts as pending.
const fn is_meant_to_run(state: &State) -> bool {
    matches!(
        state,
        State::Waiting { .. }
            | State::Starting { .. }
            | State::Running { .. }
            | State::Failed { .. }
            | State::Backoff { .. }
            | State::Stopping { .. }
    )
}

/// Build §2.5's resource.
///
/// The traversal, in order:
///
/// 1. Collect the direct edges: a `Waiting` component names the provider it waits on.
/// 2. A component is **unhealthy** if it is `Failed`, in `Backoff`, or `Waiting`.
/// 3. A **root cause** is an unhealthy component that is *not itself waiting on a provider
///    the plan names* — its problem is its own. That is what collapses five symptoms into
///    one.
/// 4. Each root cause's `blocking` list is the transitive set of components that reach it
///    through those edges, rendered as things with pages.
pub fn summary(
    statuses: &[ComponentStatus],
    newest: u64,
    awaiting: &Awaiting,
    vocab: &Vocabulary,
    now: chrono::DateTime<chrono::Utc>,
) -> SummaryView {
    let by_id: BTreeMap<&ComponentId, &ComponentStatus> =
        statuses.iter().map(|s| (&s.id, s)).collect();

    // Step 1. `waits_on[x]` is every provider `x` is blocked behind. Only providers the plan
    // actually names: a `Waiting` on a capability nothing provides is `x`'s own problem --
    // the configuration does not contain a provider -- so it stays a root cause rather than
    // pointing at a component that does not exist.
    let mut waits_on: BTreeMap<&ComponentId, Vec<&ComponentId>> = BTreeMap::new();
    for s in statuses {
        if let State::Waiting { on } = &s.state {
            let providers: Vec<&ComponentId> = on
                .iter()
                .filter_map(|w| w.provider.as_ref())
                .filter(|p| by_id.contains_key(*p))
                .collect();
            if !providers.is_empty() {
                waits_on.insert(&s.id, providers);
            }
        }
    }

    // Step 4's edges, reversed once rather than per root cause.
    let mut blocked_by: BTreeMap<&ComponentId, Vec<&ComponentId>> = BTreeMap::new();
    for (waiter, providers) in &waits_on {
        for p in providers {
            blocked_by.entry(p).or_default().push(waiter);
        }
    }

    let mut counts = Counts::default();
    let mut pending = 0u64;
    for s in statuses {
        match &s.state {
            State::Running { .. } => counts.running += 1,
            State::Waiting { .. } => counts.waiting += 1,
            State::Failed { .. } => counts.failed += 1,
            State::Backoff { .. } => counts.backoff += 1,
            State::Stopped | State::Finished { .. } | State::Planned => counts.off += 1,
            State::Starting { .. } | State::Stopping { .. } => {}
        }
        if is_pending(s, awaiting) {
            pending += 1;
        }
    }

    // Steps 2 and 3.
    let mut root_causes = Vec::new();
    for s in statuses {
        let unhealthy = matches!(
            s.state,
            State::Failed { .. } | State::Backoff { .. } | State::Waiting { .. }
        );
        if !unhealthy || waits_on.contains_key(&s.id) {
            continue;
        }
        let mut blocking: Vec<Blocked> = Vec::new();
        let mut seen: BTreeSet<&ComponentId> = BTreeSet::new();
        let mut stack: Vec<&ComponentId> = blocked_by.get(&s.id).cloned().unwrap_or_default();
        while let Some(next) = stack.pop() {
            if !seen.insert(next) {
                continue;
            }
            let d = vocab.describe(next);
            blocking.push(Blocked {
                display: d.one_line(),
                href: d.href,
            });
            if let Some(more) = blocked_by.get(next) {
                stack.extend(more.iter().copied());
            }
        }
        blocking.sort_by(|a, b| a.display.cmp(&b.display));
        let d = vocab.describe(&s.id);
        root_causes.push(RootCause {
            id: s.id.as_str().to_string(),
            display: d.one_line(),
            state: s.state.as_str().to_string(),
            message: display::sentence_for(&s.state, vocab, now),
            detail: s.last_error.clone(),
            since: s.since,
            retry_at: match &s.state {
                State::Backoff { until, .. } => Some(*until),
                _ => None,
            },
            blocking,
            unaffected_note: unaffected_note(statuses),
        });
    }
    root_causes.sort_by(|a, b| {
        b.blocking
            .len()
            .cmp(&a.blocking.len())
            .then(a.id.cmp(&b.id))
    });

    SummaryView {
        generation: newest,
        health: health(statuses, &root_causes).to_string(),
        root_causes,
        counts,
        pending,
    }
}

/// `ok`, `degraded` or `broken`.
///
/// **`broken` is reserved for the public website not serving.** The alternative readings --
/// "anything failed" or "a majority failed" -- both collapse the one distinction the operator
/// acts on differently: an uploader that cannot authenticate is an afternoon's work, and a
/// site that is down is now.
fn health(statuses: &[ComponentStatus], root_causes: &[RootCause]) -> &'static str {
    if !serving(statuses) {
        return "broken";
    }
    let degraded = statuses.iter().any(|s| {
        matches!(&s.state, State::Running { degraded } if !degraded.is_empty())
            || matches!(
                s.state,
                State::Failed { .. } | State::Backoff { .. } | State::Waiting { .. }
            )
    });
    if degraded || !root_causes.is_empty() {
        return "degraded";
    }
    "ok"
}

/// Whether the one listener is up, which is what "the website is working" means.
///
/// `http:listener` is **unconditional** (`mw_http::register`'s own words: there is no node
/// that should not be listening), so its absence from the plan is a wiring mistake rather
/// than a configuration -- and in that case there is nothing to claim is serving.
fn serving(statuses: &[ComponentStatus]) -> bool {
    statuses
        .iter()
        .find(|s| s.id.as_str() == "http:listener")
        .is_some_and(|s| s.state.is_running())
}

/// §2.5's last line, computed rather than asserted.
fn unaffected_note(statuses: &[ComponentStatus]) -> Option<String> {
    let running = |id: &str| {
        statuses
            .iter()
            .find(|s| s.id.as_str() == id)
            .is_some_and(|s| s.state.is_running())
    };
    // `api:legacy`'s only hard need is the snapshot, whose provider has no hard need of its
    // own -- which is **D-48**, and the reason the frozen routes keep answering with the
    // database gone. This sentence is that property, said to the operator.
    (running("http:listener") && running("api:legacy"))
        .then(|| "Your website and the phone app are still working.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::{Capability, Counters, Generation, ScheduleState, WaitingOn};

    fn id(s: &str) -> ComponentId {
        ComponentId::new(s).expect("a literal id")
    }

    fn status(id_str: &str, state: State) -> ComponentStatus {
        ComponentStatus {
            id: id(id_str),
            trigger: "Service",
            state,
            since: chrono::Utc::now(),
            applied_generation: Generation::new(42),
            schedule: ScheduleState::default(),
            counters: Counters::default(),
            last_error: None,
        }
    }

    fn waiting(on: &[(&str, Capability)]) -> State {
        State::Waiting {
            on: on
                .iter()
                .map(|(p, c)| WaitingOn {
                    capability: *c,
                    provider: Some(id(p)),
                })
                .collect(),
        }
    }

    fn vocab() -> Vocabulary {
        Vocabulary::default()
    }

    /// The set a just-saved change is waiting on, as `ConfigAdmin` records it.
    fn awaiting(generation: u64, ids: &[&str]) -> Awaiting {
        let mut ids: Vec<String> = ids.iter().map(|s| (*s).to_string()).collect();
        ids.sort();
        Awaiting { generation, ids }
    }

    /// **§2.5's worked example.** The database goes away and five things move to `Waiting`.
    /// The operator must see **one** problem with five things behind it, which is the whole
    /// reason the traversal is on the server.
    #[test]
    fn one_outage_is_one_root_cause_and_not_five() {
        let now = chrono::Utc::now();
        let statuses = vec![
            status(
                "db:postgres",
                State::Backoff {
                    until: now + chrono::Duration::seconds(12),
                    attempt: 3,
                },
            ),
            status(
                "writer:timescale",
                waiting(&[("db:postgres", Capability::ReadingStore)]),
            ),
            status(
                "window:rollup",
                waiting(&[("db:postgres", Capability::ReadingStore)]),
            ),
            status(
                "wu:backyard",
                waiting(&[("db:postgres", Capability::ReadingStore)]),
            ),
            status(
                "pws:backyard",
                waiting(&[("db:postgres", Capability::ReadingStore)]),
            ),
            status(
                "aprs:backyard",
                waiting(&[("db:postgres", Capability::ReadingStore)]),
            ),
            status(
                "http:listener",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
            status(
                "api:legacy",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
        ];
        let s = summary(&statuses, 42, &Awaiting::default(), &vocab(), now);
        assert_eq!(s.root_causes.len(), 1, "{:?}", s.root_causes);
        let rc = &s.root_causes[0];
        assert_eq!(rc.id, "db:postgres");
        assert_eq!(rc.blocking.len(), 5);
        assert!(rc.message.contains("Retrying in"));
        assert_eq!(rc.retry_at, Some(now + chrono::Duration::seconds(12)));
        // W3 §2a.6's measured result, said to the operator rather than asserted by it.
        assert_eq!(
            rc.unaffected_note.as_deref(),
            Some("Your website and the phone app are still working.")
        );
        // The site is up, so the owner's visitors see weather: `degraded`, not `broken`.
        assert_eq!(s.health, "degraded");
        assert_eq!(s.counts.waiting, 5);
        assert_eq!(s.counts.backoff, 1);
        assert_eq!(s.counts.running, 2);
    }

    /// A chain — `a` waits on `b`, `b` waits on `c` — collapses to `c`, with both of the
    /// others behind it. Without the transitive step the operator sees two problems and
    /// fixes neither.
    #[test]
    fn a_chain_collapses_to_its_far_end() {
        let now = chrono::Utc::now();
        let statuses = vec![
            status(
                "http:listener",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
            status("a:one", waiting(&[("b:two", Capability::WindowedView)])),
            status("b:two", waiting(&[("c:three", Capability::ReadingStore)])),
            status(
                "c:three",
                State::Failed {
                    error: "connection refused".into(),
                    at: now,
                    attempt: 1,
                },
            ),
        ];
        let s = summary(&statuses, 1, &Awaiting::default(), &vocab(), now);
        assert_eq!(s.root_causes.len(), 1);
        assert_eq!(s.root_causes[0].id, "c:three");
        assert_eq!(s.root_causes[0].blocking.len(), 2);
    }

    /// A `Waiting` on a capability **nothing provides** is the component's own problem: the
    /// configuration does not contain a provider, and pointing at a component that is not in
    /// the plan would be a link to nowhere.
    #[test]
    fn waiting_on_a_provider_the_plan_does_not_have_stays_its_own_problem() {
        let now = chrono::Utc::now();
        let statuses = vec![
            status(
                "http:listener",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
            status(
                "wu:backyard",
                State::Waiting {
                    on: vec![WaitingOn {
                        capability: Capability::ReadingStore,
                        provider: None,
                    }],
                },
            ),
        ];
        let s = summary(&statuses, 1, &Awaiting::default(), &vocab(), now);
        assert_eq!(s.root_causes.len(), 1);
        assert_eq!(s.root_causes[0].id, "wu:backyard");
        assert!(s.root_causes[0].blocking.is_empty());
    }

    /// `broken` is the website being down and nothing else, because that is the one state
    /// the operator acts on differently.
    #[test]
    fn broken_means_the_website_is_not_serving() {
        let now = chrono::Utc::now();
        let down = vec![status(
            "http:listener",
            State::Backoff {
                until: now,
                attempt: 2,
            },
        )];
        assert_eq!(
            summary(&down, 1, &Awaiting::default(), &vocab(), now).health,
            "broken"
        );

        let healthy = vec![
            status(
                "http:listener",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
            status(
                "api:legacy",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
        ];
        assert_eq!(
            summary(&healthy, 1, &Awaiting::default(), &vocab(), now).health,
            "ok"
        );
    }

    /// §2.2's derived fact, and the two ways it spins §2.4's banner for ever if it is got
    /// wrong.
    ///
    /// A component the change **asked for** and that has not caught up is pending. A
    /// `Stopped` one is not, because it has no running instance to report a generation. And
    /// -- the one an end-to-end test caught -- a component the change did **not** affect is
    /// not pending either, however far behind its `applied_generation` is: a plan diff
    /// deliberately does not touch it, so it was never asked to move. Counting it would mean
    /// every save after the first reported progress for ever.
    #[test]
    fn a_stopped_component_is_never_pending() {
        let now = chrono::Utc::now();
        let mut stale = status(
            "wu:backyard",
            State::Running {
                degraded: Vec::new(),
            },
        );
        stale.applied_generation = Generation::new(41);
        let mut off = status("pws:backyard", State::Stopped);
        off.applied_generation = Generation::new(41);
        let statuses = vec![
            status(
                "http:listener",
                State::Running {
                    degraded: Vec::new(),
                },
            ),
            stale,
            off,
        ];
        let v = components(
            &statuses,
            42,
            &awaiting(42, &["wu:backyard", "pws:backyard"]),
            &vocab(),
            now,
        );
        let by = |id: &str| {
            v.components
                .iter()
                .find(|c| c.id == id)
                .expect("the component is in the view")
        };
        assert!(by("wu:backyard").pending);
        assert!(!by("pws:backyard").pending);
        assert_eq!(
            summary(
                &statuses,
                42,
                &awaiting(42, &["wu:backyard", "pws:backyard"]),
                &vocab(),
                now
            )
            .pending,
            1
        );

        // The untouched component. `http:listener` is at generation 0 and the newest is 42,
        // and it is not pending, because the change never named it.
        let untouched = components(
            &statuses,
            42,
            &awaiting(42, &["wu:backyard"]),
            &vocab(),
            now,
        );
        assert!(untouched
            .components
            .iter()
            .all(|c| c.id == "wu:backyard" || !c.pending));
        assert_eq!(
            summary(
                &statuses,
                42,
                &awaiting(42, &["wu:backyard"]),
                &vocab(),
                now
            )
            .pending,
            1
        );

        // And before anything has been saved, nothing is outstanding at all.
        assert_eq!(
            summary(&statuses, 42, &Awaiting::default(), &vocab(), now).pending,
            0
        );
    }

    /// The blocker is named as the **thing**, with a page to go to — which is what makes
    /// §2.5's "waiting for the weather database" actionable where "waiting on ReadingStore"
    /// is not.
    #[test]
    fn a_blocker_carries_the_page_it_is_on() {
        let now = chrono::Utc::now();
        let s = status(
            "wu:backyard",
            waiting(&[("db:postgres", Capability::ReadingStore)]),
        );
        let v = component(&s, &Awaiting::default(), &vocab(), now);
        assert_eq!(v.waiting_on.len(), 1);
        assert_eq!(v.waiting_on[0].provider_display, "the weather database");
        assert_eq!(v.waiting_on[0].href.as_deref(), Some("/data-storage"));
        assert_eq!(v.badge.label, "Waiting");
    }
}
