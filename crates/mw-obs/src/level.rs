//! Runtime-adjustable levels, and the concurrency story (§5.5).
//!
//! `tracing_subscriber::reload::Handle<EnvFilter, _>`, captured in `main` and handed to
//! the supervisor as a root fact (**D-52**). Three properties, each one a decision rather
//! than an implementation detail:
//!
//! - **The parse happens before the lock is taken.** `handle.reload` takes an internal
//!   write lock that every event takes for reading; parsing one directive inside it would
//!   hold it for the duration of a parse, and a *failed* parse would have taken it for
//!   nothing. So a rejected directive changes nothing and costs no event a wait.
//! - **Poisoning is handled rather than unwrapped.** A panic while the write lock is held
//!   would poison it and silence logging -- the worst available failure for a logging
//!   subsystem. [`ObsError::ReloadPoisoned`] is named, and this module never unwraps.
//! - **The directive in force is remembered here**, not read back out of the filter.
//!   `EnvFilter`'s `Display` reconstructs a directive list rather than echoing what was
//!   submitted, so "what did I set?" would come back subtly rewritten.
//!
//! What is *not* here: the TTL on a scoped raise. §5.5 puts the expiry in the supervisor's
//! timer heap as an `OnDemand` occurrence, which is the right place for it -- it is one
//! clock for the whole process (§3.3) -- and the management API that offers the TTL is
//! implementation-plan.md M7-1. This type is the half that has to exist first: the thing
//! that actually changes the level, and that can say no.

use std::fmt;
use std::sync::Mutex;

use tracing_subscriber::reload;
use tracing_subscriber::EnvFilter;

use crate::error::ObsError;

/// Parse a filter directive in `RUST_LOG` syntax.
///
/// Separate from [`Reload::set`] because the binary needs it at startup, before there is
/// anything to reload into.
pub fn parse(directive: &str) -> Result<EnvFilter, ObsError> {
    EnvFilter::try_new(directive).map_err(|e| ObsError::FilterParse {
        directive: directive.to_owned(),
        reason: e.to_string(),
    })
}

/// The reload handle, erased over the subscriber type.
///
/// `reload::Handle<L, S>` carries the composed subscriber type in `S`, which in a
/// six-layer stack is a type nobody can write down. The handle only ever needs one
/// operation, so one trait object removes the type from every signature in the workspace --
/// including the `LogControl` implementation the binary writes over this.
trait Reloadable: Send + Sync {
    fn swap(&self, filter: EnvFilter) -> Result<(), reload::Error>;
}

impl<S: 'static> Reloadable for reload::Handle<EnvFilter, S> {
    fn swap(&self, filter: EnvFilter) -> Result<(), reload::Error> {
        self.reload(filter)
    }
}

/// The runtime log-level control.
///
/// The binary implements `mw_component::LogControl` over this: that trait lives one layer
/// above this crate (`ci/layers.toml` puts `mw-obs` in `pure` and `mw-component` in
/// `component`), so the two meet in the one place a consumer and a provider may --
/// the binary (§1.3 rule 1).
pub struct Reload {
    handle: Box<dyn Reloadable>,
    /// What the process started with, for [`Reload::reset`].
    initial: String,
    current: Mutex<String>,
}

impl fmt::Debug for Reload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reload")
            .field("initial", &self.initial)
            .field("current", &self.current())
            .finish_non_exhaustive()
    }
}

impl Reload {
    /// Wrap a handle and the directive the process started with.
    pub fn new<S: 'static>(handle: reload::Handle<EnvFilter, S>, initial: &str) -> Self {
        Reload {
            handle: Box::new(handle),
            initial: initial.to_owned(),
            current: Mutex::new(initial.to_owned()),
        }
    }

    /// The directive in force, in `RUST_LOG` syntax.
    pub fn current(&self) -> String {
        self.lock().clone()
    }

    /// The directive the process started with.
    pub fn initial(&self) -> &str {
        &self.initial
    }

    /// Replace the directive. Parsed first; on a parse failure nothing changes.
    pub fn set(&self, directive: &str) -> Result<(), ObsError> {
        let filter = parse(directive)?;
        self.handle.swap(filter).map_err(|e| {
            if e.is_poisoned() {
                ObsError::ReloadPoisoned
            } else {
                ObsError::ReloadUnavailable
            }
        })?;
        *self.lock() = directive.to_owned();
        Ok(())
    }

    /// Return to the directive the process started with.
    pub fn reset(&self) -> Result<(), ObsError> {
        let initial = self.initial.clone();
        self.set(&initial)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, String> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    /// A reload handle over a registry with nothing else in it: enough to exercise the
    /// swap, the rejection and the reset without installing a global subscriber.
    ///
    /// The subscriber is returned alongside because the handle holds a `Weak` to the
    /// layer's `Arc`: drop the subscriber and every `set` reports
    /// [`ObsError::ReloadUnavailable`], which is its own test below.
    fn handle() -> (Reload, impl std::any::Any) {
        let (layer, handle) = reload::Layer::new(parse("info").unwrap());
        let keep = tracing_subscriber::registry().with(layer);
        (Reload::new(handle, "info"), keep)
    }

    #[test]
    fn a_valid_directive_is_adopted_and_readable_back_verbatim() {
        let (r, _keep) = handle();
        assert_eq!(r.current(), "info");
        r.set("mw_station::davis=debug,info").unwrap();
        assert_eq!(r.current(), "mw_station::davis=debug,info");
    }

    #[test]
    fn a_typo_changes_nothing_at_all() {
        let (r, _keep) = handle();
        let e = r.set("mw_station=nonsense").expect_err("rejected");
        assert!(
            matches!(e, ObsError::FilterParse { .. }),
            "{e:?} should be a parse failure"
        );
        assert_eq!(
            r.current(),
            "info",
            "a rejected directive must leave the level alone, not silence the process"
        );
    }

    #[test]
    fn reset_returns_to_the_starting_directive() {
        let (r, _keep) = handle();
        r.set("trace").unwrap();
        r.reset().unwrap();
        assert_eq!(r.current(), "info");
        assert_eq!(r.initial(), "info");
    }

    #[test]
    fn a_dropped_subscriber_is_reported_rather_than_panicking() {
        let (layer, h) =
            reload::Layer::<EnvFilter, tracing_subscriber::Registry>::new(parse("info").unwrap());
        drop(layer);
        let r = Reload::new(h, "info");
        assert!(matches!(
            r.set("debug").expect_err("the layer is gone"),
            ObsError::ReloadUnavailable
        ));
    }
}
