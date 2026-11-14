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

/// The log rings, as this crate needs them.
pub trait LogSource: Send + Sync + 'static {
    /// The newest `limit` entries from `stream`, oldest first.
    fn read(&self, stream: Stream, limit: usize) -> LogsView;
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
