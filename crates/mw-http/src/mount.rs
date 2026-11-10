//! Mounting: N route tables into **one** router, and the host dispatch in front of it.
//!
//! ```text
//!   request ──▶ [ Vary: Accept-Encoding ]            outermost: one value, capitalised
//!               [ Compression: gzip, deflate ]
//!               [ Dispatch ]  Host ──▶ site router ──fallback──▶ shared router
//!                                                                 ──fallback──▶ catch-all
//! ```
//!
//! **Three `ArcSwap`s, and they are the replacement for a measured race** (**D-28**): this
//! module's [`Mount`] holds the router and the host table, and [`crate::sni`] holds the
//! certificate store. The Go controller holds all three as plain unsynchronised fields and
//! replaces them wholesale from the management API while in-flight requests read them
//! (`controller.go:708`, `:734`), which in Go is an unrecoverable fatal error.
//!
//! **Nothing swaps them yet, and that is worth saying rather than implying.** A swap is a
//! plan diff (§3.7), and configuration cannot change at runtime until `ConfigAdmin` exists
//! (implementation-plan.md M7-1). What exists here is the swap *mechanism* and a test that
//! it is observed by a request made across it; what does not exist is a writer. The
//! alternative -- building the router behind a lock, or rebuilding the listener -- is the
//! thing that would have to be undone at M7-1, which is why it is this shape now.
//!
//! # Why the host dispatch is a `tower::Service` and not `Router::merge`
//!
//! Three websites each declare `/`. `Router::merge` of two routers that both claim a path
//! panics, and `Scope::Site` exists precisely so that they are *not* in conflict (§6.3
//! routes by host first). So the per-website routers stay separate and a service in front
//! picks one by `Host`. `axum::Router::fallback_service` then chains them without a second
//! dispatcher: a site router falls back to the shared router, which falls back to the
//! catch-all.

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use axum::Router;
use mw_component::{ComponentId, Scope};
use mw_types::WebsiteId;
use tower::Service;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::host::HostTable;
use crate::table::RouteTable;

/// Go's `http.NotFound` body, which is the one 404 this listener gives.
///
/// `404 page not found\n` as `text/plain; charset=utf-8`, which `asset-not-found` and
/// `html-portal-on-non-portal` assert byte for byte (**DEV-3** replaces the directory
/// listings with it as well). `X-Content-Type-Options: nosniff` is what Go also sets and is
/// deliberately **not** reproduced, for `mw_api_legacy::reply::text`'s reason: no fixture
/// asserts it, and adding a header a client might switch on is a change this item has no
/// reason to make.
pub const NOT_FOUND_BODY: &str = "404 page not found\n";

/// [`NOT_FOUND_BODY`] as a response.
pub fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .body(Body::from(NOT_FOUND_BODY))
        // `Builder::body` fails only on a header this function wrote, so the fallback is
        // unreachable; it is a bare status rather than a panic because a 404 handler that
        // can panic is a 500 waiting for a bad header value.
        .unwrap_or_else(|_| {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        })
}

/// Why a set of route tables cannot be mounted.
///
/// Every variant is a **wiring** mistake rather than a configuration one: `plan()` has
/// already rejected a duplicate path (**D-51**), so what is left here is a table whose
/// scope this listener has nowhere to put and a second claim on the one fallback.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// Two tables declared themselves the catch-all.
    #[error("`{a}` and `{b}` both declare the catch-all route table; exactly one may")]
    TwoCatchAlls {
        /// The table that claimed it first.
        a: ComponentId,
        /// The one that claimed it second.
        b: ComponentId,
    },

    /// A scope this listener does not serve.
    #[error(
        "`{owner}` declares scope `{scope}`, which this listener does not mount: the \
         management API has its own socket (A-W5-3) and implementation-plan.md M7-1 owns it"
    )]
    UnsupportedScope {
        /// Whose table it is.
        owner: ComponentId,
        /// What it declared.
        scope: Scope,
    },

    /// Two tables in one scope claim the same path.
    ///
    /// **`plan()` catches this before any socket exists** (**D-51**) for every table that
    /// travels on a `Provision`. This check exists for the one that does not -- the
    /// listener's own `/metrics` table (§5.10) -- and because the consequence of missing it
    /// is an `axum` panic inside `Router::merge` rather than an error anybody can read.
    #[error("`{a}` and `{b}` both claim `{pattern}` in scope `{scope}`")]
    RouteConflict {
        /// The table that claimed it first.
        a: ComponentId,
        /// The one that claimed it second.
        b: ComponentId,
        /// Which router they are in.
        scope: Scope,
        /// The overlapping pattern, as the second table declared it.
        pattern: String,
    },

    /// A per-website table claims one of the five frozen patterns.
    ///
    /// **This is M4-1's constraint 1 as a check rather than a note.** A `Scope::Site` router
    /// is tried *before* the shared one, so a per-website table declaring `/latest` would
    /// shadow the frozen surface for that host -- and `plan()` cannot see it, because two
    /// tables in different scopes are not a route conflict (three websites declaring `/` is
    /// the case `Scope::Site` exists for). **D-48** says a frozen path always answers its
    /// contract response; the mount is the last place that can be broken, so it is the
    /// place it is refused.
    #[error(
        "`{owner}` declares `{pattern}` in scope `{scope}`, which would shadow the frozen \
         surface for that host; the five frozen patterns are served for every host by \
         `api:legacy` in `Scope::Any` (D-48)"
    )]
    FrozenPatternShadowed {
        /// Whose table it is.
        owner: ComponentId,
        /// Which frozen pattern it claimed.
        pattern: String,
        /// The scope it claimed it in.
        scope: Scope,
    },

    /// Two tables claim the same website.
    ///
    /// `plan()` cannot catch this one: two `Scope::Site(3)` tables declaring *different*
    /// paths are not a route conflict, and the second would silently replace the first.
    #[error("`{a}` and `{b}` both declare the route table for website {website}")]
    TwoSiteTables {
        /// The table that claimed it first.
        a: ComponentId,
        /// The one that claimed it second.
        b: ComponentId,
        /// Which website.
        website: WebsiteId,
    },
}

/// Every router, as one generation.
#[derive(Debug)]
struct Mounted {
    /// Host → website.
    hosts: HostTable,
    /// One router per website, each already falling back to [`Mounted::shared`].
    sites: HashMap<WebsiteId, Router>,
    /// Every `Scope::Any` table, merged, falling back to the catch-all.
    shared: Router,
    /// Which component owns each mounted table, for the pairing the binary asserts against
    /// the plan. A `BTreeSet<String>` rather than borrowed ids because a reader holds an
    /// `arc_swap::Guard` and cannot lend out of it.
    owners: BTreeSet<String>,
}

impl Mounted {
    /// The router a request for this `Host` enters.
    fn route(&self, host: &str) -> &Router {
        self.hosts
            .resolve(host)
            .and_then(|id| self.sites.get(&id))
            .unwrap_or(&self.shared)
    }
}

/// The mounted router set, swappable without dropping a connection.
#[derive(Debug, Clone)]
pub struct Mount {
    inner: Arc<ArcSwap<Mounted>>,
}

impl Mount {
    /// Mount a generation of route tables over a host table.
    pub fn new(
        tables: Vec<RouteTable>,
        hosts: HostTable,
    ) -> Result<(Self, MountReport), MountError> {
        let (mounted, report) = build(tables, hosts)?;
        Ok((
            Self {
                inner: Arc::new(ArcSwap::from_pointee(mounted)),
            },
            report,
        ))
    }

    /// Replace every router and the host table with one atomic store.
    ///
    /// In-flight requests keep the `Arc` they loaded; the next one takes the new
    /// generation. Nothing calls this yet -- see the module documentation.
    pub fn swap(
        &self,
        tables: Vec<RouteTable>,
        hosts: HostTable,
    ) -> Result<MountReport, MountError> {
        let (mounted, report) = build(tables, hosts)?;
        self.inner.store(Arc::new(mounted));
        Ok(report)
    }

    /// The service to serve: the host dispatch under compression and the one `Vary`.
    ///
    /// **The `Vary` override is outermost and that is the whole of it.** Every one of the
    /// 70 fixtures asserts `Vary: Accept-Encoding` with that exact spelling, and the runner
    /// compares a header's **value**. `tower_http`'s `CompressionLayer` writes
    /// `vary: accept-encoding` in lower case, and `mw_api_legacy::reply` already sets the
    /// capitalised one on every response of the frozen surface -- so appending would give
    /// `Accept-Encoding, accept-encoding` and leaving it to compression would give the
    /// lower-case spelling on everything else. `overriding` replaces whatever is there with
    /// exactly one value, after compression has run, which is the one arrangement that is
    /// right for both. `mw_api_legacy::reply`'s module documentation records the
    /// constraint; [`crate::tests`] asserts it.
    ///
    /// **gzip and deflate only.** `br` and `zstd` are deliberately off: advertising them
    /// would change `Content-Encoding` for clients that accept them, and the fixtures are
    /// `identity` (§6.6).
    ///
    /// **No `NormalizePath` layer**, and its absence is load-bearing: routes are exact
    /// rather than prefixes, and `/latest/` must 404 into the catch-all rather than being
    /// redirected or stripped onto `/latest` (§6.6).
    pub fn router(&self) -> Router {
        Router::new()
            .fallback_service(Dispatch {
                inner: Arc::clone(&self.inner),
            })
            .layer(
                CompressionLayer::new()
                    .gzip(true)
                    .deflate(true)
                    .br(false)
                    .zstd(false),
            )
            .layer(SetResponseHeaderLayer::overriding(
                header::VARY,
                HeaderValue::from_static("Accept-Encoding"),
            ))
    }

    /// How many websites have their own router, for a log line and a test.
    pub fn sites(&self) -> usize {
        self.inner.load().sites.len()
    }

    /// Which component owns each mounted table.
    ///
    /// The binary pairs this against the set of planned `HttpRoutes` providers, in both
    /// directions -- see `crates/modernweather/src/boot.rs`'s
    /// `assert_route_tables_match_the_plan` for why each direction is a different defect.
    pub fn owners(&self) -> BTreeSet<String> {
        self.inner.load().owners.clone()
    }
}

/// What one mount produced, for the log line that says what is being served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountReport {
    /// How many `Scope::Any` tables were merged.
    pub shared: usize,
    /// How many websites got their own router.
    pub sites: usize,
    /// How many declared paths were mounted, across every table.
    pub paths: usize,
    /// Which component owns the catch-all, if any.
    pub catch_all: Option<ComponentId>,
    /// How many hostnames resolve.
    pub hosts: usize,
}

/// Whether `table` overlaps a table already accepted **in the same scope**.
///
/// Scope-local on purpose: two scopes are two routers, which is the whole of why three
/// websites may each declare `/` (§6.3 routes by host first). The cross-scope hazard that
/// *does* matter -- a site table shadowing a frozen pattern -- is
/// [`MountError::FrozenPatternShadowed`] rather than a conflict, because it is one-sided.
fn conflict_with(
    table: &RouteTable,
    shared: &[RouteTable],
    sites: &[(WebsiteId, RouteTable)],
) -> Option<MountError> {
    let seen: Vec<&RouteTable> = match table.scope() {
        Scope::Any => shared.iter().collect(),
        scope => sites
            .iter()
            .filter(|(_, t)| t.scope() == scope)
            .map(|(_, t)| t)
            .collect(),
    };
    for other in seen {
        for entry in &table.manifest().entries {
            if let Some(clash) = other
                .manifest()
                .entries
                .iter()
                .find(|e| e.conflicts_with(entry))
            {
                return Some(MountError::RouteConflict {
                    a: other.owner().clone(),
                    b: table.owner().clone(),
                    scope: table.scope(),
                    pattern: clash.pattern.clone(),
                });
            }
        }
    }
    None
}

fn build(tables: Vec<RouteTable>, hosts: HostTable) -> Result<(Mounted, MountReport), MountError> {
    let mut report = MountReport {
        shared: 0,
        sites: 0,
        paths: 0,
        catch_all: None,
        hosts: hosts.len(),
    };
    let mut owners: BTreeSet<String> = BTreeSet::new();
    let mut shared_tables: Vec<RouteTable> = Vec::new();
    let mut site_tables: Vec<(WebsiteId, RouteTable)> = Vec::new();
    let mut catch_all: Option<RouteTable> = None;

    for table in tables {
        report.paths += table.manifest().entries.len();
        owners.insert(table.owner().as_str().to_string());
        // **The catch-all is exempt from both checks below, and it has to be.** It claims
        // `/` as a prefix, which contains every other pattern in the workspace -- but it is
        // mounted as the shared router's *fallback*, so it only ever answers a path nothing
        // else matched. A fallback cannot shadow.
        if table.is_catch_all() {
            if let Some(first) = &catch_all {
                return Err(MountError::TwoCatchAlls {
                    a: first.owner().clone(),
                    b: table.owner().clone(),
                });
            }
            report.catch_all = Some(table.owner().clone());
            catch_all = Some(table);
            continue;
        }
        if !matches!(table.scope(), Scope::Any) {
            if let Some(entry) = table
                .manifest()
                .entries
                .iter()
                .find(|e| crate::FROZEN_PATTERNS.contains(&e.pattern.as_str()))
            {
                return Err(MountError::FrozenPatternShadowed {
                    owner: table.owner().clone(),
                    pattern: entry.pattern.clone(),
                    scope: table.scope(),
                });
            }
        }
        if let Some(conflict) = conflict_with(&table, &shared_tables, &site_tables) {
            return Err(conflict);
        }
        match table.scope() {
            Scope::Any => shared_tables.push(table),
            Scope::Site(id) => {
                if let Some((_, first)) = site_tables.iter().find(|(seen, _)| *seen == id) {
                    return Err(MountError::TwoSiteTables {
                        a: first.owner().clone(),
                        b: table.owner().clone(),
                        website: id,
                    });
                }
                site_tables.push((id, table));
            }
            scope @ Scope::Management => {
                return Err(MountError::UnsupportedScope {
                    owner: table.owner().clone(),
                    scope,
                })
            }
        }
    }

    report.shared = shared_tables.len();
    report.sites = site_tables.len();

    // The catch-all is the shared router's fallback, which is the Go router's last entry:
    // `router.PathPrefix("/")`. With none mounted, the listener's own 404 answers.
    let mut shared = match catch_all {
        Some(table) => Router::new().fallback_service(table.into_router().into_service::<Body>()),
        None => Router::new().fallback(|| async { not_found() }),
    };
    for table in shared_tables {
        shared = shared.merge(table.into_router());
    }

    // A site's router falls back to the shared one, so `/latest` answers on every host
    // without being declared per website -- which is what `Scope::Any` means.
    let sites = site_tables
        .into_iter()
        .map(|(id, table)| {
            (
                id,
                table
                    .into_router()
                    .fallback_service(shared.clone().into_service::<Body>()),
            )
        })
        .collect();

    Ok((
        Mounted {
            hosts,
            sites,
            shared,
            owners,
        },
        report,
    ))
}

/// Picks the router for a request's `Host`.
///
/// One `ArcSwap` load per request and a `Router` clone, which is a refcount bump: axum's
/// router is `Arc` inside.
#[derive(Clone)]
struct Dispatch {
    inner: Arc<ArcSwap<Mounted>>,
}

impl std::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mounted = self.inner.load();
        f.debug_struct("Dispatch")
            .field("sites", &mounted.sites.len())
            .field("hosts", &mounted.hosts.len())
            .finish_non_exhaustive()
    }
}

impl Service<Request<Body>> for Dispatch {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // An absent or non-ASCII `Host` normalises to the empty string, which resolves to
        // the default website -- the same answer an unmatched one gets (Q-18), so there is
        // no error path here and no response the contract has no bytes for.
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let mounted = self.inner.load();
        let mut router = mounted.route(&host).clone();
        Box::pin(async move { router.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{any, get};
    use mw_component::{Methods, RouteEntry, RouteManifest};
    use tower::ServiceExt;

    fn id(name: &str) -> ComponentId {
        ComponentId::new(name).expect("a legal id")
    }

    fn table(owner: &str, scope: Scope, path: &'static str, body: &'static str) -> RouteTable {
        RouteTable::new(
            id(owner),
            RouteManifest::new(scope, vec![RouteEntry::exact(path, Methods::ANY)]),
            Router::new().route(path, any(move || async move { body })),
        )
    }

    async fn get_with_host(mount: &Mount, host: &str, path: &str) -> (StatusCode, String, String) {
        let req = Request::builder()
            .uri(path)
            .header(header::HOST, host)
            .body(Body::empty())
            .expect("a legal request");
        let resp = mount
            .router()
            .oneshot(req)
            .await
            .expect("the router is infallible");
        let status = resp.status();
        let vary = resp
            .headers()
            .get_all(header::VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(", ");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("a readable body");
        (status, String::from_utf8_lossy(&body).into_owned(), vary)
    }

    fn hosts() -> HostTable {
        HostTable::new([
            ("one.test".to_string(), WebsiteId::new(1)),
            ("two.test".to_string(), WebsiteId::new(2)),
        ])
    }

    fn mount() -> Mount {
        Mount::new(
            vec![
                table("api:legacy", Scope::Any, "/latest", "latest"),
                table(
                    "web:site:1",
                    Scope::Site(WebsiteId::new(1)),
                    "/",
                    "site one",
                ),
                table(
                    "web:site:2",
                    Scope::Site(WebsiteId::new(2)),
                    "/",
                    "site two",
                ),
            ],
            hosts(),
        )
        .expect("the tables mount")
        .0
    }

    /// Three websites each declaring `/` is the case `Scope::Site` exists for.
    #[tokio::test]
    async fn the_host_picks_the_site_router() {
        let m = mount();
        assert_eq!(get_with_host(&m, "one.test", "/").await.1, "site one");
        assert_eq!(get_with_host(&m, "two.test", "/").await.1, "site two");
        assert_eq!(m.sites(), 2);
    }

    /// Q-18: an unmatched `Host` gets the default website, and nothing in the response
    /// says so.
    #[tokio::test]
    async fn an_unmatched_host_gets_the_default_site() {
        assert_eq!(
            get_with_host(&mount(), "nobody.test", "/").await.1,
            "site one"
        );
    }

    /// A `Scope::Any` path answers on a host that has its own router, which is what the
    /// site router's fallback buys.
    #[tokio::test]
    async fn a_shared_route_answers_through_a_site_router() {
        let m = mount();
        assert_eq!(get_with_host(&m, "one.test", "/latest").await.1, "latest");
        assert_eq!(
            get_with_host(&m, "nobody.test", "/latest").await.1,
            "latest"
        );
    }

    #[tokio::test]
    async fn an_unrouted_path_is_gos_404() {
        let (status, body, _) = get_with_host(&mount(), "one.test", "/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "404 page not found\n");
    }

    /// §6.6: routes are exact, so `/latest/` 404s rather than being redirected.
    #[tokio::test]
    async fn a_trailing_slash_is_not_normalised_onto_the_route() {
        let (status, body, _) = get_with_host(&mount(), "one.test", "/latest/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "404 page not found\n");
    }

    /// The constraint `mw_api_legacy::reply` records: exactly one `Vary`, capitalised,
    /// even though compression writes its own in lower case and the frozen surface writes
    /// one of its own.
    #[tokio::test]
    async fn there_is_exactly_one_vary_and_it_is_capitalised() {
        let m = Mount::new(
            vec![RouteTable::new(
                id("api:legacy"),
                RouteManifest::new(Scope::Any, vec![RouteEntry::exact("/v", Methods::ANY)]),
                Router::new().route(
                    "/v",
                    get(|| async {
                        Response::builder()
                            .header(header::VARY, "Accept-Encoding")
                            .body(Body::from("x"))
                            .expect("a legal response")
                    }),
                ),
            )],
            hosts(),
        )
        .expect("mounts")
        .0;
        assert_eq!(
            get_with_host(&m, "one.test", "/v").await.2,
            "Accept-Encoding"
        );
        // And on a response that set none at all.
        assert_eq!(
            get_with_host(&m, "one.test", "/nope").await.2,
            "Accept-Encoding"
        );
    }

    #[tokio::test]
    async fn a_catch_all_table_owns_the_404() {
        let catch = RouteTable::new(
            id("web:assets"),
            RouteManifest::new(Scope::Any, vec![RouteEntry::prefix("/", Methods::READ)]),
            Router::new().fallback(|| async { (StatusCode::NOT_FOUND, "assets say no") }),
        )
        .catch_all();
        let (m, report) = Mount::new(
            vec![table("api:legacy", Scope::Any, "/latest", "latest"), catch],
            hosts(),
        )
        .expect("mounts");
        assert_eq!(
            report.catch_all.as_ref().map(|c| c.as_str()),
            Some("web:assets")
        );
        assert_eq!(
            get_with_host(&m, "one.test", "/nope").await.1,
            "assets say no"
        );
        assert_eq!(get_with_host(&m, "one.test", "/latest").await.1, "latest");
    }

    #[test]
    fn two_catch_alls_are_refused() {
        let catch = |owner: &str| {
            RouteTable::new(
                id(owner),
                RouteManifest::new(Scope::Any, vec![]),
                Router::new(),
            )
            .catch_all()
        };
        let err = Mount::new(vec![catch("a:one"), catch("b:two")], hosts())
            .expect_err("two catch-alls cannot both be the fallback");
        assert!(matches!(err, MountError::TwoCatchAlls { .. }), "{err}");
    }

    #[test]
    fn two_tables_for_one_website_are_refused() {
        let err = Mount::new(
            vec![
                table("web:site:1", Scope::Site(WebsiteId::new(1)), "/", "a"),
                table("other:1", Scope::Site(WebsiteId::new(1)), "/x", "b"),
            ],
            hosts(),
        )
        .expect_err("the second would silently replace the first");
        assert!(matches!(err, MountError::TwoSiteTables { .. }), "{err}");
    }

    #[test]
    fn a_management_table_is_refused_rather_than_dropped() {
        let err = Mount::new(
            vec![table("api:management", Scope::Management, "/api/v1", "m")],
            hosts(),
        )
        .expect_err("the management API has its own socket");
        assert!(matches!(err, MountError::UnsupportedScope { .. }), "{err}");
    }

    /// The `ArcSwap` is observed by a request made across it, which is the property the
    /// plan diff will rest on at M7-1.
    #[tokio::test]
    async fn a_swap_is_observed_by_the_next_request() {
        let m = mount();
        assert_eq!(get_with_host(&m, "one.test", "/").await.1, "site one");
        m.swap(
            vec![table(
                "web:site:1",
                Scope::Site(WebsiteId::new(1)),
                "/",
                "renamed",
            )],
            hosts(),
        )
        .expect("the new generation mounts");
        assert_eq!(get_with_host(&m, "one.test", "/").await.1, "renamed");
    }

    #[tokio::test]
    async fn the_report_counts_what_was_mounted() {
        let (_, report) = Mount::new(
            vec![
                table("api:legacy", Scope::Any, "/latest", "latest"),
                table("web:site:1", Scope::Site(WebsiteId::new(1)), "/", "one"),
            ],
            hosts(),
        )
        .expect("mounts");
        assert_eq!(
            report,
            MountReport {
                shared: 1,
                sites: 1,
                paths: 2,
                catch_all: None,
                hosts: 2,
            }
        );
    }
}
