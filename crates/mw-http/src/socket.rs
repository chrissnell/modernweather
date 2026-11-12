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
//! The services mounted on it are `station:grpcrx` (M6-1) and `ingest:grpc` (M6-4), as
//! ordinary `axum` routes on the shared router -- `hyper-util`'s auto connection builder,
//! which `axum::serve` uses, accepts HTTP/1.1 and HTTP/2-with-prior-knowledge on the same
//! plaintext socket, so a peer that speaks h2c works on both the main and the gRPC port
//! (§6.6) and no `tower::steer::Steer` dispatcher is needed.
//!
//! # The gRPC socket's own TLS, with client certificates (W7 §5.4 layer 1)
//!
//! The gRPC socket takes a **second** `ServerConfig` when one is supplied: the same
//! [`SniResolver`] for the certificate it presents, and a client-certificate verifier
//! beside it (W3 assumption 47). That is W7 **D-34**'s first admission layer and the only
//! one that can refuse a peer "before any frame is decoded" -- and it is why `ingest:grpc`
//! no longer needs `config.ingest.allow_insecure` to be set.
//!
//! **Who builds the verifier is deliberately not this crate.** The decision is per
//! `auth_kind` and the pinned case has to consult the `peers` snapshot, which is three
//! layers up; this crate takes an `Arc<ServerConfig>` and binds it. What that buys is that
//! `mw-http` still knows nothing about peers, and the whole of "who is allowed" lives in
//! one crate.
//!
//! **The peer's certificate chain reaches the request through the connection**, as
//! [`PeerCerts`] in `ConnectInfo`: the handshake runs lazily inside the per-connection task
//! (see [`LazyTls`]), so it has not happened when `accept` returns and the chain cannot be
//! captured there. A shared slot is filled by the handshake and read by the handler, which
//! is sound because no request byte can arrive before the handshake that produced it.
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
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use axum::Router;
use rustls::pki_types::CertificateDer;
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
    grpc: Either,
    router: Router,
    redirect_to: Option<u16>,
    /// The host table, for the redirector's one use of it: refusing a `Location` built from
    /// a hostname nothing is configured to serve.
    hosts: Arc<crate::host::HostTable>,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound")
            .field("main", &self.main.addr())
            .field("tls", &matches!(self.main, Either::Tls { .. }))
            .field("redirect", &self.redirect.as_ref().map(|(_, a)| *a))
            .field("grpc", &self.grpc.addr())
            .field("grpc_tls", &matches!(self.grpc, Either::Tls { .. }))
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
    bind_with_hosts(
        listen,
        router,
        resolver,
        Arc::new(crate::host::HostTable::default()),
        None,
    )
    .await
}

/// [`bind`] with the host table the redirector validates against.
///
/// Two functions rather than one parameter on every caller: the redirector is the only
/// consumer, it exists only when a TLS port is configured, and a test that binds a plaintext
/// pair has no host table to hand. `crate::component` passes the real one.
pub async fn bind_with_hosts(
    listen: &Listen,
    router: Router,
    resolver: Option<Arc<SniResolver>>,
    hosts: Arc<crate::host::HostTable>,
    ingest_tls: Option<Arc<ServerConfig>>,
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
            // **The provider is named, not defaulted.** `ServerConfig::builder()` resolves
            // one from rustls's crate features and `expect`s the result, so a future
            // dependency enabling `aws-lc-rs` would turn `bind()` into a panic. `ring` is
            // what **A-W8-3** chose and what this crate already names in
            // `crate::sni::certified_key`, so it is named here too.
            let mut config = ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|source| HttpError::Tls {
                addr: listen.main(),
                source,
            })?
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
    // The gRPC socket, which is the ingest endpoint (W3 §6.1, **D-92**). TLS with a client
    // verifier when one was supplied, and plaintext otherwise -- the second is what every
    // node without a `config.ingest` row gets, and what `grpc:weatherv1`'s read services
    // are reached over on a LAN.
    let grpc = match ingest_tls {
        Some(config) => {
            let (listener, addr) = listen_on(listen.grpc()).await?;
            Either::Tls {
                listener,
                addr,
                acceptor: TlsAcceptor::from(config),
            }
        }
        None => {
            let (listener, addr) = listen_on(listen.grpc()).await?;
            Either::Plain(listener, addr)
        }
    };
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
        hosts,
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
        self.grpc.addr()
    }

    /// Whether the gRPC socket completes a TLS handshake (W7 §5.4 layer 1).
    ///
    /// Read by the boot log line and by `ingest:grpc`'s plan check, which is what makes
    /// **D-35**'s "plaintext ingest is a plan-time error" answerable rather than assumed.
    pub fn grpc_tls(&self) -> bool {
        matches!(self.grpc, Either::Tls { .. })
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
        let grpc_addr = self.grpc.addr();
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
            async move {
                match self.grpc {
                    // **With connect info here too**, even though there is no chain to
                    // report: what a handler needs is "this arrived on the ingest socket",
                    // and the struct's presence is that fact. See [`PeerCerts::tls`].
                    Either::Plain(listener, addr) => {
                        serve_with_peer_certs(listener, router, addr, wait(rx)).await
                    }
                    // **With connect info, and only here.** The handler that reads
                    // [`PeerCerts`] is `ingest:grpc`'s, the chain only exists on a socket
                    // that asked for a client certificate, and a request that reached the
                    // main port carries no `ConnectInfo<PeerCerts>` at all -- which is how
                    // that handler can tell "arrived over the ingest socket" from
                    // "arrived over the website's".
                    Either::Tls {
                        listener,
                        addr,
                        acceptor,
                    } => {
                        serve_with_peer_certs(
                            TlsListener { listener, acceptor },
                            router,
                            addr,
                            wait(rx),
                        )
                        .await
                    }
                }
            }
        };
        let serve_redirect = {
            let rx = signal.clone();
            let redirect = self.redirect;
            let to = self.redirect_to;
            let hosts = Arc::clone(&self.hosts);
            async move {
                match (redirect, to) {
                    (Some((listener, addr)), Some(port)) => {
                        serve_one(listener, redirector(port, hosts), addr, wait(rx)).await
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

/// [`serve_one`] with [`PeerCerts`] in each request's `ConnectInfo`.
///
/// Separate from `serve_one` rather than one generic function, because
/// `into_make_service_with_connect_info` changes the service type and only the ingest
/// socket has a certificate chain to report. The main socket deliberately does **not** get
/// this: it is `with_no_client_auth`, so the slot would always be empty and an empty slot
/// that exists is harder to reason about than one that does not.
async fn serve_with_peer_certs<L, F>(
    listener: L,
    router: Router,
    addr: SocketAddr,
    shutdown: F,
) -> Result<(), HttpError>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    // Higher-ranked: `axum` constructs an `IncomingStream` per connection, borrowed for
    // that connection, so the bound has to hold for **any** lifetime rather than for one.
    PeerCerts: for<'a> axum::extract::connect_info::Connected<axum::serve::IncomingStream<'a, L>>,
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<PeerCerts>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|source| HttpError::Serve { addr, source })
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
fn redirector(https_port: u16, hosts: Arc<crate::host::HostTable>) -> Router {
    Router::new().fallback(move |req: Request<Body>| {
        let hosts = Arc::clone(&hosts);
        async move {
            // `of_request` rather than the `Host` header: `hyper-util`'s auto builder accepts
            // HTTP/2 with prior knowledge on a plaintext socket (§6.6), so a peer can reach
            // this port with no `Host` at all.
            let host = crate::host::of_request(req.uri(), req.headers());
            // **A configured hostname, not whatever the client sent.** M4-3's code review found
            // this the one host-consuming path in the listener that did not consult the host
            // table, so any `Host` became a `Location` -- not header injection
            // (`HeaderValue::try_from` rejects CR and LF and the error arm 404s) and not an open
            // proxy, but a caching intermediary in front of this port could be made to serve
            // one client's `Location` to another. An unknown host gets the 404 instead.
            if host.is_empty() || !hosts.resolves_exactly(&host) {
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
                            certs: PeerCerts::default(),
                        },
                        addr,
                    );
                }
                // The accept loop must not exit on a per-connection error -- a descriptor
                // limit or a peer that reset between the SYN and the accept is not a reason
                // to stop listening. The pause is what stops a tight loop from burning a
                // core while the condition lasts.
                //
                // **`axum`'s own impl splits these two cases and this does not**, which
                // M4-3's code review pointed out and which is corrected here rather than in
                // the comment: a connection-level error (`ECONNABORTED` and friends) is
                // retried immediately, because the next `accept` is a different connection,
                // and anything else -- `EMFILE` is the one that happens -- waits a second,
                // because retrying it at 20 Hz only fills the log. The previous 50 ms for
                // every error logged `EMFILE` twenty times a second.
                Err(e) => {
                    if is_connection_error(&e) {
                        continue;
                    }
                    warn!(error = %e, "accept failed; retrying in 1s");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// Whether an accept error belongs to the connection rather than to the listener.
///
/// `axum`'s own `Listener` impl makes the same split for the same reason: these are errors
/// the *peer* caused between the SYN and the accept, so the next call is a different
/// connection and waiting helps nobody.
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
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
    certs: PeerCerts,
}

/// The client certificate chain one connection presented, or nothing.
///
/// A shared slot rather than a value, because of **when** the two halves happen: the
/// handshake runs inside the per-connection task on the first `poll_read` ([`LazyTls`]), and
/// `axum`'s `Connected::connect_info` runs before that, as the connection is accepted. So
/// the slot is handed over empty and filled by the handshake. Reading it from a request
/// handler is sound without any synchronisation argument beyond the `OnceLock`'s own: a
/// request is bytes that arrived *after* the handshake that decrypted them.
///
/// `Clone` is a pointer copy, and the chain itself is shared rather than copied per request.
#[derive(Clone, Debug)]
pub struct PeerCerts {
    chain: Arc<OnceLock<Arc<Vec<CertificateDer<'static>>>>>,
    remote: SocketAddr,
    tls: bool,
}

impl Default for PeerCerts {
    /// An empty slot with an unspecified source address, for a test and for the accept
    /// path, which fills the address in before the slot is handed over.
    fn default() -> Self {
        Self {
            chain: Arc::new(OnceLock::new()),
            remote: SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
            tls: false,
        }
    }
}

impl PeerCerts {
    /// The end-entity certificate, which is `chain[0]` (RFC 8446 §4.4.2).
    ///
    /// `None` on a connection that presented no certificate -- a `psk` ingest socket, or a
    /// verifier configured as optional -- and on one whose handshake has not completed,
    /// which no request can be observed from.
    pub fn leaf(&self) -> Option<CertificateDer<'static>> {
        self.chain.get()?.first().cloned()
    }

    /// Whether a chain was presented at all.
    pub fn is_present(&self) -> bool {
        self.chain.get().is_some_and(|chain| !chain.is_empty())
    }

    /// Where the connection came from.
    ///
    /// Carried here rather than as a second `ConnectInfo` because `axum` holds one type per
    /// service, and the enrolment RPC needs both: the chain to authenticate with, and the
    /// address to rate-limit and to log (W7 §5.3 requires every attempt to be logged with
    /// its source).
    pub fn remote(&self) -> SocketAddr {
        self.remote
    }

    /// Whether this connection completed a TLS handshake.
    ///
    /// **The reason this is a field and not "`leaf()` is `Some`".** A `psk` ingest socket
    /// asks for no client certificate, and an ingest socket an operator has explicitly
    /// opted into running in plaintext (W7 **D-35**) has no handshake at all — so on both
    /// of those `leaf()` is `None` and the connection is still one a peer is allowed to
    /// make. What a handler actually needs to know is **which socket this arrived on**, and
    /// the presence of this whole struct in `ConnectInfo` is that fact: it is installed on
    /// the gRPC socket and on no other, TLS or not. This flag is the second half — "and it
    /// was encrypted" — which is what lets `mw_collector` refuse the two `mtls-*` kinds on
    /// a plaintext socket while letting `psk` through.
    ///
    /// Inferring the socket from `leaf().is_some()` was the first version and it made
    /// D-35's documented escape hatch non-functional: every peer, `psk` included, was
    /// refused on a plaintext ingest socket while the startup warning said the opposite.
    pub fn tls(&self) -> bool {
        self.tls
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>>
    for PeerCerts
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        let mut info = stream.io().certs.clone();
        info.remote = *stream.remote_addr();
        info.tls = true;
        info
    }
}

/// The same report for a **plaintext** connection: no chain, and `tls()` is `false`.
///
/// Installed on the gRPC socket when it has no TLS, which is W7 **D-35**'s explicit
/// opt-out. Without it that socket reported nothing at all and `mw_collector` could not
/// tell "arrived over the ingest port with no encryption" from "arrived over the website's
/// port", so it refused both — see [`PeerCerts::tls`].
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TcpListener>>
    for PeerCerts
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        Self {
            chain: Arc::new(OnceLock::new()),
            remote: *stream.remote_addr(),
            tls: false,
        }
    }
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
                Poll::Ready(Ok(stream)) => {
                    // Recorded once, here, because this is the only point at which the
                    // handshake's result is in hand. `set` can only fail if a second
                    // handshake completed on one connection, which cannot happen.
                    let _ = self.certs.chain.set(Arc::new(
                        stream
                            .get_ref()
                            .1
                            .peer_certificates()
                            .map(<[CertificateDer<'static>]>::to_vec)
                            .unwrap_or_else(Vec::new),
                    ));
                    self.state = State::Ready(Box::new(stream));
                }
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
    /// A host table that serves the names the tests below redirect for.
    fn redirect_hosts() -> Arc<crate::host::HostTable> {
        Arc::new(crate::host::HostTable::new([(
            "example.com".to_string(),
            mw_types::WebsiteId::new(1),
        )]))
    }

    #[tokio::test]
    async fn the_redirect_preserves_the_path_and_query() {
        use tower::ServiceExt;
        let location = |port: u16, host: &str, uri: &str| {
            let router = redirector(port, redirect_hosts());
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

    /// M4-3's code review: this was the one host-consuming path that did not consult the
    /// host table, so any `Host` became a `Location` a caching intermediary could be made
    /// to serve to another client.
    #[tokio::test]
    async fn an_unconfigured_host_is_not_redirected_to() {
        use tower::ServiceExt;
        let resp = redirector(443, redirect_hosts())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "attacker.test")
                    .body(Body::empty())
                    .expect("a legal request"),
            )
            .await
            .expect("infallible");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(resp.headers().get(header::LOCATION).is_none());
    }

    #[tokio::test]
    async fn a_request_with_no_host_cannot_be_redirected() {
        use tower::ServiceExt;
        let resp = redirector(443, redirect_hosts())
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
