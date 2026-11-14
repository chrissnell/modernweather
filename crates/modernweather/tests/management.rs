//! **M7-1's done-criterion, against the real binary**: a save's preview names exactly the
//! components that then restart.
//!
//! That sentence is the whole reason `POST /api/v1/config/preview` is worth having rather
//! than decorative (W5 §2.4), and it is not a property any unit test can establish: it is a
//! claim about a *process* -- that the diff a preview reports, the diff a save reports, and
//! the set of components whose running instance is afterwards built from the new generation
//! are one set. So the daemon is a subprocess, the API is reached over its own socket, and
//! the assertion is on `applied_generation`, which is §2.2's mechanism and the one number
//! the server cannot fake.
//!
//! Not run under ThreadSanitizer, for `tests/boot.rs`' reasons exactly -- the subject is a
//! child process, and a report inside it surfaces here as a connection reset.
#![cfg(not(mw_sanitizer))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixDatagram};
use tokio::process::{Child, Command};

/// Generous for the same reason `tests/boot.rs`' is: the point of a timeout here is to fail
/// with a message rather than to hang a run.
const PATIENCE: Duration = Duration::from_secs(30);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mw-mgmt-{}-{tag}-{}",
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
/// `ci/layers.toml`'s `only-mw-http-binds-a-socket` rule scans `tests/` as well as `src/`,
/// and it is right to: the design's answer to seven listener-owning sites in the Go tree is
/// that there is exactly one place a listener can exist.
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
        // No ingest TLS: this probe only wants the ports the kernel chose, and a plaintext
        // gRPC socket is what every node without a `config.ingest` row binds anyway.
        None,
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

/// The session every request below carries, as `mw_session=<id>.<secret>`.
///
/// **Fixed bytes, seeded by [`configure`]**, which is what keeps implementation-plan.md
/// **M7-2**'s gate out of the way of M7-1's subject. Every management route is authenticated
/// now, so a test that asserted on `/api/v1/config` without a session would assert on a
/// `401` -- and signing in per test would put an Argon2id hash at m=64 MiB in front of each
/// of the eleven tests below for no gain, because what they are about is the plan diff.
///
/// The authentication itself is asserted in `tests/auth.rs`, against the real handler set
/// and the real `claim`/`login` path.
const SESSION_ID: [u8; 16] = [0xa5; 16];
const SESSION_SECRET: [u8; 32] = [0x5c; 32];

/// Lowercase hex, for the two halves of the cookie.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `Cookie` header value for [`SESSION_ID`].
fn session_cookie() -> String {
    format!("mw_session={}.{}", hex(&SESSION_ID), hex(&SESSION_SECRET))
}

/// A configuration with one station and one website, which is the smallest shape in which
/// every component family this item can restart is actually planned.
///
/// Written with SQL rather than through the API because the API is what is under test: a
/// fixture built by the thing being asserted about proves nothing about either.
///
/// **The station has `enabled = 0`**, and that is load-bearing rather than incidental. A
/// collecting `tcp_out` station dials its datalogger at *build* time, so on a machine with
/// nothing on the other end it never reaches `Running` -- which is W5 §2.4's state ⑤
/// ("Saved, but it won't start") and a perfectly good thing for the API to report, but it
/// means the component never reports an applied generation and these tests would be waiting
/// for a convergence that correctly never comes. A station that is configured and not
/// collecting still produces every surface this item restarts: it is in the snapshot key
/// set, on the website, and in the configuration views.
async fn configure(path: &Path, http: u16, grpc: u16, management: u16) {
    let pool = mw_store::config_db::open_and_migrate(path)
        .await
        .expect("the configuration schema applies from empty");
    sqlx::query(
        "UPDATE listener SET listen_addr = '127.0.0.1', http_port = ?1, grpc_port = ?2, \
         management_port = ?3 WHERE id = 1",
    )
    .bind(i64::from(http))
    .bind(i64::from(grpc))
    .bind(i64::from(management))
    .execute(&pool)
    .await
    .expect("the seeded listener row is updatable");

    // A node identity, because a station needs an owner and a spool needs a path.
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

    sqlx::query(
        "INSERT INTO stations (station_key, owning_node_id, name, type, enabled, \
         identity_kind, latitude, longitude, timezone, transport_kind, transport_host, \
         transport_port) \
         VALUES ('backyard', ?1, 'Backyard', 'campbellscientific', 0, 'fixed', 39.5, \
         -105.1, 'America/Denver', 'tcp_out', '127.0.0.1', 7001)",
    )
    .bind(vec![7u8; 16])
    .execute(&pool)
    .await
    .expect("the station row is insertable");

    sqlx::query(
        "INSERT INTO website (id, name, hostname, page_title, primary_station_key) \
         VALUES (1, 'Home', '', 'Backyard Weather', 'backyard')",
    )
    .execute(&pool)
    .await
    .expect("the website row is insertable");

    // **An administrator and a live session**, because M7-2 put a gate in front of every
    // route these tests read. Two consequences, both wanted: the instance is *claimed*, so
    // the management socket binds the configured `127.0.0.1` rather than being narrowed to
    // it, and `session_cookie()` resolves.
    //
    // The hash is produced by the same function login verifies against, so this fixture
    // cannot drift from the policy; the password is never used, because these tests present
    // the session rather than signing in.
    sqlx::query("INSERT INTO admin_credential (id, username, password_phc) VALUES (1, 'test', ?1)")
        .bind(
            mw_api_mgmt::auth::hash_password(&secrecy::SecretString::from(
                "correct horse battery staple",
            ))
            .expect("the policy parameters hash"),
        )
        .execute(&pool)
        .await
        .expect("the administrator row is insertable");
    sqlx::query(
        "INSERT INTO admin_session (id, token_hash, expires_at) \
         VALUES (?1, ?2, unixepoch() + 86400)",
    )
    .bind(SESSION_ID.to_vec())
    .bind(mw_api_mgmt::auth::digest(&SESSION_SECRET).to_vec())
    .execute(&pool)
    .await
    .expect("the session row is insertable");

    pool.close().await;
}

fn notify_socket(path: &Path) -> UnixDatagram {
    UnixDatagram::bind(path).expect("the notify socket is bindable")
}

/// Read notifications until `READY=1` arrives, so the assertions start against a converged
/// daemon rather than against a race.
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

fn spawn(config: &Path, notify: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_modernweather"))
        .arg("--config")
        .arg(config)
        .env("NOTIFY_SOCKET", notify)
        .env("MW_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        .kill_on_drop(true)
        .spawn()
        .expect("the binary runs")
}

/// Ask the daemon to stop, and **wait for it to**.
///
/// Waited for rather than left to `kill_on_drop`, for two reasons. An ordered exit is what
/// this daemon's `Type=notify` contract promises, and a `SIGKILL` on drop would stop
/// asserting it. And under `cargo llvm-cov` a child that is killed never writes its profile,
/// so every path these tests exercised inside the subprocess -- which is most of what they
/// are about -- would be missing from the coverage ratchet's measurement.
async fn terminate(child: &mut Child) {
    let pid = child.id().expect("the child has not been reaped");
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let _ = tokio::time::timeout(PATIENCE, child.wait()).await;
}

/// One request against the management API, as a status and a parsed body.
///
/// Written out by hand for the reason `mw_output::https` and `mw_station::http` are: D-32
/// names no HTTP client, and a test dependency on one would be a client in the dependency
/// graph of the thing it is testing.
async fn request(port: u16, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut socket = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the management API is listening");
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    // The session and the CSRF header on every request, including the reads. §3.3 requires
    // `X-MW-Request` on state-changing methods only, and sending it on a `GET` as well keeps
    // this helper to one shape -- the gate ignores it there.
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Cookie: {}\r\nX-MW-Request: 1\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        session_cookie(),
        payload.len()
    );
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
    // The body is after the blank line. Chunked encoding is not in play: `axum` sets a
    // `Content-Length` for a `Json` body, and nothing here streams.
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .trim();
    let parsed = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {body}"))
    };
    (status, parsed)
}

/// The ids of every component whose **running instance** was built from `generation`.
///
/// This is §2.2's comparison, read off the wire: `applied_generation` is the generation the
/// live instance was built from, so the set that equals the newest generation is exactly the
/// set that has been rebuilt since.
async fn built_from(port: u16, generation: u64) -> Vec<String> {
    let (status, body) = request(port, "GET", "/api/v1/runtime/components", None).await;
    assert_eq!(status, 200, "{body}");
    let mut ids: Vec<String> = body["components"]
        .as_array()
        .expect("the components resource is a list")
        .iter()
        .filter(|c| c["applied_generation"].as_u64() == Some(generation))
        .map(|c| c["id"].as_str().expect("an id").to_string())
        .collect();
    ids.sort();
    ids
}

/// Poll until nothing is still catching up with the newest generation.
///
/// The polling is the *test's*, not a screen's: W5 **D-46** makes the only client-side timer
/// a countdown over a server-supplied timestamp, and §2.4's banner is driven by the SSE
/// channel M7-3 adds. Here it is just how a test waits for the supervisor to converge.
async fn settle(port: u16) -> u64 {
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        let (status, body) = request(port, "GET", "/api/v1/runtime/summary", None).await;
        assert_eq!(status, 200, "{body}");
        if body["pending"].as_u64() == Some(0) {
            return body["generation"].as_u64().expect("a generation");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the supervisor never caught up: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn ids(diff: &Value, bucket: &str) -> Vec<String> {
    let mut v: Vec<String> = diff[bucket]
        .as_array()
        .unwrap_or_else(|| panic!("`{bucket}` is a list in {diff}"))
        .iter()
        .map(|n| n["id"].as_str().expect("an id").to_string())
        .collect();
    v.sort();
    v
}

/// **The done-criterion.**
///
/// A preview of a website edit, the same edit saved, and the set of components that were
/// actually rebuilt afterwards. All three have to be the same set, and the third is read
/// from `applied_generation` rather than from anything the save said about itself.
///
/// The edit is a **page title**, chosen because it is in no shared root state's inputs: it
/// restarts the components that render it and must leave everything else alone, which is the
/// half of W2a Decision 25 that "saving a website no longer disturbs the APRS schedule" is
/// about. A station or website being *added* is a different case, and the test below covers
/// that one separately.
#[tokio::test]
async fn a_saves_preview_names_exactly_the_components_that_then_restart() {
    let scratch = Scratch::new("preview");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    // The website as it is, so the edit changes one field and nothing else.
    let (status, website) = request(mgmt, "GET", "/api/v1/websites/1", None).await;
    assert_eq!(status, 200, "{website}");
    assert_eq!(website["page_title"], "Backyard Weather");

    let edited = json!({
        "name": website["name"],
        "hostname": website["hostname"],
        "page_title": "Weather at the Back Yard",
        "about_station_html": website["about_station_html"],
        "primary_station_key": website["primary_station_key"],
        "snow_enabled": website["snow_enabled"],
        "air_quality_enabled": website["air_quality_enabled"],
        "is_portal": website["is_portal"],
        "apple_app_id": website["apple_app_id"],
        "is_default_tls": false,
    });

    // ① The preview, which is what §2.4 puts behind the Save button.
    let (status, preview) = request(
        mgmt,
        "POST",
        "/api/v1/config/preview",
        Some(json!({"change": "put_website", "id": 1, "website": edited})),
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["committed"], false, "a preview commits nothing");
    assert!(preview["valid"].as_bool().unwrap_or(false));
    let predicted_restart = ids(&preview["diff"], "restart");
    let predicted_start = ids(&preview["diff"], "start");
    assert!(
        !predicted_restart.is_empty(),
        "a page-title change should restart what renders it: {preview}"
    );
    assert!(
        preview["diff"]["unchanged"].as_u64().unwrap_or(0) > 0,
        "something has to be left alone, or the preview says nothing: {preview}"
    );

    // A preview really is a dry run: the stored configuration is untouched.
    let (_, unchanged) = request(mgmt, "GET", "/api/v1/websites/1", None).await;
    assert_eq!(unchanged["page_title"], "Backyard Weather");

    // ② The save, through the ordinary resource route rather than through the preview.
    let (status, saved) = request(mgmt, "PUT", "/api/v1/websites/1", Some(edited)).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["committed"], true);
    let generation = saved["generation"].as_u64().expect("a generation");

    assert_eq!(
        ids(&saved["diff"], "restart"),
        predicted_restart,
        "the save restarted a different set from the one the preview named"
    );
    assert_eq!(ids(&saved["diff"], "start"), predicted_start);

    // ③ What was actually rebuilt, read from `applied_generation`.
    let settled = settle(mgmt).await;
    assert_eq!(settled, generation, "the generation moved under the test");
    let mut expected: Vec<String> = predicted_start
        .iter()
        .chain(predicted_restart.iter())
        .cloned()
        .collect();
    expected.sort();
    assert_eq!(
        built_from(mgmt, generation).await,
        expected,
        "the components built from generation {generation} are not the ones the preview named"
    );

    // And the edit is live, which is the other half of the claim.
    let (_, after) = request(mgmt, "GET", "/api/v1/websites/1", None).await;
    assert_eq!(after["page_title"], "Weather at the Back Yard");

    terminate(&mut child).await;
}

/// Adding a station restarts **everything**, and the preview says so in advance.
///
/// This is the cost `crate::boot::Slots` records, asserted rather than left in a comment: the
/// rollup state's and the snapshot table's key sets are fixed at construction, so a new
/// station means new ones -- and every component that holds either has to be rebuilt, or two
/// writers would be writing two states. The operator is not surprised by it, because the same
/// preview that reports a one-component restart for a page title reports the whole plan here.
#[tokio::test]
async fn adding_a_station_restarts_the_whole_plan_and_the_preview_says_so() {
    let scratch = Scratch::new("epoch");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let before = request(mgmt, "GET", "/api/v1/runtime/components", None)
        .await
        .1["components"]
        .as_array()
        .expect("a list")
        .len();

    // Not collecting, for `configure`'s reason: the assertion is about the plan diff, and a
    // station that cannot reach its datalogger would never report an applied generation.
    let station = json!({
        "key": "summit",
        "name": "Summit",
        "kind": "campbellscientific",
        "collecting": false,
        "identity_kind": "fixed",
        "latitude": 39.6,
        "longitude": -105.2,
        "timezone": "America/Denver",
        "transport": {"kind": "tcp_out", "host": "127.0.0.1", "port": 7002},
    });

    let (status, preview) = request(
        mgmt,
        "POST",
        "/api/v1/config/preview",
        Some(json!({"change": "put_station", "station": station})),
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(
        preview["diff"]["unchanged"].as_u64(),
        Some(0),
        "a new station changes the shared root state, so nothing is left untouched: {preview}"
    );
    assert_eq!(
        ids(&preview["diff"], "restart").len(),
        before,
        "every component that existed has to be rebuilt: {preview}"
    );

    let (status, saved) = request(mgmt, "POST", "/api/v1/stations", Some(station)).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(
        ids(&saved["diff"], "restart"),
        ids(&preview["diff"], "restart")
    );
    let generation = saved["generation"].as_u64().expect("a generation");

    let settled = settle(mgmt).await;
    assert_eq!(settled, generation);
    let mut expected: Vec<String> = ids(&saved["diff"], "start")
        .into_iter()
        .chain(ids(&saved["diff"], "restart"))
        .collect();
    expected.sort();
    assert_eq!(built_from(mgmt, generation).await, expected);

    terminate(&mut child).await;
}

/// **An abandoned preview leaves nothing behind.**
///
/// The one the code review found: `assemble` rebuilds the shared root state and bumps the
/// epoch when the proposed configuration's fingerprint differs, and a preview rolls the
/// transaction back but used to keep that. The next save then compared against a
/// fingerprint no committed configuration ever had and restarted the **entire plan** for an
/// unrelated edit — emptying the snapshot table and the rollup windows, which is the defect
/// the reuse mechanism exists to prevent, reached through a button that commits nothing.
#[tokio::test]
async fn an_abandoned_preview_does_not_disturb_the_next_save() {
    let scratch = Scratch::new("abandon");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    // ① Preview a change that *would* rebuild the shared state — then abandon it.
    let (status, preview) = request(
        mgmt,
        "POST",
        "/api/v1/config/preview",
        Some(json!({"change": "put_station", "station": {
            "key": "summit",
            "name": "Summit",
            "kind": "campbellscientific",
            "collecting": false,
            "identity_kind": "fixed",
            "timezone": "America/Denver",
            "transport": {"kind": "tcp_out", "host": "127.0.0.1", "port": 7002},
        }})),
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["diff"]["unchanged"].as_u64(), Some(0));

    // ② Now save something that touches one component: a page title.
    let (_, website) = request(mgmt, "GET", "/api/v1/websites/1", None).await;
    let (status, saved) = request(
        mgmt,
        "PUT",
        "/api/v1/websites/1",
        Some(json!({
            "name": website["name"],
            "hostname": website["hostname"],
            "page_title": "Something Else",
            "about_station_html": "",
            "primary_station_key": website["primary_station_key"],
            "snow_enabled": false,
            "air_quality_enabled": false,
            "is_portal": false,
            "apple_app_id": "",
            "is_default_tls": false,
        })),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    assert!(
        saved["diff"]["unchanged"].as_u64().unwrap_or(0) > 0,
        "the abandoned preview restarted the whole plan: {saved}"
    );

    terminate(&mut child).await;
}

/// Rotating a station's protocol secret takes effect **without a restart**.
///
/// `mw_station::config` declines to restart a station for a secret rotation on the grounds
/// that the registry is what compares the secret — so the registry has to actually be
/// republished, or a revoked secret keeps authenticating uploads until the daemon restarts.
#[tokio::test]
async fn rotating_a_protocol_secret_restarts_nothing_and_still_takes_effect() {
    let scratch = Scratch::new("secret");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (_, before) = request(mgmt, "GET", "/api/v1/stations/backyard", None).await;
    assert_eq!(before["protocol_secret"]["is_set"], false);

    let (status, saved) = request(
        mgmt,
        "PUT",
        "/api/v1/stations/backyard",
        Some(json!({
            "name": "Backyard",
            "kind": "campbellscientific",
            "collecting": false,
            "identity_kind": "fixed",
            "latitude": 39.5,
            "longitude": -105.1,
            "timezone": "America/Denver",
            "protocol_secret": "a-new-secret",
            "transport": {"kind": "tcp_out", "host": "127.0.0.1", "port": 7001},
        })),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    // Nothing restarts: the secret is not in any component's digest, by design.
    assert_eq!(
        ids(&saved["diff"], "restart"),
        Vec::<String>::new(),
        "{saved}"
    );
    assert!(saved["diff"]["unchanged"].as_u64().unwrap_or(0) > 0);

    let (_, after) = request(mgmt, "GET", "/api/v1/stations/backyard", None).await;
    assert_eq!(after["protocol_secret"]["is_set"], true);

    terminate(&mut child).await;
}

/// A station's APRS callsign survives a read-modify-write.
///
/// It is the one non-credential column a form has to prefill: `StationView` used to omit it,
/// so a client that GET-ed a station and PUT it back wrote `NULL` over the callsign — and
/// `plan()` then refuses the whole configuration while APRS is paired, which makes the
/// station uneditable.
#[tokio::test]
async fn a_round_trip_keeps_the_aprs_callsign() {
    let scratch = Scratch::new("callsign");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let pool = mw_store::config_db::open_and_migrate(&config)
        .await
        .expect("the configuration opens");
    sqlx::query("UPDATE stations SET aprs_callsign = 'W1AW' WHERE station_key = 'backyard'")
        .execute(&pool)
        .await
        .expect("the callsign is set");
    pool.close().await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (_, station) = request(mgmt, "GET", "/api/v1/stations/backyard", None).await;
    assert_eq!(station["aprs_callsign"], "W1AW");

    // What a form does: prefill from the view, change one field, send the result. The
    // credentials are *not* carried back — they are `{"is_set": …}` on the view and a new
    // value on the input, which is the whole point — but the callsign is, and it has to be
    // there to be carried.
    let (status, saved) = request(
        mgmt,
        "PUT",
        "/api/v1/stations/backyard",
        Some(json!({
            "name": "Backyard II",
            "kind": station["kind"],
            "collecting": station["collecting"],
            "identity_kind": station["identity_kind"],
            "latitude": station["latitude"],
            "longitude": station["longitude"],
            "timezone": station["timezone"],
            "aprs_callsign": station["aprs_callsign"],
            "wind_dir_correction_degrees": station["wind_dir_correction_degrees"],
            "transport": {
                "kind": station["transport"]["kind"],
                "host": station["transport"]["host"],
                "port": station["transport"]["port"],
            },
        })),
    )
    .await;
    assert_eq!(status, 200, "{saved}");

    let (_, after) = request(mgmt, "GET", "/api/v1/stations/backyard", None).await;
    assert_eq!(after["aprs_callsign"], "W1AW", "the callsign was erased");
    assert_eq!(after["name"], "Backyard II");

    // And a body whose *shape* is wrong lands on its field rather than on `axum`'s
    // plain-text 400 — §8.5's "an error the client did not predict looks identical to one
    // it did". Posting the view's `protocol_secret` object back is the natural way to get
    // one.
    let (status, refused) = request(
        mgmt,
        "PUT",
        "/api/v1/stations/backyard",
        Some(json!({
            "name": "Backyard",
            "kind": "campbellscientific",
            "identity_kind": "fixed",
            "protocol_secret": {"is_set": true},
        })),
    )
    .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(
        refused["errors"][0]["field"], "protocol_secret",
        "{refused}"
    );

    terminate(&mut child).await;
}

/// A service the operator switched off is still readable, and says it is off.
///
/// The registrar reads only the enabled uploaders, because a disabled one is not a
/// component. A **page** for a disabled service is still a page: without this the toggle is
/// write-only and the page 404s the moment it is used.
#[tokio::test]
async fn a_switched_off_service_is_still_readable() {
    let scratch = Scratch::new("off");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let pool = mw_store::config_db::open_and_migrate(&config)
        .await
        .expect("the configuration opens");
    sqlx::query(
        "INSERT INTO uploader (kind, name, enabled, api_key) \
         VALUES ('pwsweather', 'backyard', 0, 'k')",
    )
    .execute(&pool)
    .await
    .expect("the disabled uploader is insertable");
    pool.close().await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (status, service) = request(mgmt, "GET", "/api/v1/services/pwsweather", None).await;
    assert_eq!(status, 200, "{service}");
    assert_eq!(service["enabled"], false);
    assert_eq!(service["api_key"]["is_set"], true);

    terminate(&mut child).await;
}

/// A refusal lands on the **field** that caused it, which is §7.2's rule and the thing a
/// banner cannot do.
///
/// Also the one assertion that the layering holds end to end: the message is the operator's
/// sentence from `mw_api_mgmt::change`, not SQLite's constraint name -- so the field-level
/// pass really does run before the transaction.
#[tokio::test]
async fn a_bad_value_is_refused_on_its_own_field() {
    let scratch = Scratch::new("invalid");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (status, body) = request(
        mgmt,
        "PUT",
        "/api/v1/websites/1",
        Some(json!({
            "name": "Home",
            "hostname": "Weather.Example.Com",
            "page_title": "x",
            "about_station_html": "",
            "snow_enabled": true,
        })),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let fields: Vec<&str> = body["errors"]
        .as_array()
        .expect("field errors")
        .iter()
        .map(|e| e["field"].as_str().expect("a field"))
        .collect();
    assert!(fields.contains(&"hostname"), "{body}");
    assert!(fields.contains(&"snow_station_key"), "{body}");
    for e in body["errors"].as_array().expect("field errors") {
        let message = e["message"].as_str().expect("a message");
        assert!(
            message.ends_with('.') && !message.contains("CHECK"),
            "`{message}` is not a sentence an operator can act on"
        );
    }

    // Nothing was written, which is the half a `422` has to guarantee.
    let (_, website) = request(mgmt, "GET", "/api/v1/websites/1", None).await;
    assert_eq!(website["page_title"], "Backyard Weather");

    terminate(&mut child).await;
}

/// A `PUT` to a website id nothing has is a `404`, and does **not** create one.
///
/// Pinned because writing a website by id is an upsert in the store -- one-step-back has to
/// put a deleted row back under the id the frozen contract returns -- so the refusal lives
/// in the route, which is exactly the kind of check that quietly stops being there.
#[tokio::test]
async fn a_put_to_an_unknown_website_does_not_create_one() {
    let scratch = Scratch::new("unknown");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (status, body) = request(
        mgmt,
        "PUT",
        "/api/v1/websites/77",
        Some(json!({"name": "Invented", "hostname": "invented.example.com"})),
    )
    .await;
    assert_eq!(status, 404, "{body}");

    let (_, websites) = request(mgmt, "GET", "/api/v1/websites", None).await;
    let ids: Vec<i64> = websites["websites"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|w| w["id"].as_i64().expect("an id"))
        .collect();
    assert_eq!(ids, [1], "a refused PUT invented a website");

    terminate(&mut child).await;
}

/// **Undo save** puts the previous configuration back, and only the one step it kept.
#[tokio::test]
async fn one_step_back_restores_the_previous_configuration() {
    let scratch = Scratch::new("revert");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (_, before) = request(mgmt, "GET", "/api/v1/config", None).await;
    let was = before["generation"].as_u64().expect("a generation");

    let (status, saved) = request(
        mgmt,
        "PUT",
        "/api/v1/stations/backyard",
        Some(json!({
            "name": "Back Yard",
            "kind": "campbellscientific",
            "collecting": false,
            "identity_kind": "fixed",
            "latitude": 39.5,
            "longitude": -105.1,
            "altitude_m": 2100.0,
            "timezone": "America/Denver",
            "transport": {"kind": "tcp_out", "host": "127.0.0.1", "port": 7001},
        })),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let (_, edited) = request(mgmt, "GET", "/api/v1/stations/backyard", None).await;
    assert_eq!(edited["name"], "Back Yard");
    assert_eq!(edited["altitude_m"], 2100.0);

    // A generation that is not the kept step is refused, and does not spend it.
    let (status, refused) = request(
        mgmt,
        "POST",
        "/api/v1/config/revert",
        Some(json!({"to_generation": 999})),
    )
    .await;
    assert_eq!(status, 409, "{refused}");

    let (status, reverted) = request(
        mgmt,
        "POST",
        "/api/v1/config/revert",
        Some(json!({"to_generation": was})),
    )
    .await;
    assert_eq!(status, 200, "{reverted}");
    let (_, back) = request(mgmt, "GET", "/api/v1/stations/backyard", None).await;
    assert_eq!(back["name"], "Backyard");
    assert_eq!(
        back.get("altitude_m"),
        None,
        "the altitude was not set before"
    );

    // One step, and only one: the step is spent.
    let (status, exhausted) = request(
        mgmt,
        "POST",
        "/api/v1/config/revert",
        Some(json!({"to_generation": was})),
    )
    .await;
    assert_eq!(status, 409, "{exhausted}");

    terminate(&mut child).await;
}

/// Changing the management port actually moves the socket.
///
/// The claim being pinned is that the management address is part of `http:listener`'s
/// `ConfigDigest`: without it, the one configuration value a save could not apply would be
/// the port the save was made on. W5 §1.4's "changing the port shows an explicit warning and
/// the new URL" is only honest if the new URL is where the API then is.
#[tokio::test]
async fn changing_the_management_port_moves_the_socket() {
    let scratch = Scratch::new("port");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    // A fourth port, from a second probe: the daemon is about to be told to move onto it.
    let (_, _, moved) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;
    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    let (status, listener) = request(mgmt, "GET", "/api/v1/website-server", None).await;
    assert_eq!(status, 200, "{listener}");
    assert_eq!(listener["management_port"].as_u64(), Some(u64::from(mgmt)));

    let (status, saved) = request(
        mgmt,
        "PUT",
        "/api/v1/website-server",
        Some(json!({
            "listen_addr": "127.0.0.1",
            "http_port": http,
            "grpc_port": grpc,
            "management_listen_addr": "127.0.0.1",
            "management_port": moved,
        })),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    // The listener is in the restart list, which is the preview telling the operator that the
    // thing they are talking to is about to move.
    assert!(
        ids(&saved["diff"], "restart").contains(&"http:listener".to_string()),
        "{saved}"
    );

    // The new socket answers. Polled, because the old one is draining while the new one binds
    // and the save's response necessarily arrives on the old one.
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        if let Ok(socket) = TcpStream::connect(("127.0.0.1", moved)).await {
            drop(socket);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the management API never appeared on {moved}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, listener) = request(moved, "GET", "/api/v1/website-server", None).await;
    assert_eq!(status, 200, "{listener}");
    assert_eq!(listener["management_port"].as_u64(), Some(u64::from(moved)));

    terminate(&mut child).await;
}

/// Every secret is **omitted**, on every route that could carry one.
///
/// Asserted over the whole `GET /api/v1/config` body rather than field by field, because the
/// property is about the surface and not about a field somebody remembered: a credential
/// that reached a response would be a value a client could round-trip back into a `PUT`.
#[tokio::test]
async fn no_response_carries_a_credential() {
    let scratch = Scratch::new("secrets");
    let config = scratch.path("config.db");
    let notify = scratch.path("notify.sock");
    let (http, grpc, mgmt) = free_ports().await;
    configure(&config, http, grpc, mgmt).await;

    // A credential of each kind, planted directly so the API has something to leak.
    let pool = mw_store::config_db::open_and_migrate(&config)
        .await
        .expect("the configuration opens");
    sqlx::query(
        "INSERT INTO weather_database (id, url) \
         VALUES (1, 'postgres://mw:tr0ub4dor@127.0.0.1:5432/weather')",
    )
    .execute(&pool)
    .await
    .expect("the weather database row is insertable");
    sqlx::query(
        "INSERT INTO uploader (kind, name, enabled, api_key) \
         VALUES ('wunderground', 'backyard', 1, 'correct-horse-battery')",
    )
    .execute(&pool)
    .await
    .expect("the uploader row is insertable");
    sqlx::query(
        "INSERT INTO station_service (station_key, service, uploader_id, remote_station_id) \
         SELECT 'backyard', 'wunderground', id, 'KCOTEST1' FROM uploader WHERE kind = \
         'wunderground'",
    )
    .execute(&pool)
    .await
    .expect("the pairing row is insertable");
    // A second station, because `transport_auth` belongs to an `http_out` transport and
    // nothing else -- the column's own `CHECK` says so, which is the schema refusing to hold
    // a credential for a transport that would never read it.
    sqlx::query(
        "INSERT INTO stations (station_key, owning_node_id, name, type, enabled, \
         identity_kind, timezone, transport_kind, transport_url, transport_auth) \
         VALUES ('polled', ?1, 'Polled', 'airgradient', 0, 'fixed', 'UTC', 'http_out', \
         'http://127.0.0.1:7003/measures', 'hunter2')",
    )
    .bind(vec![7u8; 16])
    .execute(&pool)
    .await
    .expect("the polled station row is insertable");
    pool.close().await;

    let socket = notify_socket(&notify);
    let mut child = spawn(&config, &notify);
    wait_ready(&socket).await;

    for path in [
        "/api/v1/config",
        "/api/v1/stations",
        "/api/v1/stations/backyard",
        "/api/v1/stations/polled",
        "/api/v1/services",
        "/api/v1/services/wunderground",
        "/api/v1/storage",
    ] {
        let (status, body) = request(mgmt, "GET", path, None).await;
        assert_eq!(status, 200, "{path}: {body}");
        let rendered = body.to_string();
        for secret in ["tr0ub4dor", "correct-horse-battery", "hunter2"] {
            assert!(
                !rendered.contains(secret),
                "{path} carried a credential: {rendered}"
            );
        }
        // Omitted, not redacted: a placeholder is a value a client can send back.
        assert!(
            !rendered.contains("***"),
            "{path} redacted rather than omitted"
        );
    }

    // And it says *whether* one is stored, which is the thing the form needs.
    let (_, service) = request(mgmt, "GET", "/api/v1/services/wunderground", None).await;
    assert_eq!(service["api_key"]["is_set"], true);
    let (_, station) = request(mgmt, "GET", "/api/v1/stations/polled", None).await;
    assert_eq!(station["transport"]["credential"]["is_set"], true);

    terminate(&mut child).await;
}
