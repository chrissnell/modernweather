//! [`Capability`], [`Need`], [`Provision`] — W2a §2 as code.
//!
//! The graph these build answers two questions and no others: **in what order do
//! components start**, and **is this component `Waiting`** (W2a §2.2). Everything else a
//! dependency graph is usually asked to carry is a different concern: data presence is
//! [`Freshness`], not an edge.

use std::fmt;

use crate::id::ComponentId;
use crate::routes::RouteManifest;

/// The twelve capabilities of W2a §2.1, each justified by something that exists today.
///
/// A closed enum, not a string: a capability is a compile-time fact about this program,
/// and the capability a component requires is written in that component's crate rather
/// than in configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Capability {
    // --- roots: provided by the runtime spine before any component exists ---
    /// The live reading fan-out. The supervisor delivers from it; no component holds a
    /// receiver (W2a §5).
    ReadingStream,
    /// Read component state; start / stop / run-now. Provided by the supervisor, which is
    /// a root and therefore cannot participate in a cycle (W2a §2.5). Also carries the
    /// runtime log-level handle (**D-52**) and `applied_generation` (**D-53**).
    ComponentControl,

    // --- provided by `config:sqlite` ---
    /// Read configuration, and subscribe to the change watch.
    ConfigStore,
    /// Validated configuration mutation. Exactly one component requires it.
    ConfigAdmin,
    /// Station identity, coordinates, callsign, secrets, registration.
    StationRegistry,

    // --- provided by `db:postgres` ---
    /// Queryable reading history.
    ReadingStore,
    /// Aeris forecast rows.
    ForecastStore,
    /// Aeris alert rows.
    AlertStore,
    /// Snow depth / event / smoothing cache rows.
    SnowCache,

    // --- provided by ordinary components ---
    /// `(Metric, Window, Agg)` resolution over recent aggregate buckets (W2a §4b).
    WindowedView,
    /// Pre-serialised per-station current conditions. The `/latest` handler performs no
    /// queries because it is handed this and not a [`Capability::ReadingStore`].
    LatestSnapshot,
    /// Additive: route tables mounted by the one listener.
    HttpRoutes,
}

/// Who may provide a capability.
///
/// This is the distinction behind two different plan errors. A missing **root** means the
/// binary was wired wrong and no configuration change can help; a missing
/// **component-provided** capability means the configuration does not contain its
/// provider, which an operator fixes. W2a Decision 2 lists "an unknown capability" and "a
/// requirement nobody provides" as separate structural errors, and this is the difference
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderClass {
    /// Provided by the runtime spine. A component may not declare it as a provision.
    Root,
    /// Provided by an ordinary component, named in the plan.
    Component,
}

impl Capability {
    /// Every capability, for the plan-time checks and for exhaustive tests.
    pub const ALL: &'static [Capability] = &[
        Capability::ReadingStream,
        Capability::ComponentControl,
        Capability::ConfigStore,
        Capability::ConfigAdmin,
        Capability::StationRegistry,
        Capability::ReadingStore,
        Capability::ForecastStore,
        Capability::AlertStore,
        Capability::SnowCache,
        Capability::WindowedView,
        Capability::LatestSnapshot,
        Capability::HttpRoutes,
    ];

    /// The internal name. Internal: an operator never sees it (**D-36**, W5 §0 owns the
    /// translation), but a log field and a metric label do.
    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::ReadingStream => "ReadingStream",
            Capability::ComponentControl => "ComponentControl",
            Capability::ConfigStore => "ConfigStore",
            Capability::ConfigAdmin => "ConfigAdmin",
            Capability::StationRegistry => "StationRegistry",
            Capability::ReadingStore => "ReadingStore",
            Capability::ForecastStore => "ForecastStore",
            Capability::AlertStore => "AlertStore",
            Capability::SnowCache => "SnowCache",
            Capability::WindowedView => "WindowedView",
            Capability::LatestSnapshot => "LatestSnapshot",
            Capability::HttpRoutes => "HttpRoutes",
        }
    }

    /// Whether the runtime spine provides this, or a component does.
    pub const fn provider_class(self) -> ProviderClass {
        match self {
            Capability::ReadingStream | Capability::ComponentControl => ProviderClass::Root,
            _ => ProviderClass::Component,
        }
    }

    /// The arity every provider of this capability must declare.
    ///
    /// `HttpRoutes` is the only additive capability (W2a Decision 18), and it is the one
    /// whose consumer must not wait for all of its providers — a management UI that is
    /// unreachable exactly when something is broken is the worst possible failure.
    pub const fn arity(self) -> Arity {
        match self {
            Capability::HttpRoutes => Arity::Additive,
            _ => Arity::Exclusive,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declared requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// What is required.
    pub capability: Capability,
    /// Whether the component starts without it.
    pub strength: Strength,
    /// Read or write. Declared because it is reviewable: `aprs:*` requiring
    /// `WindowedView` read-only is W2a Decision 50 written down.
    pub access: Access,
}

impl Need {
    /// A hard, read-only requirement — the common case.
    pub const fn hard(capability: Capability) -> Self {
        Self {
            capability,
            strength: Strength::Hard,
            access: Access::Read,
        }
    }

    /// A hard requirement with write access.
    pub const fn hard_write(capability: Capability) -> Self {
        Self {
            capability,
            strength: Strength::Hard,
            access: Access::Write,
        }
    }

    /// A soft, read-only requirement: the component starts regardless and degrades in a
    /// defined way. This is what keeps the website from waiting on Aeris.
    pub const fn soft(capability: Capability) -> Self {
        Self {
            capability,
            strength: Strength::Soft,
            access: Access::Read,
        }
    }
}

/// Whether a missing capability means `Waiting` or means degraded operation.
///
/// **Only `Hard` edges form the ordering graph** (W2a Decision 17), which also makes
/// `Soft` the escape hatch if a legitimate cycle ever appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    /// The component does not start without it, and returns to `Waiting` if it is revoked.
    Hard,
    /// The component starts regardless; the handle resolves to unavailable.
    Soft,
}

/// Read or write access to a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only.
    Read,
    /// Mutates. `StationRegistry` is `Write` for `station:grpcrx` alone, because
    /// registration mints keys.
    Write,
}

/// One declared provision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provision {
    /// What is provided.
    pub capability: Capability,
    /// How many providers the capability permits.
    pub arity: Arity,
    /// The pure half of what is provided, where there is one.
    pub detail: ProvisionDetail,
}

impl Provision {
    /// Provide `capability` with the arity the capability itself declares.
    pub const fn of(capability: Capability) -> Self {
        Self {
            capability,
            arity: capability.arity(),
            detail: ProvisionDetail::None,
        }
    }

    /// Provide `HttpRoutes`, declaring the route table's paths so that a conflict is a
    /// [`crate::PlanError`] naming both owners rather than silent last-registration-wins
    /// (**D-51**).
    pub fn routes(manifest: RouteManifest) -> Self {
        Self {
            capability: Capability::HttpRoutes,
            arity: Arity::Additive,
            detail: ProvisionDetail::Routes(manifest),
        }
    }
}

/// The pure half of a provision — available from the spec, before anything is built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProvisionDetail {
    /// Nothing beyond the capability itself.
    None,
    /// **D-51.** The declared route patterns, so the union over every component is a pure
    /// function of configuration and conflict detection is a unit test rather than a
    /// mount-time failure in the one component everything else's reachability needs.
    Routes(RouteManifest),
}

/// How many providers a capability permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly one provider. Two is a plan error.
    Exclusive,
    /// Many providers, one consumer, late-bindable.
    Additive,
}

/// A declared **data presence** expectation — not an edge.
///
/// `/forecast` depends on Aeris having fetched at least once. Modelling that as a startup
/// edge would mean the website does not serve until Aeris has succeeded, which is worse
/// than today. It is a freshness property, consumed by W5 and by the `/test/*` checks and
/// **never by the scheduler** (W2a Decision 20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    /// The capability whose *contents* are expected to be recent.
    pub capability: Capability,
    /// The component that populates it.
    pub populated_by: ComponentId,
    /// Older than this and W5 says so.
    pub expected_max_age: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_variant() {
        // The only way to keep `ALL` honest without a derive: a new variant that is not
        // added here fails this match, which is the compiler reminding whoever adds one.
        for c in Capability::ALL {
            match c {
                Capability::ReadingStream
                | Capability::ComponentControl
                | Capability::ConfigStore
                | Capability::ConfigAdmin
                | Capability::StationRegistry
                | Capability::ReadingStore
                | Capability::ForecastStore
                | Capability::AlertStore
                | Capability::SnowCache
                | Capability::WindowedView
                | Capability::LatestSnapshot
                | Capability::HttpRoutes => {}
            }
        }
        assert_eq!(Capability::ALL.len(), 12);
    }

    #[test]
    fn http_routes_is_the_only_additive_capability() {
        let additive: Vec<_> = Capability::ALL
            .iter()
            .filter(|c| c.arity() == Arity::Additive)
            .collect();
        assert_eq!(additive, vec![&Capability::HttpRoutes]);
    }

    #[test]
    fn the_two_roots_are_the_stream_and_control() {
        let roots: Vec<_> = Capability::ALL
            .iter()
            .filter(|c| c.provider_class() == ProviderClass::Root)
            .copied()
            .collect();
        assert_eq!(
            roots,
            vec![Capability::ReadingStream, Capability::ComponentControl]
        );
    }
}
