//! Per-website TLS over a real handshake: ALPN, the SNI resolver's chain, and **DEV-6**'s
//! refusal.
//!
//! The unit tests in `src/sni.rs` assert the store's chain over hand-built `CertifiedKey`s,
//! which is the right shape for the matching rules and says nothing about whether a client
//! can actually connect. This drives `tokio_rustls` on both ends over a loopback socket, so
//! the things only a handshake can show are shown:
//!
//! | What | Why a unit test cannot see it |
//! |---|---|
//! | the certificate loader | `rustls_pemfile` and `ring`'s key parse are real I/O and a real key |
//! | ALPN | negotiated in the handshake, and `h2` must be offered first to match Go's `NextProtos` |
//! | the **lazy** handshake | `LazyTls` drives it from the first `poll_read`, which is a code path no unit test enters |
//! | **DEV-6**'s refusal | a resolver returning `None` has to fail the connection, not serve a wrong certificate |
//! | the wildcard's one label | the same rule, against a client that really asked |
//!
//! The certificates are checked in; `tests/data/README.md` has why and how they were made.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use mw_http::{bind, CertPaths, CertStore, HostTable, Listen, Mount, SniResolver};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

fn paths() -> Vec<CertPaths> {
    vec![
        CertPaths {
            hostname: "exact.test".into(),
            cert: data("exact-cert.pem"),
            key: data("exact-key.pem"),
            is_default: false,
        },
        CertPaths {
            hostname: "*.wild.test".into(),
            cert: data("wild-cert.pem"),
            key: data("wild-key.pem"),
            is_default: false,
        },
    ]
}

/// A client that trusts the test CA and asks for one name.
///
/// Trusting the **CA** rather than the leaves is what makes a failure attributable: both
/// leaves are then acceptable to the client, so a failed handshake is the server refusing or
/// the name not matching, and never "the client did not know this certificate".
///
/// `h2` first, then `http/1.1`, which is what the server offers -- so the negotiated
/// protocol is the server's preference and asserting it means something.
fn client_config() -> ClientConfig {
    let mut roots = RootCertStore::empty();
    let pem = std::fs::read(data("ca-cert.pem")).expect("the checked-in CA is readable");
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots
            .add(cert.expect("a PEM certificate"))
            .expect("a usable root");
    }
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

/// One handshake, returning the stream or the error that stopped it.
async fn handshake(
    addr: SocketAddr,
    server_name: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config()));
    let stream = TcpStream::connect(addr).await?;
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|e| std::io::Error::other(format!("{server_name}: {e}")))?;
    connector.connect(name, stream).await
}

fn router() -> Router {
    let (mount, _) = Mount::new(
        vec![mw_http::metrics_table(mw_http::MetricsRoute::new(
            Arc::new(|| "mw_up 1\n".to_string()),
            "text/plain; version=0.0.4",
        ))],
        HostTable::default(),
    )
    .expect("the metrics table mounts");
    mount.router()
}

/// Bind a TLS listener on an ephemeral port over the two checked-in certificates.
async fn tls_listener(default: Option<&str>) -> (mw_http::Bound, SocketAddr) {
    let store = CertStore::load(&paths(), default).expect("the certificates load");
    let resolver = Arc::new(SniResolver::new(store));
    let listen = Listen {
        addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        http_port: 0,
        https_port: Some(0),
        grpc_port: 0,
    };
    let bound = bind(&listen, router(), Some(resolver))
        .await
        .expect("the TLS port binds");
    let addr = bound.local_addr();
    (bound, addr)
}

/// Serve `bound` for the duration of `client`, then stop.
async fn with_server<F, Fut, T>(bound: mw_http::Bound, client: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = bound.serve(async move {
        let _ = stopped.await;
    });
    let run = async move {
        let out = client().await;
        let _ = stop.send(());
        out
    };
    let (served, out) =
        tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(server, run) })
            .await
            .expect("the exchange finishes well inside the deadline");
    served.expect("the accept loop ends cleanly");
    out
}

/// Assert the **server refused** rather than that anything went wrong.
///
/// The distinction is the whole of **DEV-6**'s security half, and M4-3's code review found
/// the earlier `is_err()` assertions blind to it. There are two ways a handshake for an
/// uncovered name can fail, and only one of them is this design:
///
/// | Outcome | What the client sees | Which behaviour |
/// |---|---|---|
/// | the server refuses | `received fatal alert: AccessDenied` | **DEV-6** -- the resolver returned `None` |
/// | the server serves a wrong certificate | `invalid peer certificate: ... not valid for name ...` | the Go behaviour DEV-6 replaces |
///
/// The client here trusts the test CA, so both leaves are acceptable to it -- which is what
/// makes an `invalid peer certificate` message attributable to the *server* having presented
/// the wrong one rather than to a trust-store gap.
///
/// `access_denied` rather than `unrecognized_name` is rustls's choice for a resolver that
/// returns `None`; `src/sni.rs` and `docs/reconciliation.md`'s A-M4-3-4 have why.
fn assert_refused(
    outcome: std::io::Result<tokio_rustls::client::TlsStream<TcpStream>>,
    name: &str,
) {
    let err = match outcome {
        Ok(_) => panic!("{name}: the handshake succeeded; nothing covers that name"),
        Err(e) => e.to_string(),
    };
    assert!(
        !err.contains("invalid peer certificate"),
        "{name}: the server handed out a certificate for a name it does not cover, which is \
         the Go behaviour DEV-6 replaces -- the client is what rejected it: {err}"
    );
    assert!(
        err.contains("AccessDenied"),
        "{name}: expected the server's refusal alert, got: {err}"
    );
}

/// The happy path, end to end: the loader, the resolver, ALPN, and a request over TLS.
#[tokio::test]
async fn an_exact_name_handshakes_negotiates_h2_and_serves() {
    let (bound, addr) = tls_listener(None).await;
    with_server(bound, || async move {
        let stream = handshake(addr, "exact.test")
            .await
            .expect("the exact name resolves and the client trusts it");
        let (_, connection) = stream.get_ref();
        assert_eq!(
            connection.alpn_protocol(),
            Some(&b"h2"[..]),
            "`h2` first, matching the Go server's NextProtos (§6.6)"
        );
        // The certificate the server actually presented is the one keyed by the name.
        let presented: Vec<CertificateDer<'_>> = connection
            .peer_certificates()
            .expect("the server presented a chain")
            .to_vec();
        let expected = std::fs::read(data("exact-cert.pem")).expect("readable");
        let expected: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut expected.as_slice())
                .map(|c| c.expect("a PEM certificate"))
                .collect();
        assert_eq!(presented, expected, "the exact entry, not the wildcard");

        // **And a request, which is what covers `LazyTls`'s write half.** M4-3's code
        // review found that nothing in the suite served a byte over TLS, so
        // `poll_write`/`poll_flush` -- the half the lazy handshake has to drive on first use
        // -- was never entered. `http/1.1` rather than `h2` so the exchange is readable
        // without an h2 client; the ALPN assertion above is what covers the h2 offer.
        let mut plain = client_config();
        plain.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(plain));
        let tcp = TcpStream::connect(addr).await.expect("the port accepts");
        let mut stream = connector
            .connect(
                ServerName::try_from("exact.test".to_string()).expect("a legal name"),
                tcp,
            )
            .await
            .expect("the handshake completes");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: exact.test\r\nConnection: close\r\n\r\n")
            .await
            .expect("the request is written");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("the response is read to the close");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("mw_up 1\n"), "{response}");
    })
    .await;
}

/// **DEV-6**, over a real handshake: a wildcard covers exactly one label.
#[tokio::test]
async fn a_wildcard_covers_one_label_and_not_two() {
    let (bound, addr) = tls_listener(None).await;
    with_server(bound, || async move {
        let stream = handshake(addr, "a.wild.test")
            .await
            .expect("one label is covered");
        let (_, connection) = stream.get_ref();
        let wild = std::fs::read(data("wild-cert.pem")).expect("readable");
        let wild: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut wild.as_slice())
            .map(|c| c.expect("a PEM certificate"))
            .collect();
        assert_eq!(
            connection.peer_certificates().expect("a chain").to_vec(),
            wild
        );
        // Two labels deep, which the Go `strings.HasSuffix` check accepts.
        //
        // **`assert_refused`, not `is_err`.** M4-3's code review found that `is_err` is
        // satisfied by both outcomes: if the server handed out the wildcard certificate --
        // which is what Go does -- the *client* would reject it for the name, so the
        // assertion passed either way and the test guarding DEV-6's security half was not a
        // gate. Reverting `matches_one_label` to `strings.HasSuffix` left it green.
        assert_refused(handshake(addr, "a.b.wild.test").await, "a.b.wild.test");
        // And the name that merely ends with the suffix, which is the certificate the Go
        // check hands out for a domain it does not cover.
        assert_refused(handshake(addr, "evilwild.test").await, "evilwild.test");
    })
    .await;
}

/// The other half of **DEV-6**: a miss with no designated default **refuses** rather than
/// serving an arbitrary certificate.
///
/// What the Go version does instead is iterate a map and return the first certificate it
/// finds (`controller.go:826`) -- non-deterministic between runs, and a browser interstitial
/// that blames the user. The refusal's alert is `access_denied` rather than
/// `unrecognized_name`, which is rustls's choice; `src/sni.rs` has why.
#[tokio::test]
async fn a_miss_with_no_default_refuses_the_handshake() {
    let (bound, addr) = tls_listener(None).await;
    with_server(bound, || async move {
        assert_refused(handshake(addr, "nobody.test").await, "nobody.test");
    })
    .await;
}

/// And with a designated default, the same miss succeeds -- which is what makes the test
/// above a refusal rather than a broken listener.
#[tokio::test]
async fn a_miss_takes_the_designated_default() {
    let (bound, addr) = tls_listener(Some("exact.test")).await;
    with_server(bound, || async move {
        let stream = handshake(addr, "exact.test")
            .await
            .expect("the designated default is `exact.test`'s own certificate");
        drop(stream);
        // A client sending a name nothing covers now gets the default certificate. It does
        // not *trust* it for that name -- no client would -- so the handshake fails on the
        // client's side with a name mismatch, which is a different failure from the
        // refusal above and is the one this asserts.
        let err = handshake(addr, "nobody.test")
            .await
            .expect_err("the default certificate does not cover `nobody.test`");
        let text = err.to_string().to_ascii_lowercase();
        assert!(
            text.contains("name") || text.contains("certificate"),
            "the server served the default and the client rejected it for the name: {err}"
        );
    })
    .await;
}

/// The plaintext redirector, over a real socket: the other half of the N-socket shape when a
/// TLS port is configured (§6.1).
#[tokio::test]
async fn the_plaintext_port_redirects_to_the_tls_one() {
    let store = CertStore::load(&paths(), None).expect("the certificates load");
    let resolver = Arc::new(SniResolver::new(store));
    let listen = Listen {
        addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        http_port: 0,
        https_port: Some(0),
        grpc_port: 0,
    };
    // The redirector refuses a `Location` built from a hostname nothing serves, so the
    // host table has to name the one the request sends.
    let bound = mw_http::bind_with_hosts(
        &listen,
        router(),
        Some(resolver),
        Arc::new(HostTable::new([(
            "exact.test".to_string(),
            mw_types::WebsiteId::new(1),
        )])),
        // No ingest TLS: this test is about the main port's SNI chain, and the gRPC socket
        // stays plaintext exactly as it does on a node with no `config.ingest` row.
        None,
    )
    .await
    .expect("both ports bind");
    let tls_port = bound.local_addr().port();
    let plain = bound
        .redirect_addr()
        .expect("a TLS port means a redirecting plaintext port");
    with_server(bound, || async move {
        let mut socket = TcpStream::connect(plain).await.expect("the port accepts");
        socket
            .write_all(
                b"GET /metrics?x=1 HTTP/1.1\r\nHost: exact.test\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("the request is written");
        let mut response = String::new();
        socket
            .read_to_string(&mut response)
            .await
            .expect("the response is read");
        assert!(
            response.starts_with("HTTP/1.1 301 Moved Permanently"),
            "{response}"
        );
        assert!(
            response.contains(&format!(
                "location: https://exact.test:{tls_port}/metrics?x=1"
            )),
            "the path and query survive: {response}"
        );

        // And a hostname nothing is configured to serve is not reflected into a
        // `Location` -- M4-3's code review found this path not consulting the host table.
        let mut socket = TcpStream::connect(plain).await.expect("the port accepts");
        socket
            .write_all(b"GET / HTTP/1.1\r\nHost: attacker.test\r\nConnection: close\r\n\r\n")
            .await
            .expect("the request is written");
        let mut refused = String::new();
        socket
            .read_to_string(&mut refused)
            .await
            .expect("the response is read");
        assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
        assert!(!refused.contains("attacker.test"), "{refused}");
    })
    .await;
}
