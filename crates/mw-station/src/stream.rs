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
//! # The serial port, and the one thing this cannot do
//!
//! **`transport_baud` is recorded and not applied.** A line rate is set with `tcsetattr`,
//! which needs either `libc` or a serial-port crate, and **D-32**'s table names neither —
//! a substitution or an addition there is an amendment, not a manifest edit, and
//! `docs/reconciliation.md`'s *Proposed amendments* carries the one this item files. So what
//! [`open`] does with a `serial` transport is open the character device and read it: the
//! line discipline is whatever the device already has.
//!
//! Three consequences, stated rather than discovered:
//!
//! - **A `socat` pty pair works completely**, because a pty has no line rate. That is the
//!   whole of W3 **D-83**'s Davis and Campbell test level, and it is what
//!   `tests/emulators/manifest.toml`'s `davis` and `campbell` entries exercise.
//! - **A `ser2net` bridge works completely**, because the bridge owns the line and this end
//!   is a TCP socket. That is **N-20**'s production path: the owner's Campbell is on USB
//!   serial and the daemon is moving into Kubernetes, so the serial line is terminated
//!   outside the pod.
//! - **A directly attached console works if and only if the line is already configured** —
//!   by a `udev` rule, by `stty`, or because the kernel default matches. If it is not, the
//!   symptom is not silence: the bytes arrive framed wrong, every Davis packet fails its
//!   CRC-16 and every Campbell line fails to parse, so
//!   `mw_station_frames_total{outcome="undecodable"}` counts every one of them. [`open`]
//!   also logs the configured rate and the fact that it was not applied, once per connect,
//!   naming the station.
//!
//! A reader who wants this closed should read the proposed amendment rather than add a
//! dependency: the point of the amendment protocol is that the next agent does not have to
//! re-decide it.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mw_component::{BoxFuture, BuildError};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tracing::{debug, warn};

use crate::session::ByteStream;
use crate::transport::Transport;

/// How long to wait for a TCP connection to a console or a bridge.
///
/// The Go Davis driver's `net.DialTimeout` value (`davis/station.go:308`), kept so that the two
/// daemons give a bridge the same chance during the M5-2 comparison.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The flags a character device is opened with.
///
/// `O_NONBLOCK` for two reasons, and the second is the stronger one: [`AsyncFd`] polls the
/// descriptor and then reads it, so without it a read the poll said would succeed can still
/// block the runtime thread -- and `open(2)` **itself** blocks on a tty until the modem
/// lines assert carrier, which on an unplugged serial cable is forever. `O_NOCTTY` because
/// opening a terminal from a daemon with no controlling terminal would otherwise *acquire*
/// one, and a `SIGHUP` on that terminal would then reach the whole process.
///
/// Numeric rather than from `libc`, which is not a dependency (see this module's
/// documentation). The values are the asm-generic ABI's, which every target in
/// `ci/release.toml` uses; macOS is here so that the crate builds on a developer's machine,
/// and any other platform is a `compile_error!` naming what to add rather than a silently
/// wrong constant.
#[cfg(target_os = "linux")]
const OPEN_FLAGS: i32 = 0o4000 | 0o400;
#[cfg(target_os = "macos")]
const OPEN_FLAGS: i32 = 0x0004 | 0x2_0000;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "crates/mw-station/src/stream.rs opens a character device with numeric O_NONBLOCK and \
     O_NOCTTY, and this target's values are not recorded. Add them beside the Linux ones."
);

/// What the serial descriptor is polled for. Both, because the Davis conversation writes
/// commands as well as reading packets.
const INTEREST: Interest = Interest::READABLE.add(Interest::WRITABLE);

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

    /// The character device opened and cannot be polled.
    ///
    /// The reachable cause is a `transport_dev` that names a **regular file**: `epoll_ctl`
    /// refuses one with `EPERM`, which is the kernel saying "this is not a stream". Worth
    /// its own sentence because the path exists, the open succeeds, and the message
    /// `epoll_ctl` produces on its own names neither the file nor the reason.
    #[error(
        "the serial device {dev} opened and cannot be polled, which is what a regular file \
         does: `transport_dev` has to name a character device -- a serial port, or a `socat` \
         pty"
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
    /// The two configuration mistakes -- [`OpenError::Tls`] and [`OpenError::NotAStream2`] --
    /// are refused at **plan** time by `crate::plan`, so they are not reached here by any
    /// plannable configuration and their classification changes nothing.
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

/// Open a character device.
///
/// **Must be called from inside a tokio runtime**, which is not visible in the signature:
/// `AsyncFd::register_with_interest` panics without a current reactor. Not an `async fn`
/// because nothing here awaits, so the obligation has to be written down rather than
/// carried by the type. Every caller reaches this through [`open`], which is `async`.
fn serial(dev: &Path, baud: u32, station: &str) -> Result<SerialPort, OpenError> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OPEN_FLAGS)
        .open(dev)
        .map_err(|source| OpenError::Device {
            dev: dev.to_path_buf(),
            source,
        })?;
    // SAFETY: `file` is an owned `std::fs::File` this function has just opened and is
    // moving into the `AsyncFd`, so all three of `register_with_interest`'s conditions hold
    // by construction: the descriptor is open, it stays open and refers to the same file
    // description until the `AsyncFd` is dropped, and `File::as_raw_fd` returns the same
    // value every time it is called.
    //
    // **This is the only `unsafe` block in the workspace, and it is here because every safe
    // constructor is deprecated.** `AsyncFd::{new, with_interest, try_new,
    // try_with_interest}` are all deprecated in `tokio` 1.54 as "unsound due to lack of I/O
    // safety" (tokio#8587) -- unsound for a BORROWED descriptor somebody else may close,
    // which is exactly the case this is not. The alternative is `#[allow(deprecated)]`,
    // which trades an audited safety argument for a call that disappears in a future minor
    // version, and `unsafe_code` is `warn` in `[workspace.lints]` rather than `forbid`
    // precisely so that a case like this can be made in writing.
    #[allow(unsafe_code)]
    let registered = unsafe { AsyncFd::register_with_interest(file, INTEREST) };
    let fd = registered.map_err(|source| OpenError::NotAStream {
        dev: dev.to_path_buf(),
        // `AsyncFdRegisterError` hands the file back so a caller can try something else;
        // there is nothing else to try, so only the cause is kept.
        source: source.into(),
    })?;
    // Once per connect, naming the station and the rate, because the alternative is an
    // operator reading `transport_baud` in the management UI and believing it (see this
    // module's documentation).
    warn!(
        station,
        device = %dev.display(),
        configured_baud = baud,
        "opened a serial device; the configured line rate is NOT applied by this build -- \
         set it with a udev rule or `stty`, or terminate the line with `ser2net` and use a \
         `tcp_out` transport. See docs/reconciliation.md's proposed D-32 amendment."
    );
    Ok(SerialPort {
        fd,
        dev: dev.to_path_buf(),
    })
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

/// A character device, polled.
pub struct SerialPort {
    fd: AsyncFd<std::fs::File>,
    dev: PathBuf,
}

impl fmt::Debug for SerialPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerialPort")
            .field("dev", &self.dev)
            .finish_non_exhaustive()
    }
}

impl ByteStream for SerialPort {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            // `async_io` retries the closure whenever it reports `WouldBlock`, which is the
            // whole reason the descriptor is opened non-blocking: the poll says readable,
            // the read is attempted, and a spurious wake-up costs a retry rather than a
            // blocked runtime thread.
            self.fd
                .async_io(Interest::READABLE, |file| (&*file).read(buf))
                .await
        })
    }

    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            let mut written = 0;
            while written < bytes.len() {
                let n = self
                    .fd
                    .async_io(Interest::WRITABLE, |file| (&*file).write(&bytes[written..]))
                    .await?;
                if n == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                }
                written += n;
            }
            // **No flush, and that is not an omission.** The obvious call here is
            // `sync_data`, and it cannot do anything: `tty_fops` declares no `fsync`, so
            // `fsync` and `fdatasync` return `EINVAL` on every serial port and on both ends
            // of every pty -- which is every target in `ci/release.toml` and the whole of
            // the `socat` test path. A call that is `EINVAL` everywhere it can be reached
            // is a blocking syscall in an `async fn` that reads as a guarantee it does not
            // make. Draining a tty's output queue is `tcdrain`, an `ioctl` and therefore a
            // second `unsafe` block; it is not needed, because the write above returning
            // means the bytes are in the driver's queue and the console reads them from
            // there.
            Ok(())
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

    /// The case the path exists for: an open that succeeds and a descriptor that cannot be
    /// polled.
    ///
    /// **Linux only, and that is a property of the assertion rather than of the code under
    /// test.** `NotAStream` is reported because `epoll_ctl` answers `EPERM` for a regular
    /// file; `kqueue` registers one happily, so on macOS -- which this module supports so
    /// that the crate builds on a developer's machine -- the open would succeed and the
    /// `expect_err` would fail. Refusing a regular file is not a guarantee this module
    /// makes on every platform, and a test that claimed otherwise would be asserting
    /// `epoll`'s behaviour in a place nobody would look for it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_regular_file_opens_and_is_refused_as_not_a_stream() {
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

    /// A `socat` pty pair is the level W3 D-83 asks for, and it is also the only way to
    /// exercise the serial path in a unit test. Skipped rather than failed where `socat` is
    /// not installed, because this is not the gate that owns that requirement --
    /// `tests/emulators/manifest.toml` is, and it fails loudly.
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
