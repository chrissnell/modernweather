//! The numbers a rate-limited output owns.
//!
//! Two series, and the reason there are two. The supervisor already carries what happened
//! to an *occurrence* — `mw_component_ticks_total{component,outcome}` — and that is the
//! wrong grain for this component in both directions: one occurrence can make three
//! attempts, and an occurrence that sends nothing because the station has reported nothing
//! is `ok` from the supervisor's point of view and is not an upload.
//!
//! | Series | Labels | Answers |
//! |---|---|---|
//! | `mw_output_uploads_total` | `component`, `service`, `outcome` | what happened to the occurrence |
//! | `mw_output_attempts_total` | `component`, `service`, `outcome` | what happened to each attempt |
//!
//! Cardinality is bounded by construction (§5.10): `component` is a component id, so there
//! are as many as configuration says; `service` is one of three; and `outcome` is the closed
//! enum below. A failure's `outcome` is `UploadError::reason`, which is a closed set too.
//!
//! The Go answer to each of these is `log.Errorf` once per failed send, which on a service
//! that is refusing a bad API key is a line per minute forever and a number nobody can
//! alert on.

use mw_component::ComponentId;

/// What happened to one occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The service accepted it.
    Sent,
    /// The station has no value for anything this service declares, so nothing was sent.
    ///
    /// Distinct from a failure, and the distinction matters: a newly configured station
    /// answers this until its windows fill, and an operator watching for "is my upload
    /// working" needs to see that rather than a flat zero.
    NoData,
    /// The service asked us to slow down and the hold had not expired, so the occurrence
    /// was not attempted.
    ///
    /// W2a Decision 59's visible half: *"a service telling us to slow down actually slows
    /// us down, instead of being retried on the next fixed tick"* — and this is the count
    /// that shows it happening.
    Held,
    /// Every attempt failed. The reason is on `mw_output_attempts_total`.
    Failed,
    /// The component was cancelled mid-occurrence.
    Cancelled,
}

impl Outcome {
    /// The metric label, which is also the log field.
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Sent => "sent",
            Outcome::NoData => "no_data",
            Outcome::Held => "held",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
        }
    }

    /// Every outcome, so the series can be zeroed at startup and a component that has sent
    /// nothing is distinguishable from one whose metrics are broken.
    pub const ALL: &'static [Outcome] = &[
        Outcome::Sent,
        Outcome::NoData,
        Outcome::Held,
        Outcome::Failed,
        Outcome::Cancelled,
    ];
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The label an attempt that succeeded carries.
pub const ATTEMPT_OK: &str = "ok";

/// Every label `mw_output_attempts_total` can carry — [`ATTEMPT_OK`] and every
/// `UploadError::reason`.
///
/// Here rather than derived, because `UploadError::reason`'s set is a `match` and a `const`
/// list is what [`arm`] can iterate. `crate::error`'s own test asserts the two agree.
pub const ATTEMPT_OUTCOMES: &[&str] = &[
    ATTEMPT_OK,
    "connect",
    "tls",
    "malformed",
    "rejected",
    "rate_limited",
    "timeout",
    "login_refused",
    "cancelled",
];

/// Describe the series. Called once, by the binary, beside the other `register`s.
pub fn register() {
    metrics::describe_counter!(
        "mw_output_uploads_total",
        "Occurrences of a rate-limited output, by what happened to them. `no_data` is a \
         station that has reported nothing yet and `held` is a service that asked us to \
         slow down; neither is a failure."
    );
    metrics::describe_counter!(
        "mw_output_attempts_total",
        "Delivery attempts, by outcome. One occurrence can make several: the retry budget \
         re-enters the rate limiter, so each attempt takes its own permit."
    );
}

/// Zero every series for one component, so a component that has sent nothing is
/// distinguishable from one whose metrics are broken.
///
/// `register` cannot do this: the `component` label is not known until `plan()` has run, so
/// arming is per component and happens in the builder. The same shape
/// `mw_station::metrics::arm` has, and for the same reason — a counter that only appears
/// after the first event makes "no uploads yet" and "this component was never planned" the
/// same observation, which is the question an operator asks first.
pub fn arm(id: &ComponentId, service: &'static str) {
    for outcome in Outcome::ALL {
        metrics::counter!(
            "mw_output_uploads_total",
            "component" => id.to_string(),
            "service" => service,
            "outcome" => outcome.as_str(),
        )
        .increment(0);
    }
    // The attempt series' failure labels are `UploadError::reason`'s closed set; `ok` is
    // the other one. Armed from the same list the error type owns, so a new variant cannot
    // be armed and uncounted or counted and unarmed.
    for reason in ATTEMPT_OUTCOMES {
        metrics::counter!(
            "mw_output_attempts_total",
            "component" => id.to_string(),
            "service" => service,
            "outcome" => *reason,
        )
        .increment(0);
    }
}

/// Count one occurrence.
pub fn occurrence(id: &ComponentId, service: &'static str, outcome: Outcome) {
    metrics::counter!(
        "mw_output_uploads_total",
        "component" => id.to_string(),
        "service" => service,
        "outcome" => outcome.as_str(),
    )
    .increment(1);
}

/// Count one attempt. `outcome` is [`ATTEMPT_OK`] or a `UploadError::reason`.
pub fn attempt(id: &ComponentId, service: &'static str, outcome: &'static str) {
    metrics::counter!(
        "mw_output_attempts_total",
        "component" => id.to_string(),
        "service" => service,
        "outcome" => outcome,
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The labels are a closed set, which is what bounds the series' cardinality.
    #[test]
    fn every_outcome_has_one_label_and_the_set_is_closed() {
        let labels: Vec<&str> = Outcome::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            labels,
            vec!["sent", "no_data", "held", "failed", "cancelled"]
        );
        assert_eq!(Outcome::Sent.to_string(), "sent");
        // A new variant has to be added to `ALL`, which this count is what catches.
        assert_eq!(Outcome::ALL.len(), 5);
    }

    /// `register` describes both series and `arm` zeroes them; neither panics without a
    /// recorder installed, which is what a unit test runs without.
    #[test]
    fn the_series_can_be_described_and_armed_without_a_recorder() {
        register();
        let id = ComponentId::new("wu:backyard").expect("a component id");
        arm(&id, "wunderground");
        occurrence(&id, "wunderground", Outcome::Sent);
        attempt(&id, "wunderground", ATTEMPT_OK);
    }

    /// **The armed labels and the emitted ones are the same set.** A failure label that is
    /// emitted and not armed is a series that appears only after something has gone wrong,
    /// which is the observation `arm` exists to prevent.
    #[test]
    fn every_emitted_attempt_label_is_armed() {
        use crate::error::{Target, UploadError};
        use std::time::Duration;
        let target = || Target::peer("test", 1);
        let every = [
            UploadError::Connect {
                what: "x",
                target: target(),
                source: std::io::Error::other("x"),
            },
            UploadError::Tls {
                target: target(),
                source: Box::new(std::io::Error::other("x")),
            },
            UploadError::Malformed {
                target: target(),
                expected: "x",
                why: "x",
            },
            UploadError::Rejected {
                target: target(),
                status: 400,
                body: String::new(),
            },
            UploadError::RateLimited {
                target: target(),
                retry_after: None,
            },
            UploadError::Timeout {
                target: target(),
                after: Duration::ZERO,
            },
            UploadError::LoginRefused {
                target: target(),
                response: String::new(),
            },
            UploadError::Cancelled { target: target() },
        ];
        for e in &every {
            assert!(
                ATTEMPT_OUTCOMES.contains(&e.reason()),
                "`{}` is emitted and not armed",
                e.reason()
            );
        }
        // Nine labels: `ok` plus one per variant, and the count is what catches a variant
        // added to the error type without a label here.
        assert_eq!(ATTEMPT_OUTCOMES.len(), every.len() + 1);
    }
}
