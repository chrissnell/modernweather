//! **The one listener.**
//!
//! `axum` + `tonic` on one socket, `RouteTable` mounting, host routing, the SNI resolver,
//! ALPN, compression and CORS. The Go version has seven listener-owning sites and a single
//! type that owns an HTTP server, an HTTPS server, a gRPC server, SNI, middleware,
//! database queries and config reload; the direct structural answer is that **only this
//! crate may bind a socket**, enforced by `cargo xtask gate layout`
//! (docs/service-architecture.md §1.4, §1.5).
//!
//! It mounts route tables; it does not know what is in them. So it may not depend on
//! `mw-store`, `mw-pipeline`, `mw-api-legacy` or `mw-web`.
//!
//! # What exists so far
//!
//! One listener, `/metrics`, and **one place to merge a route table into it**. The router
//! proper -- host routing, `ArcSwap` rebuilds, N sockets, the SNI resolver -- is
//! implementation-plan.md M4-3 and is not here.
//!
//! [`bind`] takes an `Option<Router>` because M4-2 has a table to serve and no component to
//! mount it from: `api:legacy`'s routes are built by the binary over a state slot the
//! component fills while it runs, so what this crate needs from that item is one `merge`
//! and nothing else. The alternative -- giving this crate the route table type -- would
//! mean naming the surface crates it is forbidden to name, and the alternative to *that* --
//! bringing M4-3's `RouteTable` forward -- is a router rebuild, a host table and an
//! `ArcSwap` for one caller that needs none of them.
//!
//! **The merged table owns the fallback.** `Router::merge` of two routers each with a
//! fallback is a panic, so the caller's table must not declare one; this crate's is the
//! single 404 (DEV-3) and stays that way.
//!
//! What forces a socket to exist now is M2-4's own done-criteria: the daemon has to serve
//! `/metrics`, §5.10 puts it on **an additive route on the one listener**, and §1.4 says
//! only this crate may bind. So [`bind`] is the one listener in the shape it has today,
//! with exactly the route that has a consumer. When M4-3 arrives, `/metrics` becomes one
//! entry in the route table instead of the only one, and nothing outside this crate has to
//! change: the binary already hands in a renderer rather than a registry.
//!
//! The metrics *recorder* is `mw-obs`'s (§5.10), and this crate does not depend on
//! `metrics` or on its exporter. It is handed a closure that renders the current payload,
//! which is also why `metrics-exporter-prometheus` is pinned `default-features = false` in
//! `[workspace.dependencies]`: with `http-listener` on, the exporter would bind a port of
//! its own and be the seventh listener-owning site this design exists to remove.
//!
//! **What does change outside this crate when M4-3 arrives:** today the binary binds and
//! serves directly, which makes the listener a fourth root resource owned by `main`
//! (§3.1 names three) and leaves its teardown outside the supervisor's reverse-topological
//! shutdown (§3.6). That is recorded as a step in `crates/modernweather/src/boot.rs`'s own
//! sequence rather than left implicit, and M4-3 removes it by making `http:listener` the
//! component §6.1 describes -- at which point `bind`, the `serve` future and the binary's
//! `join!` arm all go. What does *not* have to change is this crate's shape: the route is
//! handed a renderer rather than a registry, so it becomes one entry in a route table
//! instead of the only one.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

/// Renders the current metrics payload on demand.
///
/// A closure rather than a trait: the only implementation is `mw-obs`'s handle, and a
/// trait would have to live in `mw-component` -- a capability for one consumer, which is
/// the thing **D-52** declined to add for the log handle.
pub type Render = Arc<dyn Fn() -> String + Send + Sync>;

/// What `/metrics` serves, and in what content type.
#[derive(Clone)]
pub struct MetricsRoute {
    render: Render,
    content_type: &'static str,
}

impl std::fmt::Debug for MetricsRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsRoute")
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

impl MetricsRoute {
    /// A `/metrics` route over a renderer and the content type its payload is in.
    pub fn new(render: Render, content_type: &'static str) -> Self {
        MetricsRoute {
            render,
            content_type,
        }
    }
}

/// Why the listener could not be bound or served.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The address is in use, or the port is privileged and the capability is not held.
    ///
    /// The address is in the message because "permission denied" without one is the least
    /// useful thing a failed bind can say, and the unit grants `CAP_NET_BIND_SERVICE`
    /// precisely so that a low port works -- so an operator seeing this needs to know
    /// which address was tried.
    #[error("cannot bind {addr}")]
    Bind {
        /// What was tried.
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// The accept loop failed.
    #[error("the listener on {addr} stopped serving")]
    Serve {
        /// Where it was listening.
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

impl mw_types::Classify for HttpError {
    /// Both fatal, which is right for the case that happens: a port another process holds
    /// is not released by asking again, and §3.4's classification exists so that the
    /// supervisor does not re-decide this at the call site.
    ///
    /// The case this is *arguably* wrong for is `EADDRNOTAVAIL` -- a configured address on
    /// an interface that has not come up -- where `Fatal` plus `Restart=always` is a
    /// restart loop until it does. Left fatal deliberately: `After=network-online.target`
    /// in W8's unit is what makes it unlikely, the loop is self-healing, and a retry here
    /// would be a second place that decides how long to wait for a network.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

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

/// A bound socket that is not yet serving.
///
/// Bind and serve are separate so that the caller knows the port is actually held before
/// it tells the service manager anything. §3.5's `READY=1` means the supervisor has
/// converged; a readiness notification sent while the socket might still fail to bind
/// would be the `Type=simple` problem with extra steps.
#[derive(Debug)]
pub struct Bound {
    listener: TcpListener,
    router: Router,
    addr: SocketAddr,
}

impl Bound {
    /// The address actually bound, which is how a caller that asked for port 0 finds out
    /// what it got.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serve until `shutdown` completes, then drain in-flight requests for at most
    /// [`DRAIN`].
    pub async fn serve<F>(self, shutdown: F) -> Result<(), HttpError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.serve_with_drain(shutdown, DRAIN).await
    }

    /// [`Bound::serve`] with the drain deadline chosen by the caller, for a test that does
    /// not want to wait ten seconds to prove the deadline exists.
    pub async fn serve_with_drain<F>(self, shutdown: F, drain: Duration) -> Result<(), HttpError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let addr = self.addr;
        // The shutdown future is consumed by `with_graceful_shutdown`, so the deadline is
        // started from a second observation of the same signal: the channel fires when the
        // first one resolves.
        let (signalled, signal) = tokio::sync::oneshot::channel();
        let shutdown = async move {
            shutdown.await;
            let _ = signalled.send(());
        };
        let serving = axum::serve(self.listener, self.router).with_graceful_shutdown(shutdown);
        tokio::select! {
            result = serving => result.map_err(|source| HttpError::Serve { addr, source }),
            () = async move {
                let _ = signal.await;
                tokio::time::sleep(drain).await;
            } => {
                // Abandoning the drain is a `WARN` rather than an error: every component
                // is already stopped by the time the binary gets here, and a client that
                // never finished its request is not a reason to exit non-zero.
                tracing::warn!(
                    addr = %addr,
                    drain_s = drain.as_secs(),
                    "connections did not drain inside the deadline; closing the listener"
                );
                Ok(())
            }
        }
    }
}

/// Bind the listener, optionally merging one additional route table into it.
///
/// `routes` is `None` for a node with no surface to serve -- which is no node today, and is
/// the shape a test that only wants `/metrics` uses. See the module documentation for why
/// the parameter is an `axum::Router` rather than a type this crate owns.
pub async fn bind(
    addr: SocketAddr,
    metrics: MetricsRoute,
    routes: Option<Router>,
) -> Result<Bound, HttpError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| HttpError::Bind { addr, source })?;
    // `0.0.0.0:0` is a legitimate request and the kernel chooses the port, so the bound
    // address is read back rather than echoed.
    let addr = listener
        .local_addr()
        .map_err(|source| HttpError::Bind { addr, source })?;
    Ok(Bound {
        listener,
        router: router(metrics, routes),
        addr,
    })
}

fn router(metrics: MetricsRoute, routes: Option<Router>) -> Router {
    let mounted = match routes {
        Some(routes) => routes,
        None => Router::new(),
    };
    mounted.merge(base(metrics))
}

/// `/metrics` and the one fallback.
fn base(metrics: MetricsRoute) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let metrics = metrics.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, metrics.content_type)],
                        (metrics.render)(),
                    )
                }
            }),
        )
        // DEV-3 in advance: a path with no route is a 404 with no body rather than a
        // directory listing or a 405. One route table means one fallback.
        .fallback(|| async { StatusCode::NOT_FOUND })
}
