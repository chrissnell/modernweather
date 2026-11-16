//! W7 §7's uplink and collector tables — "how far behind is each satellite" as numbers.
//!
//! Today it is invisible: there is no counter, no gauge and no log line from which an
//! operator can derive how much a satellite is holding, because the satellite holds nothing
//! — a failed send is a dropped reading and an `Errorf` (`grpcstream/client.go:362-367`).
//!
//! One function per fact, as `mw_pipeline::metrics` has it, and the `mw_` prefix `mw-obs`
//! establishes. W7 §7 writes the names without it; one namespace for one process's metrics
//! is worth more than two sections' literal text, and `mw_pipeline::metrics` already made
//! that call for the spool's half of the same table.
//!
//! # Cardinality
//!
//! Every label here is a **peer name** (`peers.name`, so N is in the configuration), an
//! **origin** rendered as hex, or a closed enum. The `origin` label is the one that needs
//! its own argument, because `mw_pipeline::metrics` declined it: there it was bounded by
//! `MAX_ORIGINS`, a constant, which passes the letter of §5.10's rule and fails its test.
//! Here it is bounded by `peer_origins` — an operator's explicit per-peer grant — so "there
//! are N of these and N is in the configuration" is true, which is exactly the condition
//! that module said would make the breakdown land with this one.
//!
//! **`spool_lag_envelopes` and the rest of the spool's gauges are not here.** They are on
//! every node, not only on a satellite, and `mw_pipeline::metrics` owns them. W7 §7's first
//! table is that module's; the second and third are this one's.

use mw_component::{PeerName, Reject};

/// `uplink_state`, as the number W5 maps back to a sentence (§4.3's state machine).
///
/// An enum rather than one gauge per state: a state machine in N gauges can report two
/// states at once, and the first time that happens is during a transition nobody is
/// watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkState {
    /// No connection, and none being made.
    Disconnected,
    /// Dialling, or completing the TLS handshake.
    Connecting,
    /// Connected; `Hello` in flight.
    Hello,
    /// Sending batches and reading acks.
    Streaming,
    /// Shutting down: no new batches, waiting for outstanding acks.
    Draining,
}

impl UplinkState {
    /// The gauge's value. Ordered as §4.3's diagram is, so that a dashboard's y-axis reads
    /// as progress.
    pub const fn as_f64(self) -> f64 {
        match self {
            UplinkState::Disconnected => 0.0,
            UplinkState::Connecting => 1.0,
            UplinkState::Hello => 2.0,
            UplinkState::Streaming => 3.0,
            UplinkState::Draining => 4.0,
        }
    }

    /// The name a log line uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            UplinkState::Disconnected => "disconnected",
            UplinkState::Connecting => "connecting",
            UplinkState::Hello => "hello",
            UplinkState::Streaming => "streaming",
            UplinkState::Draining => "draining",
        }
    }

    /// Every state, so the gauge can be zeroed at startup.
    pub const ALL: &'static [UplinkState] = &[
        UplinkState::Disconnected,
        UplinkState::Connecting,
        UplinkState::Hello,
        UplinkState::Streaming,
        UplinkState::Draining,
    ];
}

/// Why an uplink is running below the contract it was configured for (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    /// The collector answered `Hello` with `UNIMPLEMENTED`, so it is a Go-era node and
    /// there is no ack to wait for: the cursor advances on **send** and this uplink is
    /// at-most-once, exactly as today.
    NoAckSupport,
}

impl Degradation {
    /// The `reason` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Degradation::NoAckSupport => "no_ack_support",
        }
    }
}

/// Describe and zero everything this crate owns.
///
/// Called once from the boot sequence, for `mw_pipeline::metrics::register`'s reason: a
/// Prometheus counter does not exist in the exposition until something touches it, so a
/// collector that has refused nothing and a collector whose metrics are broken look
/// identical to a scraper.
///
/// **The per-peer series are not zeroed here and the node-wide ones are**, because zeroing
/// a per-peer series needs the peer set and this function has no configuration. The
/// components zero their own on start — `crate::uplink` on `build`, `crate::service` when
/// it mounts — which is also when the label value is known to be a configured one.
///
/// The node-wide ones are `ingest_handshake_refused_total` and `ingest_enrolments_total`,
/// and both are here. An earlier version zeroed only the first, so this paragraph claimed
/// a set larger than the code's.
pub fn register() {
    metrics::describe_gauge!(
        "mw_uplink_state",
        "Which state each uplink's state machine is in (0 disconnected .. 4 draining)"
    );
    metrics::describe_gauge!(
        "mw_uplink_ack_lag_seconds",
        "How long ago each uplink's cursor was last advanced by an acknowledgement"
    );
    metrics::describe_gauge!(
        "mw_uplink_unacked_envelopes",
        "The in-flight window plus the unsent remainder, per uplink"
    );
    metrics::describe_counter!(
        "mw_uplink_batches_sent_total",
        "Batches handed to a collector, per uplink"
    );
    metrics::describe_counter!(
        "mw_uplink_envelopes_sent_total",
        "Envelopes handed to a collector, per uplink"
    );
    metrics::describe_counter!(
        "mw_uplink_filtered_total",
        "Envelopes the loop filter skipped, per uplink -- a mutual uplink is visible here"
    );
    metrics::describe_counter!(
        "mw_uplink_loop_refused_total",
        "Envelopes `Envelope::forward` refused. NON-ZERO IS A BUG IN THIS CODE (W7 D-41)"
    );
    metrics::describe_counter!(
        "mw_uplink_resume_skipped_total",
        "Envelopes not re-sent because HelloAck.known_through already covered them"
    );
    metrics::describe_counter!(
        "mw_uplink_path_full_total",
        "Envelopes `forward` refused at the hop bound. W7 D-8's bound, NOT D-41's bug"
    );
    metrics::describe_counter!(
        "mw_uplink_rejected_total",
        "Envelopes a collector acknowledged as refused, per uplink"
    );
    metrics::describe_counter!(
        "mw_uplink_reconnects_total",
        "Connections established, per uplink"
    );
    metrics::describe_counter!(
        "mw_uplink_duplicates_acked_total",
        "Envelopes a collector acknowledged as duplicates, per uplink"
    );
    metrics::describe_gauge!(
        "mw_uplink_degraded",
        "1 while an uplink is running below its configured contract, by reason"
    );
    metrics::describe_gauge!(
        "mw_clock_skew_seconds",
        "This node's clock minus its collector's, from HelloAck.server_time"
    );
    metrics::describe_counter!(
        "mw_ingest_envelopes_total",
        "Envelopes durably appended on behalf of a peer, per peer and origin"
    );
    metrics::describe_counter!(
        "mw_ingest_duplicates_total",
        "Envelopes the dedup gate already had, per origin -- expected in a diamond"
    );
    metrics::describe_counter!(
        "mw_ingest_rejected_total",
        "Envelopes refused by admission, per peer and reason"
    );
    metrics::describe_counter!(
        "mw_ingest_handshake_refused_total",
        "Handshakes refused at the ingest socket before any frame was decoded"
    );
    metrics::describe_counter!(
        "mw_ingest_enrolments_total",
        "Enrolment attempts, by outcome"
    );
    metrics::describe_gauge!(
        "mw_peer_last_seen_seconds",
        "How long ago each peer last opened a stream -- silence is the failure mode a \
         stream-based protocol hides"
    );
    metrics::describe_gauge!(
        "mw_peer_reported_depth_envelopes",
        "IngestBatch.sender_depth. PEER-REPORTED, not this node's own measurement"
    );
    metrics::describe_gauge!(
        "mw_peer_software_info",
        "1, labelled with the software version each peer last reported (W7 OQ-W7-6)"
    );
    metrics::counter!("mw_ingest_handshake_refused_total").absolute(0);
    // Node-wide and a closed label set, so it is zeroed here rather than per peer: an
    // enrolment surface nobody has used and one whose metrics are broken would otherwise
    // read the same.
    for outcome in ENROLMENT_OUTCOMES {
        metrics::counter!("mw_ingest_enrolments_total", "outcome" => outcome).absolute(0);
    }
}

/// Zero every series an uplink owns, at `build`, so a peer that has sent nothing is
/// distinguishable from one whose metrics are broken.
pub fn arm_uplink(peer: &PeerName) {
    let peer = peer.as_str().to_string();
    // One series per peer, not one per state: the state IS the value. `Disconnected` is
    // the honest starting value rather than a zero that happens to coincide with it.
    metrics::gauge!("mw_uplink_state", "peer" => peer.clone())
        .set(UplinkState::Disconnected.as_f64());
    for name in [
        "mw_uplink_batches_sent_total",
        "mw_uplink_envelopes_sent_total",
        "mw_uplink_filtered_total",
        "mw_uplink_resume_skipped_total",
        "mw_uplink_path_full_total",
        "mw_uplink_rejected_total",
        "mw_uplink_loop_refused_total",
        "mw_uplink_reconnects_total",
        "mw_uplink_duplicates_acked_total",
    ] {
        metrics::counter!(name, "peer" => peer.clone()).absolute(0);
    }
    metrics::gauge!("mw_uplink_unacked_envelopes", "peer" => peer.clone()).set(0.0);
    metrics::gauge!(
        "mw_uplink_degraded",
        "peer" => peer.clone(),
        "reason" => Degradation::NoAckSupport.as_str(),
    )
    .set(0.0);
}

/// Zero every series a peer owns on a collector, when the ingest service mounts.
///
/// `origins` is the peer's own node plus whatever it may relay for (§5.5's grant), because
/// `ingest_envelopes_total` and `ingest_duplicates_total` are labelled by origin and a
/// collector that has received nothing from a configured peer must be distinguishable from
/// one whose ingest path is broken — which is the whole point of arming. Bounded by
/// configuration: the grant is a `peers` row, not something a connection claims.
pub fn arm_peer(peer: &PeerName, origins: &[mw_types::OriginId]) {
    let peer = peer.as_str().to_string();
    for reason in Reject::ALL {
        metrics::counter!(
            "mw_ingest_rejected_total",
            "peer" => peer.clone(),
            "reason" => reason.as_str(),
        )
        .absolute(0);
    }
    for origin in origins {
        // `to_string`, which is what `count_by_origin` labels with -- a different rendering
        // here would arm a series nothing ever increments.
        let origin = origin.to_string();
        metrics::counter!(
            "mw_ingest_envelopes_total",
            "peer" => peer.clone(),
            "origin" => origin.clone(),
        )
        .absolute(0);
        metrics::counter!("mw_ingest_duplicates_total", "origin" => origin).absolute(0);
    }
}

/// §4.3's state machine, as a number.
pub fn uplink_state(peer: &PeerName, state: UplinkState) {
    metrics::gauge!("mw_uplink_state", "peer" => peer.as_str().to_string()).set(state.as_f64());
}

/// **The headline number**: now − the instant this uplink's cursor last advanced
/// (`Uplink::acked_at`, which is where the arithmetic lives).
///
/// Reported on every `readings` call, successful or not, because the quantity is the age of
/// the last advance: a link that has been down for six hours must read six hours, and a
/// gauge written only on success stays at its last good value for ever instead.
pub fn ack_lag(peer: &PeerName, seconds: f64) {
    metrics::gauge!("mw_uplink_ack_lag_seconds", "peer" => peer.as_str().to_string()).set(seconds);
    retain(&ACK_LAGS, peer.as_str(), seconds);
}

/// The in-flight window plus the unsent remainder.
pub fn unacked(peer: &PeerName, envelopes: u64) {
    #[allow(clippy::cast_precision_loss)]
    metrics::gauge!("mw_uplink_unacked_envelopes", "peer" => peer.as_str().to_string())
        .set(envelopes as f64);
    #[allow(clippy::cast_precision_loss)]
    retain(&UNACKED, peer.as_str(), envelopes as f64);
}

/// One batch handed over, and the envelopes in it.
pub fn sent(peer: &PeerName, envelopes: u64) {
    metrics::counter!("mw_uplink_batches_sent_total", "peer" => peer.as_str().to_string())
        .increment(1);
    metrics::counter!("mw_uplink_envelopes_sent_total", "peer" => peer.as_str().to_string())
        .increment(envelopes);
}

/// The loop filter fired: a mutual uplink is visible rather than silent (§1.4).
pub fn filtered(peer: &PeerName, envelopes: u64) {
    metrics::counter!("mw_uplink_filtered_total", "peer" => peer.as_str().to_string())
        .increment(envelopes);
}

/// `Envelope::forward` refused.
///
/// **Non-zero means a bug in this code** (W7 **D-41**): the filter should have caught it and
/// the invariant did. Stated here as well as in the metric's description so that the counter
/// has a meaning when it moves.
pub fn loop_refused(peer: &PeerName, envelopes: u64) {
    metrics::counter!("mw_uplink_loop_refused_total", "peer" => peer.as_str().to_string())
        .increment(envelopes);
}

/// Envelopes the fast-resume skip did not re-send, because `known_through` covered them
/// (§4.2).
///
/// Separate from [`filtered`]: a filtered envelope was never for this peer, and a skipped
/// one was for it and had already arrived. The two together are "not sent"; one is a
/// topology fact and the other is a lost ack.
pub fn resume_skipped(peer: &PeerName, envelopes: u64) {
    metrics::counter!("mw_uplink_resume_skipped_total", "peer" => peer.as_str().to_string())
        .increment(envelopes);
}

/// `Envelope::forward` refused because the path is at `MAX_PATH` (W7 **D-8**).
///
/// **Not** [`loop_refused`], and the separation is the point: this is the hop bound
/// working, where that one is documented as "a bug in this code".
pub fn path_full(peer: &PeerName, envelopes: u64) {
    metrics::counter!("mw_uplink_path_full_total", "peer" => peer.as_str().to_string())
        .increment(envelopes);
}

/// Envelopes a collector acknowledged as refused (`IngestAck.rejected`).
///
/// **Counted on every status, not only on `ACK_REJECTED`.** A batch of 256 where 255 were
/// spooled and one was refused answers `ACK_OK`, so without this series that one envelope
/// is lost at the collector with nothing on the sending side saying so.
pub fn rejected_by_collector(peer: &PeerName, envelopes: u64) {
    metrics::counter!("mw_uplink_rejected_total", "peer" => peer.as_str().to_string())
        .increment(envelopes);
}

/// A connection was established.
pub fn reconnected(peer: &PeerName) {
    metrics::counter!("mw_uplink_reconnects_total", "peer" => peer.as_str().to_string())
        .increment(1);
}

/// The collector already had these: resend volume, which is what makes a lost ack visible.
pub fn duplicates_acked(peer: &PeerName, envelopes: u64) {
    metrics::counter!(
        "mw_uplink_duplicates_acked_total",
        "peer" => peer.as_str().to_string()
    )
    .increment(envelopes);
}

/// Whether this uplink is running below its configured contract (§9.3).
pub fn degraded(peer: &PeerName, reason: Degradation, on: bool) {
    metrics::gauge!(
        "mw_uplink_degraded",
        "peer" => peer.as_str().to_string(),
        "reason" => reason.as_str(),
    )
    .set(if on { 1.0 } else { 0.0 });
}

/// This node's clock minus its collector's, from `HelloAck.server_time` (§4.6).
///
/// A drifting clock is a number before it is a rejection, which is the whole reason
/// `server_time` is on the ack.
pub fn clock_skew(peer: &PeerName, seconds: f64) {
    metrics::gauge!("mw_clock_skew_seconds", "peer" => peer.as_str().to_string()).set(seconds);
}

/// Envelopes durably appended for a peer, by origin.
pub fn ingested(peer: &PeerName, origin: &str, envelopes: u64) {
    metrics::counter!(
        "mw_ingest_envelopes_total",
        "peer" => peer.as_str().to_string(),
        "origin" => origin.to_string(),
    )
    .increment(envelopes);
}

/// The dedup gate firing — expected and non-zero in §2.3's diamond.
pub fn duplicates(origin: &str, envelopes: u64) {
    metrics::counter!("mw_ingest_duplicates_total", "origin" => origin.to_string())
        .increment(envelopes);
}

/// An envelope refused by admission, with §5.4's full `Reject` set as the label.
pub fn rejected(peer: &PeerName, reason: Reject, envelopes: u64) {
    metrics::counter!(
        "mw_ingest_rejected_total",
        "peer" => peer.as_str().to_string(),
        "reason" => reason.as_str(),
    )
    .increment(envelopes);
}

/// A refusal before a peer was identified at all.
///
/// Unlabelled on purpose: at the handshake there is no peer name, and labelling it with
/// anything the connection presented would be a label whose values come from a request.
pub fn handshake_refused() {
    metrics::counter!("mw_ingest_handshake_refused_total").increment(1);
}

/// Every value the `outcome` label can take, so that [`register`] can zero them.
///
/// A closed set named in one place: the alternative is a label whose series appears only
/// once the thing it counts has happened, which is what `register` exists against. The
/// assertion in this module's tests is what keeps it the same set the call sites use.
pub const ENROLMENT_OUTCOMES: [&str; 8] = [
    "accepted",
    "rate_limited",
    "closed",
    "no_material",
    "too_many_stations",
    "name_taken",
    "cannot_issue",
    "error",
];

/// How an enrolment attempt ended. Every attempt is counted, successful or not (§5.3).
pub fn enrolment(outcome: &'static str) {
    metrics::counter!("mw_ingest_enrolments_total", "outcome" => outcome).increment(1);
}

/// A peer opened a stream: `peer_last_seen_seconds` resets to zero.
///
/// The collector's own authoritative view, as against the peer-reported depth beside it.
pub fn peer_seen(peer: &PeerName) {
    peer_last_seen(peer, 0.0);
}

/// How long ago a peer was last heard from.
///
/// Written by [`peer_seen`] with zero when a batch arrives, and recomputed for every known
/// peer by `Handlers::sweep_last_seen` on the scrape path -- which is the half that makes
/// the gauge able to show silence at all.
pub fn peer_last_seen(peer: &PeerName, seconds: f64) {
    metrics::gauge!("mw_peer_last_seen_seconds", "peer" => peer.as_str().to_string()).set(seconds);
}

// -------------------------------------------------------------------------------------
// N-11: the three per-peer numbers the management UI renders, retained as well as
// exported.
//
// The `metrics` facade is **write-only by design** -- there is no way to read a gauge back
// out of a recorder -- and W5's `/data-storage/send` and `/data-storage/receive` have to
// render three of these gauges as numbers on a page (docs/reconciliation.md C-W7-29).
// Retaining the last value beside the gauge keeps **one** writer per number, which is the
// property that matters: a second owner computing the same figure its own way is how a
// page and a dashboard come to disagree about whether an operator is losing readings.
//
// Each map is bounded by the peer count, which is bounded by the `peers` table. A name
// that is no longer a peer keeps a few words until the process restarts, which is cheaper
// than this module having to learn when a peer was revoked.
// -------------------------------------------------------------------------------------

/// One retained gauge value, with when it was recorded.
#[derive(Debug, Clone, Copy)]
pub struct Retained {
    /// The value.
    pub value: f64,
    /// When it was written, so an age can be turned back into an instant.
    pub at: chrono::DateTime<chrono::Utc>,
}

type Retention =
    std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<String, Retained>>>;

fn retain(map: &Retention, peer: &str, value: f64) {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            peer.to_string(),
            Retained {
                value,
                at: chrono::Utc::now(),
            },
        );
}

fn retained(map: &Retention) -> std::collections::BTreeMap<String, Retained> {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

static PEER_DEPTHS: Retention =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
static ACK_LAGS: Retention =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
static UNACKED: Retention =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

/// `IngestBatch.sender_depth`. **Peer-reported** — W5 must label it as such.
pub fn peer_depth(peer: &PeerName, envelopes: u64) {
    #[allow(clippy::cast_precision_loss)]
    metrics::gauge!(
        "mw_peer_reported_depth_envelopes",
        "peer" => peer.as_str().to_string()
    )
    .set(envelopes as f64);
    #[allow(clippy::cast_precision_loss)]
    retain(&PEER_DEPTHS, peer.as_str(), envelopes as f64);
}

/// What each peer last reported about its own unsent readings (**N-11**).
///
/// **Peer-reported**, and every consumer is required to say so: W7 D-40 makes the label
/// part of the number, because this is a satellite's claim about itself and not a
/// measurement this node made.
pub fn peer_reported_depths() -> std::collections::BTreeMap<String, Retained> {
    retained(&PEER_DEPTHS)
}

/// How long ago each uplink's cursor was last advanced by an acknowledgement (**N-11**).
///
/// The age rather than the instant, because that is what the gauge is: reported on every
/// `readings` call whether it succeeded or not, so a link that has been down for six hours
/// reads six hours instead of staying at its last good value for ever. The absolute
/// instant W5's "last successful delivery" shows is `at - value seconds`.
pub fn ack_lags() -> std::collections::BTreeMap<String, Retained> {
    retained(&ACK_LAGS)
}

/// The in-flight window plus the unsent remainder, per peer, in envelopes (**N-11**).
pub fn unacked_envelopes() -> std::collections::BTreeMap<String, Retained> {
    retained(&UNACKED)
}

/// What a peer whose `software` is NULL is labelled with.
///
/// **This is the Go-era signal** (§9, OQ-W7-6), and it is a signal rather than a guess:
/// `peers.software` is written by `Hello`, and `Hello` is the one RPC a Go-era satellite
/// cannot make — it speaks `WeatherV1.SendWeatherReadings`, whose message has no field for
/// a version. So "this peer has never completed a `Hello`" and "this peer cannot" are the
/// same observation, and the shim's removal condition is *no series carries this label*.
///
/// A freshly enrolled peer that has not connected yet also reads `unknown`, and that is
/// correct for the same reason: nothing has yet shown that it speaks the new protocol.
pub const SOFTWARE_UNKNOWN: &str = "unknown";

/// The longest a reported version may be as a label.
const MAX_SOFTWARE_LABEL: usize = 32;

/// What a version that is not one collapses to.
pub const SOFTWARE_INVALID: &str = "invalid";

/// The label a reported version is rendered as.
///
/// **`Hello.software` is peer-controlled**, which makes it the one value in this module
/// that is not a configured name, an origin or a closed enum — and §5.10's cardinality rule
/// is about exactly that. `Hello` carries no rate limit (unlike `Enroll`), so without a
/// bound an enrolled-but-misbehaving satellite could mint a fresh series per call by
/// sending a random string. So: at most [`MAX_SOFTWARE_LABEL`] bytes, and only the
/// characters a version number has. Anything else is [`SOFTWARE_INVALID`] — a label an
/// operator can alert on, rather than a refusal, because what a peer calls itself is not
/// worth closing a stream over.
pub fn software_label(version: &str) -> &str {
    let version = version.trim();
    if version.is_empty() {
        return SOFTWARE_UNKNOWN;
    }
    let legal = version.len() <= MAX_SOFTWARE_LABEL
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '+' | '-'));
    if legal {
        version
    } else {
        SOFTWARE_INVALID
    }
}

/// Which peers are still Go-era (§9), which is what W7 OQ-W7-6 gates the shim's removal on.
///
/// The label goes through [`software_label`]. `previous` is the label this peer last
/// carried, if any: its series is zeroed, because an info gauge that is only ever set to 1
/// leaves every version a peer has ever reported reading as current — and OQ-W7-6's removal
/// condition is "no series carries `unknown`", which a stale 1 makes permanently
/// unsatisfiable.
pub fn peer_software(peer: &PeerName, version: &str, previous: Option<&str>) {
    let version = software_label(version);
    if let Some(previous) = previous.filter(|p| *p != version) {
        metrics::gauge!(
            "mw_peer_software_info",
            "peer" => peer.as_str().to_string(),
            "version" => previous.to_string(),
        )
        .set(0.0);
    }
    metrics::gauge!(
        "mw_peer_software_info",
        "peer" => peer.as_str().to_string(),
        "version" => version.to_string(),
    )
    .set(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state machine's gauge has to be injective, or two states read as one.
    #[test]
    fn every_uplink_state_has_a_distinct_value_and_name() {
        let mut values: Vec<u64> = UplinkState::ALL
            .iter()
            .map(|s| s.as_f64().to_bits())
            .collect();
        let before = values.len();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), before);
        let mut names: Vec<&str> = UplinkState::ALL.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "two states share a name");
    }

    /// **The one peer-controlled label in this module** (§5.10), so the bound is asserted
    /// rather than assumed: `Hello.software` is a string a satellite chooses and `Hello`
    /// carries no rate limit, so an unbounded label here is a cardinality explosion one
    /// enrolled peer can cause on its own.
    #[test]
    fn a_peer_cannot_choose_an_unbounded_software_label() {
        assert_eq!(software_label("modernweather 0.1.0"), "modernweather 0.1.0");
        assert_eq!(
            software_label("  modernweather 0.1.0  "),
            "modernweather 0.1.0"
        );
        // Empty is the Go-era signal, not an invalid value (§9, OQ-W7-6).
        assert_eq!(software_label(""), SOFTWARE_UNKNOWN);
        assert_eq!(software_label("   "), SOFTWARE_UNKNOWN);
        // Too long, and a random string of legal characters is the cheap attack.
        assert_eq!(
            software_label(&"a".repeat(MAX_SOFTWARE_LABEL + 1)),
            SOFTWARE_INVALID
        );
        assert_eq!(software_label(&"a".repeat(MAX_SOFTWARE_LABEL)).len(), 32);
        // A label that would break the exposition format, or carry a newline into it.
        assert_eq!(software_label("go\n1.21"), SOFTWARE_INVALID);
        assert_eq!(software_label("a\"b"), SOFTWARE_INVALID);
        assert_eq!(software_label("{peer=\"x\"}"), SOFTWARE_INVALID);
        // Every value collapses to one of a bounded set, which is the property.
        for version in ["\u{1}", "ünïcödé", "；", "go\u{7}1.21"] {
            assert_eq!(software_label(version), SOFTWARE_INVALID, "{version:?}");
        }
        // Whitespace alone is trimmed away and reads as a peer that reported nothing,
        // which is `unknown` rather than `invalid`.
        assert_eq!(software_label("\t"), SOFTWARE_UNKNOWN);
    }

    /// **`ENROLMENT_OUTCOMES` has to be the set the call sites use**, or `register` zeroes
    /// a series nothing writes and leaves a written one absent — which is the failure
    /// `register` exists to prevent, reintroduced by a label added in one place.
    ///
    /// Read out of the source rather than asserted by hand, because a hand-kept list is the
    /// thing being checked. The two forms a call site takes are `m::enrolment("literal")`
    /// and the `match` in `Handlers::enrol` that resolves a `PeerWriteError` to one of
    /// these strings.
    #[test]
    fn every_enrolment_outcome_the_service_records_is_armed() {
        let source = include_str!("service.rs");
        let mut used: Vec<&str> = Vec::new();
        for (marker, close) in [("m::enrolment(\"", '"'), ("=> \"", '"')] {
            for rest in source.split(marker).skip(1) {
                if let Some(value) = rest.split(close).next() {
                    // The `=> "..."` form catches every string-valued match arm in the
                    // file, so it is filtered to the ones that look like an outcome: lower
                    // snake case, which every value in the list is.
                    if !value.is_empty()
                        && value.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        && (marker.starts_with("m::") || ENROLMENT_OUTCOMES.contains(&value))
                    {
                        used.push(value);
                    }
                }
            }
        }
        used.sort_unstable();
        used.dedup();
        assert!(
            !used.is_empty(),
            "the scan found no call sites, so it proves nothing"
        );
        for outcome in &used {
            assert!(
                ENROLMENT_OUTCOMES.contains(outcome),
                "`{outcome}` is recorded by the service and not armed by `register`"
            );
        }
        for outcome in ENROLMENT_OUTCOMES {
            assert!(
                used.contains(&outcome),
                "`{outcome}` is armed and nothing records it"
            );
        }
    }

    #[test]
    fn the_states_are_ordered_as_the_state_machine_is() {
        assert_eq!(UplinkState::Disconnected.as_f64(), 0.0);
        assert!(UplinkState::Streaming.as_f64() > UplinkState::Hello.as_f64());
        assert!(UplinkState::Hello.as_f64() > UplinkState::Connecting.as_f64());
    }
}
