//! The one thing the spool needs from a clock, and why it is a trait here.
//!
//! docs/service-architecture.md §3.3 gives the process one clock, and that clock is
//! `mw_supervisor::clock::Clock` — which this crate cannot name, because `mw-supervisor`
//! and `mw-pipeline` are the same layer and `ci/layers.toml` rejects a same-layer
//! dependency. The supervisor's clock is a *scheduler*: it owns the timer heap, the
//! `Instant`-based firing and the step detection. What the spool needs is narrower — the
//! wall-clock instant that goes into a cursor's `acked_at`, a segment's `created_at` and
//! `mw_spool_oldest_unacked_age_seconds` — so the seam is one method rather than a
//! dependency inversion of the whole scheduler.
//!
//! It is a trait rather than a direct `Utc::now()` for one reason that is not testing
//! fashion: `max_lag` is **seven days**, the ceiling warning compares a byte rate against
//! it, and neither is assertable in a test that cannot move the clock. Both are
//! correctness properties (W7 **D-16**), so both have to be tested rather than reasoned
//! about.

use std::sync::Arc;

use chrono::{DateTime, Utc};

/// Wall-clock now.
pub trait Clock: Send + Sync + 'static {
    /// The current instant, UTC.
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock. What the daemon runs on.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock a test moves by hand.
///
/// Public rather than `#[cfg(test)]`, because this crate's integration tests are separate
/// compilation units and the one for `max_lag` has to advance seven days.
#[derive(Debug, Clone)]
pub struct TestClock {
    at: Arc<std::sync::Mutex<DateTime<Utc>>>,
}

impl TestClock {
    /// A clock stopped at `at`.
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            at: Arc::new(std::sync::Mutex::new(at)),
        }
    }

    /// A clock stopped at a fixed, arbitrary instant with no significance beyond being a
    /// real one: 2026-11-03T12:00:00Z.
    ///
    /// On a **whole second**, which is convenient and was once a blind spot: the envelope's
    /// wire instants are milliseconds and `reading.time` is nanoseconds, so a mismatch
    /// between them is invisible to every test on this clock. `Ingress::admit` now
    /// truncates all three together, and
    /// `the_envelopes_instants_survive_the_spool_on_a_nanosecond_clock` in
    /// `tests/ingress.rs` holds that down with a clock that has sub-millisecond nanos.
    pub fn fixed() -> Self {
        Self::new(
            DateTime::from_timestamp(1_762_171_200, 0).expect("a constant that is an instant"),
        )
    }

    /// Move the clock forward.
    pub fn advance(&self, by: chrono::Duration) {
        let mut at = self.at.lock().expect("the test clock is not poisoned");
        *at += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.at.lock().expect("the test clock is not poisoned")
    }
}
