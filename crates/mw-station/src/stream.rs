//! Opening a [`ByteStream`] over the two transports a station conversation runs on:
//! `Serial` and `TcpOut`.
//!
//! `Transport` says *how the bytes move* and holds no connection (`crate::transport`); this
//! is where it becomes one. Separate from the session above it and from the driver above
//! that, because the three fail differently and the supervisor needs to tell them apart: a
//! device path that does not exist is a `BuildError::Resource` and therefore `Backoff`
//! (**W2a Decision 3**), where a console that stops answering mid-conversation is a session
//! failure and therefore a restart.
//!
//! # The serial port, and the line rate it now sets
//!
//! **`transport_baud` is applied.** `serial` opens the character device through
//! `tokio_serial`, which is `tcgetattr`, `cfmakeraw`, 8N1 with no flow control, the rate,
//! `tcsetattr` — and then the same `AsyncFd` registration this module used to perform by
//! hand. It reads the rate back out of the line afterwards and logs it, because "we called
//! `tcsetattr`" and "the line is at 115200" are not the same claim.
//!
//! That is amendment **A-M6-1-1**, accepted 2026-11-14 (GRA-788) after being recorded as
//! *proposed* by M6-1 on 2026-11-11. What it replaced was a build that carried `baud` this
//! far and dropped it: the device was opened with `O_NONBLOCK | O_NOCTTY` and polled, so the
//! line discipline was whatever the device already had, and a directly attached console
//! worked if and only if a `udev` rule or an `stty` had already configured it. **The failure
//! that produced was not silence** — the bytes arrive framed wrong, so every Davis packet
//! fails its CRC-16 and every Campbell line fails to parse, and
//! `mw_station_frames_total{outcome="undecodable"}` and `mw_station_resync_bytes_total` count
//! every one of them while the station looks connected. The reason it was left is in
//! `docs/reconciliation.md` A-M6-1-1: setting a rate is `tcsetattr`, **D-32**'s crate table
//! named neither `libc` nor a serial-port crate, and `[workspace.lints.rust]`'s
//! `unsafe_code` makes hand-rolling the `ioctl` a build error — so the table gained a row
//! rather than this module gaining an `unsafe`.
//!
//! It also **deleted the only `unsafe` block in this workspace**, which was not the goal and
//! is the better half of the trade: `tokio_serial::SerialStream` is an `AsyncFd` over an
//! owned `mio_serial::SerialStream`, so the safety argument that used to live at the
//! registration call site is now the dependency's.
//!
//! What the three transports do, stated rather than discovered:
//!
//! - **A directly attached console works**, which is the configuration this change exists
//!   for. `transport_baud` is the rate the line runs at.
//! - **A `ser2net` bridge is unaffected**, because the bridge owns the line and this end is
//!   a TCP socket. That is **N-20**'s production path, and it worked before this change too.
//! - **A `socat` pty pair works, and now asserts something.** A pty has no line rate — no
//!   bits are clocked at any speed — but it is a real tty with real `termios`, and Linux
//!   stores and reports the rate set on it. So
//!   `the_configured_line_rate_is_set_on_the_tty` is a `tcgetattr` assertion that would have
//!   failed against the previous build, which is the test criterion 3 of GRA-788 asked for.
//!   **What it does not prove is timing**, and no test in this repository does: that the
//!   kernel accepted and recorded 115200 is not evidence that a UART clocked 115200, and
//!   the only apparatus that could say so is hardware.
//!
//! # The upgrade hazard, which is the inverse of the defect
//!
//! **`transport_baud` was decorative and is now authoritative, and that can break a line
//! that worked.** The column has been *mandatory* for a serial station since
//! `migrations/config/20261102230000_config_schema_initial.sql`'s
//! `CHECK (transport_kind <> 'serial' OR (transport_dev IS NOT NULL AND transport_baud IS
//! NOT NULL))`, and until this change nothing ever compared it against reality. So every
//! existing serial row carries a value that has never been validated. A deployment whose
//! `udev` rule or `stty` set the line to the **correct** rate while the column holds a stale
//! one worked before and fails after — with precisely the CRC-failure symptom this change
//! exists to remove.
//!
//! The readback below does not catch that, and it is worth being clear about why: the kernel
//! accepts a *wrong* rate as readily as a right one, so the mismatch arm never fires and the
//! operator sees `undecodable` frames again. **The check is an operator's, not this
//! module's**: on upgrade, confirm `transport_baud` — and `uploader.aprs_kiss_serial_baud`,
//! which `mw_output::plan` defaults to 9600 when the column is NULL and now puts that on the
//! radio — against whatever was configuring the line before.
//!
//! The open is also **exclusive** as of this change, which is a behaviour change in its own
//! right; `serial`'s documentation has the three consequences.
//!
//! **One platform cost, and it is `serialport`'s own recorded note.** On macOS the rate is
//! set with the `IOSSIOSPEED` ioctl, which **refuses a pseudo terminal with `ENOTTY`**
//! (`serialport-4.10.1/src/posix/termios.rs:93`). So the pty tests in this module are
//! `cfg`'d off there, where before they ran: macOS is supported so that the crate builds on
//! a developer's machine, and a real `/dev/cu.*` is unaffected. Linux, which is every target
//! in `ci/release.toml`, uses `BOTHER` with `TCSETS2` and takes arbitrary rates.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mw_component::{BoxFuture, BuildError};
use tokio_serial::{SerialPort as _, SerialPortBuilderExt};
use tracing::{debug, warn};

use crate::session::ByteStream;
use crate::transport::Transport;

/// How long to wait for a TCP connection to a console or a bridge.
///
/// The Go Davis driver's `net.DialTimeout` value (`davis/station.go:308`), kept so that the two
/// daemons give a bridge the same chance during the M5-2 comparison.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a station's transport could not be opened.
///
/// Every variant is a **build** failure rather than a tick failure, which is W2a Decision 3:
/// a station whose console is unplugged is `Backoff`ing with a message an operator can read
/// in `ComponentStatus::last_error`, not a component that starts and reports nothing.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The character device could not be opened.
    #[error("the serial device {dev} could not be opened")]
    Device {
        /// Which device.
        dev: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// `transport_dev` names a path that exists and is a regular file.
    ///
    /// Worth its own variant because the path exists, so `Device`'s "could not be opened"
    /// reads like a cable fault. What actually happens is that the **`TIOCEXCL` ioctl**
    /// answers `ENOTTY` -- it runs before the `tcgetattr` (`posix/tty.rs:134-141`), so it is
    /// the first call to notice there is no line here -- and `serialport` reports that in
    /// its catch-all `ErrorKind::Unknown`, which is why `classify` decides this from the
    /// file type rather than from the errno. Measured rather than inferred from the source:
    /// on a regular file the open succeeds, `TIOCEXCL` returns `ENOTTY`, and the `tcgetattr`
    /// behind it would have returned `ENOTTY` too.
    #[error(
        "the serial device {dev} is a regular file: `transport_dev` has to name a character \
         device -- a serial port, or a `socat` pty"
    )]
    NotAStream {
        /// Which device.
        dev: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The host could not be resolved or connected to.
    #[error("{host}:{port} could not be reached")]
    Connect {
        /// The configured host.
        host: String,
        /// The port.
        port: u16,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The connection attempt exceeded [`CONNECT_TIMEOUT`].
    #[error("{host}:{port} did not accept a connection within {seconds}s")]
    Timeout {
        /// The configured host.
        host: String,
        /// The port.
        port: u16,
        /// The window.
        seconds: u64,
    },

    /// A `tcp_out` transport with TLS files, which no station driver loads yet.
    #[error(
        "station transport TLS is configured for {host}:{port} and no station driver loads \
         it: a Davis or Campbell bridge on a LAN is plain TCP, and the vendor-cloud case \
         that needs this is docs/component-model.md §4a.7's"
    )]
    Tls {
        /// The configured host.
        host: String,
        /// The port.
        port: u16,
    },

    /// A transport that is not one a conversation runs over.
    #[error("a `{kind}` transport is not one a station session runs over")]
    NotAStream2 {
        /// The configured kind.
        kind: &'static str,
    },
}

impl OpenError {
    /// This failure as a [`BuildError`], which is how it reaches the supervisor.
    pub fn build(self, what: String) -> BuildError {
        BuildError::Resource {
            what,
            source: Box::new(self),
        }
    }
}

impl mw_types::Classify for OpenError {
    /// Retryable, including the two that look permanent. A device path that does not exist
    /// is a USB cable somebody is about to plug in, and a host that does not resolve is DNS
    /// that is about to come up -- and `Backoff` caps the retry rate either way (**D-27**),
    /// so the cost of being wrong in this direction is one log line per minute and the cost
    /// of being wrong in the other is a station that stays dead until the process restarts.
    ///
    /// Two of the three configuration mistakes -- [`OpenError::Tls`] and
    /// [`OpenError::NotAStream2`] -- are refused at **plan** time by `crate::plan`, so they
    /// are not reached here by any plannable configuration and their classification changes
    /// nothing. **[`OpenError::NotAStream`] is the third and is not refused anywhere**: a
    /// `transport_dev` naming a regular file is plannable, so it reaches this and retries
    /// forever under `Backoff` against a mistake no amount of waiting fixes. That is the
    /// existing policy rather than a new one -- the alternative is a station that stays dead
    /// across a `transport_dev` an operator is about to correct -- and `last_error` names the
    /// file and the reason once a minute. Noted because this change sharpened the variant
    /// into a pure configuration error, which is the moment it became worth saying.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::RETRYABLE
    }
}

/// Open the stream one station's session will run over.
///
/// `station` is for the log line and nothing else.
pub async fn open(transport: &Transport, station: &str) -> Result<Box<dyn ByteStream>, OpenError> {
    match transport {
        Transport::Serial { dev, baud } => {
            let port = serial(dev, *baud, station)?;
            Ok(Box::new(port))
        }
        Transport::TcpOut { host, port, tls } => {
            if tls.is_some() {
                return Err(OpenError::Tls {
                    host: host.clone(),
                    port: *port,
                });
            }
            let socket = tcp(host, *port).await?;
            Ok(Box::new(socket))
        }
        other => Err(OpenError::NotAStream2 { kind: other.kind() }),
    }
}

/// Open a character device and set its line rate.
///
/// **Must be called from inside a tokio runtime**, which is not visible in the signature:
/// `SerialStream::open` registers an `AsyncFd` and panics without a current reactor. Not an
/// `async fn` because nothing here awaits — the open, the `tcsetattr` and the registration
/// are all synchronous — so the obligation has to be written down rather than carried by the
/// type. Every caller reaches this through [`open`], which is `async`.
///
/// **The two open flags this module used to set by hand are still set, and that is worth
/// recording rather than trusting.** `serialport` opens with `O_NOCTTY | O_NONBLOCK`
/// (`posix/tty.rs:127`) and `mio_serial` re-sets `O_NONBLOCK` on the final descriptor after
/// `serialport` clears it (`mio-serial-5.0.7/src/lib.rs:538`). Both matter and neither is
/// cosmetic: `open(2)` on a tty blocks until the modem lines assert carrier, which on an
/// unplugged serial cable is forever, and opening a terminal from a daemon with no
/// controlling terminal would otherwise *acquire* one, so a `SIGHUP` on it would reach the
/// whole process.
///
/// 8N1 and no flow control are stated rather than left to the crate's defaults, because they
/// are what both station protocols need and what the Go daemon used: a Davis console is
/// 8N1 (`davis/station.go`'s `serial.Config`) and so is a Campbell datalogger. They happen to
/// be `serialport`'s defaults today; saying so means a change to those defaults is not a
/// change to this program.
///
/// # The open is exclusive, which is a behaviour change and is the one intended
///
/// `.exclusive(true)` is `serialport`'s default and is written out here for the same reason
/// 8N1 is: an inherited default that decides whether a second process can read the line is
/// not a thing to leave implicit. It takes **`TIOCEXCL` and an exclusive `flock`**
/// (`posix/tty.rs:134-137`), and the previous `OpenOptions::open` took neither.
///
/// **It is on purpose and it is the second defect this change fixes.** One serial line has
/// one reader. Two readers do not each get the stream; they split it, and both ends then
/// produce gap-ridden data that still parses — which is the exact failure
/// `tests/serial_bridge.rs` exists to prove `ser2net` does *not* have, and the reason
/// **N-20** terminates the line outside the pod rather than opening it twice. A refusal an
/// operator can read in `ComponentStatus::last_error` is strictly better than two components
/// silently halving each other's frames.
///
/// **Three consequences, all of them operator-visible, none of them silent:**
///
/// - **Two components on one `transport_dev` no longer both start.** Nothing refuses that
///   configuration at plan time — `crate::plan` compares no device paths, and neither does
///   `mw_output::plan`, so a station and an APRS `kiss-serial` sink may both name
///   `/dev/ttyUSB0`. Whichever is built second now gets `EBUSY`
///   (`ErrorKind::NoDevice` → [`OpenError::Device`], carrying "Device or resource busy")
///   and `Backoff`s. **Which one loses depends on build order, so the failure is not
///   deterministic** — that is worse than it should be and better than the split stream it
///   replaced. Refusing it at plan time, where [`OpenError::Tls`] and
///   [`OpenError::NotAStream2`] already are, is a separate work item.
/// - **A device another process holds is refused**, where it previously opened. If `ser2net`
///   or `gpsd` owns the line, this daemon's transport is `tcp_out` and not `serial`; that is
///   N-20's path and it is unaffected.
/// - **The reverse also holds**: this daemon now holds the `flock`, so a `ser2net` started
///   afterwards against the same device answers `Device open failure: Object was already in
///   use`. `packaging/serial-bridge/README.md` documents that symptom from the other
///   direction; this is the new direction.
fn serial(dev: &Path, baud: u32, station: &str) -> Result<SerialPort, OpenError> {
    // `to_string_lossy` because `serialport`'s builder takes a `str` and a device path is an
    // `OsStr`. A non-UTF-8 `transport_dev` therefore fails to open rather than opening the
    // wrong thing, and the error names the lossy form, which is the one an operator can see
    // in the management UI anyway.
    let port = tokio_serial::new(dev.to_string_lossy(), baud)
        .data_bits(tokio_serial::DataBits::Eight)
        .parity(tokio_serial::Parity::None)
        .stop_bits(tokio_serial::StopBits::One)
        .flow_control(tokio_serial::FlowControl::None)
        .exclusive(true)
        .open_native_async()
        .map_err(|e| classify(dev, e))?;

    // Read the rate back off the line, once per connect. The point is the gap between the
    // two claims: `tcsetattr` returning says the kernel accepted the request, and this says
    // what `tcgetattr` reports afterwards. They agree on every Linux target -- `BOTHER` with
    // `TCSETS2` takes arbitrary rates -- and a driver that clamps or a platform that refuses
    // is exactly the case where an operator needs to be told rather than left to infer it
    // from CRC failures. Not an error: the open succeeded and the station may well work.
    match port.baud_rate() {
        Ok(actual) if actual == baud => {
            debug!(
                station,
                device = %dev.display(),
                baud,
                "opened a serial device and set the line rate"
            );
        }
        Ok(actual) => {
            warn!(
                station,
                device = %dev.display(),
                configured_baud = baud,
                line_baud = actual,
                "opened a serial device and the line is NOT at the configured rate -- the \
                 driver or the platform refused it. Frames will arrive framed wrong and be \
                 counted undecodable."
            );
        }
        Err(e) => {
            debug!(
                station,
                device = %dev.display(),
                baud,
                error = %e,
                "opened a serial device and set the line rate; reading it back was refused"
            );
        }
    }

    Ok(SerialPort {
        port,
        dev: dev.to_path_buf(),
    })
}

/// Which [`OpenError`] a `tokio_serial` failure is.
///
/// **Decided from the file type rather than from the error**, and that is the crate's
/// limitation rather than a preference. `serialport` translates **twelve** errnos and answers
/// `ErrorKind::Unknown` for every other one (`posix/error.rs`), and the two this path can
/// actually produce — `ENOTTY` from the `TIOCEXCL` and the `tcgetattr` that read the line's
/// current state, and `EINVAL` from the `tcsetattr` that writes the new one — are both in
/// that catch-all, alongside errnos meaning the opposite. A `stat` is a direct measurement of
/// the thing [`OpenError::NotAStream`] is about, so it is used instead of inferring it.
///
/// **What this predicate does and does not catch**, because `is_file` is narrower than the
/// question. A regular file and a symlink to one are `NotAStream`, which is the only case
/// reachable from a configuration somebody would write. A **FIFO** or a **block device**
/// opens, fails `TIOCEXCL` the same way, and lands in `Device` with a "could not be opened"
/// message about a path that opened — imprecise, and left that way deliberately:
/// `FileType::is_char_device` would classify those two correctly and is
/// `std::os::unix::fs::FileTypeExt`, which would make this module Unix-only again for a
/// message on a path no configuration reaches. `Device` with the real `ENOTTY` attached is
/// not a wrong answer there, just a less pointed one.
///
/// The `stat` follows symlinks on purpose: `socat`'s `link=` is a symlink to a `/dev/pts/N`.
/// A path that has gone away between the open and here is `Device`, which is what it would
/// have been anyway.
fn classify(dev: &Path, e: tokio_serial::Error) -> OpenError {
    let dev = dev.to_path_buf();
    let source = std::io::Error::from(e);
    if std::fs::metadata(&dev)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        OpenError::NotAStream { dev, source }
    } else {
        OpenError::Device { dev, source }
    }
}

/// Connect to a console or a serial-to-Ethernet bridge.
async fn tcp(host: &str, port: u16) -> Result<TcpByteStream, OpenError> {
    let address = format!("{host}:{port}");
    let connect = tokio::net::TcpStream::connect(&address);
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(source)) => {
            return Err(OpenError::Connect {
                host: host.to_string(),
                port,
                source,
            })
        }
        Err(_) => {
            return Err(OpenError::Timeout {
                host: host.to_string(),
                port,
                seconds: CONNECT_TIMEOUT.as_secs(),
            })
        }
    };
    // A console or a bridge sends one small packet at a time and expects a command
    // immediately: Nagle would hold `LOOP 20\n` until the previous write was acked, which
    // on a 1.5-second packet cadence is a round trip of latency per command.
    if let Err(e) = stream.set_nodelay(true) {
        debug!(address, error = %e, "TCP_NODELAY was refused");
    }
    debug!(address, "connected to a station over TCP");
    Ok(TcpByteStream(stream))
}

/// A character device at a configured line rate, polled.
pub struct SerialPort {
    port: tokio_serial::SerialStream,
    dev: PathBuf,
}

impl fmt::Debug for SerialPort {
    /// By hand rather than derived, so that a `{:?}` in a log line stays one short field.
    /// `SerialStream`'s own `Debug` prints the descriptor and the `AsyncFd` around it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerialPort")
            .field("dev", &self.dev)
            .finish_non_exhaustive()
    }
}

impl SerialPort {
    /// The rate the line is actually at.
    ///
    /// **On Linux this asks the kernel** — `TCGETS2`, not plain `tcgetattr`, which is the
    /// mechanism that makes it able to report an arbitrary `BOTHER` rate at all
    /// (`serialport`'s `posix/tty.rs:605`). That distinction is the whole of amendment
    /// **A-M6-1-1**: the previous build had the configured value and nothing else, so this
    /// accessor returns the measurement and there is no field for the request to be confused
    /// with.
    ///
    /// **On macOS and iOS it is `Ok(self.baud_rate)`** — the value the port was opened with,
    /// held in a field (`posix/tty.rs:654`). So there the readback in `serial` is a
    /// tautology and its mismatch arm cannot fire. Said here because an accessor that is a
    /// measurement on one platform and an echo on another is exactly the kind of thing a
    /// reader is entitled to be told rather than left to find.
    pub fn baud_rate(&self) -> Result<u32, std::io::Error> {
        self.port.baud_rate().map_err(std::io::Error::from)
    }
}

impl ByteStream for SerialPort {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            use tokio::io::AsyncReadExt;
            self.port.read(buf).await
        })
    }

    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            self.port.write_all(bytes).await
            // **No flush, and that is not an omission.** It is now reachable -- `AsyncWrite`
            // for `SerialStream` forwards `poll_flush` to `tcdrain` -- where before this
            // module's only option was `sync_data`, which is `EINVAL` on every tty because
            // `tty_fops` declares no `fsync`. It is still not called, for the reason that
            // always applied: the write above returning means the bytes are in the driver's
            // queue and the console reads them from there. `tcdrain` would add a syscall that
            // blocks until the queue empties -- at 9600 baud, milliseconds of it -- inside an
            // `async fn`, to guarantee something no caller asked for.
        })
    }
}

/// A TCP connection to a console or a bridge.
#[derive(Debug)]
pub struct TcpByteStream(tokio::net::TcpStream);

impl ByteStream for TcpByteStream {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            use tokio::io::AsyncReadExt;
            self.0.read(buf).await
        })
    }

    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            self.0.write_all(bytes).await?;
            self.0.flush().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TlsFiles;
    use mw_types::{Class, Classify};

    /// Read until at least `want` bytes have arrived. A pty delivers a write in as many
    /// reads as it likes, and a test that assumed one would be flaky rather than wrong.
    async fn read_at_least(stream: &mut dyn ByteStream, buf: &mut [u8], want: usize) -> usize {
        let mut seen = 0;
        while seen < want {
            let n = stream.read(&mut buf[seen..]).await.expect("reads");
            assert_ne!(n, 0, "the other end closed after {seen} of {want} bytes");
            seen += n;
        }
        seen
    }

    #[tokio::test]
    async fn a_device_path_that_does_not_exist_names_the_device() {
        let transport = Transport::Serial {
            dev: PathBuf::from("/dev/there-is-no-such-port"),
            baud: 19_200,
        };
        let e = open(&transport, "backyard").await.expect_err("no device");
        assert!(matches!(e, OpenError::Device { .. }), "{e}");
        assert!(e.to_string().contains("/dev/there-is-no-such-port"), "{e}");
        assert_eq!(e.class(), Class::RETRYABLE);
    }

    /// The case the path exists for: a `transport_dev` that names a regular file.
    ///
    /// **No longer Linux-only**, and that is worth a sentence because it used to be. The
    /// refusal came from `epoll_ctl` answering `EPERM`, which `kqueue` does not do, so the
    /// assertion could not be made on macOS. Now the refusal comes from `tcgetattr` -- a
    /// regular file has no line discipline to read on any Unix -- and [`classify`] decides
    /// the variant from the file type, so the answer is the same everywhere this crate
    /// builds.
    #[tokio::test]
    async fn a_regular_file_is_refused_as_not_a_stream() {
        let dir = std::env::temp_dir().join(format!("mw-station-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a writable temporary directory");
        let path = dir.join("not-a-tty");
        std::fs::write(&path, b"LOO").expect("writable");
        let transport = Transport::Serial {
            dev: path.clone(),
            baud: 19_200,
        };
        let e = open(&transport, "backyard")
            .await
            .expect_err("not a stream");
        assert!(matches!(e, OpenError::NotAStream { .. }), "{e}");
        assert!(e.to_string().contains("character device"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_tcp_console_that_is_not_listening_names_the_address() {
        // Port 1 on the loopback: privileged, unbound, and refused immediately rather than
        // after the connect timeout.
        let transport = Transport::TcpOut {
            host: "127.0.0.1".into(),
            port: 1,
            tls: None,
        };
        let e = open(&transport, "backyard").await.expect_err("refused");
        assert!(matches!(e, OpenError::Connect { .. }), "{e}");
        assert!(e.to_string().contains("127.0.0.1:1"), "{e}");
    }

    // There is no unit test here that connects to a listener this crate bound, and that is
    // `ci/layers.toml`'s `only-mw-http-binds-a-socket` rule rather than an omission: its
    // needle list names the listener types and the scan covers `tests/` as well as `src/`,
    // so only `mw-http` may open one anywhere in this workspace. What exercises the TCP
    // path end to end is `tests/emulator_davis.rs` and `tests/emulator_campbell.rs`
    // against the vendored `cmd/davis-emulator` and `cmd/campbell-emulator`, which bind
    // their own ports in their own processes -- the
    // stronger test anyway, and what W3 D-82's third level means.

    #[tokio::test]
    async fn the_two_configuration_mistakes_plan_refuses_are_still_named_here() {
        let tls = Transport::TcpOut {
            host: "davis.lan".into(),
            port: 22_222,
            tls: Some(TlsFiles {
                ca: Some(PathBuf::from("/etc/ssl/ca.pem")),
                cert: None,
                key: None,
            }),
        };
        let e = open(&tls, "backyard").await.expect_err("no TLS loader");
        assert!(matches!(e, OpenError::Tls { .. }), "{e}");

        let inbound = Transport::UdpIn {
            port: 22_222,
            group: None,
        };
        let e = open(&inbound, "roof")
            .await
            .expect_err("not a conversation");
        assert!(
            matches!(e, OpenError::NotAStream2 { kind: "udp_in" }),
            "{e}"
        );
    }

    /// **The test that would have failed against the previous build** — GRA-788's criterion
    /// 3, and the only one of its five that is a measurement rather than a decision.
    ///
    /// `serial` is called rather than [`open`] because this asserts on a `SerialPort` and
    /// `open` erases it to a `Box<dyn ByteStream>`. The rate asked for is **115200**, which
    /// is the owner's Campbell datalogger's and the number GRA-787 raised this against; the
    /// pty's own default is 38400, so the assertion distinguishes "the rate was set" from
    /// "the rate happened to match".
    ///
    /// # What this proves, and what it does not
    ///
    /// `SerialPort::baud_rate` is a `tcgetattr` on the descriptor the daemon is about to
    /// read from. A pty is a real tty with real `termios`: Linux stores the rate set on it
    /// and reports it back, which is why this assertion is possible at all and why
    /// `stty -F <link> speed` answers 115200 after it. **Against the previous build it would
    /// have answered 38400**, because that build never called `tcsetattr` — so this is not a
    /// test that passes either way, which is the trap `docs/reconciliation.md` **G-1** names
    /// and the reason the old emulator tests could not be allowed to stand in for it.
    ///
    /// **It is not evidence about timing, and nothing in this repository is.** No bits are
    /// clocked at any rate on a pty. What is proven is that the configured value reaches the
    /// kernel and is accepted for this device, which is exactly the defect A-M6-1-1 fixed;
    /// that a UART then shifts bits at 115200 is a property of hardware, and the only
    /// apparatus that could assert it is a Campbell datalogger on a real serial cable.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[tokio::test]
    async fn the_configured_line_rate_is_set_on_the_tty() {
        let pair = crate::testing::PtyPair::spawn("stream-baud")
            .await
            .expect("`socat` is a test dependency of this crate; see the round-trip test");

        let port = serial(pair.daemon_side(), 115_200, "backyard").expect("opens the pty");
        assert_eq!(
            port.baud_rate().expect("the pty reports its line rate"),
            115_200,
            "`transport_baud` reached the line: a fresh `socat` pty's own rate is 38400, so \
             this distinguishes a `tcsetattr` that happened from one that did not"
        );

        // The negative control, on the SAME device: drop the port -- which releases the
        // `TIOCEXCL` and the `flock` `serialport` takes -- and open it again at a different
        // rate. Two rates on two devices would leave open the reading that each device
        // happened to be at the rate asked for; two rates on one device cannot. 9600 is
        // `mw-output`'s KISS TNC default (`FrameSink::DEFAULT_BAUD`), which reaches this
        // same function through `mw_station::stream::open`.
        drop(port);
        let port = serial(pair.daemon_side(), 9_600, "backyard").expect("reopens the pty");
        assert_eq!(
            port.baud_rate().expect("the pty reports its line rate"),
            9_600,
            "the line rate follows `transport_baud` rather than being a constant"
        );
    }

    /// A `socat` pty pair is the level W3 D-83 asks for, and it is also the only way to
    /// exercise the serial path in a unit test. Skipped rather than failed where `socat` is
    /// not installed, because this is not the gate that owns that requirement --
    /// `tests/emulators/manifest.toml` is, and it fails loudly.
    ///
    /// **Not on macOS**, where it used to run: the rate is set there with the `IOSSIOSPEED`
    /// ioctl, which refuses a pseudo terminal with `ENOTTY` -- `serialport`'s own code says
    /// so at `posix/termios.rs:93` -- so the open fails on a pty and succeeds on a real
    /// `/dev/cu.*`. That is a cost of amendment A-M6-1-1 rather than a cost of this test, and
    /// it is named at this module's documentation too. Every target in `ci/release.toml` is
    /// Linux, where the rate goes on with `BOTHER` and `TCSETS2`.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[tokio::test]
    async fn a_pty_pair_round_trips_bytes_in_both_directions() {
        let pair = crate::testing::PtyPair::spawn("stream-roundtrip")
            .await
            .expect(
                "`socat` is a test dependency of this crate: W3 D-83 requires the serial \
                 path to be exercised over a real character device, and a pty pair is how. \
                 Install it, or run `cargo test -p mw-station -- --skip pty`.",
            );
        let serial = |dev: &Path| Transport::Serial {
            dev: dev.to_path_buf(),
            baud: 19_200,
        };
        let mut daemon = open(&serial(pair.daemon_side()), "backyard")
            .await
            .expect("opens the daemon side");
        let mut device = open(&serial(pair.device_side()), "backyard")
            .await
            .expect("opens the device side");

        daemon.write_all(b"LOOP 20\n").await.expect("writes");
        let mut buf = [0u8; 64];
        let n = read_at_least(&mut *device, &mut buf, 8).await;
        assert_eq!(&buf[..n], b"LOOP 20\n");

        device.write_all(b"\x06").await.expect("acks");
        let n = read_at_least(&mut *daemon, &mut buf, 1).await;
        assert_eq!(&buf[..n], b"\x06");
    }
}
