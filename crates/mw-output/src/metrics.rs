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

    /// `register` describes both series and does not panic without a recorder installed,
    /// which is what a unit test runs without.
    #[test]
    fn the_series_can_be_described_without_a_recorder() {
        register();
        let id = ComponentId::new("wu:backyard").expect("a component id");
        occurrence(&id, "wunderground", Outcome::Sent);
        attempt(&id, "wunderground", ATTEMPT_OK);
    }
}
