//! [`ClassPolicy`] — the retry decision, made once and read generically.
//!
//! > *The mechanism that makes this more than documentation: `tower`'s retry policy is
//! > implemented once, generically, over `E: Classify`, and every tower stack in the
//! > process uses it.*
//! > — `docs/service-architecture.md` §4.2
//!
//! This is that policy. It reads nothing but [`Classify`], so what happens about a failure
//! is decided beside the error's definition — in `crate::error` — rather than at the call
//! site that observed it. A new error type acquires a retry behaviour by implementing one
//! trait.
//!
//! # Why this is not `impl tower::retry::Policy`
//!
//! `tower::retry::Retry<P, S>` requires `S: Clone`, and `tower::limit::RateLimit<S>` is
//! **not** `Clone` — it owns a `Pin<Box<Sleep>>`. So the stack `docs/component-model.md`
//! §4b.5 and `docs/service-architecture.md` §4.2 both write literally —
//!
//! ```text
//! ServiceBuilder::new()
//!     .layer(RetryLayer::new(ClassPolicy::new(budget)))  // outermost
//!     .layer(RateLimitLayer::new(1, period))
//!     .layer(TimeoutLayer::new(attempt_timeout))         // innermost
//!     .service(http)
//! ```
//!
//! — does not compile against `tower` 0.5: `Retry` cannot wrap `RateLimit`. The one
//! sanctioned bridge in `tower` is `buffer::Buffer`, which makes a service `Clone` by
//! moving it behind a channel into a **spawned task** — and `clippy.toml` forbids
//! `tokio::task::spawn` with "spawn through `mw_supervisor::task`, so the task table owns
//! the handle and a leak is unexpressible".
//!
//! **The ordering is what the decision is about, and the ordering is kept.** `crate::uploader`
//! drives the retry as a loop that re-enters the limiter's `poll_ready` before every
//! attempt, which is the property D-57 states in words: *"each attempt takes its own
//! permit"*. What is lost is one layer in a builder expression; what is kept is the
//! behaviour, the typed error and the single policy. Recorded in
//! `docs/reconciliation.md` as **A-M6-2-3**.
//!
//! The policy is deliberately still generic over `E: Classify` and knows nothing about
//! uploads, so the first stack that *can* use `tower::retry` has one line to write.

use std::time::Duration;

use mw_types::{Class, Classify};

/// How many attempts an occurrence makes beyond the first, by default.
///
/// Two retries, so three attempts. Chosen against the deadline arithmetic
/// `docs/component-model.md` §4b.5 requires `plan()` to check —
/// `deadline >= retries x (attempt_timeout + rate_limit_period)` — and against what a retry
/// is for here: a transient network failure to a public endpoint either clears in seconds
/// or does not clear this period, and the period *is* the retry for everything else.
pub const DEFAULT_BUDGET: u8 = 2;

/// The retry decision for one occurrence.
///
/// Holds the remaining budget, so it is constructed per occurrence rather than per
/// component: a budget that survived between occurrences would make the second occurrence
/// after a bad minute un-retryable for no reason a reader could find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassPolicy {
    budget: u8,
}

impl ClassPolicy {
    /// A policy with `budget` retries.
    pub fn new(budget: u8) -> Self {
        Self { budget }
    }

    /// How many retries are left.
    pub fn remaining(&self) -> u8 {
        self.budget
    }

    /// What to do about one failure.
    ///
    /// `None` means stop. `Some(delay)` means wait that long and attempt again, and spends
    /// one of the budget — including when the delay is zero, which is what stops a
    /// `Retryable` with no hint from looping.
    ///
    /// **`Class::Ignorable` is not retried and is not an error either.** The caller decides
    /// what an ignorable failure means for the occurrence; what this says is only that
    /// attempting again would produce the same answer.
    pub fn consider<E: Classify>(&mut self, error: &E) -> Option<Duration> {
        match error.class() {
            Class::Retryable { after } if self.budget > 0 => {
                self.budget -= 1;
                Some(after.unwrap_or(Duration::ZERO))
            }
            // Out of budget, or an error no retry can help.
            Class::Retryable { .. } | Class::Fatal | Class::Ignorable => None,
        }
    }
}

impl Default for ClassPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test error whose class is whatever the case wants, so the policy is tested over
    /// `Classify` rather than over `UploadError` — which is what "generic over
    /// `E: Classify`" has to mean to be worth anything.
    #[derive(Debug)]
    struct Fake(Class);

    impl Classify for Fake {
        fn class(&self) -> Class {
            self.0
        }
    }

    #[test]
    fn a_retryable_failure_spends_the_budget_and_then_stops() {
        let mut policy = ClassPolicy::new(2);
        let e = Fake(Class::RETRYABLE);
        assert_eq!(policy.consider(&e), Some(Duration::ZERO));
        assert_eq!(policy.remaining(), 1);
        assert_eq!(policy.consider(&e), Some(Duration::ZERO));
        assert_eq!(policy.remaining(), 0);
        // Out of budget: the period is the retry from here.
        assert_eq!(policy.consider(&e), None);
        assert_eq!(policy.consider(&e), None);
    }

    /// The externally supplied hint is honoured, which is the half of W2a Decision 59 that
    /// happens inside the occurrence.
    #[test]
    fn an_external_hint_becomes_the_delay() {
        let mut policy = ClassPolicy::new(1);
        let e = Fake(Class::Retryable {
            after: Some(Duration::from_secs(45)),
        });
        assert_eq!(policy.consider(&e), Some(Duration::from_secs(45)));
    }

    /// Neither a configuration error nor an expected absence is retried, whatever the
    /// budget.
    #[test]
    fn nothing_else_is_retried() {
        for class in [Class::Fatal, Class::Ignorable] {
            let mut policy = ClassPolicy::new(10);
            assert_eq!(policy.consider(&Fake(class)), None, "{class:?}");
            assert_eq!(policy.remaining(), 10, "the budget is not spent");
        }
    }

    #[test]
    fn the_default_budget_is_two_retries() {
        assert_eq!(ClassPolicy::default().remaining(), DEFAULT_BUDGET);
        assert_eq!(DEFAULT_BUDGET, 2);
    }
}
