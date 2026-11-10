//! **N sockets, one router** (§6.1, **D-31** as amended by **A-W3-4**).
//!
//! | Socket | When it exists | What it serves |
//! |---|---|---|
//! | the main port | always | the one router; TLS when `https_port` is configured |
//! | the plaintext port | only when `https_port` is | a redirect to the main port |
//! | the gRPC port | always | the same router |
//!
//! > The unit that is singular is the router and the route table, not the file descriptor.
//!
//! **The gRPC port keeps its own socket, and that is not a concession** (**W3 D-92**).
//! `grpc_port` is a real configuration column with a default of 50051 and the schema
//! validates that it differs from the HTTP, HTTPS and management ports -- so a separate
//! socket is the configured, validated, shipped behaviour, and folding it onto 443 would
//! break every machine-to-machine peer at upgrade. It serves the **same** `Router`, so
//! there is still exactly one route table.
//!
//! **No gRPC *service* is mounted on it yet**, and the reason is that nothing provides one:
//! `grpc:weatherv1`, `grpc:weatherappsv1` and `station:grpcrx` are implementation-plan.md
//! M6-1 and M6-4. What this item owes them is the socket and a serving path that accepts
//! HTTP/2 -- `hyper-util`'s auto connection builder, which `axum::serve` uses, accepts
//! HTTP/1.1 and HTTP/2-with-prior-knowledge on the same plaintext socket, so a peer that
//! speaks h2c works on both the main and the gRPC port today (§6.6). Mounting a
//! `tonic` service is one `tower::steer::Steer` over a `content-type: application/grpc`
//! test in the fallback, and it is deliberately not written here: a dispatcher with nothing
//! to dispatch to cannot be tested, and a gate that cannot fail is worth nothing.
//!
//! # Binding and serving are separate calls
//!
//! [`bind`] holds every port before anything is served, so a port another process holds is
//! an error the caller sees *before* it tells the service manager anything. §3.5's `READY=1`
//! means the supervisor has converged; a readiness notification sent while a socket might
//! still fail to bind would be the `Type=simple` problem with extra steps.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use axum::Router;
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::warn;

use crate::sni::SniResolver;
use crate::HttpError;

/// Where the listener binds, as the `listener` row holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listen {
    /// `listener.listen_addr`, already parsed.
    pub addr: std::net::IpAddr,
    /// `listener.http_port`.
    pub http_port: u16,
    /// `listener.https_port`. `Some` makes the main socket TLS and `http_port` a redirect.
    pub https_port: Option<u16>,
    /// `listener.grpc_port`, which always has a value (the column is `NOT NULL DEFAULT
    /// 50051`).
    pub grpc_port: u16,
}

impl Listen {
    /// The main socket's address: the TLS port when one is configured, else the HTTP port.
    pub fn main(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.https_port.unwrap_or(self.http_port))
    }

    /// The redirecting plaintext socket's address, when there is one.
    pub fn redirect(&self) -> Option<SocketAddr> {
        self.https_port
            .map(|_| SocketAddr::new(self.addr, self.http_port))
    }

    /// The legacy gRPC socket's address.
    pub fn grpc(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.grpc_port)
    }

    /// Whether the main socket is TLS.
    pub fn tls(&self) -> bool {
        self.https_port.is_some()
    }
}

/// Every bound socket, not yet serving.
pub struct Bound {
    main: Either,
    redirect: Option<(TcpListener, SocketAddr)>,
    grpc: (TcpListener, SocketAddr),
    router: Router,
    redirect_to: Option<u16>,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound")
            .field("main", &self.main.addr())
            .field("tls", &matches!(self.main, Either::Tls { .. }))
            .field("redirect", &self.redirect.as_ref().map(|(_, a)| *a))
            .field("grpc", &self.grpc.1)
            .finish_non_exhaustive()
    }
}

enum Either {
    Plain(TcpListener, SocketAddr),
    Tls {
        listener: TcpListener,
        addr: SocketAddr,
        acceptor: TlsAcceptor,
    },
}

impl Either {
    fn addr(&self) -> SocketAddr {
        match self {
            Either::Plain(_, addr) => *addr,
            Either::Tls { addr, .. } => *addr,
        }
    }
}

/// ALPN, matching the Go server's `NextProtos` exactly (§6.6).
///
/// gRPC is HTTP/2 with `content-type: application/grpc`, so no byte sniffing and no second
/// TLS context is needed; `hyper-util`'s auto builder also detects the HTTP/2 preface, so
/// the two agree on a plaintext socket where there is no ALPN at all.
const ALPN: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// How long in-flight requests get to finish once shutdown has been signalled.
///
/// §3.6's default grace, and for the same reason: `TimeoutStopSec=30` in W8's unit leaves
/// room for it, and the daemon's own done-criterion is that it exits inside ten seconds.
///
/// **It has to be bounded, and that is not a precaution.** `with_graceful_shutdown` waits
/// for every connection with an unfinished request and nothing else -- so one client that
/// sent a request line and no blank line holds the drain open for as long as it likes, and
/// `/metrics` is on the unauthenticated main port. Without a deadline, a stalled TCP peer
/// or a scraper that died mid-request is enough to keep the process alive until systemd
/// reaches `TimeoutStopSec` and sends SIGKILL -- or, outside systemd, indefinitely.
pub const DRAIN: Duration = Duration::from_secs(10);

/// Bind every socket the configuration names.
///
/// `resolver` is required exactly when `listen.https_port` is set; a TLS port with no
/// resolver -- or a resolver that can answer no handshake -- is a configuration that cannot
/// serve anything, which is [`HttpError::NoCertificate`] rather than a listener that accepts
/// connections and fails every one of them.
pub async fn bind(
    listen: &Listen,
    router: Router,
    resolver: Option<Arc<SniResolver>>,
) -> Result<Bound, HttpError> {
    let main = match listen.https_port {
        Some(_) => {
            let resolver = resolver.ok_or(HttpError::NoCertificate {
                addr: listen.main(),
            })?;
            if resolver.is_empty() {
                return Err(HttpError::NoCertificate {
                    addr: listen.main(),
                });
            }
            let mut config = ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(resolver);
            config.alpn_protocols = ALPN.iter().map(|p| p.to_vec()).collect();
            let (listener, addr) = listen_on(listen.main()).await?;
            Either::Tls {
                listener,
                addr,
                acceptor: TlsAcceptor::from(Arc::new(config)),
            }
        }
        None => {
            let (listener, addr) = listen_on(listen.main()).await?;
            Either::Plain(listener, addr)
        }
    };
    let main_addr = main.addr();
    let redirect = match listen.redirect() {
        Some(addr) => Some(listen_on(addr).await?),
        None => None,
    };
    let grpc = listen_on(listen.grpc()).await?;
    Ok(Bound {
        main,
        redirect,
        grpc,
        router,
        // **The port actually bound, not the configured one.** `listener.https_port` can be
        // `0`, which asks the kernel to choose -- and a `Location` naming port 0 is a
        // redirect to nowhere. Reading it back from the socket is the same reason
        // `local_addr` is read back rather than echoed.
        redirect_to: listen.https_port.map(|_| main_addr.port()),
    })
}

async fn listen_on(addr: SocketAddr) -> Result<(TcpListener, SocketAddr), HttpError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| HttpError::Bind { addr, source })?;
    // `0.0.0.0:0` is a legitimate request and the kernel chooses the port, so the bound
    // address is read back rather than echoed.
    let bound = listener
        .local_addr()
        .map_err(|source| HttpError::Bind { addr, source })?;
    Ok((listener, bound))
}

impl Bound {
    /// The main socket's address, which is how a caller that asked for port 0 finds out
    /// what it got.
    pub fn local_addr(&self) -> SocketAddr {
        self.main.addr()
    }

    /// The redirecting socket's address, when there is one.
    pub fn redirect_addr(&self) -> Option<SocketAddr> {
        self.redirect.as_ref().map(|(_, addr)| *addr)
    }

    /// The gRPC socket's address.
    pub fn grpc_addr(&self) -> SocketAddr {
        self.grpc.1
    }

    /// Serve every socket until `shutdown` completes, then drain for at most [`DRAIN`].
    pub async fn serve<F>(self, shutdown: F) -> Result<(), HttpError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.serve_with_drain(shutdown, DRAIN).await
    }

    /// [`Bound::serve`] with the drain deadline chosen by the caller, for a test that does
    /// not want to wait ten seconds to prove the deadline exists.
    ///
    /// **One shutdown path for N sockets**, which is §6.1's other half: the signal is
    /// observed once and fanned out to each `axum::serve`, and the drain deadline is
    /// started from the same observation rather than from three.
    pub async fn serve_with_drain<F>(self, shutdown: F, drain: Duration) -> Result<(), HttpError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let main_addr = self.main.addr();
        let grpc_addr = self.grpc.1;
        let redirect_addr = self.redirect.as_ref().map(|(_, a)| *a);
        // One observation of the signal, fanned out. `watch` rather than three
        // `oneshot`s: the number of sockets is configuration, and a channel per socket
        // would have to be built per shape.
        let (signalled, signal) = tokio::sync::watch::channel(false);
        let fanout = async move {
            shutdown.await;
            let _ = signalled.send(true);
        };
        let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
            // `changed` resolves on the send above; an `Err` means the sender is gone,
            // which is shutdown by another name.
            let _ = rx.changed().await;
        };

        let serve_main = {
            let router = self.router.clone();
            let rx = signal.clone();
            async move {
                match self.main {
                    Either::Plain(listener, addr) => {
                        serve_one(listener, router, addr, wait(rx)).await
                    }
                    Either::Tls {
                        listener,
                        addr,
                        acceptor,
                    } => {
                        serve_one(TlsListener { listener, acceptor }, router, addr, wait(rx)).await
                    }
                }
            }
        };
        let serve_grpc = {
            let router = self.router.clone();
            let rx = signal.clone();
            let (listener, addr) = self.grpc;
            async move { serve_one(listener, router, addr, wait(rx)).await }
        };
        let serve_redirect = {
            let rx = signal.clone();
            let redirect = self.redirect;
            let to = self.redirect_to;
            async move {
                match (redirect, to) {
                    (Some((listener, addr)), Some(port)) => {
                        serve_one(listener, redirector(port), addr, wait(rx)).await
                    }
                    _ => Ok(()),
                }
            }
        };
        let deadline = {
            let rx = signal.clone();
            async move {
                wait(rx).await;
                tokio::time::sleep(drain).await;
            }
        };

        tokio::select! {
            (main, grpc, redirect, ()) =
                async { tokio::join!(serve_main, serve_grpc, serve_redirect, fanout) } => {
                main.or(grpc).or(redirect)
            }
            () = deadline => {
                // Abandoning the drain is a `WARN` rather than an error: every component is
                // already stopped by the time this is reached, and a client that never
                // finished its request is not a reason to exit non-zero.
                warn!(
                    main = %main_addr,
                    grpc = %grpc_addr,
                    redirect = ?redirect_addr,
                    drain_s = drain.as_secs(),
                    "connections did not drain inside the deadline; closing the listeners"
                );
                Ok(())
            }
        }
    }
}

async fn serve_one<L, F>(
    listener: L,
    router: Router,
    addr: SocketAddr,
    shutdown: F,
) -> Result<(), HttpError>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|source| HttpError::Serve { addr, source })
}

/// The plaintext port's whole job when a TLS port is configured: send the client to it.
///
/// **New behaviour, and deliberately so.** The Go server has no plaintext port at all when
/// TLS is on -- `startRESTServer` makes the *one* port TLS (`controller.go:355`) -- so
/// nothing is being reproduced here and nothing is being broken: a client that reaches this
/// socket today reaches a closed port. `301` rather than `302` because the configuration,
/// not the request, decides it, and the path and query are preserved so a bookmark survives.
///
/// The port appears in the `Location` only when it is not 443, which is what a browser
/// would elide anyway and what keeps the common case's `Location` identical to the request
/// a user typed.
fn redirector(https_port: u16) -> Router {
    Router::new().fallback(move |req: Request<Body>| async move {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(crate::host::normalise)
            .unwrap_or_default();
        if host.is_empty() {
            // Nothing to redirect to. An HTTP/1.1 request without a `Host` is already
            // malformed, so this is the one case with no better answer than the listener's
            // own 404.
            return crate::not_found();
        }
        let tail = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        let location = if https_port == 443 {
            format!("https://{host}{tail}")
        } else {
            format!("https://{host}:{https_port}{tail}")
        };
        match HeaderValue::try_from(location) {
            Ok(value) => Response::builder()
                .status(StatusCode::MOVED_PERMANENTLY)
                .header(header::LOCATION, value)
                .body(Body::empty())
                .unwrap_or_else(|_| crate::not_found()),
            // A `Host` with a byte no header value may carry. `Uri`'s own parse would
            // reject it too; a 404 is the honest answer rather than a 500.
            Err(_) => crate::not_found(),
        }
    })
}

/// A `TcpListener` whose accepted streams complete a TLS handshake on first use.
///
/// **The handshake is not awaited inside `accept`, and that is the point.**
/// `axum::serve::Listener::accept` is called in series by the accept loop, so awaiting a
/// handshake there would let one stalled peer -- a client that opens a connection and sends
/// nothing -- block every subsequent connection to the port. [`LazyTls`] drives the
/// handshake from the first `poll_read`, which is inside the per-connection task, so a
/// stalled handshake costs that connection and nothing else.
struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = LazyTls;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => {
                    let acceptor = self.acceptor.clone();
                    return (
                        LazyTls {
                            state: State::Handshaking(Box::pin(async move {
                                acceptor.accept(stream).await
                            })),
                        },
                        addr,
                    );
                }
                // The accept loop must not exit on a per-connection error -- a descriptor
                // limit or a peer that reset between the SYN and the accept is not a reason
                // to stop listening. The pause is what stops a tight loop from burning a
                // core while the condition lasts; `axum`'s own `TcpListener` impl does the
                // same for the same reason.
                Err(e) => {
                    warn!(error = %e, "accept failed; retrying");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

type Handshake = Pin<Box<dyn Future<Output = io::Result<TlsStream<TcpStream>>> + Send>>;

enum State {
    Handshaking(Handshake),
    /// `Box`ed because a `TlsStream` is ~1.2 kB -- a rustls connection's record buffers --
    /// and the other two variants are a pointer and nothing. One allocation per accepted
    /// connection against 1.2 kB on every `LazyTls` the accept loop holds, which is
    /// `clippy::large_enum_variant`'s own trade and the right side of it here.
    Ready(Box<TlsStream<TcpStream>>),
    /// The handshake failed. Kept as a state rather than collapsing the connection, so that
    /// every subsequent poll returns the same error instead of panicking on a taken future.
    Failed,
}

/// A TLS stream whose handshake runs on first use. See [`TlsListener`].
pub struct LazyTls {
    state: State,
}

impl std::fmt::Debug for LazyTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state {
            State::Handshaking(_) => "handshaking",
            State::Ready(_) => "ready",
            State::Failed => "failed",
        };
        f.debug_struct("LazyTls").field("state", &state).finish()
    }
}

impl LazyTls {
    fn poll_stream(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<&mut TlsStream<TcpStream>>> {
        if let State::Handshaking(accept) = &mut self.state {
            match accept.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(stream)) => self.state = State::Ready(Box::new(stream)),
                Poll::Ready(Err(e)) => {
                    self.state = State::Failed;
                    return Poll::Ready(Err(e));
                }
            }
        }
        match &mut self.state {
            State::Ready(stream) => Poll::Ready(Ok(stream)),
            State::Failed => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the TLS handshake failed",
            ))),
            State::Handshaking(_) => {
                unreachable!("the handshake was polled to completion above")
            }
        }
    }
}

impl AsyncRead for LazyTls {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.poll_stream(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(stream)) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for LazyTls {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.poll_stream(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(stream)) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_stream(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(stream)) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            // Never handshook and never will: there is nothing to shut down, and driving
            // the handshake here would make a dropped connection wait for a peer.
            State::Handshaking(_) | State::Failed => Poll::Ready(Ok(())),
            State::Ready(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn listen(https: Option<u16>) -> Listen {
        Listen {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 8080,
            https_port: https,
            grpc_port: 50051,
        }
    }

    #[test]
    fn the_main_socket_is_the_tls_port_when_one_is_configured() {
        let l = listen(Some(8443));
        assert_eq!(l.main().port(), 8443);
        assert_eq!(l.redirect().expect("a redirect socket").port(), 8080);
        assert_eq!(l.grpc().port(), 50051);
        assert!(l.tls());
    }

    #[test]
    fn with_no_tls_port_there_is_no_redirect_socket() {
        let l = listen(None);
        assert_eq!(l.main().port(), 8080);
        assert_eq!(l.redirect(), None);
        assert!(!l.tls());
    }

    /// Two sockets with no TLS: the main port and the gRPC port, both serving the same
    /// router.
    #[tokio::test]
    async fn binding_holds_every_port_and_reads_each_one_back() {
        let l = Listen {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 0,
            https_port: None,
            grpc_port: 0,
        };
        let bound = bind(&l, Router::new().fallback(|| async { "ok" }), None)
            .await
            .expect("both ports bind");
        assert_ne!(bound.local_addr().port(), 0, "port 0 is read back");
        assert_ne!(bound.grpc_addr().port(), 0);
        assert_eq!(bound.redirect_addr(), None);
    }

    #[tokio::test]
    async fn a_tls_port_with_no_certificate_is_refused_at_bind() {
        let l = Listen {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 0,
            https_port: Some(0),
            grpc_port: 0,
        };
        let err = bind(&l, Router::new(), None)
            .await
            .expect_err("a TLS port with no resolver cannot serve anything");
        assert!(matches!(err, HttpError::NoCertificate { .. }), "{err}");
        let err = bind(
            &l,
            Router::new(),
            Some(Arc::new(SniResolver::new(crate::sni::CertStore::default()))),
        )
        .await
        .expect_err("an empty store would fail every handshake");
        assert!(matches!(err, HttpError::NoCertificate { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_port_already_held_names_the_address() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a free port");
        let addr = held.local_addr().expect("its address");
        let l = Listen {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: addr.port(),
            https_port: None,
            grpc_port: 0,
        };
        let err = bind(&l, Router::new(), None)
            .await
            .expect_err("the port is held");
        assert!(
            mw_types::DisplayChain(&err)
                .to_string()
                .contains(&addr.to_string()),
            "{}",
            mw_types::DisplayChain(&err)
        );
    }

    /// The redirector, driven directly: the path and query survive and the port is elided
    /// at 443.
    #[tokio::test]
    async fn the_redirect_preserves_the_path_and_query() {
        use tower::ServiceExt;
        let location = |port: u16, host: &str, uri: &str| {
            let router = redirector(port);
            let req = Request::builder()
                .uri(uri)
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("a legal request");
            async move {
                let resp = router.oneshot(req).await.expect("infallible");
                (
                    resp.status(),
                    resp.headers()
                        .get(header::LOCATION)
                        .map(|v| v.to_str().expect("ascii").to_string()),
                )
            }
        };
        assert_eq!(
            location(443, "example.com", "/span/24h?station=a").await,
            (
                StatusCode::MOVED_PERMANENTLY,
                Some("https://example.com/span/24h?station=a".to_string())
            )
        );
        assert_eq!(
            location(8443, "example.com:8080", "/").await,
            (
                StatusCode::MOVED_PERMANENTLY,
                Some("https://example.com:8443/".to_string())
            )
        );
    }

    #[tokio::test]
    async fn a_request_with_no_host_cannot_be_redirected() {
        use tower::ServiceExt;
        let resp = redirector(443)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("a legal request"),
            )
            .await
            .expect("infallible");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
