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
        assert!(
            handshake(addr, "a.b.wild.test").await.is_err(),
            "the Go check matches any depth; this one does not"
        );
        // And the name that merely ends with the suffix, which is the certificate the Go
        // check hands out for a domain it does not cover.
        assert!(
            handshake(addr, "evilwild.test").await.is_err(),
            "`evilwild.test` must not get `*.wild.test`'s certificate"
        );
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
        let err = handshake(addr, "nobody.test")
            .await
            .expect_err("nothing resolves `nobody.test`");
        // The failure is the server's, not the client's trust decision: the client trusts
        // both checked-in certificates, so a presented one would have been accepted.
        assert!(
            !err.to_string()
                .to_ascii_lowercase()
                .contains("unknown issuer"),
            "the client rejected a certificate instead of the server refusing: {err}"
        );
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
    let bound = bind(&listen, router(), Some(resolver))
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
    })
    .await;
}
