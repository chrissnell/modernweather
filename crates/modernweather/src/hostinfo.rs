//! The log rings and the machine, as the management API asks for them.
//!
//! `mw_api_mgmt` declares [`mw_api_mgmt::LogSource`] and [`mw_api_mgmt::Machine`] rather
//! than depending on `mw-obs` or reading `/dev`, because implementation-plan.md **M7-1**
//! makes it the API **over `ConfigAdmin` and `ComponentControl` only**. The rings are
//! `main`'s root resource and the filesystem is the binary's, so this is where the two
//! traits are implemented -- the same arrangement as [`crate::admin`], one layer out.

use std::path::Path;
use std::sync::Arc;

use mw_api_mgmt::error::ApiError;
use mw_api_mgmt::host::{
    LogEntry, LogField, LogSource, LogsView, Machine, SerialPort, SerialPortsView, Stream,
    SystemInfoView,
};
use mw_component::BoxFuture;

/// The two log rings, read through `mw-obs`.
#[derive(Debug)]
pub struct Rings(Arc<mw_obs::LogRings>);

impl Rings {
    /// Over the rings `mw_obs::install` returned.
    pub fn new(rings: Arc<mw_obs::LogRings>) -> Self {
        Self(rings)
    }
}

impl LogSource for Rings {
    /// The newest `limit` entries, oldest first.
    ///
    /// **The rings stay separate and the presentation merges** (W5 §1.8): W3 §5.7's
    /// eviction-asymmetry argument is why there are two -- the access ring is one to two
    /// orders of magnitude the volume, so one shared ring would evict the application log
    /// out from under an incident -- and correlating a 500 with the query that failed is why
    /// the default view is both.
    fn read(&self, stream: Stream, limit: usize) -> LogsView {
        let (records, dropped) = match stream {
            Stream::App => {
                let ring = self.0.ring(mw_obs::Stream::App);
                (ring.snapshot(), ring.dropped())
            }
            Stream::Access => {
                let ring = self.0.ring(mw_obs::Stream::Access);
                (ring.snapshot(), ring.dropped())
            }
            // `merged` interleaves by timestamp, which is the whole point of the merged
            // view; the drop counts are summed because the gap marker §0.4 renders is one
            // marker over one timeline.
            Stream::Both => (
                self.0.merged(),
                self.0.ring(mw_obs::Stream::App).dropped()
                    + self.0.ring(mw_obs::Stream::Access).dropped(),
            ),
        };
        // The newest `limit`, still oldest first: a log view scrolled to the bottom wants
        // the tail, and reversing here would make the client reverse it back.
        let start = records.len().saturating_sub(limit);
        LogsView {
            entries: records[start..]
                .iter()
                .map(|r| LogEntry {
                    ts: r.ts,
                    level: r.level.to_string(),
                    stream: r.stream.as_str().to_string(),
                    target: r.target.to_string(),
                    spans: r.spans.iter().map(|s| (*s).to_string()).collect(),
                    message: r.message.clone(),
                    causes: r.causes.clone(),
                    fields: r
                        .fields
                        .iter()
                        .map(|(name, value)| LogField {
                            name: (*name).to_string(),
                            value: value.to_string(),
                        })
                        .collect(),
                })
                .collect(),
            dropped,
        }
    }
}

/// This process and this host.
#[derive(Debug)]
pub struct Host {
    started_at: chrono::DateTime<chrono::Utc>,
}

impl Host {
    /// Record when this process started.
    ///
    /// Taken once, at boot, rather than read from `/proc` per request: the uptime an operator
    /// wants is the **daemon's**, and a value derived from the process table would change
    /// meaning on a platform without one.
    pub fn new() -> Self {
        Self {
            started_at: chrono::Utc::now(),
        }
    }
}

/// Where the kernel lists serial devices, and where it lists stable names for them.
///
/// Two directories because they answer two different questions. `/sys/class/tty` is every
/// TTY the kernel knows, including the virtual consoles and the ones with no device behind
/// them; `/dev/serial/by-id` is the subset `udev` has given a stable name, which is the one
/// an operator should actually store (`/dev/ttyUSB0` is assigned in probe order, so a Pi with
/// a console and a TNC plugged in swaps them across a reboot).
const TTY_CLASS: &str = "/sys/class/tty";
const BY_ID: &str = "/dev/serial/by-id";

impl Machine for Host {
    fn info(&self) -> SystemInfoView {
        let now = chrono::Utc::now();
        SystemInfoView {
            version: env!("CARGO_PKG_VERSION").to_string(),
            // The triple this binary was built for, which is the one number that tells an
            // operator whether they are running the musl artefact or the gnu one.
            target: target(),
            hostname: read_trimmed(Path::new("/proc/sys/kernel/hostname"))
                .unwrap_or_else(|| "unknown".to_string()),
            os: os_release(),
            started_at: self.started_at,
            uptime_seconds: (now - self.started_at).num_seconds().max(0) as u64,
            pid: std::process::id(),
        }
    }

    fn serial_ports(&self) -> BoxFuture<'_, Result<SerialPortsView, ApiError>> {
        Box::pin(async move {
            let stable = stable_names().await;
            let mut ports: Vec<SerialPort> = Vec::new();
            let mut entries = match tokio::fs::read_dir(TTY_CLASS).await {
                Ok(e) => e,
                // Not an error: a container without `/sys` mounted has no serial ports to
                // offer, and the picker's empty state is the honest answer. A read that
                // fails for another reason is reported, because "you have no serial ports"
                // would send the operator looking at their cable.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(SerialPortsView { ports })
                }
                Err(e) => {
                    return Err(ApiError::Internal {
                        summary: "The serial ports could not be listed.".to_string(),
                        detail: Some(format!("{TTY_CLASS}: {e}")),
                    })
                }
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                // A TTY with no `device` symlink is a virtual console or a pty master, not
                // something a weather station is plugged into.
                if !entry.path().join("device").exists() {
                    continue;
                }
                let device = format!("/dev/{name}");
                ports.push(SerialPort {
                    description: read_trimmed(&entry.path().join("device/../product"))
                        .or_else(|| read_trimmed(&entry.path().join("device/driver/module"))),
                    stable_device: stable.get(&device).cloned(),
                    device,
                });
            }
            ports.sort_by(|a, b| a.device.cmp(&b.device));
            Ok(SerialPortsView { ports })
        })
    }
}

/// `/dev/ttyUSB0` → `/dev/serial/by-id/usb-FTDI_...`, where `udev` has made one.
async fn stable_names() -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(mut entries) = tokio::fs::read_dir(BY_ID).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let link = entry.path();
        if let Ok(target) = tokio::fs::canonicalize(&link).await {
            out.insert(
                target.to_string_lossy().to_string(),
                link.to_string_lossy().to_string(),
            );
        }
    }
    out
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The operating system, as `PRETTY_NAME` or as the kernel release.
///
/// `/etc/os-release` first because that is what an operator recognises -- "Debian GNU/Linux
/// 13 (trixie)" rather than `6.12.0-rpi`. The kernel release is the fallback, and it is the
/// only answer inside a scratch container.
fn os_release() -> String {
    if let Some(text) = read_trimmed(Path::new("/etc/os-release")) {
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                return value.trim_matches('"').to_string();
            }
        }
    }
    read_trimmed(Path::new("/proc/sys/kernel/osrelease"))
        .map(|release| format!("Linux {release}"))
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// The build's architecture and operating system, as one string.
///
/// Not the full target triple: Rust exposes that only to a build script, and adding a
/// `build.rs` to this crate for a diagnostics string would be a build step for a line of
/// text. `std::env::consts` carries the two parts an operator asks about -- "am I running
/// the ARM build or the x86 one" -- and the libc half, which is the part a triple adds, is
/// visible in the operating-system line beside it.
fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The uptime is the **daemon's**, counted from the instant recorded at boot, so a
    /// support question about "how long has it been up" answers about this process rather
    /// than about the machine.
    #[test]
    fn the_uptime_is_this_process_s() {
        let host = Host::new();
        let info = host.info();
        assert!(info.uptime_seconds < 5);
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.target.is_empty());
    }

    /// A read with a smaller limit than the ring holds returns the **newest** entries, still
    /// oldest first: a log view opens at the bottom, and reversing here would make the
    /// client reverse it back.
    #[tokio::test]
    async fn a_limited_read_returns_the_newest_entries_oldest_first() {
        let rings = Arc::new(mw_obs::LogRings::with_capacities(16, 16));
        for i in 0..10 {
            rings.ring(mw_obs::Stream::App).push(mw_obs::Record {
                ts: chrono::Utc::now(),
                level: tracing::Level::INFO,
                target: "test",
                module: "test",
                spans: Vec::new(),
                fields: Vec::new(),
                message: format!("{i}"),
                causes: Vec::new(),
                stream: mw_obs::Stream::App,
            });
        }
        let view = Rings::new(rings).read(Stream::App, 3);
        let messages: Vec<&str> = view.entries.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, ["7", "8", "9"]);
        assert_eq!(view.dropped, 0);
    }

    /// A ring that has evicted reports it **twice, and deliberately**: as `mw-obs`'s own
    /// gap record, dated at the oldest surviving entry, and as the total on the view.
    ///
    /// §0.4 renders the count as the **gap marker in the log view** rather than as a number
    /// in a corner, because "where is the hole" is the question it answers -- and that is
    /// `Ring::snapshot`'s synthetic record, which this crate must therefore pass through
    /// rather than filter out. `dropped` stays beside it for the case the marker is not in
    /// view: a tail read slices the oldest entries off, and the summary is then the only
    /// thing that says the ring has been losing records.
    #[tokio::test]
    async fn an_evicting_ring_reports_what_it_dropped() {
        let rings = Arc::new(mw_obs::LogRings::with_capacities(2, 2));
        for i in 0..5 {
            rings.ring(mw_obs::Stream::App).push(mw_obs::Record {
                ts: chrono::Utc::now(),
                level: tracing::Level::WARN,
                target: "test",
                module: "test",
                spans: Vec::new(),
                fields: Vec::new(),
                message: format!("{i}"),
                causes: Vec::new(),
                stream: mw_obs::Stream::App,
            });
        }
        let view = Rings::new(rings).read(Stream::App, 100);
        // The two survivors, and the gap marker in front of them.
        assert_eq!(view.entries.len(), 3);
        assert!(
            view.entries[0].message.contains("dropped"),
            "{:?}",
            view.entries[0]
        );
        let survived: Vec<&str> = view.entries[1..]
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(survived, ["3", "4"]);
        assert_eq!(view.dropped, 3);
    }
}
