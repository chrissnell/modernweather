//! The `ComponentControl` capability: component state, run-now, the log-level handle
//! (**D-52**) and the configuration generation (**D-53**).
//!
//! Provided by the supervisor, which is a **root** — it exists before any component and
//! requires nothing, so it cannot participate in a cycle. That is half of how the
//! `AppReloader` cycle is eliminated (W2a §2.5): the management API no longer reaches back
//! into the application object that owns the manager that constructs every other
//! component; it writes configuration through `ConfigAdmin` and observes state through
//! this.
//!
//! **Why the log handle and the generation are here rather than in capabilities of their
//! own.** Both have one consumer and the same root provider, and a thirteenth capability
//! for one consumer is noise in a graph whose only jobs are ordering and `Waiting`
//! (D-52). `applied_generation` is supervisor state by construction — computing it
//! anywhere else would mean a second source of truth for "is this change live", and W5
//! §2.2 is unimplementable without it (D-53).

use std::fmt;

use crate::capability::Capability;
use crate::cx::CapabilityHandle;
use crate::id::ComponentId;
use crate::state::ComponentStatus;

/// A monotonic configuration generation, assigned by `ConfigAdmin` on every committed
/// write.
///
/// One integer does most of the work of W5 §2.2: a change is live exactly when every
/// affected component's `applied_generation >= generation`, which turns "did my change
/// take effect?" from an unanswerable question into a comparison the server cannot fake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(u64);

impl Generation {
    /// The generation of a configuration nothing has written to yet.
    pub const ZERO: Self = Self(0);

    /// Wrap a generation read from the configuration store.
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The underlying integer, for the management API's JSON.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next generation.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Observe and steer components.
///
/// Note what is **not** here: no `reload`. A write through `ConfigAdmin` bumps the
/// configuration generation, the supervisor is a watch subscriber, and it reconciles the
/// plan itself. `AppReloader::ReloadConfiguration` has no replacement because nothing
/// calls it (W2a §2.5).
pub trait ComponentControl: Send + Sync + 'static {
    /// Every component's status, for W5's runtime resource.
    fn components(&self) -> Vec<ComponentStatus>;

    /// One component's status.
    fn component(&self, id: &ComponentId) -> Option<ComponentStatus>;

    /// The newest committed configuration generation (**D-53**).
    fn generation(&self) -> Generation;

    /// Run a `Tick`-bodied component now, in addition to its schedule.
    ///
    /// Deduplicated by occurrence key, so a double-click in the UI runs it once. This is
    /// also how the seven `/test/*` connectivity checks run: they are `OnDemand`
    /// components, which is where "can we authenticate to APRS-IS" belongs — when a human
    /// asks, rather than every 90 seconds forever.
    fn run_now(&self, id: &ComponentId) -> Result<(), ControlError>;

    /// Start a stopped component.
    fn start(&self, id: &ComponentId) -> Result<(), ControlError>;

    /// Stop a running component: cancel, then await its task within the grace deadline.
    ///
    /// Unlike `RemoveController`, which can only delete a map entry and log that the
    /// controller will stop at the next application restart while its listeners and
    /// goroutines leak (**D-24**).
    fn stop(&self, id: &ComponentId) -> Result<(), ControlError>;

    /// The runtime log-level handle (**D-52**).
    fn log_control(&self) -> &dyn LogControl;
}

impl CapabilityHandle for std::sync::Arc<dyn ComponentControl> {
    const CAPABILITY: Capability = Capability::ComponentControl;
}

/// Why a control request could not be carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// No component in the current plan has that id.
    #[error("no component `{id}` is planned")]
    NoSuchComponent {
        /// What was asked for.
        id: ComponentId,
    },

    /// `run_now` on a component whose body is not a `Tick` — a `Service` has no
    /// occurrence to run and an `OnReading` component runs when a reading arrives.
    #[error("`{id}` has trigger `{trigger}`, which has no occurrence to run on demand")]
    NotRunnable {
        /// The component.
        id: ComponentId,
        /// Its trigger's kind.
        trigger: &'static str,
    },

    /// `run_now` on a component that is not live — `Waiting` on a capability, `Stopped`,
    /// or still `Starting`.
    ///
    /// Reported rather than accepted, because the supervisor would log the request and
    /// drop it: a 200 for a check that did not run is worse than a refusal that says
    /// which state the component is in.
    #[error("`{id}` is {state}, so there is nothing to run now")]
    NotLive {
        /// The component.
        id: ComponentId,
        /// Its state's one-word name.
        state: &'static str,
    },

    /// The supervisor is shutting down and is not accepting control requests.
    #[error("the supervisor is shutting down")]
    ShuttingDown,
}

impl mw_types::Classify for ControlError {
    /// `ShuttingDown` is the one transient case: the supervisor is going away and the
    /// request would succeed against the next process. The other two are the caller
    /// asking for something that does not exist or cannot be done, which no retry fixes.
    fn class(&self) -> mw_types::Class {
        match self {
            ControlError::ShuttingDown => mw_types::Class::RETRYABLE,
            ControlError::NoSuchComponent { .. }
            | ControlError::NotRunnable { .. }
            | ControlError::NotLive { .. } => mw_types::Class::Fatal,
        }
    }
}

impl mw_types::Classify for LogControlError {
    /// A rejected directive is the operator's to fix; a missing reload handle is this
    /// build's to fix. Neither is retryable.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

/// The runtime log-level handle.
///
/// `mw-obs` implements this over the `tracing-subscriber` reload handle; it is reached
/// through `ComponentControl` so that the management API needs one capability rather than
/// two to do its two jobs.
pub trait LogControl: Send + Sync + 'static {
    /// The filter directive in force, as `RUST_LOG` syntax.
    fn current(&self) -> String;

    /// Replace the filter directive. Per-station and per-component filtering is what the
    /// span fields make possible (docs/service-architecture.md §5.4).
    fn set(&self, directive: &str) -> Result<(), LogControlError>;

    /// Return to the directive the process started with.
    fn reset(&self) -> Result<(), LogControlError>;
}

/// Why a log-level change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogControlError {
    /// The directive is not valid filter syntax. Rejected without changing anything, so a
    /// typo cannot leave the process with no logging at all.
    #[error("`{directive}` is not a valid filter: {reason}")]
    FilterParse {
        /// What was submitted.
        directive: String,
        /// What the parser said.
        reason: String,
    },

    /// The subscriber's reload handle is gone, which can only happen if the subscriber was
    /// never installed.
    #[error("the logging subscriber cannot be reconfigured")]
    Unavailable,
}
