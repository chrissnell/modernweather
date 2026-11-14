//! **M7-2's done-criteria, against the real binary**: first run, the gate, the recovery
//! command, and the widening.
//!
//! Four of these are claims about a *process* that no unit test can establish, which is why
//! the daemon is a subprocess here and the API is reached over its own socket:
//!
//! | | |
//! |---|---|
//! | the setup code goes to **stderr only and never the log ring** | W5 **D-27**. The ring is what `GET /api/v1/logs` returns, so the only honest assertion is to read both and compare |
//! | the management API **binds loopback while unclaimed** and widens after | the bind address is a plan input; it is observable in the listener's own generation |
//! | `modernweather reset-password` **works on the host** | a second process writing the database the daemon has open |
//! | changing the password **revokes every other session and keeps the current one** | two real sessions over two real cookies |
//!
//! Not run under ThreadSanitizer, for `tests/boot.rs`' and `tests/management.rs`' reasons
//! exactly -- the subject is a child process, and a report inside it surfaces here as a
//! connection reset.
#![cfg(not(mw_sanitizer))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixDatagram};
use tokio::process::{Child, Command};

/// Generous for `tests/management.rs`' reason: the point of a timeout here is to fail with a
/// message rather than to hang a run.
const PATIENCE: Duration = Duration::from_secs(30);

/// The password the fixtures use. Twelve characters or more, and not on the breached list.
const PASSWORD: &str = "correct horse battery staple";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mw-auth-{}-{tag}-{}",
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

/// Three ports nothing is listening on, asked for through the crate that owns binding.
///
/// `ci/layers.toml`'s `only-mw-http-binds-a-socket` rule scans `tests/` as well as `src/`.
async fn free_ports() -> (u16, u16, u16) {
    let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let probe = mw_http::bind_all(
        &mw_http::Listen {
            addr: loopback,
            http_port: 0,
            https_port: None,
            grpc_port: 0,
        },
        axum::Router::new(),
        None,
        std::sync::Arc::new(mw_http::HostTable::default()),
        Some(mw_http::Management {
            addr: std::net::SocketAddr::new(loopback, 0),
            router: axum::Router::new(),
            tls: None,
        }),
    )
    .await
    .expect("three loopback ports are available");
    (
        probe.local_addr().port(),
        probe.grpc_addr().port(),
        probe
            .management_addr()
            .expect("the management socket was asked for")
            .port(),
    )
}

/// The smallest configuration the daemon will converge on, with the management API's
/// configured bind address left to the caller.
///
/// **`management_listen_addr` is the subject of two of the tests below**, so it is a
/// parameter rather than a constant: `0.0.0.0` is what makes the unclaimed narrowing
/// observable, because the narrowed address and the configured one then differ.
async fn configure(path: &Path, http: u16, grpc: u16, management: u16, mgmt_addr: &str) {
    let pool = mw_store::config_db::open_and_migrate(path)
        .await
        .expect("the configuration schema applies from empty");
    sqlx::query(
        "UPDATE listener SET listen_addr = '127.0.0.1', http_port = ?1, grpc_port = ?2, \
         management_port = ?3, management_listen_addr = ?4 WHERE id = 1",
    )
    .bind(i64::from(http))
    .bind(i64::from(grpc))
    .bind(i64::from(management))
    .bind(mgmt_addr)
    .execute(&pool)
    .await
    .expect("the seeded listener row is updatable");
    sqlx::query(
        "INSERT INTO node (id, node_id, node_name, timezone, spool_path) \
         VALUES (1, ?1, 'test', 'UTC', ?2)",
    )
    .bind(vec![7u8; 16])
    .bind(
        path.parent()
            .expect("the config path has a directory")
            .join("spool")
            .to_string_lossy()
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("the node row is insertable");
    pool.close().await;
}

/// Seed an administrator account, for the tests whose subject is not first run.
///
/// Written with SQL and the real hashing function, so the fixture cannot drift from the
/// policy login verifies against.
async fn claim_already(path: &Path) {
    let pool = mw_store::config_db::open_and_migrate(path)
        .await
        .expect("the schema applies");
    sqlx::query(
        "INSERT INTO admin_credential (id, username, password_phc) VALUES (1, 'chris', ?1)",
    )
    .bind(
        mw_api_mgmt::auth::hash_password(&secrecy::SecretString::from(PASSWORD))
            .expect("the policy parameters hash"),
    )
    .execute(&pool)
    .await
    .expect("the administrator row is insertable");
    pool.close().await;
}

fn notify_socket(path: &Path) -> UnixDatagram {
    UnixDatagram::bind(path).expect("the notify socket is bindable")
}

async fn wait_ready(socket: &UnixDatagram) {
    let mut seen = Vec::new();
    loop {
        let mut buf = vec![0u8; 4096];
        let read = tokio::time::timeout(PATIENCE, socket.recv(&mut buf))
            .await
            .expect("a notification arrived before the deadline")
            .expect("the datagram is readable");
        let message = String::from_utf8_lossy(&buf[..read]).into_owned();
        let ready = message.contains("READY=1");
        seen.push(message);
        if ready {
            return;
        }
        assert!(seen.len() < 64, "READY=1 never arrived; saw {seen:#?}");
    }
}

/// A running daemon, with its console output captured.
///
/// The capture is the whole reason this is a struct rather than a `Child`: the setup code is
/// on stderr by design (**D-27**) and nowhere else, so a test that wants to use it has to
/// read the console the way an operator does.
///
/// **Drained on demand rather than by a pump task.** `clippy.toml` forbids `tokio::spawn`
/// across this workspace -- "spawn through `mw_supervisor::task`, so the task table owns the
/// handle and a leak is unexpressible" -- and a stderr pump in a test is not a reason to
/// carve an exception into a rule about the daemon's task table. [`Daemon::drain`] reads
/// whatever has arrived whenever an assertion needs it, which is also what keeps the pipe
/// from filling.
struct Daemon {
    child: Child,
    reader: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    seen: Vec<String>,
}

impl Daemon {
    fn spawn(config: &Path, notify: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_modernweather"));
        command
            .arg("--config")
            .arg(config)
            .args(extra)
            .env("NOTIFY_SOCKET", notify)
            .env("MW_LOG_FORMAT", "json")
            .env("RUST_LOG", "info")
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("the binary runs");
        let stderr = child.stderr.take().expect("stderr was piped");
        Self {
            child,
            reader: BufReader::new(stderr).lines(),
            seen: Vec::new(),
        }
    }

    /// Read every console line that has arrived.
    ///
    /// The timeout is what makes "has arrived" decidable: `next_line()` on a live child does
    /// not return until it writes another line.
    async fn drain(&mut self) {
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_millis(50), self.reader.next_line()).await
        {
            self.seen.push(line);
        }
    }

    fn lines(&self) -> &[String] {
        &self.seen
    }

    /// Wait for a console line matching `want`, and return it.
    async fn wait_for(&mut self, what: &str, want: impl Fn(&str) -> bool) -> String {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            self.drain().await;
            if let Some(line) = self.seen.iter().find(|l| want(l)) {
                return line.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no console line for {what}; saw {:#?}",
                self.seen
            );
        }
    }

    /// Drain whatever is left on the console, then ask the daemon to stop.
    ///
    /// The drain is not decoration: stderr is a pipe, and a test that asserted nothing about
    /// the console would otherwise leave every line it wrote in a buffer that is only 64 KB
    /// deep. Reading it here keeps that bounded for the tests whose subject is the API.
    async fn terminate(&mut self) {
        self.drain().await;
        let pid = self.child.id().expect("the child has not been reaped");
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        // **Waited for, not left to `kill_on_drop`.** Two reasons, and the second is why it
        // is here rather than in the test bodies: an ordered exit is what this daemon's
        // `Type=notify` contract promises and a `SIGKILL` on drop would stop asserting it,
        // and under `cargo llvm-cov` a child that is killed never writes its profile — so
        // the coverage of every path this test exercised in the subprocess would be lost.
        let _ = tokio::time::timeout(PATIENCE, self.child.wait()).await;
    }
}

/// One request against the management API.
///
/// Written out by hand for `tests/management.rs`' reason: D-32 names no HTTP client, and a
/// test dependency on one would be a client in the dependency graph of the thing it is
/// testing. The three header knobs are this item's subject -- the cookie, the CSRF header --
/// so they are arguments rather than constants.
async fn request(
    port: u16,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    csrf: bool,
    body: Option<Value>,
) -> (u16, Value, Vec<String>) {
    // **Connect with a retry**, which is this item's doing rather than flake tolerance: a
    // successful claim re-plans, and `http:listener`'s digest covers the management socket,
    // so the port is deliberately closed for as long as the restart takes. A helper that
    // panicked on the first refusal would be asserting something the design does not
    // promise.
    let mut socket = connect(port).await;
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n",
        payload.len()
    );
    if let Some(cookie) = cookie {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    if csrf {
        head.push_str("X-MW-Request: 1\r\n");
    }
    head.push_str("\r\n");
    socket
        .write_all(head.as_bytes())
        .await
        .expect("the request head is written");
    socket
        .write_all(payload.as_bytes())
        .await
        .expect("the request body is written");
    let mut response = String::new();
    tokio::time::timeout(PATIENCE, socket.read_to_string(&mut response))
        .await
        .expect("the response arrived before the deadline")
        .expect("the response is read");

    let status: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response}"));
    let (head, body) = response
        .split_once("\r\n\r\n")
        .unwrap_or((response.as_str(), ""));
    let headers: Vec<String> = head.lines().skip(1).map(str::to_string).collect();
    let parsed = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body.trim())
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {body}"))
    };
    (status, parsed, headers)
}

/// A connection to the management socket, waiting out a restart.
async fn connect(port: u16) -> TcpStream {
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(socket) => return socket,
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("the management API never came back on {port}: {e}"),
        }
    }
}

/// The `mw_session=...` cookie out of a response's `Set-Cookie` headers.
fn cookie_in(headers: &[String]) -> String {
    headers
        .iter()
        .filter_map(|h| {
            h.strip_prefix("set-cookie: ")
                .or(h.strip_prefix("Set-Cookie: "))
        })
        .find(|v| v.starts_with("mw_session=") && !v.starts_with("mw_session=;"))
        .and_then(|v| v.split(';').next())
        .unwrap_or_else(|| panic!("no session cookie in {headers:#?}"))
        .to_string()
}

/// Sign in, and return the cookie.
async fn login(port: u16, password: &str) -> String {
    let (status, body, headers) = request(
        port,
        "POST",
        "/api/v1/auth/login",
        None,
        true,
        Some(json!({"username": "chris", "password": password})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    cookie_in(&headers)
}

/// The setup code out of the console banner.
///
/// Recognised by shape -- thirteen groups of four Crockford base32 characters -- rather than
/// by the line before it, so the assertion does not depend on the banner's prose.
fn code_in(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| {
        let candidate = line.trim();
        let groups: Vec<&str> = candidate.split('-').collect();
        let looks_right = groups.len() == 13
            && groups.iter().all(|g| {
                g.len() == 4
                    && g.chars()
                        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
            });
        looks_right.then(|| candidate.to_string())
    })
}

/// **First run, end to end**: the banner, the loopback narrowing, the gate, the claim, the
/// widening, and **D-27** — the code is on the console and not in the ring.
#[tokio::test]
async fn first_run_prints_a_code_to_the_console_that_never_reaches_the_log_ring() {
    let scratch = Scratch::new("firstrun");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    // Configured to bind every interface, so "narrowed to loopback" is a different address
    // from "the configured one" and the widening is observable.
    configure(&config, http, grpc, mgmt, "0.0.0.0").await;
    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &[]);
    wait_ready(&socket).await;

    // The banner, and the narrowing it explains.
    let banner_line = daemon
        .wait_for("the setup code", |l| {
            code_in(std::slice::from_ref(&l.to_string())).is_some()
        })
        .await;
    let code = code_in(std::slice::from_ref(&banner_line)).expect("the banner line is a code");
    let banner = daemon.lines().join("\n");
    assert!(
        banner.contains("ModernWeather is not set up yet."),
        "{banner}"
    );
    assert!(
        banner.contains(&format!("ssh -L {mgmt}:localhost:{mgmt}")),
        "the banner offers the tunnel on the port this daemon actually uses: {banner}"
    );
    assert!(
        banner.contains(&format!("http://localhost:{mgmt}/")),
        "the banner's URL names the configured port and scheme: {banner}"
    );
    assert!(
        !banner.contains(":8081"),
        "the banner hardcodes the column's default port: {banner}"
    );
    daemon
        .wait_for("the loopback narrowing", |l| {
            l.contains("no administrator account yet") && l.contains("127.0.0.1")
        })
        .await;
    daemon
        .wait_for("the narrowed listener", |l| {
            l.contains("the one listener is serving")
                && l.contains(&format!("\"management\":\"Some(127.0.0.1:{mgmt})\""))
        })
        .await;

    // Unclaimed and unauthenticated: the one read that answers, and a representative
    // sample of M7-1's surface that does not.
    let (status, body, _) = request(mgmt, "GET", "/api/v1/auth/session", None, false, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["claimed"], false);
    assert_eq!(body["authenticated"], false);
    for (method, path) in [
        ("GET", "/api/v1/config"),
        ("GET", "/api/v1/stations"),
        ("GET", "/api/v1/runtime/summary"),
        ("GET", "/api/v1/logs"),
        ("GET", "/api/v1/system/info"),
        ("GET", "/api/v1/auth/sessions"),
    ] {
        let (status, body, _) = request(mgmt, method, path, None, true, None).await;
        assert_eq!(status, 401, "{method} {path} answered: {body}");
    }
    // §3.3's second CSRF defence, on the one write a caller with no session may make.
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/claim",
        None,
        false,
        Some(json!({
            "setup_code": code,
            "username": "chris",
            "password": PASSWORD,
            "password_again": PASSWORD,
        })),
    )
    .await;
    assert_eq!(status, 422, "{body}");

    // A wrong code does not spend the right one.
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/claim",
        None,
        true,
        Some(json!({
            "setup_code": "0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000",
            "username": "chris",
            "password": PASSWORD,
            "password_again": PASSWORD,
        })),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["errors"][0]["field"], "setup_code");

    // The claim, with the code as it was printed -- hyphens and all.
    let (status, body, headers) = request(
        mgmt,
        "POST",
        "/api/v1/auth/claim",
        None,
        true,
        Some(json!({
            "setup_code": code,
            "username": "chris",
            "password": PASSWORD,
            "password_again": PASSWORD,
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["username"], "chris");
    let cookie = cookie_in(&headers);

    // **D-27**, which is the reason the banner is not a `tracing` event: the ring the
    // management UI displays must not contain the code.
    let (status, logs, _) = request(
        mgmt,
        "GET",
        "/api/v1/logs?limit=2000",
        Some(&cookie),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200, "{logs}");
    let rendered = logs.to_string();
    assert!(
        !rendered.contains(&code),
        "the setup code reached the log ring"
    );
    // Nor any fragment of it: a redaction that split it would still be a leak.
    for group in code.split('-') {
        assert!(!rendered.contains(group), "`{group}` reached the log ring");
    }
    // The issue itself **is** audited, without its value.
    assert!(
        rendered.contains("a one-time setup code was issued"),
        "the issue was not audited: {rendered}"
    );
    assert!(
        rendered.contains("administrator account created"),
        "the claim was not audited: {rendered}"
    );

    // The same code again is refused, because the account now exists -- single use.
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/claim",
        None,
        true,
        Some(json!({
            "setup_code": code,
            "username": "someone-else",
            "password": "another good passphrase",
            "password_again": "another good passphrase",
        })),
    )
    .await;
    assert_eq!(status, 409, "{body}");

    // **The widening.** The claim re-plans, and the listener is rebuilt on the configured
    // address -- which is a different socket from the one that was narrowed to. Read off
    // `http:listener`'s own line rather than the boot sequence's `plan validated`, because
    // the boot sequence runs once and this is the *second* plan: what says where the socket
    // actually is, is the component that bound it.
    daemon
        .wait_for("the widened listener", |l| {
            l.contains("the one listener is serving")
                && l.contains(&format!("\"management\":\"Some(0.0.0.0:{mgmt})\""))
        })
        .await;
    daemon
        .wait_for("the re-plan the claim took", |l| {
            l.contains("the configuration was re-planned")
                && l.contains("the instance has been claimed")
        })
        .await;
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        let (status, body, _) = request(
            mgmt,
            "GET",
            "/api/v1/runtime/components",
            Some(&cookie),
            false,
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let listener = body["components"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|c| c["id"] == "http:listener")
            .expect("the listener is planned")
            .clone();
        if listener["applied_generation"].as_u64() == Some(1) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the listener was never rebuilt at the claim's generation: {listener}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // And the surface is open to the session the claim produced.
    let (status, body, _) =
        request(mgmt, "GET", "/api/v1/config", Some(&cookie), false, None).await;
    assert_eq!(status, 200, "{body}");
    // No credential material anywhere in the configuration view (§3.2).
    let rendered = body.to_string();
    assert!(!rendered.contains("argon2"), "{rendered}");
    assert!(!rendered.contains(PASSWORD), "{rendered}");

    daemon.terminate().await;
}

/// `--setup-listen` widens the unclaimed bind, deliberately and with a warning (§3.4).
#[tokio::test]
async fn setup_listen_widens_the_unclaimed_window_and_says_so() {
    let scratch = Scratch::new("setuplisten");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    // **The schema's own default**, deliberately. `A-W5-3` makes `management_listen_addr`
    // `127.0.0.1`, so this is the configuration a fresh headless install has -- and the one
    // in which a `--setup-listen` consulted after a loopback early return would silently do
    // nothing.
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;
    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &["--setup-listen", "0.0.0.0"]);
    wait_ready(&socket).await;

    // The warning §3.4 asks for, naming what it widens.
    let warning = daemon
        .wait_for("the --setup-listen warning", |l| {
            l.contains("--setup-listen widens the management API")
        })
        .await;
    assert!(warning.contains("\"level\":\"WARN\""), "{warning}");
    assert!(
        warning.contains("\"configured\":\"127.0.0.1\""),
        "{warning}"
    );
    // And the banner says the surface is reachable rather than offering a tunnel, because it
    // is built from the same `management_addr` the listener is.
    let banner = daemon.lines().join("\n");
    assert!(
        banner.contains("reachable from your network"),
        "the banner still describes a loopback-only instance: {banner}"
    );
    assert!(!banner.contains("ssh -L"), "{banner}");
    // And the socket really is the widened one rather than loopback.
    daemon
        .wait_for("the widened listener", |l| {
            l.contains("the one listener is serving")
                && l.contains(&format!("\"management\":\"Some(0.0.0.0:{mgmt})\""))
        })
        .await;
    // It is still unclaimed, so the gate is still closed.
    let (status, body, _) = request(mgmt, "GET", "/api/v1/config", None, true, None).await;
    assert_eq!(status, 401, "{body}");

    daemon.terminate().await;
}

/// **M7-2's done-criterion**: changing the password revokes every other session and keeps
/// the current one.
#[tokio::test]
async fn changing_the_password_revokes_every_other_session_and_keeps_this_one() {
    let scratch = Scratch::new("password");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;
    claim_already(&config).await;
    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &[]);
    wait_ready(&socket).await;

    // A claimed instance prints no banner and narrows nothing.
    daemon.drain().await;
    let console = daemon.lines().join("\n");
    assert!(code_in(daemon.lines()).is_none(), "{console}");
    assert!(!console.contains("is not set up yet"), "{console}");

    let laptop = login(mgmt, PASSWORD).await;
    let desktop = login(mgmt, PASSWORD).await;
    let (status, body, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&desktop),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["sessions"].as_array().map(Vec::len), Some(2));

    // The wrong current password changes nothing (§3.5).
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/password",
        Some(&desktop),
        true,
        Some(json!({
            "current_password": "not the current one",
            "new_password": "a brand new passphrase",
            "new_password_again": "a brand new passphrase",
        })),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["errors"][0]["field"], "current_password");

    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/password",
        Some(&desktop),
        true,
        Some(json!({
            "current_password": PASSWORD,
            "new_password": "a brand new passphrase",
            "new_password_again": "a brand new passphrase",
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // This browser is still signed in; the other is not.
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&desktop),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&laptop),
        false,
        None,
    )
    .await;
    assert_eq!(status, 401, "the stolen laptop is still signed in");

    // And the new password is the one that works.
    let fresh = login(mgmt, "a brand new passphrase").await;
    assert_ne!(fresh, desktop);
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/login",
        None,
        true,
        Some(json!({"username": "chris", "password": PASSWORD})),
    )
    .await;
    assert_eq!(status, 401, "{body}");

    daemon.terminate().await;
}

/// **M7-2's other done-criterion**: `modernweather reset-password` works on the host.
///
/// A second process, writing the database the running daemon has open -- which is what the
/// command really is, and why the assertion is that the daemon's own API honours the change
/// without being restarted.
#[tokio::test]
async fn reset_password_works_on_the_host_while_the_daemon_runs() {
    let scratch = Scratch::new("reset");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;
    claim_already(&config).await;
    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &[]);
    wait_ready(&socket).await;

    let before = login(mgmt, PASSWORD).await;
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&before),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200);

    // The command, with the new password on standard input -- §3.6's scriptable path.
    let output = run_command(
        &config,
        &["reset-password"],
        Some("a different passphrase\n"),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(said.contains("has been changed"), "{said}");
    assert!(said.contains("1 session(s) were signed out"), "{said}");

    // Every session is gone, including the one that was live a moment ago.
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&before),
        false,
        None,
    )
    .await;
    assert_eq!(
        status, 401,
        "reset-password did not revoke the live session"
    );
    // The old password no longer works and the new one does -- without restarting anything.
    let (status, _, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/login",
        None,
        true,
        Some(json!({"username": "chris", "password": PASSWORD})),
    )
    .await;
    assert_eq!(status, 401);
    let after = login(mgmt, "a different passphrase").await;
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&after),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200);

    // A password the policy refuses is refused here too, and changes nothing.
    let output = run_command(&config, &["reset-password"], Some("short\n")).await;
    assert!(!output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(said.contains("at least 12 characters"), "{said}");
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&after),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200, "a refused reset revoked a session");

    // `reset-setup-token` on a claimed instance is refused and names the right command.
    let output = run_command(&config, &["reset-setup-token"], None).await;
    assert!(!output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(said.contains("reset-password"), "{said}");

    // `sessions revoke --all` signs out of everything.
    let output = run_command(&config, &["sessions", "revoke", "--all"], None).await;
    assert!(output.status.success(), "{output:?}");
    let (status, _, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/sessions",
        Some(&after),
        false,
        None,
    )
    .await;
    assert_eq!(status, 401);

    daemon.terminate().await;
}

/// `reset-setup-token` on an instance that was never claimed prints a code that works.
#[tokio::test]
async fn reset_setup_token_prints_a_code_that_claims_the_instance() {
    let scratch = Scratch::new("reissue");
    let config = scratch.path("config.db");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;

    // No daemon at all: this is the locked-out case, run on the host.
    let output = run_command(&config, &["reset-setup-token"], None).await;
    assert!(output.status.success(), "{output:?}");
    let lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::to_string)
        .collect();
    let code = code_in(&lines).unwrap_or_else(|| panic!("no code in {lines:#?}"));

    // Start the daemon and claim with it. The code outlives the process that printed it,
    // because only its hash is stored.
    let notify = scratch.path("notify.sock");
    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &[]);
    wait_ready(&socket).await;
    // And the daemon does **not** print a second banner: there is already a code
    // outstanding, and re-issuing on every start would invalidate the one being typed.
    let console = daemon
        .wait_for("the already-issued notice", |l| {
            l.contains("a setup code was issued on an earlier start")
        })
        .await;
    assert!(console.contains("reset-setup-token"), "{console}");
    assert!(
        code_in(daemon.lines()).is_none(),
        "a second code was printed"
    );

    let (status, body, headers) = request(
        mgmt,
        "POST",
        "/api/v1/auth/claim",
        None,
        true,
        // Retyped the way a person would: lower case, and with the hyphens dropped.
        Some(json!({
            "setup_code": code.replace('-', "").to_lowercase(),
            "username": "chris",
            "password": PASSWORD,
            "password_again": PASSWORD,
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let cookie = cookie_in(&headers);
    let (status, body, _) = request(
        mgmt,
        "GET",
        "/api/v1/auth/session",
        Some(&cookie),
        false,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["claimed"], true);
    assert_eq!(body["authenticated"], true);

    daemon.terminate().await;
}

/// Run one of W5 §3.6's commands as its own process.
async fn run_command(config: &Path, words: &[&str], stdin: Option<&str>) -> std::process::Output {
    use std::process::Stdio;
    let mut command = Command::new(env!("CARGO_BIN_EXE_modernweather"));
    command
        .args(words)
        .arg("--config")
        .arg(config)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("the binary runs");
    if let Some(stdin) = stdin {
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(stdin.as_bytes())
            .await
            .expect("the password is written");
    }
    tokio::time::timeout(PATIENCE, child.wait_with_output())
        .await
        .expect("the command finished before the deadline")
        .expect("the command ran")
}

/// `reset-password` on an instance that has no administrator points at the command that
/// does have an answer, and changes nothing.
///
/// The locked-out case has two shapes -- "I forgot the password" and "I never set one" --
/// and §3.6's command only answers the first. Saying which one this is beats "no such row".
#[tokio::test]
async fn reset_password_before_the_claim_names_the_command_that_applies() {
    let scratch = Scratch::new("unclaimed-reset");
    let config = scratch.path("config.db");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;

    let output = run_command(&config, &["reset-password"], Some("a good passphrase\n")).await;
    assert!(!output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(said.contains("no administrator account yet"), "{said}");
    assert!(said.contains("reset-setup-token"), "{said}");

    // Nothing was created, so the instance is still claimable the normal way.
    let pool = mw_store::config_db::open_and_migrate(&config)
        .await
        .expect("the schema applies");
    assert!(
        !mw_store::admin_auth::is_claimed(&pool)
            .await
            .expect("readable"),
        "a refused reset created an account"
    );
    pool.close().await;
}

/// `--username` renames the administrator while resetting the password (§3.6).
///
/// Part of the command rather than an endpoint because a forgotten *username* is
/// indistinguishable from a forgotten password from the outside: the API cannot tell an
/// operator which name they chose.
#[tokio::test]
async fn reset_password_can_rename_the_administrator() {
    let scratch = Scratch::new("rename");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;
    claim_already(&config).await;

    let output = run_command(
        &config,
        &["reset-password", "--username", "someone-else"],
        Some("a renamed passphrase\n"),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(said.contains("`someone-else`"), "{said}");

    // An empty `--username` is refused rather than written: the column has no `CHECK`
    // against one, so it would leave a credential that only an empty username can sign into
    // -- a lockout produced by the command that exists to end one.
    let refused = run_command(
        &config,
        &["reset-password", "--username", "   "],
        Some("another passphrase here\n"),
    )
    .await;
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("cannot be empty"),
        "{refused:?}"
    );

    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &[]);
    wait_ready(&socket).await;
    // The old name no longer signs in and the new one does.
    let (status, _, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/login",
        None,
        true,
        Some(json!({"username": "chris", "password": "a renamed passphrase"})),
    )
    .await;
    assert_eq!(status, 401);
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/auth/login",
        None,
        true,
        Some(json!({"username": "someone-else", "password": "a renamed passphrase"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["username"], "someone-else");

    daemon.terminate().await;
}

/// The security headers and the CORS allowlist, on the real socket.
#[tokio::test]
async fn every_response_carries_the_security_headers_and_no_wildcard_origin() {
    let scratch = Scratch::new("headers");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt, "127.0.0.1").await;
    claim_already(&config).await;
    let socket = notify_socket(&notify);
    let mut daemon = Daemon::spawn(&config, &notify, &[]);
    wait_ready(&socket).await;

    for (path, cookie, expected) in [
        ("/api/v1/auth/session", None, 200u16),
        ("/api/v1/config", None, 401),
    ] {
        let (status, _, headers) = request(mgmt, "GET", path, cookie, false, None).await;
        assert_eq!(status, expected, "{path}");
        let rendered = headers.join("\n").to_lowercase();
        for header in [
            "content-security-policy: default-src 'self'; frame-ancestors 'none'",
            "x-content-type-options: nosniff",
            "referrer-policy: no-referrer",
            "cache-control: no-store",
        ] {
            assert!(
                rendered.contains(header),
                "{path} is missing `{header}`: {rendered}"
            );
        }
        // The defect §3.1 lists: `Access-Control-Allow-Origin: *` on the admin API. The
        // allowlist is empty by default, so there is no such header at all.
        assert!(
            !rendered.contains("access-control-allow-origin"),
            "{path}: {rendered}"
        );
    }

    let cookie = login(mgmt, PASSWORD).await;
    // The cookie is `HttpOnly` and `SameSite=Strict`, and **not** `Secure` on a plaintext
    // socket -- a browser would refuse a `__Host-` cookie here (§3.3).
    let (_, _, headers) = request(
        mgmt,
        "POST",
        "/api/v1/auth/login",
        None,
        true,
        Some(json!({"username": "chris", "password": PASSWORD})),
    )
    .await;
    let set = headers
        .iter()
        .find(|h| h.to_lowercase().starts_with("set-cookie:"))
        .expect("a Set-Cookie")
        .clone();
    assert!(set.contains("HttpOnly"), "{set}");
    assert!(set.contains("SameSite=Strict"), "{set}");
    assert!(!set.contains("Secure"), "{set}");
    assert!(!set.contains("__Host-"), "{set}");

    // Every write needs the header, on M7-1's surface as well as on the auth routes.
    let (status, body, _) = request(
        mgmt,
        "POST",
        "/api/v1/config/preview",
        Some(&cookie),
        false,
        Some(json!({"kind": "put_listener", "listener": {}})),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("did not come from the management interface"),
        "{body}"
    );

    daemon.terminate().await;
}
