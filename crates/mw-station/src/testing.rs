//! Test support: a recorded transcript, and an [`Ingress`] that records instead of
//! storing.
//!
//! Public rather than `#[cfg(test)]`, for the reason `mw_pipeline::testing` and
//! `mw_supervisor::testing` are: this crate's integration tests are separate compilation
//! units, and W3 **D-82**'s middle level — "`Session` against a recorded transcript" — has
//! to be reachable from one.
//!
//! Nothing here has a production use. The real `Ingress` is `mw-pipeline`'s over the spool;
//! the real byte streams are a serial port, a TCP connection and a UDP socket.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use mw_component::{
    AdmitError, Admitted, BoxFuture, FrameMeta, Ingress, Reject, StationMeta, StationProjection,
    StationRegistry,
};
use mw_types::{Envelope, OriginId, PartialReading, Seq, StationKey};

use crate::session::ByteStream;

/// A [`ByteStream`] over a script: bytes to hand back, and bytes the session must write.
///
/// **A mismatched write is an error from the stream**, not a panic and not a silent pass.
/// That is what makes this a transcript rather than a stub: a session that stops re-issuing
/// its command does not merely read fewer frames, it fails the script at the point the
/// protocol required a write — which is the shape of the Davis defect W3 D-82 names.
#[derive(Debug)]
pub struct Transcript {
    reads: VecDeque<Vec<u8>>,
    expected: VecDeque<Vec<u8>>,
    written: Vec<Vec<u8>>,
}

impl Transcript {
    /// A transcript that only hands bytes back, for a session that writes nothing.
    pub fn reading<I, B>(reads: I) -> Self
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        Self {
            reads: reads.into_iter().map(|b| b.as_ref().to_vec()).collect(),
            expected: VecDeque::new(),
            written: Vec::new(),
        }
    }

    /// A transcript that also asserts what the session writes, in order.
    pub fn scripted<R, W, B, C>(reads: R, writes: W) -> Self
    where
        R: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
        W: IntoIterator<Item = C>,
        C: AsRef<[u8]>,
    {
        Self {
            reads: reads.into_iter().map(|b| b.as_ref().to_vec()).collect(),
            expected: writes.into_iter().map(|b| b.as_ref().to_vec()).collect(),
            written: Vec::new(),
        }
    }

    /// Everything the session wrote, in order.
    pub fn written(&self) -> &[Vec<u8>] {
        &self.written
    }

    /// How many scripted reads are left.
    pub fn remaining(&self) -> usize {
        self.reads.len()
    }

    /// Whether every scripted write was performed.
    pub fn exhausted(&self) -> bool {
        self.expected.is_empty()
    }
}

impl ByteStream for Transcript {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            let Some(chunk) = self.reads.pop_front() else {
                // The end of the script is the end of the stream, which is what a station
                // that hung up looks like.
                return Ok(0);
            };
            if chunk.len() > buf.len() {
                return Err(std::io::Error::other(format!(
                    "the transcript's next chunk is {} bytes and the session offered a \
                     {}-byte buffer",
                    chunk.len(),
                    buf.len()
                )));
            }
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        })
    }

    fn write_all<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            self.written.push(bytes.to_vec());
            match self.expected.pop_front() {
                None => Ok(()),
                Some(want) if want == bytes => Ok(()),
                Some(want) => Err(std::io::Error::other(format!(
                    "the transcript expected the session to write {want:?} and it wrote \
                     {bytes:?}"
                ))),
            }
        })
    }
}

/// What one [`RecordingIngress::admit`] was handed.
#[derive(Debug, Clone)]
pub struct Admission {
    /// The readings the codec produced.
    pub batch: Vec<PartialReading>,
    /// Which component received the frame.
    pub receiver: String,
    /// What the frame claimed, for a non-`Fixed` identity.
    pub presented_key: Option<String>,
    /// When the transport observed the frame.
    pub received_at: DateTime<Utc>,
}

/// An [`Ingress`] that records what it was handed.
///
/// `admit` only. `accept` is the peer path and no station driver takes it, so it refuses
/// with a reason rather than recording — a driver that called it would be a bug this double
/// should surface, not absorb.
#[derive(Debug, Default)]
pub struct RecordingIngress {
    admitted: Mutex<Vec<Admission>>,
    refuse: Mutex<Option<IngressFailure>>,
}

/// How a [`RecordingIngress`] fails, which is one per [`mw_types::Class`].
///
/// A double that could only produce `Rejected` could only exercise the `Ignorable` arm, so
/// every caller's `Retryable` and `Fatal` handling went untested while reading as though it
/// were covered. `AdmitError` is not `Clone`, which is why this is a separate enum rather
/// than the error itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressFailure {
    /// `Ignorable`: the gate refused the frame.
    Rejected(Reject),
    /// `Retryable`: the spool is at its ceiling.
    SpoolFull,
    /// `Fatal`: the spool could not be written.
    Spool,
}

impl RecordingIngress {
    /// An ingress that accepts everything.
    pub fn accepting() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// An ingress whose identity resolution refuses everything — what an unknown station
    /// looks like from inside a driver.
    pub fn refusing(reason: Reject) -> Arc<Self> {
        Arc::new(Self {
            admitted: Mutex::new(Vec::new()),
            refuse: Mutex::new(Some(IngressFailure::Rejected(reason))),
        })
    }

    /// An ingress that fails the way `how` says, which is how a caller's `Retryable` and
    /// `Fatal` arms are reached at all.
    pub fn failing(how: IngressFailure) -> Arc<Self> {
        Arc::new(Self {
            admitted: Mutex::new(Vec::new()),
            refuse: Mutex::new(Some(how)),
        })
    }

    /// Start or stop refusing, which is how a test models one gate decision per frame.
    ///
    /// The real gate answers per frame — a wrong secret is refused and the next frame from
    /// the same console with the right one is admitted — and a double fixed at
    /// construction cannot express the sequence that follows a refusal.
    pub fn refuse(&self, reason: Option<Reject>) {
        *self.refuse.lock().expect("not poisoned") = reason.map(IngressFailure::Rejected);
    }

    /// Every admission, in order.
    pub fn admissions(&self) -> Vec<Admission> {
        self.admitted
            .lock()
            .expect("the recording ingress is not poisoned")
            .clone()
    }

    /// How many readings have been admitted in total.
    pub fn readings(&self) -> usize {
        self.admissions().iter().map(|a| a.batch.len()).sum()
    }
}

impl Ingress for RecordingIngress {
    fn admit<'a>(
        &'a self,
        batch: &'a [PartialReading],
        meta: &'a FrameMeta,
    ) -> BoxFuture<'a, Result<Admitted, AdmitError>> {
        Box::pin(async move {
            if let Some(how) = *self.refuse.lock().expect("not poisoned") {
                return Err(match how {
                    IngressFailure::Rejected(reason) => AdmitError::Rejected(reason),
                    IngressFailure::SpoolFull => AdmitError::SpoolFull {
                        depth_bytes: 1 << 30,
                    },
                    IngressFailure::Spool => AdmitError::spool(std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )),
                });
            }
            let mut log = self.admitted.lock().expect("not poisoned");
            log.push(Admission {
                batch: batch.to_vec(),
                receiver: meta.receiver.as_str().to_string(),
                presented_key: meta.presented_key.clone(),
                received_at: meta.received_at,
            });
            Ok(Admitted {
                accepted: u32::try_from(batch.len()).unwrap_or(u32::MAX),
                rejected: 0,
                seq_high: Seq::new(log.len() as u64),
            })
        })
    }

    fn accept<'a>(
        &'a self,
        _batch: &'a [Envelope],
        _from: OriginId,
    ) -> BoxFuture<'a, Result<mw_component::Accepted, mw_component::AcceptError>> {
        Box::pin(async {
            Err(mw_component::AcceptError::Rejected(
                Reject::UnauthorizedPeer,
            ))
        })
    }
}

/// A `socat` pty pair: two character devices wired back to back.
///
/// W3 **D-83**'s requirement for Davis and Campbell — "serial framing and read-timeout
/// behaviour is where that class of defect lives", so the serial path is exercised over a
/// real character device rather than over a transcript. One end is the daemon's
/// `transport_dev`; the other is where an emulator writes.
///
/// **`socat` is a declared test dependency of this crate and every caller `expect`s the
/// pair.** `spawn` returns `None` so that the caller's message can say what is missing and
/// why, and **not** so that the test can skip: a check that silently does nothing is
/// `docs/reconciliation.md` G-1, which is the failure this whole item's gate exists against.
/// The cost is that `cargo test -p mw-station` needs `socat` on `PATH`, which is recorded
/// here, in each caller's `expect` and in the pull request rather than discovered.
#[derive(Debug)]
pub struct PtyPair {
    child: tokio::process::Child,
    dir: std::path::PathBuf,
    daemon: std::path::PathBuf,
    device: std::path::PathBuf,
}

impl PtyPair {
    /// Spawn `socat` and wait for both links to appear. `None` if `socat` is not on `PATH`.
    ///
    /// `name` distinguishes one pair's directory from another's, because two tests running
    /// concurrently in one process would otherwise race on the link paths.
    pub async fn spawn(name: &str) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!("mw-pty-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let daemon = dir.join("daemon");
        let device = dir.join("device");
        let child = tokio::process::Command::new("socat")
            // `raw` so that no line discipline rewrites a Davis packet's `\r`, and
            // `echo=0` so that a command written at one end does not come back at it --
            // both of which a real serial port's `cfmakeraw` setup would also do.
            .arg(format!("PTY,raw,echo=0,link={}", daemon.display()))
            .arg(format!("PTY,raw,echo=0,link={}", device.display()))
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        // `socat` creates the links after it has opened both ptys, so the paths do not
        // exist when `spawn` returns. Polled rather than slept on: a fixed sleep is either
        // slower than it needs to be or flaky under load.
        for _ in 0..200 {
            if daemon.exists() && device.exists() {
                return Some(Self {
                    child,
                    dir,
                    daemon,
                    device,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    /// The end a station component opens as its `transport_dev`.
    pub fn daemon_side(&self) -> &std::path::Path {
        &self.daemon
    }

    /// The end an emulator writes packets into.
    pub fn device_side(&self) -> &std::path::Path {
        &self.device
    }

    /// `socat`'s process id, for a test that wants to say which process it is waiting on.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

impl Drop for PtyPair {
    /// `kill_on_drop` handles the process; the links are ours.
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// A Go emulator from `tests/emulators/<name>`, built once per test binary and killed on
/// drop.
///
/// Level 3 of W3 **D-82**'s three -- "the whole driver against the pinned Go emulator" --
/// and the three properties it has that the two levels below it cannot: a separate
/// process, a different language, and real sockets.
/// `tests/emulators/manifest.toml` is what declares which emulators exist, which tests
/// each one must run, and what each one's origin is.
///
/// **Built rather than `go run`**, and the reason is a hang rather than a preference.
/// `go run` compiles to a cache directory and then execs the result as a child **of its
/// own**, so `kill_on_drop` kills the wrapper and the emulator keeps the descriptors it
/// inherited -- which means a harness reading this test's output through a pipe never sees
/// end-of-file and waits forever. `cargo xtask emulator-tests` reads exactly that pipe, so
/// the gate would hang rather than fail.
#[derive(Debug)]
pub struct GoEmulator {
    child: tokio::process::Child,
    name: String,
    lines: Option<tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>>,
}

/// How long any one wait in an emulator test may take.
///
/// Generous, because this is a process start on a shared runner; every assertion is on a
/// fact and never on a duration.
pub const EMULATOR_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

impl GoEmulator {
    /// Build `tests/emulators/<name>` and hand back the executable, once per test binary.
    pub fn binary(name: &str) -> std::path::PathBuf {
        static BUILT: Mutex<Option<std::collections::BTreeMap<String, std::path::PathBuf>>> =
            Mutex::new(None);
        let mut cache = BUILT
            .lock()
            .expect("the emulator build cache is not poisoned");
        let cache = cache.get_or_insert_with(std::collections::BTreeMap::new);
        if let Some(path) = cache.get(name) {
            return path.clone();
        }
        // `CARGO_MANIFEST_DIR` is this crate's and expands where this function is
        // *compiled*, so it is the same answer for a test in this crate and for one in the
        // binary's -- which is the reason this helper is here rather than copied into each
        // test target.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/emulators")
            .join(name)
            .canonicalize()
            .unwrap_or_else(|e| panic!("tests/emulators/{name} is in the tree: {e}"));
        let out = std::env::temp_dir().join(format!("mw-emulator-{name}-{}", std::process::id()));
        let status = std::process::Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg(".")
            .current_dir(&dir)
            .status()
            .unwrap_or_else(|e| {
                panic!(
                    "the `{name}` emulator needs a Go toolchain on PATH (every test that \
                     uses one is `--ignored` for that reason): {e}"
                )
            });
        assert!(status.success(), "the `{name}` emulator did not build");
        cache.insert(name.to_string(), out.clone());
        out
    }

    /// Start an emulator with `args`, its stdout piped.
    pub async fn spawn(name: &str, args: &[String]) -> Self {
        let mut child = tokio::process::Command::new(Self::binary(name))
            .args(args)
            .stdout(std::process::Stdio::piped())
            // Not inherited: an inherited descriptor outlives a killed child's siblings and
            // is what makes a harness's pipe never close. Every emulator here writes to
            // stderr only when it cannot start, which surfaces as the port never answering.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("the `{name}` emulator starts: {e}"));
        let stdout = child.stdout.take().expect("stdout is piped");
        Self {
            child,
            name: name.to_string(),
            lines: Some(tokio::io::AsyncBufReadExt::lines(
                tokio::io::BufReader::new(stdout),
            )),
        }
    }

    /// Start an emulator that listens on a TCP port it is **told**, and wait until the port
    /// answers.
    ///
    /// The two vendored reference emulators take `-port` and do not report what they bound,
    /// so a port has to be chosen here. Derived from the process id and then retried on the
    /// next candidate, because a port another process holds is a flaky test and a port
    /// chosen by the kernel is not available to ask for. The gate runs these one at a time
    /// (`cargo xtask emulator-tests` passes `--test-threads=1`), so the only contention is
    /// with something else on the machine.
    pub async fn serving_tcp(name: &str, extra: &[&str]) -> (Self, u16) {
        let base = 24_000 + u16::try_from(std::process::id() % 20_000).unwrap_or(0);
        let mut tried = Vec::new();
        for attempt in 0..8u16 {
            let port = base.saturating_add(attempt);
            let mut args = vec!["-port".to_string(), port.to_string()];
            args.extend(extra.iter().map(|a| (*a).to_string()));
            let emulator = Self::spawn(name, &args).await;
            if answers(port).await {
                return (emulator, port);
            }
            tried.push(port);
        }
        panic!("the `{name}` emulator did not answer on any of {tried:?}");
    }

    /// The next line the emulator printed.
    pub async fn next_line(&mut self) -> Option<String> {
        let name = self.name.clone();
        let lines = self.lines.as_mut()?;
        tokio::time::timeout(EMULATOR_DEADLINE, lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("the `{name}` emulator printed a line"))
            .unwrap_or_else(|e| panic!("the `{name}` emulator's stdout is readable: {e}"))
    }

    /// Wait for the emulator to exit, and assert that it did so successfully.
    pub async fn finished(mut self) {
        let name = self.name.clone();
        let status = tokio::time::timeout(EMULATOR_DEADLINE, self.child.wait())
            .await
            .unwrap_or_else(|_| panic!("the `{name}` emulator exited"))
            .unwrap_or_else(|e| panic!("waiting for the `{name}` emulator: {e}"));
        assert!(status.success(), "the `{name}` emulator exited {status}");
    }
}

impl Drop for GoEmulator {
    fn drop(&mut self) {
        // `kill_on_drop` covers the usual path; this makes it immediate rather than at the
        // next runtime poll, which matters when a test fails and the next one wants the
        // port.
        let _ = self.child.start_kill();
    }
}

/// Whether a TCP port answers, polled for a few seconds.
async fn answers(port: u16) -> bool {
    for _ in 0..60 {
        if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// A `socat` bridge from a pty to a TCP port: a **serial** station whose other end is a
/// network emulator.
///
/// This is W3 **D-83**'s "`socat` pty pair" with the pinned oracle on the far side of it.
/// The two vendored reference emulators speak TCP and a Davis or Campbell station on a
/// serial line speaks the same bytes, so bridging is what makes the serial half of the
/// driver testable against the emulator that was written for the hardware -- rather than
/// against a second emulator written here, which is the independent-authorship property
/// D-40 exists to protect.
///
/// What the bridge adds over connecting to the port directly is the thing the Go serial
/// path gets wrong: a **character device**, with its own buffering, so a frame arrives in
/// whatever pieces the line produces rather than in whatever pieces TCP produces.
#[derive(Debug)]
pub struct PtyBridge {
    child: tokio::process::Child,
    dir: std::path::PathBuf,
    device: std::path::PathBuf,
}

impl PtyBridge {
    /// Bridge a new pty to `127.0.0.1:port`. `None` if `socat` is not on `PATH`.
    pub async fn spawn(name: &str, port: u16) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!("mw-bridge-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let device = dir.join("device");
        let child = tokio::process::Command::new("socat")
            .arg(format!("PTY,raw,echo=0,link={}", device.display()))
            .arg(format!("TCP:127.0.0.1:{port}"))
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        for _ in 0..200 {
            if device.exists() {
                return Some(Self { child, dir, device });
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    /// The character device a station component opens as its `transport_dev`.
    pub fn device(&self) -> &std::path::Path {
        &self.device
    }

    /// `socat`'s process id, for a test that wants to say which process it is waiting on.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

impl Drop for PtyBridge {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// A **real `ser2net`** fanning one character device out to several TCP clients, with a
/// `socat` pty pair standing in for the station — GRA-787's apparatus.
///
/// This is the one piece of the station path that is neither a driver nor an emulator: it
/// is the *bridge*, and M5-2's mechanism (b) on a serial station depends on a property of
/// it that no amount of reading the documentation establishes. One serial line has one
/// reader, so feeding two daemons the same readings means a multiplexer — and if
/// that multiplexer hands bytes to whichever client reads first instead of broadcasting
/// every byte to all of them, the two daemons silently split the stream and both produce
/// gap-ridden data that still parses. `tests/serial_bridge.rs` is the assertion that it
/// broadcasts; this is what that assertion runs against.
///
/// # Why the real binary and not a double
///
/// A hand-written fan-out would prove only that the test author can write one. The claim
/// under test is about **`ser2net` 4.x's `max-connections`**, and `ser2net` is what
/// `packaging/serial-bridge/` deploys — so the binary is the thing under test and a missing
/// binary is a failure rather than a skip, the same rule [`PtyBridge`] follows for
/// `socat`.
///
/// `SER2NET` overrides the executable, because the Debian package installs it in
/// `/usr/sbin` and that is not on a non-root `PATH`. `GENSIO_LIBRARY_PATH` is gensio's own
/// variable and is honoured by it directly: `serialdev` is a dynamically loaded module, so
/// a `ser2net` whose gensio modules are somewhere unusual answers every connector with
/// `Unknown gensio type: serialdev,…` while still listening on the port.
///
/// # Two things this apparatus does that a deployed configuration must not copy
///
/// **`nouucplock`.** gensio takes a uucp lock per device — one keyed by the path and one
/// keyed by the device's *major.minor*, which for a `socat` pty is whichever `/dev/pts/N`
/// it was handed. A `ser2net` killed with `SIGKILL`, which is what `kill_on_drop` does,
/// leaves both behind; the next test gets the same `/dev/pts/N`, finds the stale lock, and
/// answers its client `Device open failure: Object was already in use` while looking
/// perfectly healthy. The lock is a real safeguard on a real port and stays on in
/// `packaging/serial-bridge/ser2net.yaml.example` — with the consequence recorded there,
/// because it
/// is also why that unit must be stopped with `SIGTERM`.
///
/// **A guessed port.** A deployment's is fixed and in its configuration; this one is
/// chosen per bridge so the tests may run beside each other.
#[derive(Debug)]
pub struct Ser2netBridge {
    socat: tokio::process::Child,
    ser2net: tokio::process::Child,
    dir: std::path::PathBuf,
    line_side: std::path::PathBuf,
    station_side: std::path::PathBuf,
    port: u16,
}

impl Ser2netBridge {
    /// A bridge with `max_connections` client slots, on an ephemeral port.
    ///
    /// The station side is a pty: a line rate cannot be set on one, which is deliberate —
    /// `crate::stream`'s module documentation has why that makes the pty the honest
    /// stand-in for a bridge and not for a directly attached console.
    ///
    /// Returns once a throwaway client has **received a byte that this apparatus put into
    /// this pty**, which is the only check that distinguishes a working bridge of ours from
    /// a listener on a recycled port that belongs to something else.
    pub async fn spawn(name: &str, max_connections: u32) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!(
            "mw-ser2net-{}-{name}-{max_connections}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok()?;
        let line_side = dir.join("line");
        let station_side = dir.join("station");
        let socat = tokio::process::Command::new("socat")
            .arg(format!("PTY,raw,echo=0,link={}", line_side.display()))
            .arg(format!("PTY,raw,echo=0,link={}", station_side.display()))
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut linked = false;
        for _ in 0..200 {
            if line_side.exists() && station_side.exists() {
                linked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if !linked {
            return None;
        }

        let config = dir.join("ser2net.yaml");
        let mut chosen = None;
        for attempt in 0..8 {
            // A guessed port, retried, rather than one the kernel chose.
            //
            // Binding zero and reading the port back is the usual trick and it is not
            // available here: `ci/layers.toml`'s `only-mw-http-binds-a-socket` forbids this
            // crate the listener types, and that rule is right — exactly one place in this
            // system binds a listener. It costs nothing, because `ser2net` takes a port
            // number in its configuration and could not have been handed a bound socket
            // anyway, and because what ends this loop is not "the port was free" but the
            // end-to-end check below: a byte this apparatus put into *this* pty coming out
            // of a client of that port. A collision therefore retries rather than producing
            // a test that reads someone else's stream.
            //
            // Below 32768, which is where Linux starts handing out ephemeral ports, so a
            // guess here does not race the kernel's own allocation.
            let port = 24_000
                + (std::process::id() as u64)
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos() as u64)
                            .unwrap_or(0),
                    )
                    .wrapping_add(attempt * 997)
                    % 8_000;
            let port = port as u16;
            std::fs::write(
                &config,
                format!(
                    "%YAML 1.1\n\
                     ---\n\
                     connection: &{name}\n\
                     \x20 accepter: tcp,127.0.0.1,{port}\n\
                     \x20 connector: serialdev(nouucplock),{dev},19200n81,local\n\
                     \x20 options:\n\
                     \x20   max-connections: {max_connections}\n",
                    dev = line_side.display(),
                ),
            )
            .ok()?;
            let mut ser2net = tokio::process::Command::new(
                std::env::var("SER2NET").unwrap_or_else(|_| "ser2net".into()),
            )
            // `-n` keeps it in the foreground so that `kill_on_drop` reaches the process
            // that holds the device; a daemonised `ser2net` would outlive the test and keep
            // it.
            .arg("-n")
            .arg("-c")
            .arg(&config)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
            if answers(port).await {
                let ours =
                    match tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                        .await
                    {
                        Ok(mut probe) => {
                            let mut one: [&mut tokio::net::TcpStream; 1] = [&mut probe];
                            deliver_sentinels(&station_side, &mut one).await
                        }
                        Err(_) => false,
                    };
                if ours {
                    chosen = Some((ser2net, port));
                    break;
                }
            }
            let _ = ser2net.start_kill();
            let _ = ser2net.wait().await;
        }
        let (ser2net, port) = chosen?;

        Some(Self {
            socat,
            ser2net,
            dir,
            line_side,
            station_side,
            port,
        })
    }

    /// The TCP port a client connects to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The bridge as a station driver's `Transport`.
    pub fn transport(&self) -> crate::transport::Transport {
        crate::transport::Transport::TcpOut {
            host: "127.0.0.1".into(),
            port: self.port,
            tls: None,
        }
    }

    /// The character device the *bridge* owns — the near end of the pty pair, standing in
    /// for the station's serial port. A test opens this to show that nothing else can.
    pub fn line_side(&self) -> &std::path::Path {
        &self.line_side
    }

    /// Block until every one of `clients` has had a byte from this pty delivered to it,
    /// then drain them.
    ///
    /// **A test that skips this reads nothing and blames the bridge.** Two states have to
    /// be reached before a byte written to the station side comes out of a client's socket,
    /// and neither is observable from the client: the bridge opens the device on the *first*
    /// client connection rather than at startup, and a byte that arrives while the device
    /// is closed is dropped, because this is a fan-out and not a spool. So readiness is
    /// established the only way that cannot be wrong — by delivering bytes — and the
    /// sentinels are then drained so that what follows is only what the test sent.
    ///
    /// Pass **only the clients that are supposed to be wired**. A client beyond
    /// `max_connections` never receives the stream, and draining it would throw away the
    /// refusal a test wants to assert on.
    pub async fn wait_until_delivering(&self, clients: &mut [&mut tokio::net::TcpStream]) -> bool {
        deliver_sentinels(&self.station_side, clients).await
    }

    /// Push bytes down the line, as the datalogger would.
    ///
    /// Opened and closed per call so that a test can say when the bytes left the station.
    pub fn feed(&self, bytes: &[u8]) -> std::io::Result<()> {
        feed(&self.station_side, bytes)
    }
}

impl Drop for Ser2netBridge {
    fn drop(&mut self) {
        // Immediate rather than at the next runtime poll: `ser2net` sets `TIOCEXCL` on the
        // device, so a bridge that is still alive when the next test starts makes that
        // test's `open(2)` fail with `EBUSY` rather than merely take a port.
        let _ = self.ser2net.start_kill();
        let _ = self.socat.start_kill();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// The sentinel [`Ser2netBridge::wait_until_delivering`] pushes down the line.
///
/// A complete Campbell record rather than arbitrary bytes, so that a driver on the far side
/// decodes it instead of counting it undecodable while the apparatus is still settling.
/// Public because a test whose other client is a *driver* has to establish that client's
/// readiness through its admissions, and so has to push the same bytes itself.
pub const SENTINEL: &[u8] = b"{\"airtemp_f\":32.0,\"rh\":50.0}\n";

fn feed(station_side: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut dev = std::fs::OpenOptions::new().write(true).open(station_side)?;
    dev.write_all(bytes)?;
    dev.flush()
}

/// [`Ser2netBridge::wait_until_delivering`]'s body, also used by `spawn` before there is a
/// `Self` to call it on.
async fn deliver_sentinels(
    station_side: &std::path::Path,
    clients: &mut [&mut tokio::net::TcpStream],
) -> bool {
    use tokio::io::AsyncReadExt;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut seen = vec![false; clients.len()];
    while !seen.iter().all(|s| *s) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        if feed(station_side, SENTINEL).is_err() {
            return false;
        }
        for (i, client) in clients.iter_mut().enumerate() {
            let mut buf = [0_u8; 4096];
            if let Ok(Ok(n)) =
                tokio::time::timeout(std::time::Duration::from_millis(50), client.read(&mut buf))
                    .await
            {
                if n > 0 {
                    seen[i] = true;
                }
            }
        }
    }
    // Every sentinel is now accounted for, so a test's next read sees only what the test
    // sent. Quiet rather than counted, because how many sentinels each client received
    // depends on when it was wired.
    for client in clients.iter_mut() {
        let mut buf = [0_u8; 16 * 1024];
        while let Ok(Ok(n)) =
            tokio::time::timeout(std::time::Duration::from_millis(300), client.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
        }
    }
    true
}

/// A [`StationRegistry`] that resolves one claim, for a test that needs the gate itself
/// rather than a driver's view of it.
#[derive(Debug)]
pub struct OneStation {
    claim: String,
    station: StationKey,
}

impl OneStation {
    /// A registry in which `claim` is `station` and nothing else resolves.
    pub fn new(claim: &str, station: &str) -> Arc<Self> {
        Arc::new(Self {
            claim: claim.to_ascii_lowercase(),
            station: StationKey::new(station),
        })
    }
}

impl StationRegistry for OneStation {
    fn resolve(&self, meta: &FrameMeta) -> Result<StationKey, Reject> {
        match meta.presented_key.as_deref() {
            Some(claim) if claim.to_ascii_lowercase() == self.claim => Ok(self.station.clone()),
            _ => Err(Reject::UnknownStation),
        }
    }

    /// Nothing configured. A driver test asserts what the *codec* decoded, and an enriched
    /// column here would make the expected reading depend on this registry's coordinates
    /// rather than on the frame -- `mw_pipeline::enrich`'s own tests are where the derived
    /// values are asserted.
    fn metadata(&self, _station: &StationKey) -> StationMeta {
        StationMeta::EMPTY
    }

    fn projection(&self, _station: &StationKey) -> Option<StationProjection> {
        None
    }
}
