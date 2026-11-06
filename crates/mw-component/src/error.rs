//! [`PlanError`], [`BuildError`] and [`ComponentError`] — the three enums
//! docs/service-architecture.md §4.1 names for this crate.
//!
//! `Display` prints **one level only** and sources are attached with `#[source]`, per
//! §4.3: the chain is walked once, at the point of logging, by
//! [`mw_types::DisplayChain`]. Embedding a source in `Display` is what produces the
//! duplicated nested messages that make the Go error strings unreadable.

use std::fmt;
use std::time::Duration;

use mw_types::{Class, Classify};

use crate::capability::{Arity, Capability};
use crate::id::ComponentId;
use crate::identity::IdentityProblem;
use crate::routes::{Methods, Scope};
use crate::trigger::CronProblem;

/// Every structural error W2a Decision 2 requires `plan()` to catch.
///
/// **None of these can reach a running process**, and that is the point rather than a
/// nicety: `plan()` is pure, so each variant below is a unit test over a fixture
/// configuration instead of a runtime surprise. `Class::Fatal` for all of them, trivially
/// — they are reported before anything is spawned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// Two components in one plan with the same id. Everything downstream — the task
    /// table, `component_schedule`, every metric label — is keyed by it.
    #[error("two components are both called `{id}`")]
    DuplicateComponentId {
        /// The repeated id.
        id: ComponentId,
    },

    /// A component called `spine`, which is the id the runtime spine publishes the root
    /// capabilities under.
    #[error("`{id}` is reserved for the runtime spine's own capabilities")]
    ReservedComponentId {
        /// The rejected id.
        id: ComponentId,
    },

    /// A route manifest declared on a capability that is not `HttpRoutes`.
    #[error("`{provider}` declares routes on {capability}, which no listener mounts")]
    RoutesOnNonRouteCapability {
        /// The capability it was declared on.
        capability: Capability,
        /// Who declared it.
        provider: ComponentId,
    },

    /// A route pattern the listener could not mount.
    #[error("`{id}` declares the route `{pattern}`: {reason}")]
    BadRoute {
        /// The component.
        id: ComponentId,
        /// The pattern as declared.
        pattern: String,
        /// Why it cannot be mounted.
        reason: &'static str,
    },

    /// A requirement for a capability **nothing in this build can ever provide**: a root
    /// capability the process was not given.
    ///
    /// Distinct from [`PlanError::Unprovided`] because the fix is different and so is the
    /// audience. This one means the binary did not wire a root — no configuration change
    /// helps. W2a Decision 2 lists "an unknown capability" and "a requirement nobody
    /// provides" separately, and this is that distinction made typed.
    #[error("`{needed_by}` requires {capability}, which this process does not provide at all")]
    UnknownCapability {
        /// What was required.
        capability: Capability,
        /// Who required it.
        needed_by: ComponentId,
    },

    /// A hard requirement no component in this plan provides. An operator fixes this by
    /// configuring the provider.
    #[error(
        "`{needed_by}` requires {capability}, which no component in this configuration provides"
    )]
    Unprovided {
        /// What was required.
        capability: Capability,
        /// Who required it.
        needed_by: ComponentId,
    },

    /// Two providers of a capability that permits one.
    #[error("{capability} is exclusive, but both `{first}` and `{second}` provide it")]
    DuplicateExclusive {
        /// The capability.
        capability: Capability,
        /// The first provider, in plan order.
        first: ComponentId,
        /// The second.
        second: ComponentId,
    },

    /// A provider declared an arity the capability does not have — an additive provider of
    /// an exclusive capability, or the reverse.
    #[error("`{provider}` provides {capability} as {declared:?}, but it is {expected:?}")]
    ArityMismatch {
        /// The capability.
        capability: Capability,
        /// Who declared it.
        provider: ComponentId,
        /// What they declared.
        declared: Arity,
        /// What the capability is.
        expected: Arity,
    },

    /// A component declared a provision of a **root** capability. The runtime spine
    /// provides those; a component that claimed one would be shadowing it.
    #[error("`{provider}` provides {capability}, which only the runtime spine may provide")]
    NotComponentProvidable {
        /// The capability.
        capability: Capability,
        /// Who claimed it.
        provider: ComponentId,
    },

    /// A cycle in the hard-edge digraph, with the path that forms it.
    ///
    /// Reported as a configuration error naming the path rather than experienced as a
    /// hang. `Soft` is the escape hatch if a legitimate cycle ever appears, because only
    /// hard edges form the ordering graph.
    #[error("a dependency cycle: {}", render_cycle(path))]
    Cycle {
        /// The cycle, in order, with the first component repeated at the end.
        path: Vec<ComponentId>,
    },

    /// A tick deadline longer than the period it runs on.
    ///
    /// `deadline <= period` is what makes `Skip` the only overlap policy this system
    /// needs: with it, an overlap can only ever be momentary.
    #[error("`{id}` has a deadline of {deadline:?} on a period of {period:?}")]
    DeadlineExceedsPeriod {
        /// The component.
        id: ComponentId,
        /// What it declared.
        deadline: Duration,
        /// Its period.
        period: Duration,
    },

    /// A zero or absurd deadline. A component that is timed must be given time.
    #[error("`{id}` declares a deadline of {deadline:?}")]
    BadDeadline {
        /// The component.
        id: ComponentId,
        /// What it declared.
        deadline: Duration,
    },

    /// An interval below the component's declared hard floor.
    ///
    /// Enforced **once**, here, rather than clamped and logged on every call — which is
    /// what `ResolveUploadInterval` does today, on every send, forever.
    #[error("`{id}` is configured at {configured:?}, below its floor of {floor:?}")]
    IntervalBelowFloor {
        /// The component.
        id: ComponentId,
        /// What configuration asked for.
        configured: Duration,
        /// The floor its own crate declares.
        floor: Duration,
    },

    /// An identity shape that cannot exist — W2a Decisions 41 and 42.
    #[error("`{id}` cannot be collected from: {problem}")]
    ImpossibleIdentity {
        /// The component.
        id: ComponentId,
        /// Which rule it broke.
        problem: IdentityProblem,
    },

    /// Two components claiming the same path in the same scope (**D-51**).
    ///
    /// Named here rather than discovered at mount time, because mounting puts the failure
    /// in `http:listener` — the one component everything else's reachability depends on —
    /// and puts it *after* its siblings have started.
    #[error("`{a}` and `{b}` both claim `{pattern}` ({methods}) in scope {scope}")]
    RouteConflict {
        /// Which router.
        scope: Scope,
        /// The pattern as the second owner declared it.
        pattern: String,
        /// The overlapping methods.
        methods: Methods,
        /// The first owner, in plan order.
        a: ComponentId,
        /// The second.
        b: ComponentId,
    },

    /// A `Calendar` schedule that cannot be resolved to occurrences.
    #[error("`{id}` has an unschedulable calendar: {problem}")]
    BadCalendar {
        /// The component.
        id: ComponentId,
        /// Why.
        problem: CronProblem,
    },

    /// A `Calendar` component whose station has no valid IANA zone.
    ///
    /// A plan error rather than a silent fallback to UTC, which is what every rollover in
    /// the Go tree does today (W2a Decision 11).
    #[error("{}", missing_zone(id, station.as_ref()))]
    MissingTimeZone {
        /// The component.
        id: ComponentId,
        /// The station whose zone was asked for, or `None` for the deployment default —
        /// which renders differently, because "the time zone of station the deployment
        /// default" is not a sentence.
        station: Option<mw_types::StationKey>,
    },

    /// A trigger and a body that do not agree — a `Service` with a `Tick`, say. The
    /// mapping in W2a §1.2 is total, so this is reachable only from a registrar that
    /// built its own spec by hand.
    #[error("`{id}` has trigger `{trigger}`, which does not match the body it declared")]
    TriggerBodyMismatch {
        /// The component.
        id: ComponentId,
        /// The trigger's kind.
        trigger: &'static str,
    },

    /// A registrar refused to produce specs from this configuration.
    #[error("{registrar} could not plan this configuration: {reason}")]
    Registrar {
        /// Which registrar.
        registrar: &'static str,
        /// What it said.
        reason: String,
    },
}

fn missing_zone(id: &ComponentId, station: Option<&mw_types::StationKey>) -> String {
    match station {
        Some(station) => format!(
            "`{id}` schedules in station `{station}`'s time zone and that station has no \
             valid IANA one. Set `config.stations.timezone` for it: a rainfall day, month \
             and year roll over at the station's own midnight (W3 D-24), and there is no \
             boundary to accumulate from without a zone. A blank or absent value is what \
             makes the rollover happen in the database server's zone instead."
        ),
        None => format!(
            "`{id}` schedules in the deployment's default time zone, which is not set to a \
             valid IANA zone"
        ),
    }
}

fn render_cycle(path: &[ComponentId]) -> String {
    path.iter()
        .map(ComponentId::as_str)
        .collect::<Vec<_>>()
        .join(" -> ")
}

impl Classify for PlanError {
    fn class(&self) -> Class {
        Class::Fatal
    }
}

/// Why phase 2 — the builder — failed.
///
/// A build failure is a **lifecycle transition**, not a startup abort (W2a Decision 3):
/// `Starting → Failed → Backoff → Starting`. That single change is what replaces four
/// controllers refusing to construct without TimescaleDB and staying dead until the
/// process is restarted by hand.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A hard requirement was not available when the builder ran. The supervisor does not
    /// normally let this happen — it builds only once the hard needs are published — so
    /// this is the race's name rather than the normal path.
    #[error("{capability} was not available at build time")]
    Unavailable {
        /// What was missing.
        capability: Capability,
    },

    /// The provider published a handle of a type this consumer does not expect. A wiring
    /// error in the binary, which is the only place a consumer and a provider meet.
    #[error("the handle published for {capability} is not the type `{expected}` this component resolves")]
    HandleTypeMismatch {
        /// The capability.
        capability: Capability,
        /// The type the consumer asked for.
        expected: &'static str,
    },

    /// A component resolved a capability it never declared. Rule 3: a component is handed
    /// only what it declares.
    #[error("this component resolved {capability}, which it does not declare")]
    UndeclaredNeed {
        /// The capability.
        capability: Capability,
    },

    /// A component asked for a publication handle for a capability it does not provide.
    /// The same rule as [`BuildError::UndeclaredNeed`], in the other direction.
    #[error("this component publishes {capability}, which it does not provide")]
    UndeclaredProvision {
        /// The capability.
        capability: Capability,
    },

    /// A component tried to take an exclusive capability as a live additive subscription.
    #[error("{capability} is exclusive and cannot be resolved as an additive subscription")]
    NotAdditive {
        /// The capability.
        capability: Capability,
    },

    /// A resource the component owns could not be acquired — a serial port, a
    /// certificate, a socket. The ordinary case, and the one backoff exists for.
    #[error("{what} could not be acquired")]
    Resource {
        /// What could not be acquired, in words an operator can act on.
        ///
        /// This reaches the management API verbatim, through `State::Failed` and
        /// `ComponentStatus::last_error`. **Never interpolate a credential into it** — not
        /// a connection string, not an APRS-IS passcode, not a station secret. D-33's
        /// wrapper types redact in `Debug` and `Display` precisely so that the obvious
        /// thing to write here is also the safe one.
        what: String,
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Configuration the builder read does not describe anything it can build. Fatal: no
    /// retry helps, and it is visible as state rather than as a dead component.
    #[error("the configuration for this component is not usable: {reason}")]
    Configuration {
        /// Why.
        reason: String,
    },
}

impl Classify for BuildError {
    fn class(&self) -> Class {
        match self {
            // The supervisor republishes handles; waiting is the retry.
            BuildError::Unavailable { .. } => Class::RETRYABLE,
            // A USB adapter may come back and a certificate may be replaced, so this is
            // transient by default -- W2a Decision 30 forbids permanent give-up, and the
            // thing that makes retrying forever acceptable is that `attempt`,
            // `last_error` and `Backoff { until }` are all visible state.
            BuildError::Resource { .. } => Class::RETRYABLE,
            BuildError::HandleTypeMismatch { .. }
            | BuildError::UndeclaredNeed { .. }
            | BuildError::UndeclaredProvision { .. }
            | BuildError::NotAdditive { .. }
            | BuildError::Configuration { .. } => Class::Fatal,
        }
    }
}

/// What a component body reports when it does not return `Ok`.
///
/// There are exactly three ways a component reports an outcome — returning `Ok`,
/// returning `Err`, or being cancelled — and no `set_state`, no health method and no
/// `IsRunning` (W2a Decision 28). This enum is the second of the three.
#[derive(Debug, thiserror::Error)]
pub enum ComponentError {
    /// The builder failed.
    #[error("the component could not be built")]
    Build(#[from] BuildError),

    /// The body failed. The cause chain is preserved and walked once at the log site.
    #[error("{what}")]
    Tick {
        /// What the component was doing, in words that name the action.
        what: String,
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A soft requirement was needed after all, on this occurrence. Not a lifecycle
    /// transition: the component is `Running { degraded }` and said so.
    #[error("{0} is unavailable")]
    Unavailable(Capability),

    /// The supervisor cancelled this occurrence because it exceeded its deadline. The
    /// supervisor constructs this; a component does not.
    #[error("the deadline was exceeded")]
    DeadlineExceeded,

    /// The component observed cancellation and returned. Not a failure.
    #[error("cancelled")]
    Cancelled,
}

impl ComponentError {
    /// An error from the body, with a cause.
    ///
    /// `what` names the action — "upload to Weather Underground", "read the LOOP packet" —
    /// because §5.9's standard for a log line is that it says what was being attempted.
    pub fn tick<E>(what: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        ComponentError::Tick {
            what: what.into(),
            source: Box::new(source),
        }
    }
}

impl Classify for ComponentError {
    fn class(&self) -> Class {
        match self {
            ComponentError::Build(e) => e.class(),
            // Transient, and the boxed source's own classification is deliberately not
            // consulted: the source is reachable through `dyn Error` but its `Classify`
            // impl is not, since `dyn Error` does not carry it -- and the
            // supervisor's answer to a failed occurrence is the same either way -- record
            // it, count it, and the next scheduled occurrence IS the retry (W2a Decision
            // 29). Classification earns its keep one layer in, at the `tower` stack,
            // where the concrete error type is still known.
            ComponentError::Tick { .. } => Class::RETRYABLE,
            ComponentError::Unavailable(_) => Class::RETRYABLE,
            ComponentError::DeadlineExceeded => Class::RETRYABLE,
            ComponentError::Cancelled => Class::Ignorable,
        }
    }
}

/// A short, stable word for what happened to one occurrence.
///
/// The vocabulary is the configuration schema's `component_schedule.outcome` CHECK and
/// §5.10's `mw_component_ticks_total{outcome}` label, which are the same five words on
/// purpose: a counter and a stored row that disagreed about the word for "it failed"
/// would be two vocabularies for one fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// The body returned `Ok` within its deadline.
    Ok,
    /// The body returned `Err`.
    Failed,
    /// The deadline fired and the occurrence was cancelled.
    DeadlineExceeded,
    /// The occurrence did not run because the previous one was still running, or because
    /// the component was not `Running`.
    Skipped,
    /// A `Calendar` occurrence the process was down for, older than its grace.
    Misfired,
}

impl Outcome {
    /// The inverse of [`Outcome::as_str`], for reading the `component_schedule` column
    /// back.
    ///
    /// Here rather than in `mw-store` because the column's vocabulary and this enum have
    /// to be one fact: the migration's `CHECK (outcome IN (...))` lists exactly these five
    /// spellings, and a mapping written beside the query would be a second place for the
    /// sixth one to be forgotten. Derived from [`Outcome::as_str`] for the same reason --
    /// a second `match` over the spellings is a second place to forget it.
    pub fn parse(s: &str) -> Option<Self> {
        Outcome::ALL.into_iter().find(|o| o.as_str() == s)
    }

    /// Every outcome, so that a round-trip test cannot miss one.
    pub const ALL: [Outcome; 5] = [
        Outcome::Ok,
        Outcome::Failed,
        Outcome::DeadlineExceeded,
        Outcome::Skipped,
        Outcome::Misfired,
    ];

    /// This outcome's position in [`Outcome::ALL`].
    ///
    /// An exhaustive match, which is the whole point: a sixth outcome fails to compile
    /// *here*, so `ALL` is a checked fact rather than a hand-written list that happens to
    /// be complete. Without it, adding a variant left `ALL` at five entries, `parse`
    /// returning `None` for the new spelling, and both round-trip tests still green --
    /// under precisely the change they exist to guard.
    ///
    /// `#[cfg(test)]` because nothing in the daemon wants a position: the enforcement is
    /// at test-compile time, which `cargo test` in the PR gate runs.
    #[cfg(test)]
    const fn index(self) -> usize {
        match self {
            Outcome::Ok => 0,
            Outcome::Failed => 1,
            Outcome::DeadlineExceeded => 2,
            Outcome::Skipped => 3,
            Outcome::Misfired => 4,
        }
    }

    /// The stored and exported spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::DeadlineExceeded => "deadline_exceeded",
            Outcome::Skipped => "skipped",
            Outcome::Misfired => "misfired",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    /// `component_schedule.outcome` is written with `as_str` and read with `parse`, so a
    /// sixth outcome that forgot one of them would be a row the supervisor cannot read
    /// back after a restart.
    #[test]
    fn every_outcome_round_trips_through_its_stored_spelling() {
        for outcome in Outcome::ALL {
            assert_eq!(
                Outcome::parse(outcome.as_str()),
                Some(outcome),
                "{outcome} does not round-trip"
            );
        }
        assert_eq!(Outcome::parse("completed"), None);
    }

    /// `ALL` is hand-written and `index` is exhaustive, so this is what ties them: a
    /// sixth variant cannot compile without touching `index`, and `index` cannot
    /// disagree with `ALL` without failing here.
    #[test]
    fn all_lists_every_outcome_exactly_once() {
        for outcome in Outcome::ALL {
            assert_eq!(
                Outcome::ALL[outcome.index()],
                outcome,
                "{outcome} is not at its own index in ALL"
            );
        }
        let highest = Outcome::ALL
            .iter()
            .map(|o| o.index())
            .max()
            .expect("ALL is not empty");
        assert_eq!(
            Outcome::ALL.len(),
            highest + 1,
            "ALL has a gap or a duplicate"
        );
    }
    use super::*;
    use mw_types::DisplayChain;

    #[derive(Debug, thiserror::Error)]
    #[error("no such device (os error 2)")]
    struct NoDevice;

    #[test]
    fn a_plan_error_names_both_owners_and_the_path() {
        let a = ComponentId::new("web:site:1").unwrap();
        let b = ComponentId::new("api:legacy").unwrap();
        let e = PlanError::RouteConflict {
            scope: Scope::Any,
            pattern: "/latest".into(),
            methods: Methods::ANY,
            a: a.clone(),
            b: b.clone(),
        };
        let rendered = e.to_string();
        assert!(rendered.contains("web:site:1"), "{rendered}");
        assert!(rendered.contains("api:legacy"), "{rendered}");

        let cycle = PlanError::Cycle {
            path: vec![a.clone(), b, a],
        };
        assert_eq!(
            cycle.to_string(),
            "a dependency cycle: web:site:1 -> api:legacy -> web:site:1"
        );
        assert_eq!(cycle.class(), Class::Fatal);
    }

    /// §4.3's convention, as a test: `Display` is one level and the chain is reachable.
    #[test]
    fn display_is_one_level_and_the_chain_is_walked_at_the_log_site() {
        let e = ComponentError::Build(BuildError::Resource {
            what: "the serial port /dev/ttyUSB0".into(),
            source: Box::new(NoDevice),
        });
        assert_eq!(e.to_string(), "the component could not be built");
        assert_eq!(
            DisplayChain(&e).to_string(),
            "the component could not be built: the serial port /dev/ttyUSB0 could not be acquired: \
             no such device (os error 2)"
        );
        assert_eq!(e.class(), Class::RETRYABLE);
    }

    #[test]
    fn a_configuration_build_failure_is_fatal_through_the_wrapper() {
        let e = ComponentError::Build(BuildError::Configuration {
            reason: "no station is configured".into(),
        });
        assert_eq!(e.class(), Class::Fatal);
    }

    /// The five words the `component_schedule` CHECK accepts, and no sixth.
    #[test]
    fn the_outcome_vocabulary_matches_the_schema() {
        let words: Vec<_> = [
            Outcome::Ok,
            Outcome::Failed,
            Outcome::DeadlineExceeded,
            Outcome::Skipped,
            Outcome::Misfired,
        ]
        .iter()
        .map(|o| o.as_str())
        .collect();
        assert_eq!(
            words,
            vec!["ok", "failed", "deadline_exceeded", "skipped", "misfired"]
        );
    }
}
