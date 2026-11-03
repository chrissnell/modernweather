//! The lifecycle states of W2a §3, and the status the management API renders.
//!
//! **A component never sets its own state.** It reports an outcome by one of exactly three
//! means — returning `Ok`, returning `Err`, or being cancelled — and the supervisor drives
//! every transition (W2a Decision 28). There is no `set_state`, no health method and no
//! `IsRunning`. That deletes a category of code: APRS invented `Start`, `Stop`,
//! `IsRunning` and `GetHealth` because it needed them, but the `Controller` interface
//! declares only `StartController()`, so none of them is reachable through the interface
//! and `Stop()` is dead code — while a second health path runs in parallel through a
//! package-level `GlobalHealthManager` updated by a 90-second ticker inside the APRS
//! controller. **Component state is the health surface.**

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::capability::Capability;
use crate::error::Outcome;
use crate::id::ComponentId;

/// What a hard requirement is blocked on.
///
/// Carries the **provider** and not just the capability, which is the thing W2a §3.1 added
/// for W5: "waiting on `ReadingStore`" is internal jargon, and "waiting for the weather
/// database (`db:postgres`), which is retrying in 12 s" is a sentence an operator can act
/// on — reachable from the first only if the state carries the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingOn {
    /// The unavailable capability.
    pub capability: Capability,
    /// Who in the plan provides it, when the plan names anyone.
    pub provider: Option<ComponentId>,
}

/// A component's lifecycle state.
///
/// `degraded` is a field on `Running` rather than a state of its own: a degraded component
/// is running and serving, and a state machine that distinguished them would force every
/// consumer to handle both (W2a Decision 27). The website with Aeris down is
/// `Running { degraded: [ForecastStore, AlertStore] }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// In the plan, not yet considered.
    Planned,
    /// A hard requirement is unavailable. **Nothing has been constructed.**
    Waiting {
        /// What is missing, and who provides it.
        on: Vec<WaitingOn>,
    },
    /// The builder is running.
    Starting {
        /// Which attempt this is, counting from 1.
        attempt: u32,
    },
    /// Live.
    Running {
        /// Which soft requirements are unavailable. Empty is the ordinary case.
        degraded: Vec<Capability>,
    },
    /// The last attempt or occurrence failed.
    Failed {
        /// The error, rendered with its full cause chain (**D-34**) at the point it was
        /// recorded, because the typed error itself is not `Clone`.
        error: String,
        /// When.
        at: DateTime<Utc>,
        /// Which attempt.
        attempt: u32,
    },
    /// Waiting out a delay before trying again.
    Backoff {
        /// When it will retry.
        until: DateTime<Utc>,
        /// Which attempt the retry will be.
        attempt: u32,
    },
    /// `OnStartup` completed, or a removed component stopped cleanly. Terminal.
    Finished {
        /// When.
        at: DateTime<Utc>,
    },
    /// Cancelled, awaiting its task.
    Stopping {
        /// Since when, so a stop that is taking too long is visible rather than inferred.
        since: DateTime<Utc>,
    },
    /// Terminal.
    Stopped,
}

impl State {
    /// The one-word name W5 renders and `mw_component_state` labels.
    pub const fn as_str(&self) -> &'static str {
        match self {
            State::Planned => "planned",
            State::Waiting { .. } => "waiting",
            State::Starting { .. } => "starting",
            State::Running { .. } => "running",
            State::Failed { .. } => "failed",
            State::Backoff { .. } => "backoff",
            State::Finished { .. } => "finished",
            State::Stopping { .. } => "stopping",
            State::Stopped => "stopped",
        }
    }

    /// Every state name, so the `mw_component_state` gauge can publish one series per
    /// state rather than a single series whose value is an enum — which is what makes "how
    /// long has it been Waiting" a query (§5.10).
    pub const ALL: &'static [&'static str] = &[
        "planned", "waiting", "starting", "running", "failed", "backoff", "finished", "stopping",
        "stopped",
    ];

    /// Whether the component is live and may be fired.
    pub const fn is_running(&self) -> bool {
        matches!(self, State::Running { .. })
    }

    /// Whether this state is terminal — nothing will move it without a plan change.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, State::Finished { .. } | State::Stopped)
    }

    /// Whether the supervisor counts this component as converged for readiness purposes.
    ///
    /// Readiness is deliberately **not** "every component is `Running`" (W3 §3.5): an
    /// uploader waiting on a dead database would then block readiness forever and systemd
    /// would kill a daemon that is working exactly as D-27 specifies.
    pub const fn is_settled(&self) -> bool {
        matches!(
            self,
            State::Running { .. }
                | State::Waiting { .. }
                | State::Backoff { .. }
                | State::Finished { .. }
                | State::Stopped
        )
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The per-component counters of §5.10, which are W2a Decision 20's list.
///
/// State rather than local variables inside a `for { select { case <-ticker.C: } }` loop,
/// which is where they live today and where they are not observable at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Occurrences that ran, whatever the outcome.
    pub runs_total: u64,
    /// Occurrences whose body returned `Err`.
    pub failed_total: u64,
    /// Occurrences not run because the previous one had not finished, or because the
    /// component was not `Running`. Repeated skips mean the configured interval is shorter
    /// than the round trip.
    pub skipped_total: u64,
    /// Occurrences cancelled on their deadline. Repeated hits mean the remote end is hung.
    pub deadline_exceeded_total: u64,
    /// `Calendar` occurrences the process was down for, past their grace.
    pub misfired_total: u64,
    /// Builds and `Serve` exits that led to another attempt.
    pub restart_total: u64,
    /// Batches a lossy subscriber missed.
    pub lagged_total: u64,
    /// Stops that exceeded their grace and were aborted.
    pub stop_timeout_total: u64,
}

/// What the supervisor knows about one component's schedule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleState {
    /// When the last occurrence ran.
    pub last_run: Option<DateTime<Utc>>,
    /// How it went.
    pub last_outcome: Option<Outcome>,
    /// When the next one is due — a heap peek, not a guess.
    pub next_run: Option<DateTime<Utc>>,
    /// The period, for a `Trigger::Every`.
    pub period: Option<Duration>,
}

/// One component's full runtime status: the resource W5 §2.3 renders.
///
/// The internal vocabulary stops here. `display.name`, `display.qualifier` and
/// `display.section` are the management API's to produce, once, from the id and the
/// trigger — an internal term on screen is a bug rather than a documentation gap
/// (**D-36**).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentStatus {
    /// The component.
    pub id: ComponentId,
    /// Its trigger's kind, for `schedule.kind`.
    pub trigger: &'static str,
    /// Its state.
    pub state: State,
    /// Since when it has been in that state.
    pub since: DateTime<Utc>,
    /// The configuration generation the **running** instance was built from (**D-53**).
    pub applied_generation: crate::control::Generation,
    /// Schedule state.
    pub schedule: ScheduleState,
    /// The counters.
    pub counters: Counters,
    /// The last error, with its cause chain, or `None`.
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_name_and_all_lists_it() {
        let states = [
            State::Planned,
            State::Waiting { on: vec![] },
            State::Starting { attempt: 1 },
            State::Running { degraded: vec![] },
            State::Failed {
                error: "x".into(),
                at: Utc::now(),
                attempt: 1,
            },
            State::Backoff {
                until: Utc::now(),
                attempt: 2,
            },
            State::Finished { at: Utc::now() },
            State::Stopping { since: Utc::now() },
            State::Stopped,
        ];
        let names: Vec<_> = states.iter().map(State::as_str).collect();
        assert_eq!(names, State::ALL);
    }

    /// The readiness rule, as a test: `Waiting` is settled, `Starting` is not.
    #[test]
    fn a_component_waiting_on_a_dead_database_does_not_block_readiness() {
        let waiting = State::Waiting {
            on: vec![WaitingOn {
                capability: Capability::ReadingStore,
                provider: Some(ComponentId::new("db:postgres").unwrap()),
            }],
        };
        assert!(waiting.is_settled());
        assert!(!State::Starting { attempt: 1 }.is_settled());
        assert!(State::Stopped.is_terminal());
    }
}
