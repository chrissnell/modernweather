//! [`ComponentSpec`] — phase 1, and the reason there is a phase 1 at all.
//!
//! W2a's prompt puts the declaration on the component: `fn trigger(&self) -> Trigger`,
//! `fn requires(&self) -> &[Capability]`. That cannot work, and the reason is the exact
//! defect **D-27** exists to remove. To learn what a component requires you must have an
//! instance; to have an instance you must call its constructor; and in the Go
//! implementation the constructor is where this happens:
//!
//! ```text
//! // internal/controllers/weather_service_utils.go:38
//! if err := ValidateTimescaleDBConfig(configProvider, "weather service"); err != nil {
//!     return nil, err   // PWS, Weather Underground, and the snow cache die here
//! }
//! ```
//!
//! A declaration that lives behind a constructor is a declaration you can only read once
//! the thing it describes has already succeeded in starting. So the model is two-phase and
//! **the first phase is pure**: a spec is derived from configuration with no I/O, no
//! sockets and no database, which is what makes `plan()` a unit test.

use std::hash::Hash;
use std::time::Duration;

use crate::body::Builder;
use crate::capability::{Freshness, Need, Provision};
use crate::id::{ComponentId, Fnv1a};
use crate::identity::IdentityShape;
use crate::trigger::Trigger;

/// The default stop grace (W3 §3.6). Two components need more — `writer:timescale`
/// finishing an in-flight insert and `uplink:collector` flushing an ack, both 15 s — and a
/// `Service` holding a serial port needs none of it, because cancellation is observed at
/// its next await and `Drop` releases the port.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10);

/// One component, declared. Pure data plus a builder that has not run.
///
/// `Clone` because the supervisor keeps the spec for the life of the plan and builds from
/// it again after a backoff; the builder is behind an `Arc`, so cloning a spec is cheap
/// and does not duplicate the configuration it closed over.
#[derive(Debug, Clone)]
pub struct ComponentSpec {
    /// Stable identity.
    pub id: ComponentId,
    /// When it runs.
    pub trigger: Trigger,
    /// What it needs.
    pub requires: Vec<Need>,
    /// What it offers.
    pub provides: Vec<Provision>,
    /// For a station component, how its endpoint identifies a station. `None` for
    /// everything else — the checks in W2a §4a apply to collection, not to uploads.
    pub identity: Option<IdentityShape>,
    /// Data-presence expectations, for W5 and the `/test/*` checks. Never for the
    /// scheduler (W2a Decision 20).
    pub freshness: Vec<Freshness>,
    /// How long this component may take to stop before it is aborted.
    pub grace: Duration,
    /// A digest of every configuration value the builder closes over.
    pub config_digest: ConfigDigest,
    /// Phase 2, not run yet.
    pub build: Builder,
}

impl ComponentSpec {
    /// Declare a component. The remaining fields have defaults a registrar overrides with
    /// the `with_*` methods.
    pub fn new(id: ComponentId, trigger: Trigger, build: Builder) -> Self {
        Self {
            id,
            trigger,
            requires: Vec::new(),
            provides: Vec::new(),
            identity: None,
            freshness: Vec::new(),
            grace: DEFAULT_GRACE,
            config_digest: ConfigDigest::NONE,
            build,
        }
    }

    /// Declare what this component needs.
    #[must_use]
    pub fn requires(mut self, needs: impl IntoIterator<Item = Need>) -> Self {
        self.requires.extend(needs);
        self
    }

    /// Declare what this component provides.
    #[must_use]
    pub fn provides(mut self, provisions: impl IntoIterator<Item = Provision>) -> Self {
        self.provides.extend(provisions);
        self
    }

    /// Declare how this component's endpoint identifies a station.
    #[must_use]
    pub fn identity(mut self, identity: IdentityShape) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Declare a data-presence expectation.
    #[must_use]
    pub fn freshness(mut self, freshness: impl IntoIterator<Item = Freshness>) -> Self {
        self.freshness.extend(freshness);
        self
    }

    /// Override the stop grace.
    #[must_use]
    pub fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Record the digest of the configuration this spec was derived from.
    #[must_use]
    pub fn config_digest(mut self, digest: ConfigDigest) -> Self {
        self.config_digest = digest;
        self
    }

    /// The hard needs, which are the only ones that form the ordering graph.
    pub fn hard_needs(&self) -> impl Iterator<Item = &Need> {
        self.requires
            .iter()
            .filter(|n| n.strength == crate::capability::Strength::Hard)
    }

    /// The declarative half of this spec, for the plan diff.
    pub fn fingerprint(&self) -> SpecFingerprint {
        SpecFingerprint {
            trigger: self.trigger.clone(),
            requires: self.requires.clone(),
            provides: self.provides.clone(),
            identity: self.identity,
            grace: self.grace,
            config_digest: self.config_digest,
        }
    }
}

/// Everything about a spec that a plan diff compares.
///
/// **Why a fingerprint rather than `PartialEq` on the spec.** W2a §2.5 says the supervisor
/// diffs by `ComponentId` and spec equality, and a spec carries a boxed builder closure,
/// which is not comparable — and more importantly, two specs whose declarative halves are
/// identical can still close over different configuration. An APRS callsign change would
/// then compare equal and the component would not be restarted, which is the one failure
/// this mechanism exists to prevent. So [`ConfigDigest`] is part of the fingerprint, and
/// **a registrar is responsible for folding into it every configuration value its builder
/// closes over**. The diff is exactly as accurate as that digest.
///
/// `freshness` is deliberately **not** compared: it is consumed by W5 and the `/test/*`
/// checks and never by the scheduler (W2a Decision 20), so a change to it cannot change
/// what a running instance does and restarting the component for one would be churn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecFingerprint {
    trigger: Trigger,
    requires: Vec<Need>,
    provides: Vec<Provision>,
    identity: Option<IdentityShape>,
    grace: Duration,
    config_digest: ConfigDigest,
}

/// A digest of the configuration a component was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ConfigDigest(u64);

impl ConfigDigest {
    /// For a component with no configuration of its own.
    ///
    /// A component that declares this and *does* close over configuration will not be
    /// restarted when that configuration changes, which is why every registrar that reads
    /// a row calls [`ConfigDigest::of`] instead.
    pub const NONE: Self = Self(0);

    /// Digest anything hashable — in practice the configuration row, or a tuple of the
    /// fields the builder uses.
    ///
    /// FNV-1a rather than `DefaultHasher`: the comparison is within one process today, but
    /// an `applied_generation` that survived a restart would need stability, and a hash
    /// whose stability is documented as "not guaranteed" is the wrong foundation to
    /// discover that on.
    ///
    /// One layer is still missing for that, and it is worth knowing before relying on it:
    /// the digest is a function of the value's `Hash` impl as well as of the hasher, and
    /// `std`'s `Hash` impls carry no cross-version guarantee either. Within a process —
    /// which is all the plan diff needs — that does not matter. If the digest is ever
    /// persisted, it needs a canonical byte encoding rather than `Hash`.
    pub fn of<T: Hash + ?Sized>(value: &T) -> Self {
        let mut hasher = Fnv1a::default();
        value.hash(&mut hasher);
        Self(std::hash::Hasher::finish(&hasher))
    }

    /// The digest as an integer, for a log field.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Tick;
    use crate::capability::Capability;
    use crate::cx::Cx;
    use crate::error::ComponentError;

    struct Noop;
    impl Tick for Noop {
        fn tick<'a>(&'a mut self, _cx: &'a Cx) -> crate::BoxFuture<'a, Result<(), ComponentError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn spec(digest: ConfigDigest) -> ComponentSpec {
        ComponentSpec::new(
            ComponentId::new("wu:backyard").unwrap(),
            Trigger::every(Duration::from_secs(60), Duration::from_secs(30)),
            Builder::tick(|_| async { Ok(Noop) }),
        )
        .requires([Need::hard(Capability::WindowedView)])
        .config_digest(digest)
    }

    /// The property the plan diff rests on: a configuration change the declarative half
    /// cannot see still changes the fingerprint.
    #[test]
    fn the_fingerprint_follows_the_configuration_digest() {
        let a = spec(ConfigDigest::of("KE7XYZ"));
        let b = spec(ConfigDigest::of("KE7XYZ"));
        let c = spec(ConfigDigest::of("N0CALL"));
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
    }

    #[test]
    fn hard_and_soft_needs_are_distinguishable() {
        let s = spec(ConfigDigest::NONE).requires([Need::soft(Capability::SnowCache)]);
        let hard: Vec<_> = s.hard_needs().map(|n| n.capability).collect();
        assert_eq!(hard, vec![Capability::WindowedView]);
        assert_eq!(s.grace, DEFAULT_GRACE);
    }

    #[test]
    fn the_digest_is_stable_for_equal_input() {
        assert_eq!(ConfigDigest::of("x"), ConfigDigest::of("x"));
        assert_ne!(ConfigDigest::of("x"), ConfigDigest::of("y"));
        assert_eq!(ConfigDigest::NONE.get(), 0);
    }
}
