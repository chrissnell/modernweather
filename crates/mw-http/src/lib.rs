//! **The one listener.**
//!
//! `axum` on N sockets, `RouteTable` mounting, host routing, the SNI resolver, ALPN,
//! compression and the one 404. The Go version has seven listener-owning sites and a single
//! type that owns an HTTP server, an HTTPS server, a gRPC server, SNI, middleware, database
//! queries and config reload; the direct structural answer is that **only this crate may
//! bind a socket**, enforced by `cargo xtask gate layout`
//! (docs/service-architecture.md §1.4, §1.5).
//!
//! It mounts route tables; it does not know what is in them. So it may not depend on
//! `mw-store`, `mw-pipeline`, `mw-api-legacy` or `mw-web`.
//!
//! # The shape, and which module owns which decision
//!
//! | Module | Decision |
//! |---|---|
//! | [`socket`] | N sockets from configuration, one router, one shutdown path (§6.1, **A-W3-4**, **D-92**) |
//! | [`mount`] | the route tables merged into one router, with the host dispatch in front (§6.2) |
//! | [`host`] | the three `Host` normalisation steps and the host table (§6.3, **DEV-2**) |
//! | [`sni`] | the certificate store and the miss chain (§6.4, **DEV-6**) |
//! | [`component`] | `http:listener`, which owns all of the above for the supervisor |
//!
//! # `/metrics` is a route table like any other
//!
//! §5.10 puts it on **an additive route on the one listener**, and §1.4 says only this crate
//! may bind -- so the route is here and the recorder is `mw-obs`'s. [`metrics_table`] is
//! handed a closure that renders the current payload rather than a registry, which is also
//! why `metrics-exporter-prometheus` is pinned `default-features = false` in
//! `[workspace.dependencies]`: with `http-listener` on, the exporter would bind a port of
//! its own and be the seventh listener-owning site this design exists to remove.
//!
//! The table's owner is `http:listener` itself. That is not a component borrowing another's
//! identity: §5.10 makes `/metrics` the listener's own additive route, and naming an owner
//! is what lets [`mount`]'s conflict check report it by name if a component ever claims the
//! same path.
//!
//! # What the listener does **not** do
//!
//! **It does not resolve a website for the frozen surface.** `api:legacy`'s routes are
//! `Scope::Any` and its handlers select a website from the `Host` header themselves, which
//! is the frozen routing (§6.3) and the reason three websites declaring `/` are not in
//! conflict while two components declaring `/latest` would be. Both sides ask
//! [`host::normalise`], so there is one answer to "does `EXAMPLE.COM.` match `example.com`".
//!
//! **It does not mount a gRPC service.** The gRPC *socket* is here; the services are
//! implementation-plan.md M6-1 and M6-4. [`socket`]'s module documentation has why the
//! dispatcher is not written in advance.
//!
//! **It does not apply CORS or `Cache-Control` as layers.** §6.6 lists both as `RouteFlags`
//! bits, and the providers in this workspace set them per *response* rather than per table,
//! because the frozen surface's values are per endpoint and finer than a table-level flag
//! can express: `mw_api_legacy::reply` emits `Access-Control-Allow-Origin: *` on its JSON
//! bodies and deliberately **not** on its four error bodies ("None of the seven call sites
//! carries CORS"), and `Cache-Control` is `no-store` on `/latest`, `max-age=<computed>` on
//! `/span`, 3600 on `/almanac` and absent on three more. A table-level flag that the router
//! applied would add CORS to the error bodies and would have to carry a value per endpoint
//! anyway. So those two bits are **not** modelled here; what is global is global
//! ([`mount::Mount::router`]'s compression and the one `Vary`), and what is per endpoint
//! stays with the endpoint. Recorded because a reader looking for `RouteFlags` should find
//! the reason rather than the omission.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::header;
use axum::routing::get;
use axum::Router;
use mw_component::{ComponentId, Methods, RouteEntry, RouteManifest, Scope};

pub mod component;
pub mod host;
pub mod mount;
pub mod sni;
pub mod socket;
mod table;

pub use component::{register, ListenerConfig, ID};
pub use host::{matches_one_label, normalise, HostTable};
pub use mount::{not_found, Mount, MountError, MountReport, NOT_FOUND_BODY};
pub use sni::{CertError, CertPaths, CertStore, SniResolver};
pub use socket::{bind, bind_with_hosts, Bound, Listen, DRAIN};
pub use table::RouteTable;

/// The five frozen path patterns, in `axum` 0.8 syntax.
///
/// **Here rather than in `mw-api-legacy` because the check that needs them is here.**
/// M4-1's constraint 1 says the route table's mounting rule is the last place **D-48**'s
/// invariant can be broken, and [`MountError::FrozenPatternShadowed`] is that rule -- so the
/// patterns have to be nameable by a crate that may not depend on the one that serves them.
///
/// `mw-api-legacy` is a higher layer and may name this crate, so there is **one** copy and
/// a test over there asserts its own `FROZEN_PATTERNS` equals this list. Two lists with no
/// assertion between them is how a sixth frozen path would get mounted per website without
/// anything noticing.
///
/// The **five** of **D-68**, not all seven of `api:legacy`'s routes: `/almanac` and
/// `/api/stations` are coupled rather than frozen, so a website table shadowing one of them
/// is a refactor rather than a contract break.
pub const FROZEN_PATTERNS: [&str; 5] = [
    "/latest",
    "/span/{span}",
    "/snow",
    "/stationinfo",
    "/forecast/{span}",
];

/// Renders the current metrics payload on demand.
///
/// A closure rather than a trait: the only implementation is `mw-obs`'s handle, and a trait
/// would have to live in `mw-component` -- a capability for one consumer, which is the thing
/// **D-52** declined to add for the log handle.
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

/// `/metrics` as a route table, owned by the listener (§5.10).
///
/// `Methods::READ` rather than `Methods::ANY`: the frozen surface accepts every method
/// because W1 pinned that it does (Q-17), and `/metrics` is not on the frozen surface --
/// it is additive, it has one client, and that client sends `GET`.
pub fn metrics_table(metrics: MetricsRoute) -> RouteTable {
    RouteTable::new(
        // `expect` rather than `?`: the id is a literal this function owns, so a failure
        // would be a build-time typo and not a configuration error any caller can act on.
        ComponentId::new(ID).expect("`http:listener` is a legal component id"),
        RouteManifest::new(
            Scope::Any,
            vec![RouteEntry::exact("/metrics", Methods::READ)],
        ),
        Router::new().route(
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
        ),
    )
}

/// An inbound station upload as a route table, owned by the station component that
/// provides it.
///
/// The counterpart of [`metrics_table`]: a provider in the `service` layer cannot name
/// `axum` (`ci/layers.toml`), so what it hands over is a
/// [`mw_component::UploadEndpoint`] — a closure — and this is where that becomes a route.
/// `mw-station`'s Ambient driver is the one provider; `mw_component::routes` has why the
/// handle is a closure rather than a capability trait.
///
/// **The declared methods are `READ`, and that is `axum`'s doing rather than a choice.**
/// The Ambient "customized" protocol is a `GET` with the measurements in the query string,
/// so `MethodFilter::GET` is what the route is mounted with — and `axum`'s `MethodRouter`
/// answers `HEAD` with the `GET` handler whenever no `HEAD` handler is set, which cannot be
/// turned off without mounting a second handler whose whole job is to refuse. So the
/// manifest declares `GET|HEAD`, because the manifest is what route-conflict detection
/// reads and a declaration that did not match what the router answers would be worse than
/// an honest one. The consequence is bounded: a `HEAD` carrying a full query string is
/// admitted exactly as the `GET` would be and its body is discarded by the protocol, and no
/// station firmware sends one. Every other method is `405` from the router, which is why
/// [`mw_component::UploadStatus`] has no "wrong method" variant.
pub fn upload_table(
    owner: ComponentId,
    mount: &str,
    manifest: RouteManifest,
    endpoint: mw_component::UploadEndpoint,
) -> RouteTable {
    use axum::routing::{on, MethodFilter};

    let router = Router::new().route(
        mount,
        on(MethodFilter::GET, move |query: axum::extract::RawQuery| {
            let endpoint = Arc::clone(&endpoint);
            async move {
                let reply = endpoint(mw_component::Upload {
                    // `RawQuery` is `None` for a request with no `?` at all, which is an
                    // upload carrying no parameters -- a case the driver refuses by name
                    // rather than one this layer should turn into a 404.
                    query: query.0.unwrap_or_default(),
                })
                .await;
                (status(reply.status), reply.body)
            }
        }),
    );
    RouteTable::new(owner, manifest, router)
}

/// One [`mw_component::UploadStatus`] as a status code.
///
/// The whole of the mapping, in one place, because a driver that chose its own numbers
/// would be a driver with an opinion about HTTP.
fn status(status: mw_component::UploadStatus) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    use mw_component::UploadStatus as S;
    match status {
        S::Accepted => StatusCode::OK,
        S::Malformed => StatusCode::BAD_REQUEST,
        // `401` and not `403`: the station presented a credential and it was not accepted,
        // which is what `WWW-Authenticate`-less `401` means on a protocol with no challenge.
        // `403` would say "we know who you are and you may not", and one status for an
        // unknown station and a bad secret says neither. The *timing* of the two still
        // differs; `mw_component::UploadStatus::Unauthorised` is where that is recorded.
        S::Unauthorised => StatusCode::UNAUTHORIZED,
        // `503` and not `500`: the condition is "this node cannot durably accept a reading
        // right now", which is temporary and which the station's own retry is the correct
        // response to.
        S::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Why the listener could not be bound or served.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The address is in use, or the port is privileged and the capability is not held.
    ///
    /// The address is in the message because "permission denied" without one is the least
    /// useful thing a failed bind can say, and the unit grants `CAP_NET_BIND_SERVICE`
    /// precisely so that a low port works -- so an operator seeing this needs to know which
    /// address was tried.
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

    /// A TLS port is configured and no website has a usable certificate.
    ///
    /// A refusal rather than a socket that accepts connections and fails every handshake:
    /// the second is indistinguishable, from the client's side, from a certificate the
    /// client does not trust, and §6.4's whole argument is that a wrong-or-absent
    /// certificate must be an operator-visible startup failure.
    #[error(
        "`listener.https_port` names {addr} and no website has a certificate to serve it \
         with; set `website.tls_cert_path` and `website.tls_key_path`, or clear \
         `listener.https_port`"
    )]
    NoCertificate {
        /// The TLS address that would have been served.
        addr: SocketAddr,
    },

    /// The TLS configuration could not be built.
    ///
    /// One reachable cause: `rustls`'s `ring` provider refusing the safe default protocol
    /// versions, which would mean a provider built without TLS 1.2 or 1.3 support. The
    /// provider is **named** rather than defaulted (`socket::bind`), so this is a build
    /// mistake rather than a configuration one -- and it is a named error rather than the
    /// panic `ServerConfig::builder()` would have given.
    #[error("the TLS configuration for {addr} could not be built")]
    Tls {
        /// The TLS address that would have been served.
        addr: SocketAddr,
        #[source]
        source: rustls::Error,
    },

    /// A certificate is configured and could not be loaded.
    #[error("a configured certificate could not be loaded")]
    Certificate {
        #[from]
        #[source]
        source: CertError,
    },

    /// The route tables cannot be mounted.
    #[error("the route tables cannot be mounted")]
    Mount {
        #[from]
        #[source]
        source: MountError,
    },
}

impl mw_types::Classify for HttpError {
    /// All fatal, which is right for the cases that happen: a port another process holds is
    /// not released by asking again, an unreadable certificate path is not made readable by
    /// retrying, and two tables claiming one path is a wiring mistake. §3.4's
    /// classification exists so that the supervisor does not re-decide this at the call
    /// site -- and §3.4's own backoff means "fatal" is still retried with a cap rather than
    /// given up on (W2a Decision 30).
    ///
    /// The case this is *arguably* wrong for is `EADDRNOTAVAIL` -- a configured address on
    /// an interface that has not come up -- where the retry is the only thing that helps.
    /// It gets it, for the reason above.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// `/metrics` is one entry in a route table now rather than the only route, and nothing
    /// outside this crate had to change for it: the route is still handed a renderer.
    #[tokio::test]
    async fn metrics_is_a_route_table() {
        let table = metrics_table(MetricsRoute::new(
            Arc::new(|| "mw_up 1\n".to_string()),
            "text/plain; version=0.0.4",
        ));
        assert_eq!(table.owner().as_str(), "http:listener");
        assert_eq!(table.scope(), Scope::Any);
        assert_eq!(table.manifest().entries.len(), 1);

        let (mount, _) = Mount::new(vec![table], HostTable::default()).expect("mounts");
        let resp = mount
            .router()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("a legal request"),
            )
            .await
            .expect("infallible");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; version=0.0.4")
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .expect("a readable body");
        assert_eq!(&body[..], b"mw_up 1\n");
    }

    /// The upload route, driven end to end: the query reaches the endpoint verbatim, each
    /// of the three outcomes becomes its status, and a method the route does not declare
    /// never reaches the handler.
    #[tokio::test]
    async fn an_upload_endpoint_is_a_route_table_over_a_closure() {
        use mw_component::{Upload, UploadReply, UploadStatus};

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let table = upload_table(
            ComponentId::new("station:ambient").expect("a legal id"),
            "/ingest/ambient",
            // The shape the driver declares, built here because this crate may not name
            // `mw-station`: the production caller passes `mw_station::ambient::manifest`.
            RouteManifest::new(
                Scope::Any,
                vec![RouteEntry::exact("/ingest/ambient", Methods::READ)],
            ),
            Arc::new(move |upload: Upload| {
                let log = Arc::clone(&log);
                Box::pin(async move {
                    log.lock().expect("not poisoned").push(upload.query.clone());
                    match upload.query.as_str() {
                        "" => UploadReply::refused(UploadStatus::Malformed, "missing\n"),
                        q if q.contains("PASSWORD=wrong") => {
                            UploadReply::refused(UploadStatus::Unauthorised, "unauthorized\n")
                        }
                        _ => UploadReply::accepted("success"),
                    }
                })
            }),
        );
        assert_eq!(table.owner().as_str(), "station:ambient");
        assert_eq!(table.scope(), Scope::Any);
        assert_eq!(
            table.manifest().entries,
            vec![RouteEntry::exact("/ingest/ambient", Methods::READ)]
        );

        let (mount, _) = Mount::new(vec![table], HostTable::default()).expect("mounts");
        let call = |method: &str, uri: &str| {
            let router = mount.router();
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("a legal request");
            async move { router.oneshot(request).await.expect("infallible") }
        };

        let ok = call("GET", "/ingest/ambient?ID=roof&PASSWORD=right&tempf=68.4").await;
        assert_eq!(ok.status(), StatusCode::OK);
        let body = axum::body::to_bytes(ok.into_body(), 1 << 16)
            .await
            .expect("a readable body");
        assert_eq!(&body[..], b"success");

        assert_eq!(
            call("GET", "/ingest/ambient?ID=roof&PASSWORD=wrong")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // No `?` at all: an upload with no parameters, refused by the driver rather than
        // turned into a 404 by this layer.
        assert_eq!(
            call("GET", "/ingest/ambient").await.status(),
            StatusCode::BAD_REQUEST
        );
        // The method the route does not declare, answered by the router.
        assert_eq!(
            call("POST", "/ingest/ambient?ID=roof").await.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        // `HEAD` reaches the handler, because `axum` answers it with the `GET` handler --
        // which is why the manifest declares `GET|HEAD` rather than `GET`.
        assert_eq!(
            call("HEAD", "/ingest/ambient?ID=roof&PASSWORD=right")
                .await
                .status(),
            StatusCode::OK
        );

        assert_eq!(
            *seen.lock().expect("not poisoned"),
            vec![
                "ID=roof&PASSWORD=right&tempf=68.4".to_string(),
                "ID=roof&PASSWORD=wrong".to_string(),
                String::new(),
                "ID=roof&PASSWORD=right".to_string(),
            ],
            "the POST never reached the endpoint, and each query arrived verbatim"
        );
    }
}
