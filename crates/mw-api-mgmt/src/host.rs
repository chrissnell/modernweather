//! The two things this API reports that are neither configuration nor component state: the
//! log rings and the machine.
//!
//! Both are traits rather than dependencies, and that is the constraint
//! implementation-plan.md **M7-1** states: this crate is the management API **over
//! `ConfigAdmin` and `ComponentControl` only**. The log rings are `mw-obs`'s and the binary
//! owns them as a root resource; `/dev` is the binary's to read. Declaring what is needed and
//! letting the binary supply it is the same move `mw_http::ListenerConfig` makes, and it is
//! what keeps the layout gate's rule for this crate narrow enough to mean something.

use mw_component::BoxFuture;

use crate::error::ApiError;

/// One structured field on a log entry.
///
/// A named pair rather than a `(String, String)` tuple or a map, and each alternative is
/// rejected for its own reason: a tuple becomes a positional array in the schema, which is a
/// shape with no name on either side of the wire, and a map loses the order the event
/// recorded its fields in -- which is the order the log table shows them in.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LogField {
    /// The field's name.
    pub name: String,
    /// Its value, rendered.
    pub value: String,
}

/// One log entry, already rendered.
///
/// `message` is the formatted event and `causes` is its chain, rendered at the point it was
/// recorded (**D-34**) rather than reconstructed here. `fields` is flattened to strings
/// because the log view shows them and does not compute on them — and because a `Value`
/// union in the schema would make the generated TypeScript a tagged union the log table has
/// no use for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LogEntry {
    /// When.
    pub ts: chrono::DateTime<chrono::Utc>,
    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub level: String,
    /// Which ring it came from: `app` or `access`.
    ///
    /// W3 §5.7 keeps the two **rings** separate -- its eviction-asymmetry argument holds --
    /// and §1.8 merges the *presentation*, because correlating a 500 with the query that
    /// failed is the whole job.
    pub stream: String,
    /// The event's target.
    pub target: String,
    /// The spans it happened inside, outermost first. This is what makes per-station
    /// filtering possible at all (§5.4).
    pub spans: Vec<String>,
    /// The formatted message.
    pub message: String,
    /// Its cause chain.
    pub causes: Vec<String>,
    /// Its structured fields, in the order the event recorded them.
    pub fields: Vec<LogField>,
}

/// What a read of the log rings returns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LogsView {
    /// The entries, oldest first.
    pub entries: Vec<LogEntry>,
    /// How many entries the ring has evicted since the process started.
    ///
    /// §0.4 renders this as the **gap marker** in the log view rather than as a number in a
    /// corner: "{n} log entries dropped" belongs where the gap is, because that is the
    /// question it answers.
    pub dropped: u64,
}

/// A standing log-level override.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LogLevelsView {
    /// The filter directive in force, in `RUST_LOG` syntax.
    ///
    /// Carried verbatim and **not** translated: a person turning on debug logging for one
    /// station is reading documentation that uses this syntax, which is §0.3's test applied
    /// to a field only that person sees.
    pub directive: String,
}

/// What a log-level change submits.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LogLevelInput {
    /// The directive to install, or `null` to go back to the one the process started with.
    #[serde(default)]
    pub directive: Option<String>,
}

/// Which ring to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The application ring.
    App,
    /// The access ring.
    Access,
    /// Both, merged by timestamp — §1.8's presentation.
    Both,
}

impl Stream {
    /// Parse the `?stream=` parameter.
    ///
    /// Absent is `Both`, which is the view §1.8 makes the default. A value this build does
    /// not know is `None` and a `422`, not a silent fall-back: a client asking for a stream
    /// that does not exist has a bug, and answering it with *both* rings hides it behind
    /// data that looks right.
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s {
            None | Some("") => Some(Stream::Both),
            Some("app") => Some(Stream::App),
            Some("access") => Some(Stream::Access),
            _ => None,
        }
    }
}

/// Something that wants every log entry as it is recorded.
///
/// The live half of §5.7's `subscribe()`. The rings' own crate may not name a runtime
/// (`mw-obs`'s `LogTap` has the argument), so this is the shape on this side of that
/// boundary: the management API registers one sink and owns the channel behind it.
///
/// **Called on the emitting thread.** An implementation must not block and must not log.
pub trait LogSink: Send + Sync + 'static {
    /// One entry, already rendered.
    fn entry(&self, entry: LogEntry);
}

/// The log rings, as this crate needs them.
pub trait LogSource: Send + Sync + 'static {
    /// The newest `limit` entries from `stream`, oldest first.
    fn read(&self, stream: Stream, limit: usize) -> LogsView;

    /// Register a live subscriber (§5.7's follow-on half).
    ///
    /// Defaulted to a no-op so that a consumer with nothing to tap -- every `oneshot` test
    /// -- implements one method rather than two. The consequence is explicit: such a build
    /// serves the log **backfill** and no live records, which is a visibly empty stream
    /// rather than a silently stale one, because the `caught_up` marker still arrives.
    fn tap(&self, _sink: std::sync::Arc<dyn LogSink>) {}
}

/// One serial port the operator can pick.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SerialPort {
    /// The device path to store.
    pub device: String,
    /// What it is, if the kernel says: "FTDI FT232R USB UART". Shown beside the path,
    /// because `/dev/ttyUSB0` and `/dev/ttyUSB1` are indistinguishable without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A stable path that survives a reboot, where the kernel offers one.
    ///
    /// Offered because `/dev/ttyUSB0` is assigned in probe order: a Pi with a console and a
    /// TNC plugged in swaps them across a reboot, and the station silently reads the wrong
    /// device. The picker defaults to this where it exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_device: Option<String>,
}

/// What `GET /api/v1/system/serial-ports` returns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SerialPortsView {
    /// Every port found, by path.
    pub ports: Vec<SerialPort>,
}

/// One station's latest reading, as the Home tiles and §0.7 step 3 render it.
///
/// A handful of named fields rather than the 212-column reading: these are the ones §0.7's
/// panel shows, and widening it later is additive under §4.4. The units are in the field
/// names because the frozen contract's are, and a unit carried in a name cannot be
/// converted twice.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LatestReading {
    /// Which station, by the key every path segment uses.
    pub station_key: String,
    /// What the operator calls it, so the tile needs no second lookup.
    ///
    /// Filled in by the stream from the vocabulary, **not** by the implementation of
    /// [`Readings`]: D-36 puts the translation on the server in one place, and the half of
    /// the system that produces readings has no business knowing what a station is called.
    /// An implementation may leave it empty.
    pub station_name: String,
    /// When the station says it measured.
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_f: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barometer_inhg: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_speed_mph: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_dir_degrees: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solar_wm2: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rain_day_in: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snow_depth_in: Option<f32>,
}

/// The latest reading per station, as this crate needs it.
///
/// A trait for the same reason [`LogSource`] and [`Machine`] are: the live snapshot is
/// `mw-pipeline`'s and this crate may not name that crate (`ci/layers.toml`). The binary
/// is where both may be named.
pub trait Readings: Send + Sync + 'static {
    /// Every station that has produced a reading since this process started.
    ///
    /// **Called by the live stream whenever [`Readings::changes`] fires**, which is also
    /// when an implementation may fold the new reading into whatever it accumulates. That
    /// is deliberate: an implementation that accumulated on a background task of its own
    /// would be a second task in a process that owns exactly one task table (§3.2), and
    /// nothing needs the accumulation while nobody is looking at it.
    fn latest(&self) -> Vec<LatestReading>;

    /// A receiver that fires when a new reading has arrived.
    ///
    /// The channel carries the newest envelope because that is the channel the reading
    /// fan-out already publishes -- forwarding it costs nothing, and adapting it into a
    /// counter would mean a task whose only job is to re-signal an edge. The live stream
    /// reads the **value** through [`Readings::latest`]; what it uses this for is the
    /// edge.
    ///
    /// Defaulted to `None`, which is a node that measures nothing. The tiles then say "no
    /// readings yet", which is the truth rather than a stale figure.
    fn changes(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<Option<std::sync::Arc<mw_types::Envelope>>>> {
        None
    }
}

/// `System → This machine`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SystemInfoView {
    /// This build's version.
    pub version: String,
    /// The target triple it was built for.
    pub target: String,
    /// The host's name.
    pub hostname: String,
    /// The operating system and release.
    pub os: String,
    /// When this process started.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// How long it has been running, in seconds.
    pub uptime_seconds: u64,
    /// The process id, which is what an operator needs to find it in `journalctl`.
    pub pid: u32,
}

/// The machine, as this crate needs it.
pub trait Machine: Send + Sync + 'static {
    /// This process and this host.
    fn info(&self) -> SystemInfoView;

    /// Every serial port the operator could plug a station into.
    ///
    /// Fallible and asynchronous because it reads the filesystem: a `/dev` that cannot be
    /// listed is a `500` naming the cause rather than an empty picker, which would read as
    /// "you have no serial ports" and send the operator looking at their cable.
    fn serial_ports(&self) -> BoxFuture<'_, Result<SerialPortsView, ApiError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absent `?stream=` is both rings merged, which is the view §1.8 makes the default:
    /// correlating a 500 with the query that failed is the job, so the merged view is what
    /// the operator wants before they know what they are looking for.
    #[test]
    fn an_unspecified_stream_reads_both() {
        assert_eq!(Stream::parse(None), Some(Stream::Both));
        assert_eq!(Stream::parse(Some("app")), Some(Stream::App));
        assert_eq!(Stream::parse(Some("access")), Some(Stream::Access));
    }

    /// A stream this build does not know is refused rather than answered with both rings:
    /// a client asking for one that does not exist has a bug, and data that looks right is
    /// the worst possible place to hide it.
    #[test]
    fn an_unknown_stream_is_not_silently_both() {
        assert_eq!(Stream::parse(Some("nonsense")), None);
    }
}

// ---------------------------------------------------------------------------
// N-11 (docs/reconciliation.md C-W7-29): W7's fleet observability, which W5 designed
// pages for before W7 had specified the numbers.
// ---------------------------------------------------------------------------

/// The unsent-readings buffer, as `/data-storage/send` renders it.
///
/// W7 specified four numbers after W5 had finished and W5's inventory therefore has none
/// of them. Three are here and the fourth is on [`PeerView`]:
///
/// * `time_remaining_seconds` -- W7's `spool_time_remaining_seconds`;
/// * the `ceiling < max_lag x rate` advisory of W7 §3.5, as the three figures it compares
///   plus the comparison's result, so that the sentence is rendered by W5 §0's vocabulary
///   rather than assembled from a server-side string;
/// * the depth, which W5 already had.
///
/// **The comparison is made here, not in the client.** It is one inequality over three
/// numbers, and a client that re-derives it is a second opinion about whether an operator
/// is about to lose data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SpoolView {
    /// Where the buffer lives, for `System → Advanced` and for a support message.
    pub path: String,
    /// How much disk unsent readings may use before the oldest are reclaimed (W7 D-16).
    pub ceiling_bytes: u64,
    /// How long a cursor may be behind before it is abandoned.
    pub max_lag_seconds: u64,
    /// What is on disk now.
    pub depth_bytes: u64,
    /// How many readings are waiting, when the buffer can say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth_envelopes: Option<u64>,
    /// The observed write rate, over the window this process has been running.
    ///
    /// `None` until there is enough traffic to divide by: a rate computed from two
    /// appends is a number that would make the advisory below oscillate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_per_second: Option<f64>,
    /// W7's `spool_time_remaining_seconds`: how long the remaining ceiling covers at the
    /// observed rate. `None` for the same reason as the rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_remaining_seconds: Option<f64>,
    /// What the ceiling would have to be to cover `max_lag` at the observed rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needed_for_max_lag_bytes: Option<u64>,
    /// Whether the ceiling covers `max_lag` at the observed rate.
    ///
    /// `None` when there is no rate to compare against, which is **not** the same as
    /// `true`: "we cannot tell yet" and "you are fine" are different sentences, and
    /// collapsing them is what makes a ceiling policy a coin flip (W7 §3.5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling_covers_max_lag: Option<bool>,
    /// The last time a peer acknowledged a batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<chrono::DateTime<chrono::Utc>>,
}

/// One peer, as `/data-storage/receive` renders it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PeerView {
    /// The peer's name, as it enrolled.
    pub name: String,
    /// Its node id, hex, shown only under `System → Advanced`.
    pub node_id: String,
    /// When it was last heard from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
    /// Which stations it has registered, by key.
    pub stations: Vec<String>,
    /// When its enrolment was revoked, if it was.
    ///
    /// Revocation is a timestamp rather than a deletion precisely so that "this peer was
    /// trusted until Tuesday" is readable off this page -- so a view that dropped it would
    /// render a refused peer as a working one, which is the most expensive direction for a
    /// fleet page to be wrong in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    /// W7 D-40's `IngestBatch.sender_depth`: how many readings the **peer says** it still
    /// has unsent.
    ///
    /// **Peer-reported, and the field name says so**, because W7 D-40 requires W5 to label
    /// it as such wherever it appears and a field called `depth` would make that a
    /// convention the next page could forget. It is what lets this node render fleet-wide
    /// lag without scraping a satellite whose link is down -- and it is the satellite's
    /// own claim, not a measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_reported_depth_envelopes: Option<u64>,
}

/// One outstanding or historical enrolment token.
///
/// W7 D-33, as W5 is required to surface it: **leaving enrolment open is the mistake and
/// it should be visible.** `Enroll` is served only while an unexpired, unconsumed token
/// exists, so an outstanding row is a door that is open right now.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct EnrolmentToken {
    /// The row's id, hex. Never the token: the row holds an Argon2id hash and the
    /// plaintext was printed once.
    pub id: String,
    /// Which peer it was minted for.
    pub peer_name: String,
    /// When it was issued.
    pub issued_at: chrono::DateTime<chrono::Utc>,
    /// When it stops working.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// `outstanding`, `used` or `expired` -- the three states the row can be in.
    pub state: String,
    /// When it was consumed, if it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Which node consumed it, hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_by: Option<String>,
}

/// Outstanding enrolment and the history behind it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct EnrolmentView {
    /// Tokens that would admit a new peer **right now**.
    pub outstanding: Vec<EnrolmentToken>,
    /// Every token this node has minted, newest first -- the enrolment log.
    ///
    /// The log and the outstanding list come from the same rows rather than from a
    /// separate audit table, and that is a property worth stating: a history assembled
    /// from a different source than the live state is a history that can disagree with it.
    pub log: Vec<EnrolmentToken>,
}

/// What `GET /api/v1/fleet` returns.
///
/// An object whose three members are each nullable, rather than three routes, because the
/// two pages that render it show all three together and §4.4 wants one object with named
/// fields so a fourth number is additive.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FleetView {
    /// The unsent-readings buffer, or `None` on a node that has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool: Option<SpoolView>,
    /// Every peer this node knows.
    pub peers: Vec<PeerView>,
    /// Enrolment, which is `None` on a node that is not a collector.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrolment: Option<EnrolmentView>,
}

/// W7's fleet numbers, as this crate needs them (**N-11**).
///
/// A trait, like [`LogSource`], [`Machine`] and [`Readings`]: the spool is `mw-pipeline`'s,
/// the peer rows are `mw-store`'s and the peer-reported depth is `mw-collector`'s, and
/// `ci/layers.toml` forbids this crate from naming any of the three. The binary is where
/// all of them may be named -- the same move `ConfigAdmin` makes for the same reason.
pub trait Fleet: Send + Sync + 'static {
    /// The whole resource. Asynchronous because enrolment is a database read.
    fn view(&self) -> BoxFuture<'_, Result<FleetView, ApiError>>;
}
