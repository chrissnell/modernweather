//! M2-4's done-criteria, against the real binary.
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
    let stopping = notified_with(&socket, "STOPPING=1").await;
    assert!(!stopping.is_empty());
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
    // timing out. The URL still has to satisfy the schema's scheme CHECK.
    let pool = mw_store::config_db::open_and_migrate(&config)
        .await
        .expect("migrated");
    sqlx::query("INSERT INTO weather_database (id, url) VALUES (1, ?1)")
        .bind("postgres://mw:secret@127.0.0.1:1/weather")
        .execute(&pool)
        .await
        .expect("the row is insertable");
    pool.close().await;

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
