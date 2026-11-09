//! M2-4's done-criteria, against the real binary.
//!
//! **Not run under ThreadSanitizer**, and the `race` job sets `--cfg mw_sanitizer` to say
//! so. Two reasons, both about what a sanitizer can observe here:
//!
//! 1. The thing worth instrumenting is a **child process**. TSan's findings about the
//!    daemon are reported in the daemon, and with `halt_on_error=1` the first report kills
//!    it -- so a race anywhere in its dependency graph surfaces in this file as a
//!    `ConnectionReset` on an HTTP read or a timed-out `wait`, which is the least
//!    informative possible rendering of it. The in-process concurrency the `race` job
//!    exists for is `mw-supervisor`'s, and `crates/mw-supervisor/tests/concurrency.rs`
//!    covers that where TSan can actually see it.
//! 2. There is a standing false positive on the path only this binary takes. `tokio`
//!    publishes a `ScheduledIo` to its I/O driver by handing the pointer to the kernel as
//!    an `epoll_ctl` token and reading it back from `epoll_wait`; TSan does not model
//!    those syscalls as synchronisation, so it reports the allocating write against the
//!    driver thread's atomic load. It needs a **multi-threaded** runtime for the two to be
//!    different threads, which makes this binary the only thing in the workspace that
//!    triggers it -- every `#[tokio::test]` target is `current_thread`. No `tokio` version
//!    fixes it: it needs `__tsan_acquire`/`release` annotations wherever the pointer
//!    crosses the kernel. Reproduce by removing `--cfg mw_sanitizer` from the `race` job.
//!
//! The assertions are not weakened and nothing is skipped: `cargo test` runs this file in
//! the gate's own `test` job, which is where its subject -- process lifecycle, signals,
//! exit codes and the readiness protocol -- is actually testable.
#![cfg(not(mw_sanitizer))]
//!
//! > `modernweather --config <path>` starts with an empty configuration, converges,
//! > notifies readiness, serves `/metrics`, and exits cleanly on SIGTERM inside the
//! > 10-second grace.
//!
//! Five claims, one process. It is a subprocess rather than a call into a library because
//! three of the five are only true of a process: `$NOTIFY_SOCKET` is read from the
//! environment, SIGTERM is delivered to a pid, and "exits cleanly" is an exit status.
//! W8's unit is `Type=notify` and its assumption 3 makes `READY=1` the condition for not
//! reverting to `Type=simple`, so the notification is the deliverable rather than a
//! detail.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixDatagram};
use tokio::process::{Child, Command};

/// Generous, because this test also runs under ThreadSanitizer in the `race` job, where
/// everything including SQLite's migrations is instrumented. The point of each timeout is
/// to fail with a message rather than to hang a CI run.
const PATIENCE: Duration = Duration::from_secs(30);

/// §3.6's default grace. "Exits cleanly on SIGTERM inside the 10-second grace" is the
/// done-criterion, so the assertion is on this number and not on a round one.
const GRACE: Duration = Duration::from_secs(10);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mw-boot-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A port nothing is listening on, by asking the kernel for one and letting it go.
///
/// Racy in principle, and the alternative is a fixed port, which is racy in practice: two
/// of these tests and a developer's own daemon would collide on 8080.
///
/// Through `mw_http::bind` rather than `std::net`, and not to dodge a lint:
/// `ci/layers.toml`'s `only-mw-http-binds-a-socket` rule scans `tests/` as well as `src/`,
/// and it is right to. The design's answer to seven listener-owning sites in the Go tree
/// is that there is exactly one place a listener can exist -- so when a test needs the
/// kernel to choose a port, it asks through the crate that owns binding, with the route
/// table it will really serve. The socket closes when `Bound` drops.
async fn free_port() -> u16 {
    let probe = mw_http::bind(
        "127.0.0.1:0".parse().expect("a literal address"),
        mw_http::MetricsRoute::new(std::sync::Arc::new(String::new), mw_obs::CONTENT_TYPE),
        None,
    )
    .await
    .expect("a loopback port is available");
    probe.local_addr().port()
}

/// Create the configuration database and point the listener at a free loopback port.
///
/// The migrations are the binary's to apply, but a test that wants a known port has to
/// write the row first -- and `listener.http_port` has a `CHECK (http_port BETWEEN 1 AND
/// 65535)`, so "bind port 0 and tell me what you got" is not available through
/// configuration.
async fn configure(path: &Path, port: u16) {
    let pool = mw_store::config_db::open_and_migrate(path)
        .await
        .expect("the configuration schema applies from empty");
    sqlx::query("UPDATE listener SET listen_addr = '127.0.0.1', http_port = ?1 WHERE id = 1")
        .bind(i64::from(port))
        .execute(&pool)
        .await
        .expect("the seeded listener row is updatable");
    pool.close().await;
}

/// A `$NOTIFY_SOCKET` the test owns, which is what makes the readiness protocol
/// observable without systemd.
fn notify_socket(path: &Path) -> UnixDatagram {
    UnixDatagram::bind(path).expect("the notify socket is bindable")
}

async fn next_notification(socket: &UnixDatagram) -> String {
    let mut buf = vec![0u8; 4096];
    let read = tokio::time::timeout(PATIENCE, socket.recv(&mut buf))
        .await
        .expect("a notification arrived before the deadline")
        .expect("the datagram is readable");
    String::from_utf8_lossy(&buf[..read]).into_owned()
}

/// Read notifications until one carries `needle`, returning everything seen.
async fn notified_with(socket: &UnixDatagram, needle: &str) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        let message = next_notification(socket).await;
        let found = message.contains(needle);
        seen.push(message);
        if found {
            return seen;
        }
        assert!(
            seen.len() < 64,
            "{needle} never arrived; the daemon sent {seen:#?}"
        );
    }
}

fn spawn(config: &Path, notify: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_modernweather"))
        .arg("--config")
        .arg(config)
        .env("NOTIFY_SOCKET", notify)
        // Deterministic output, and the shape a journal gets (§5.3).
        .env("MW_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        .kill_on_drop(true)
        .spawn()
        .expect("the binary runs")
}

/// SIGTERM, which is what `KillSignal=SIGTERM` in the unit delivers.
///
/// Through `kill(1)` because `Child::kill` sends SIGKILL -- which would prove nothing
/// about a clean shutdown -- and because signalling from Rust otherwise means a `libc`
/// dependency for one call, which D-32's table does not carry.
fn terminate(child: &Child) {
    let pid = child.id().expect("the child has not been reaped");
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill(1) is available");
    assert!(status.success(), "SIGTERM could not be delivered to {pid}");
}

async fn get(port: u16, path: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the daemon is listening");
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("the request is written");
    let mut response = String::new();
    socket
        .read_to_string(&mut response)
        .await
        .expect("the response is read");
    response
}

#[tokio::test]
async fn an_empty_configuration_starts_converges_notifies_serves_and_stops() {
    let scratch = Scratch::new("done-when");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let port = free_port().await;
    configure(&config, port).await;
    let socket = notify_socket(&notify);

    let mut child = spawn(&config, &notify);

    // 1 + 2 + 3: it starts, converges, and says so. `READY=1` means *the supervisor has
    // converged* (§3.5), which for an empty plan is immediately -- and deliberately not
    // "every component is Running", which an empty plan could never satisfy either.
    let notifications = notified_with(&socket, "READY=1").await;
    let ready = notifications
        .last()
        .expect("notified_with returns what it saw");
    assert!(
        ready.contains("STATUS="),
        "readiness carries the status line that holds the nuance: {notifications:#?}"
    );
    assert!(
        ready.contains("0 running, 0 waiting, 0 failed"),
        "an empty configuration converges with nothing running: {ready}"
    );

    // 4: it serves /metrics.
    let metrics = get(port, "/metrics").await;
    assert!(metrics.starts_with("HTTP/1.1 200 OK"), "{metrics}");
    assert!(
        metrics.contains("content-type: text/plain; version=0.0.4"),
        "a scraper reads the content type: {metrics}"
    );
    assert!(
        metrics.contains("mw_build_info"),
        "the payload is not empty before anything has been counted: {metrics}"
    );

    // 5: SIGTERM, cleanly, inside the grace.
    terminate(&child);
    // `notified_with` panics if the needle never arrives, so reaching the next line *is*
    // the assertion that STOPPING=1 was sent -- and it was sent after READY=1, which is
    // the order §3.5 specifies.
    notified_with(&socket, "STOPPING=1").await;
    let status = tokio::time::timeout(GRACE, child.wait())
        .await
        .expect("the daemon exits inside the 10-second grace")
        .expect("the exit status is readable");
    assert!(
        status.success(),
        "a clean shutdown exits 0, not {status:?}; Restart=always makes anything else a loop"
    );
}

#[tokio::test]
async fn a_second_start_against_the_same_database_applies_no_migrations_and_still_converges() {
    let scratch = Scratch::new("restart");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let port = free_port().await;
    configure(&config, port).await;
    let socket = notify_socket(&notify);

    for run in 1..=2 {
        let mut child = spawn(&config, &notify);
        notified_with(&socket, "READY=1").await;
        terminate(&child);
        let status = tokio::time::timeout(GRACE, child.wait())
            .await
            .unwrap_or_else(|_| panic!("run {run} exits inside the grace"))
            .expect("the exit status is readable");
        assert!(status.success(), "run {run} exited {status:?}");
    }
    // A-W6-2: a second application of every migration is a no-op *because the ledger says
    // so*, not because the statements tolerate being run twice -- and the daemon applies
    // them on every start.
}

#[tokio::test]
async fn an_unreachable_weather_database_does_not_stop_the_daemon_starting() {
    let scratch = Scratch::new("db-down");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let port = free_port().await;
    configure(&config, port).await;

    // Port 1 on loopback: nothing listens there, and a connection is refused rather than
    // timing out. The URL still has to satisfy the schema's scheme CHECK. The slow shape
    // of the same failure is `a_blackholed_weather_database_does_not_hold_the_boot_open`.
    configure_weather_database(&config, "postgres://mw:secret@127.0.0.1:1/weather").await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    // **D-27**: a daemon that refuses to boot because TimescaleDB is down is the failure
    // this design rejects. The version assertion is classified `Retryable` when the
    // server cannot be reached, so the daemon starts and `db:postgres` will wait.
    notified_with(&socket, "READY=1").await;

    terminate(&child);
    let status = tokio::time::timeout(GRACE, child.wait())
        .await
        .expect("exits inside the grace")
        .expect("the exit status is readable");
    assert!(status.success(), "{status:?}");
}

/// An address that accepts no connection and refuses none either.
///
/// RFC 5737's TEST-NET-1, which is guaranteed never to be routed to a real host: a
/// connection attempt hangs until the kernel gives up, which is minutes. That is the
/// common production shape of "the database is unreachable" -- a firewall, a dead host on
/// a routed subnet, a partition -- and it is the one a refused connection on loopback does
/// not exercise.
const BLACKHOLE_URL: &str = "postgres://mw:secret@192.0.2.1:5432/weather";

async fn configure_weather_database(config: &Path, url: &str) {
    let pool = mw_store::config_db::open_and_migrate(config)
        .await
        .expect("migrated");
    sqlx::query("INSERT INTO weather_database (id, url) VALUES (1, ?1)")
        .bind(url)
        .execute(&pool)
        .await
        .expect("the row is insertable");
    pool.close().await;
}

/// **D-27**, in the shape that actually bites: the boot sequence must be *bounded*, not
/// merely tolerant of a fast failure.
///
/// Without a deadline on the D-62 probe the daemon never binds and never notifies, so
/// systemd kills the start at `TimeoutStartSec=60` and `Restart=always` makes that a loop
/// — "refusing to boot because TimescaleDB is down", which is what D-27 rejects.
#[tokio::test]
async fn a_blackholed_weather_database_does_not_hold_the_boot_open() {
    let scratch = Scratch::new("db-blackhole");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let port = free_port().await;
    configure(&config, port).await;
    configure_weather_database(&config, BLACKHOLE_URL).await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    let started = std::time::Instant::now();
    notified_with(&socket, "READY=1").await;
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the boot waited {:?} on an unreachable database",
        started.elapsed()
    );

    terminate(&child);
    let status = tokio::time::timeout(GRACE, child.wait())
        .await
        .expect("exits inside the grace")
        .expect("the exit status is readable");
    assert!(status.success(), "{status:?}");
}

/// A `systemctl stop` that arrives mid-startup is an ordered exit, not a kill.
///
/// The signal handler used to be installed when the main loop started, so every step of
/// the boot sequence ran with SIGTERM at its default disposition: the process died with
/// status `signal: 15`, no `STOPPING=1`, and "killed by signal TERM" in the journal. The
/// blackholed database is how the test gets a startup long enough to aim at, and the
/// boot's own `STATUS=` lines are how it aims without a sleep.
#[tokio::test]
async fn sigterm_during_startup_is_an_ordered_exit() {
    let scratch = Scratch::new("stop-during-start");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let port = free_port().await;
    configure(&config, port).await;
    configure_weather_database(&config, BLACKHOLE_URL).await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    // Waited for rather than slept past: the daemon says which step it is on, so the
    // signal lands inside the boot sequence by construction instead of by timing. The
    // probe's status also proves the handler is installed, since it is sent after it.
    notified_with(&socket, "asserting the database version").await;
    terminate(&child);

    let status = tokio::time::timeout(GRACE, child.wait())
        .await
        .expect("exits inside the grace")
        .expect("the exit status is readable");
    assert_eq!(
        status.code(),
        Some(0),
        "an interrupted startup exits 0 rather than dying by signal: {status:?}"
    );
    notified_with(&socket, "STOPPING=1").await;
}

#[tokio::test]
async fn a_configuration_path_that_cannot_be_opened_is_a_startup_failure_with_a_reason() {
    let scratch = Scratch::new("bad-path");
    // A directory where the database file should be: SQLite cannot open it, and the
    // failure has to name the path. `create_if_missing` makes a missing file a legitimate
    // first start, so this is the shape of "the path is wrong" that remains.
    let config = scratch.path("config.db");
    std::fs::create_dir(&config).expect("the directory is creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_modernweather"))
        .arg("--config")
        .arg(&config)
        .env("MW_LOG_FORMAT", "json")
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(
        output.status.code(),
        Some(1),
        "a daemon that could not start exits 1"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&config.display().to_string()),
        "the failure names the path it could not open: {stderr}"
    );
}

#[tokio::test]
async fn a_rejected_command_line_exits_2_and_prints_the_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_modernweather"))
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a wrong command line is not a failed start"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--config <path> is required"), "{stderr}");
    assert!(stderr.contains("Usage: modernweather"), "{stderr}");

    // And the help goes to stdout, so `modernweather --help | less` works.
    let help = Command::new(env!("CARGO_BIN_EXE_modernweather"))
        .arg("--help")
        .output()
        .await
        .expect("the binary runs");
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("--log-format"),
        "the usage is on stdout"
    );
}

/// Seed a `node` row, which is what makes step 4b open a spool at all.
///
/// The migration deliberately seeds none: `node_id` is a UUIDv7 the node mints once, and
/// `spool_path` has no default. So "this node has started before" is a state only a test or
/// W7's enrolment can create.
async fn configure_node(config: &Path, spool_path: &Path) {
    let pool = mw_store::config_db::open_and_migrate(config)
        .await
        .expect("migrated");
    sqlx::query(
        "INSERT INTO node (id, node_id, node_name, timezone, spool_path) \
         VALUES (1, ?1, 'under-test', 'UTC', ?2)",
    )
    .bind(vec![7u8; 16])
    .bind(spool_path.to_str().expect("a UTF-8 scratch path"))
    .execute(&pool)
    .await
    .expect("the row is insertable");
    pool.close().await;
}

/// Step 4b against the real binary: a configured node opens its spool, converges, and
/// stops cleanly — and the spool is on disk afterwards with the files recovery needs.
///
/// Nothing covered this path before: `tests/spool_fanout.rs` is in-process and never calls
/// `spool::open` or `boot::run`, so the 16-byte `node_id` check, both negative-value
/// guards, the `MANIFEST` creation and the shutdown-time `stats()` log were all untested.
#[tokio::test]
async fn a_configured_node_opens_its_spool_and_stops_cleanly() {
    let scratch = Scratch::new("spool-boot");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let spool = scratch.path("spool");
    let port = free_port().await;
    configure(&config, port).await;
    configure_node(&config, &spool).await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    notified_with(&socket, "READY=1").await;

    terminate(&child);
    let status = tokio::time::timeout(GRACE, child.wait())
        .await
        .expect("exits inside the grace")
        .expect("the exit status is readable");
    assert!(status.success(), "{status:?}");

    // The spool is a root resource `main` owns, so it exists by readiness and survives the
    // stop. `MANIFEST` is the file whose absence refuses the next start.
    assert!(spool.join("MANIFEST").is_file(), "the manifest was written");
    assert!(
        spool.join("cursors").is_dir(),
        "and the cursor directory exists for the first component that asks for one"
    );
    let segments = std::fs::read_dir(&spool)
        .expect("readable")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("seg"))
        .count();
    assert_eq!(segments, 1, "one open segment, created with its header");

    // And it reopens: the second start adopts the same spool rather than refusing it,
    // which is the `WrongNode` check passing rather than being untested.
    let notify = scratch.path("notify2.sock");
    let socket = notify_socket(&notify);
    let mut again = spawn(&config, &notify);
    notified_with(&socket, "READY=1").await;
    terminate(&again);
    let status = tokio::time::timeout(GRACE, again.wait())
        .await
        .expect("exits inside the grace")
        .expect("the exit status is readable");
    assert!(status.success(), "{status:?}");
}

/// A spool that cannot be opened is **fatal**, and that is the design's answer rather than
/// a convenience: §2.7 — "A daemon that cannot durably accept readings is not degraded, it
/// is wrong." D-27's start-anyway carve-out is about a *remote* server that is slow or
/// unreachable, not about a local root resource.
///
/// The one case where "fatal" was a line of code and not a tested claim.
#[tokio::test]
async fn a_spool_path_that_cannot_be_created_fails_the_start() {
    let scratch = Scratch::new("spool-unwritable");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    // A path whose parent is a *file*, so `create_dir_all` cannot succeed. Chosen over a
    // permission bit because the test suite may run as root, where a mode-000 directory is
    // still writable.
    let blocker = scratch.path("not-a-directory");
    std::fs::write(&blocker, b"this is a file").expect("writable");
    let spool = blocker.join("spool");
    let port = free_port().await;
    configure(&config, port).await;
    configure_node(&config, &spool).await;

    let mut child = spawn(&config, &notify);
    let status = tokio::time::timeout(PATIENCE, child.wait())
        .await
        .expect("a fatal spool failure exits promptly rather than hanging the start")
        .expect("the exit status is readable");
    assert!(
        !status.success(),
        "a spool that cannot be opened must exit non-zero, not start degraded: {status:?}"
    );
}
