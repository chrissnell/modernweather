//! The numbers a station driver owns.
//!
//! One series, and the reason there is exactly one. §5.10's table already carries what
//! happens *after* a frame decodes — `mw_readings_ingested_total{station,origin}` and
//! `mw_readings_rejected_total{reason}` are both emitted from `Ingress::admit` — and the
//! supervisor already carries what happens to an occurrence,
//! `mw_component_ticks_total{component,outcome}`. What neither of them can see is a frame
//! that **never became a reading**: a datagram from a device nobody configured, a body that
//! is not JSON, an empty `conditions` array. The Go receiver's answer to each of those is
//! `logger.Errorf` once per packet, which on a device with a firmware quirk is a line every
//! two seconds and a number nobody can alert on.
//!
//! Cardinality is bounded by construction, which §5.10 requires as a rule rather than an
//! observation: `component` is a component id, so there are as many as configuration says,
//! and `outcome` is the closed enum below.

use mw_component::ComponentId;

/// What happened to one frame before it reached admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Decoded, and handed to `Ingress::admit`. What happened *there* is admission's own
    /// counters' business.
    Decoded,
    /// The bytes were not what this codec decodes.
    Undecodable,
    /// The frame claimed a station this endpoint is not configured for.
    ///
    /// W2a §4a.6 for `UdpIn`: "an unknown DSN is dropped and counted, never registered".
    /// This is that count. Distinct from `mw_readings_rejected_total{unknown_station}`,
    /// which is the *registry's* refusal — this one is the endpoint's, and it happens
    /// before there is a reading to refuse, because the decoder needs the station's sensor
    /// mapping to produce one.
    UnknownStation,
    /// Admission refused it. Counted here too, so that one series answers "what is this
    /// endpoint doing with the frames it receives" without a join.
    Refused,
    /// The frame decoded and what it decoded to is a fault the station's own configuration
    /// says it cannot be.
    ///
    /// One producer: a snow gauge reporting a distance greater than its configured base
    /// distance (`mw_codec::SnowGaugeProto`). **DEV-9** makes that discard this item's
    /// constraint, and a separate label is what makes it visible: a gauge whose base
    /// distance is set two metres short otherwise reports nothing, counts nothing under a
    /// name an operator would look at, and logs a line per reading.
    Implausible,
}

impl FrameOutcome {
    /// The metric label, which is also the log field.
    pub const fn as_str(self) -> &'static str {
        match self {
            FrameOutcome::Decoded => "decoded",
            FrameOutcome::Undecodable => "undecodable",
            FrameOutcome::UnknownStation => "unknown_station",
            FrameOutcome::Refused => "refused",
            FrameOutcome::Implausible => "implausible",
        }
    }

    /// Every outcome, so the series can be zeroed at startup and a driver that has seen
    /// nothing is distinguishable from a driver whose metrics are broken.
    pub const ALL: &'static [FrameOutcome] = &[
        FrameOutcome::Decoded,
        FrameOutcome::Undecodable,
        FrameOutcome::UnknownStation,
        FrameOutcome::Refused,
        FrameOutcome::Implausible,
    ];
}

impl std::fmt::Display for FrameOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Describe the series. Called once, by the binary, beside `mw_pipeline::metrics::register`.
pub fn register() {
    metrics::describe_counter!(
        "mw_station_frames_total",
        "Frames a station endpoint received, by what became of them"
    );
    metrics::describe_counter!(
        "mw_station_resync_bytes_total",
        "Bytes a station session discarded looking for a frame boundary"
    );
}

/// Zero every outcome for one component, so a flat line is distinguishable from no line.
pub fn arm(component: &ComponentId) {
    for outcome in FrameOutcome::ALL {
        metrics::counter!(
            "mw_station_frames_total",
            "component" => component.as_str().to_string(),
            "outcome" => outcome.as_str(),
        )
        .increment(0);
    }
}

/// Zero the resynchronisation count for one component.
///
/// The same reason [`arm`] exists, and more load-bearing here: a flat zero is the whole
/// signal. A station whose serial line is at the right rate never moves this series, so
/// "absent" and "zero" have to be distinguishable or the number says nothing.
pub fn arm_resync(component: &ComponentId) {
    metrics::counter!(
        "mw_station_resync_bytes_total",
        "component" => component.as_str().to_string(),
    )
    .increment(0);
}

/// Count bytes a session dropped looking for a frame boundary.
///
/// A second series rather than a `FrameOutcome`, because these are **bytes** and the other
/// series counts **frames**: adding them to one counter would make its total meaningless.
/// `crate::driver` has why this number exists at all.
pub fn resync(component: &ComponentId, bytes: u64) {
    metrics::counter!(
        "mw_station_resync_bytes_total",
        "component" => component.as_str().to_string(),
    )
    .increment(bytes);
}

/// Count one frame.
pub fn frame(component: &ComponentId, outcome: FrameOutcome) {
    metrics::counter!(
        "mw_station_frames_total",
        "component" => component.as_str().to_string(),
        "outcome" => outcome.as_str(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_outcome_has_a_distinct_label_and_is_in_all() {
        let mut labels: Vec<&str> = FrameOutcome::ALL.iter().map(|o| o.as_str()).collect();
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), before, "two outcomes share a label");
        assert_eq!(
            FrameOutcome::ALL.len(),
            5,
            "a new FrameOutcome needs a line in ALL"
        );
        assert_eq!(FrameOutcome::Decoded.to_string(), "decoded");
    }
}
