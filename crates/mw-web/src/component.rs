//! The two components, their route tables and the handlers.
//!
//! | Component | Scope | Routes |
//! |---|---|---|
//! | `web:site:<id>` | `Scope::Site(id)` | `/`, and `/portal` or `/config.json` |
//! | `web:static` | `Scope::Any`, **catch-all** | `/privacy`, `/support`, and the asset tree as the fallback |
//!
//! # Why there are two
//!
//! `docs/component-model.md` §2.6 names `web:site:<id>` and stops, and one component per
//! website is right for the *pages*: they are per-hostname, they render per-website values,
//! and `Scope::Site` is what makes three websites each declaring `/` not a route conflict.
//!
//! The asset tree is **not** per-website. The Go handlers for `/privacy` and `/support`
//! read the file straight out of the asset filesystem and never look at the website
//! (`handlers.go:819`, `:832`); §6.3 records that the two "bypass the website check
//! entirely", and the file server behind `router.PathPrefix("/")` has no website either. So
//! attaching them to a website's table would mean one of the following, all worse: three
//! copies of the asset tree for three websites, an arbitrary website owning everyone's
//! assets, or the assets disappearing when website 1 stops. `web:static` is the honest
//! shape, and it is recorded as an addition to §2.6's table rather than smuggled in.
//!
//! # `web:site:<id>` has a state slot; `web:static` does not
//!
//! The per-website pages are installed into a [`SiteState`] when the component reaches
//! `Running` and cleared on the way out, exactly as `api:legacy` does with its handler set
//! -- so "the table is mounted" and "the component is `Running`" are one fact, and a request
//! that arrives before the first reconcile pass gets W2a Decision 19's unmounted-declared-path
//! `503`. **None of those paths is frozen**, so **D-48** is not in play: the five frozen
//! patterns are `Scope::Any` and `api:legacy`'s, and `mw_http::MountError::FrozenPatternShadowed`
//! refuses a `Scope::Site` table that claims one.
//!
//! `web:static` has no slot and serves from the first byte the listener accepts, because its
//! answer is compiled into the binary and depends on nothing. A slot there would mean
//! `/privacy` answered `503` for the few milliseconds between the socket opening and the
//! supervisor's first pass, which is strictly worse for no gain.
//!
//! # The hard `LatestSnapshot` need, and what it costs
//!
//! §2.6 requires it `Hard` for `web:site:*`. It is declared, and this build **does not read
//! it**: the page is a function of configuration and the binary's version (§7.3 says so --
//! "`/` and `/portal` depend only on `(website, config_generation, binary_version)`"), and
//! every live value on it is fetched by the browser from `api:legacy`'s endpoints.
//!
//! Declaring it has one real consequence, and [`register`] handles it the way
//! `mw_api_legacy::register` handles the same thing: `plan()` refuses a configuration whose
//! hard need nobody provides, so a node with websites and **no station** would be a daemon
//! that will not start. So the registrar asks whether a `LatestSnapshot` provider will exist
//! and plans no website component when none will, logging which websites are therefore not
//! served.
//!
//! That is carrying on under the existing decision, which is the protocol. The amendment
//! worth considering -- dropping the need, since nothing reads it, which would let a
//! station-less node serve its website -- is recorded in `docs/reconciliation.md`'s
//! *Proposed amendments* rather than taken here.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri};
use axum::routing::any;
use axum::Router;
use mw_component::{
    Builder, Capability, ComponentError, ComponentId, ComponentIdError, ComponentSpec,
    ConfigDigest, Cx, LatestSnapshot, Methods, Need, Provision, Publisher, RouteEntry,
    RouteManifest, RouteProvider, Scope, Serve, Trigger,
};
use mw_http::RouteTable;
use mw_types::WebsiteId;
use tracing::{info, warn};

use crate::config::SiteView;
use crate::page;

/// `web:site:<id>`'s id prefix. A constant because a plan test and the log lines name it.
pub const SITE_PREFIX: &str = "web:site";

/// The asset tree's component id.
pub const STATIC_ID: &str = "web:static";

/// One website's component id.
pub fn site_id(website: WebsiteId) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("{SITE_PREFIX}:{website}"))
}

/// What this crate plans and renders from.
pub trait WebConfig {
    /// Every website, resolved against its primary station, in id order.
    fn web_sites(&self) -> &[SiteView];

    /// The daemon's version, which the page's footer renders.
    fn web_version(&self) -> &str;

    /// Whether some component in this configuration will provide `LatestSnapshot`.
    ///
    /// The same question `mw_api_legacy::LegacyConfig` asks, with the same one answer behind
    /// it (`mw_pipeline::snapshot::provides_latest_snapshot`) -- see the module
    /// documentation for why a website component has to ask it at all.
    fn web_has_latest_snapshot(&self) -> bool;

    /// The per-website state slots, built by `main` as a root resource.
    ///
    /// One per website, in the same order as [`WebConfig::web_sites`]. `None` is a wiring
    /// mistake in the boot sequence.
    fn web_states(&self) -> Option<&[Arc<SiteState>]>;
}

/// The slot one website's router reads and its component writes.
///
/// `ArcSwapOption` rather than a lock, for `mw_api_legacy::RouteState`'s reason: a reader
/// does one atomic load per request and never waits on the writer (**D-28**).
#[derive(Debug, Default)]
pub struct SiteState {
    page: ArcSwapOption<Rendered>,
}

/// One website's pages, rendered once per start (§7.3).
///
/// **The rendered-page cache, and it is not a performance claim.** At 217 requests a day
/// rendering per request would be free; this is here because it makes the website the same
/// shape as every other read path -- a component maintains a published artefact and a
/// handler writes bytes -- so there is one pattern in the codebase instead of two.
/// Invalidation is the plan diff: the component's `ConfigDigest` covers every value the
/// page interpolates, so a configuration change restarts it and re-renders.
#[derive(Debug)]
pub struct Rendered {
    site: SiteView,
    /// `/` -- the weather page, or the portal page for a portal website.
    root: Arc<[u8]>,
    /// The nine-value payload `/config.json` serves.
    config: Arc<[u8]>,
}

impl SiteState {
    /// An empty slot, which is what the binary builds before `plan()` runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current rendering, or `None` while the component is not running.
    pub fn rendered(&self) -> Option<Arc<Rendered>> {
        self.page.load_full()
    }

    fn install(&self, rendered: Arc<Rendered>) {
        self.page.store(Some(rendered));
    }

    fn clear(&self) {
        self.page.store(None);
    }

    /// [`SiteState::install`], for a test that drives the router without a supervisor.
    ///
    /// Named so that it reads as test-only at every call site, which is
    /// `mw_api_legacy::RouteState::install_for_test`'s reason.
    #[doc(hidden)]
    pub fn install_for_test(&self, site: &SiteView, version: &str) {
        self.install(Arc::new(
            render(site, version).expect("the test's site renders"),
        ));
    }
}

/// Render one website's pages.
fn render(site: &SiteView, version: &str) -> Result<Rendered, page::RenderError> {
    let root: Arc<[u8]> = if site.is_portal {
        // **D-75**: the portal template consumes nothing, so it is a static asset and `/`
        // and `/portal` are the same bytes.
        Arc::from(page::portal())
    } else if site.primary_station.is_some() {
        Arc::from(page::weather_station(site, version)?.into_bytes())
    } else {
        // `handlers.go:866`: a non-portal website with no resolvable primary device is a
        // 500 with that text, not a blank page. Rendered into the slot rather than decided
        // per request so that the handler has one shape.
        Arc::from(&b""[..])
    };
    Ok(Rendered {
        site: site.clone(),
        root,
        config: Arc::from(crate::config::config_json(site).into_bytes()),
    })
}

/// Every path one website claims.
///
/// **The manifest differs per website**, which is what makes `html-portal-on-non-portal`
/// work without a branch in a handler: a non-portal website does not declare `/portal` at
/// all, so the request falls through to the catch-all and gets `404 page not found\n` --
/// the same bytes Go's `http.NotFound` writes from inside `ServePortal`.
pub fn site_manifest(site: &SiteView) -> RouteManifest {
    let mut entries = vec![RouteEntry::exact("/", Methods::ANY)];
    if site.is_portal {
        entries.push(RouteEntry::exact("/portal", Methods::ANY));
    } else {
        // Additive (**D-79**), on every non-portal website.
        //
        // **Including one whose primary station is missing**, whose `/` is the `500` Go
        // gives and whose payload therefore describes nothing: `pullFromDevice` is `""` and
        // `stationID` is `0`. That is deliberate rather than overlooked -- M4-3's code
        // review found the comment here claiming "only where there is a station to
        // describe", which the code never did. Declaring it conditionally would make the
        // endpoint's presence depend on a `primary_station_key` that `ON DELETE SET NULL`
        // can clear at any moment, so a website would gain and lose a path on a station
        // delete; a payload of empty strings is the same answer `getAirQualityDeviceID`
        // gives for an absent binding.
        entries.push(RouteEntry::exact("/config.json", Methods::ANY));
    }
    RouteManifest::new(Scope::Site(site.id), entries)
}

/// Every path the asset tree **claims**, which is two.
///
/// `/privacy` and `/support` are exact, because the Go router declares them exactly.
///
/// **The asset tree itself declares nothing, and that is not an omission.** It is the Go
/// router's last entry -- `router.PathPrefix("/").Handler(http.FileServer(...))` -- and it
/// is mounted as the listener's *fallback* ([`RouteTable::catch_all`]), so it answers
/// exactly the paths nobody else claimed. Declaring it as `RouteEntry::prefix("/")` would
/// be truthful about what it *serves* and wrong about what it *claims*:
/// `mw_component::RouteEntry::conflicts_with` makes a prefix contain every path under it,
/// so `plan()` would report a `RouteConflict` between this table and `api:legacy`'s
/// `/latest` -- two components that are not in conflict at all, because a fallback cannot
/// shadow anything.
///
/// What that costs is stated rather than hidden: a component claiming `/css/weather.css`
/// would not be a plan error, and would simply win. That is the same "the narrower route
/// wins" behaviour `conflicts_with`'s own documentation describes for a literal beside a
/// parameter, and the asset tree is the one table for which it is always the right answer.
pub fn static_manifest() -> RouteManifest {
    RouteManifest::new(
        Scope::Any,
        vec![
            RouteEntry::exact("/privacy", Methods::ANY),
            RouteEntry::exact("/support", Methods::ANY),
        ],
    )
}

/// One website's route table.
pub fn site_table(site: &SiteView, state: Arc<SiteState>) -> Result<RouteTable, ComponentIdError> {
    let mut router = Router::new().route("/", any(root));
    router = if site.is_portal {
        router.route("/portal", any(root))
    } else {
        router.route("/config.json", any(config_json))
    };
    Ok(RouteTable::new(
        site_id(site.id)?,
        site_manifest(site),
        router.with_state(state),
    ))
}

/// The asset tree's route table, which is the listener's catch-all.
pub fn static_table() -> Result<RouteTable, ComponentIdError> {
    Ok(RouteTable::new(
        ComponentId::new(STATIC_ID)?,
        static_manifest(),
        Router::new()
            .route("/privacy", any(privacy))
            .route("/support", any(support))
            // The asset tree, and the 404 a miss gets. `fallback` rather than a
            // `/{*path}` route, because this table is mounted **as** the listener's
            // fallback and a route would then have to agree with the mount about which of
            // the two answers an unrouted path.
            .fallback(any(asset)),
    )
    .catch_all())
}

async fn root(State(state): State<Arc<SiteState>>) -> Response<Body> {
    let Some(rendered) = state.rendered() else {
        return unmounted();
    };
    if rendered.root.is_empty() {
        // `handlers.go:869`, reproduced: `http.Error` writes the message, a newline and
        // `text/plain; charset=utf-8`.
        return text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "No device configured for this website",
        );
    }
    html(&rendered.root, "text/html")
}

async fn config_json(State(state): State<Arc<SiteState>>) -> Response<Body> {
    let Some(rendered) = state.rendered() else {
        return unmounted();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        // Additive and per-website, so a shared cache must not reuse one website's payload
        // for another. `no-cache` rather than `no-store`: revalidation is fine, retention
        // is not the hazard.
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        .body(Body::from(rendered.config.to_vec()))
        .unwrap_or_else(|_| unmounted())
}

async fn privacy() -> Response<Body> {
    html(page::privacy(), "text/html; charset=utf-8")
}

async fn support() -> Response<Body> {
    html(page::support(), "text/html; charset=utf-8")
}

async fn asset(uri: Uri, headers: HeaderMap) -> Response<Body> {
    crate::assets::serve(&uri, &headers)
}

/// A rendered page, with the content type the recording carries.
///
/// **`text/html` with no charset for `/` and `/portal`**, and `text/html; charset=utf-8`
/// for `/privacy` and `/support`. That asymmetry is in the Go handlers --
/// `ServeWeatherWebsiteTemplate` and `ServePortal` set the first, `ServePrivacy` and
/// `ServeSupport` the second -- and `contract/fixtures/html-weather-station` and
/// `html-privacy` assert both, so it is reproduced rather than tidied.
fn html(body: &[u8], content_type: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(content_type))
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        .body(Body::from(body.to_vec()))
        .unwrap_or_else(|_| unmounted())
}

/// Go's `http.Error`: the message, a newline, `text/plain; charset=utf-8`.
fn text(status: StatusCode, message: &str) -> Response<Body> {
    let mut body = String::with_capacity(message.len() + 1);
    body.push_str(message);
    body.push('\n');
    Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        .body(Body::from(body))
        .unwrap_or_else(|_| unmounted())
}

/// What a declared path gets while its component is not running: W2a Decision 19's
/// unmounted-declared-path `503`, with no body.
fn unmounted() -> Response<Body> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    r.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    r
}

/// Why the website components cannot be planned.
#[derive(Debug, thiserror::Error)]
pub enum WebPlanError {
    /// A website's id does not make a usable component id.
    #[error("a website id does not make a usable component id")]
    Id(#[from] ComponentIdError),

    /// `main` did not build the per-website slots.
    #[error(
        "the boot sequence did not build the website state slots, so no `{SITE_PREFIX}:*` \
         can be planned; `crates/modernweather/src/boot.rs` builds them before `plan()`"
    )]
    NoState,

    /// The slot count and the website count disagree -- a wiring mistake that would
    /// otherwise mount one website's page under another's hostname.
    #[error("{sites} websites and {states} state slots; the boot sequence built one per website")]
    StateCount {
        /// How many websites the configuration holds.
        sites: usize,
        /// How many slots `main` built.
        states: usize,
    },

    /// A page could not be rendered at plan time.
    ///
    /// Unreachable with the current template -- askama's checking is what makes a missing
    /// field a build error -- and carried rather than `expect`ed because the renderer's
    /// signature can fail and a registrar that panicked would take the daemon down for a
    /// configuration it could have refused.
    #[error("a website's page could not be rendered")]
    Render(#[from] page::RenderError),
}

/// Plan one component per website, plus the asset tree.
pub fn register<C: WebConfig>(cfg: &C, out: &mut Vec<ComponentSpec>) -> Result<(), WebPlanError> {
    // The asset tree depends on nothing and is planned on every node: `/privacy` and
    // `/support` are served by a daemon with no website at all, which is §6.3's own
    // asymmetry.
    out.push(
        ComponentSpec::new(
            ComponentId::new(STATIC_ID)?,
            Trigger::Service,
            Builder::serve(|cx| async move {
                let routes = cx.publisher::<Arc<dyn RouteProvider>>()?;
                Ok(Static { routes })
            }),
        )
        .provides([Provision::routes(static_manifest())]),
    );

    let sites = cfg.web_sites();
    if sites.is_empty() {
        return Ok(());
    }
    if !cfg.web_has_latest_snapshot() {
        warn!(
            websites = sites.len(),
            "no component in this configuration provides LatestSnapshot, so no \
             `{SITE_PREFIX}:*` is planned -- the hard need docs/component-model.md §2.6 \
             declares would refuse the whole configuration. The asset tree, `/privacy` and \
             `/support` still serve; `/` and `/portal` answer `404 page not found`. A node \
             reaches this with no station, or with neither a spool nor a weather database."
        );
        return Ok(());
    }
    let states = cfg.web_states().ok_or(WebPlanError::NoState)?;
    if states.len() != sites.len() {
        return Err(WebPlanError::StateCount {
            sites: sites.len(),
            states: states.len(),
        });
    }

    let version = cfg.web_version().to_string();
    for (site, state) in sites.iter().zip(states) {
        let id = site_id(site.id)?;
        // Rendered at **plan** time rather than at build time, so that a template failure
        // is a configuration refusal rather than a component that backs off for ever.
        let rendered = Arc::new(render(site, &version)?);
        let state = Arc::clone(state);
        let digest = ConfigDigest::of(&digest_input(site, &version));
        out.push(
            ComponentSpec::new(
                id,
                Trigger::Service,
                Builder::serve(move |cx| {
                    let state = Arc::clone(&state);
                    let rendered = Arc::clone(&rendered);
                    async move {
                        let routes = cx.publisher::<Arc<dyn RouteProvider>>()?;
                        // Declared `Hard` by §2.6 and resolved so that the declaration is
                        // not a fiction -- see the module documentation for what this build
                        // does and does not do with it.
                        let _snapshot = cx.hard::<Arc<dyn LatestSnapshot>>()?;
                        info!(
                            website = %rendered.site.id,
                            hostname = %rendered.site.hostname,
                            portal = rendered.site.is_portal,
                            bytes = rendered.root.len(),
                            "a website is mounted"
                        );
                        Ok(Site {
                            state,
                            rendered,
                            routes,
                        })
                    }
                }),
            )
            .requires([Need::hard(Capability::LatestSnapshot)])
            .provides([Provision::routes(site_manifest(site))])
            .config_digest(digest),
        );
    }
    Ok(())
}

/// Every route table this crate contributes, in the order the mount wants them.
///
/// Built by the binary beside [`register`] and from the same configuration, which is the
/// shape `mw_api_legacy::routes` established: the manifest travels on the `Provision` so
/// that `plan()` can see a conflict, and the router travels as a root resource because
/// `mw-component` may not name `axum`.
pub fn tables<C: WebConfig>(cfg: &C) -> Result<Vec<RouteTable>, WebPlanError> {
    let mut tables = vec![static_table()?];
    let sites = cfg.web_sites();
    if sites.is_empty() || !cfg.web_has_latest_snapshot() {
        return Ok(tables);
    }
    let states = cfg.web_states().ok_or(WebPlanError::NoState)?;
    if states.len() != sites.len() {
        return Err(WebPlanError::StateCount {
            sites: sites.len(),
            states: states.len(),
        });
    }
    for (site, state) in sites.iter().zip(states) {
        tables.push(site_table(site, Arc::clone(state))?);
    }
    Ok(tables)
}

/// A canonical text rendering of everything one website's page depends on.
///
/// Text rather than `#[derive(Hash)]` for `mw_api_legacy::component::digest_input`'s
/// reason: two fields are `Option<f64>` and `f64` is deliberately not `Hash`.
fn digest_input(site: &SiteView, version: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(256);
    let _ = write!(
        out,
        "version={version}|id={}|host={}|name={}|title={}|apple={}|portal={}|radar={} {}|\
         snow={} {}|aq={} {} {}|station={:?} {} {}|loc={:?} {:?}|aeris={}",
        site.id,
        site.hostname,
        site.name,
        site.page_title,
        site.apple_app_id,
        site.is_portal,
        site.radar_enabled,
        site.radar_token,
        site.snow_enabled,
        site.snow_device,
        site.air_quality_enabled,
        site.air_quality_device,
        site.air_quality_device_id,
        site.primary_station,
        site.primary_station_name,
        site.primary_station_id,
        site.latitude.map(f64::to_bits),
        site.longitude.map(f64::to_bits),
        site.aeris_enabled,
    );
    out
}

/// The object a route provider publishes under `Capability::HttpRoutes`.
#[derive(Debug)]
struct Mounted;

impl RouteProvider for Mounted {}

/// `web:static`'s body: publish, hold, revoke.
struct Static {
    routes: Publisher<Arc<dyn RouteProvider>>,
}

impl std::fmt::Debug for Static {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Static").finish_non_exhaustive()
    }
}

impl Serve for Static {
    fn serve(
        self: Box<Self>,
        cx: Cx,
    ) -> mw_component::BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            self.routes
                .publish(Arc::new(Mounted) as Arc<dyn RouteProvider>);
            cx.cancelled().await;
            self.routes.revoke();
            Ok(())
        })
    }
}

/// `web:site:<id>`'s body: install, hold, clear.
struct Site {
    state: Arc<SiteState>,
    rendered: Arc<Rendered>,
    routes: Publisher<Arc<dyn RouteProvider>>,
}

impl std::fmt::Debug for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Site")
            .field("website", &self.rendered.site.id)
            .finish_non_exhaustive()
    }
}

impl Serve for Site {
    fn serve(
        self: Box<Self>,
        cx: Cx,
    ) -> mw_component::BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            // Installed before the capability is published, so a consumer that reacts to
            // the publication cannot observe a mounted-but-empty table.
            self.state.install(Arc::clone(&self.rendered));
            self.routes
                .publish(Arc::new(Mounted) as Arc<dyn RouteProvider>);
            cx.cancelled().await;
            // Revoke before clearing, which is the ordering §3.6 asks of a provider.
            self.routes.revoke();
            self.state.clear();
            info!(website = %self.rendered.site.id, "a website is unmounted");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{portal_site, suncrest};

    #[derive(Debug)]
    struct Cfg {
        sites: Vec<SiteView>,
        states: Option<Vec<Arc<SiteState>>>,
        snapshot: bool,
    }

    impl Cfg {
        fn new(sites: Vec<SiteView>) -> Self {
            let states = sites.iter().map(|_| Arc::new(SiteState::new())).collect();
            Self {
                sites,
                states: Some(states),
                snapshot: true,
            }
        }
    }

    impl WebConfig for Cfg {
        fn web_sites(&self) -> &[SiteView] {
            &self.sites
        }
        fn web_version(&self) -> &str {
            "0.1.0"
        }
        fn web_has_latest_snapshot(&self) -> bool {
            self.snapshot
        }
        fn web_states(&self) -> Option<&[Arc<SiteState>]> {
            self.states.as_deref()
        }
    }

    #[test]
    fn one_component_per_website_plus_the_asset_tree() {
        let mut out = Vec::new();
        register(&Cfg::new(vec![suncrest(), portal_site()]), &mut out).expect("plans");
        let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["web:static", "web:site:1", "web:site:3"]);
        for spec in &out {
            assert!(matches!(spec.trigger, Trigger::Service));
            assert_eq!(spec.provides.len(), 1);
        }
    }

    /// §2.6's hard need, declared -- and the asset tree's absence of one, which is what
    /// lets `/privacy` serve on a node with no station.
    #[test]
    fn the_site_need_is_hard_and_the_static_tree_has_none() {
        let mut out = Vec::new();
        register(&Cfg::new(vec![suncrest()]), &mut out).expect("plans");
        let statics = out
            .iter()
            .find(|s| s.id.as_str() == STATIC_ID)
            .expect("planned");
        assert!(statics.requires.is_empty());
        let site = out
            .iter()
            .find(|s| s.id.as_str() == "web:site:1")
            .expect("planned");
        assert_eq!(
            site.requires,
            vec![Need::hard(Capability::LatestSnapshot)],
            "docs/component-model.md §2.6"
        );
    }

    /// The consequence of that hard need, handled the way `api:legacy` handles it: no
    /// website component rather than a daemon that refuses to start.
    #[test]
    fn with_no_snapshot_provider_the_websites_are_not_planned_but_the_assets_are() {
        let mut cfg = Cfg::new(vec![suncrest()]);
        cfg.snapshot = false;
        let mut out = Vec::new();
        register(&cfg, &mut out).expect("plans");
        assert_eq!(
            out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["web:static"]
        );
        // And the tables agree with the plan, which is the pairing M4-2's review found
        // broken the other way round for `api:legacy`.
        let tables = tables(&cfg).expect("tables");
        assert_eq!(tables.len(), 1);
        assert!(tables[0].is_catch_all());
    }

    #[test]
    fn a_portal_declares_portal_and_a_site_declares_config_json() {
        let site = site_manifest(&suncrest());
        assert_eq!(
            site.entries
                .iter()
                .map(|e| e.pattern.as_str())
                .collect::<Vec<_>>(),
            vec!["/", "/config.json"]
        );
        let portal = site_manifest(&portal_site());
        assert_eq!(
            portal
                .entries
                .iter()
                .map(|e| e.pattern.as_str())
                .collect::<Vec<_>>(),
            vec!["/", "/portal"]
        );
    }

    /// The manifest and the router are built from the same `SiteView`, so a pattern in one
    /// and not the other is the mismatch §6.2 calls `BuildError::UndeclaredRoute`. There is
    /// no way to read a `Router`'s paths back out, so this asserts the next best thing: the
    /// two functions agree about which shape a website is.
    #[test]
    fn the_table_and_the_manifest_agree_about_every_website() {
        for site in [suncrest(), portal_site()] {
            let state = Arc::new(SiteState::new());
            let table = site_table(&site, state).expect("a table");
            assert_eq!(table.manifest(), &site_manifest(&site));
            assert_eq!(table.scope(), Scope::Site(site.id));
            assert_eq!(table.owner().as_str(), format!("web:site:{}", site.id));
            assert!(!table.is_catch_all());
        }
    }

    #[test]
    fn the_asset_tree_is_the_catch_all() {
        let table = static_table().expect("a table");
        assert!(table.is_catch_all());
        assert_eq!(table.scope(), Scope::Any);
        assert_eq!(table.owner().as_str(), STATIC_ID);
    }

    #[test]
    fn a_missing_state_slot_is_a_named_wiring_error() {
        let mut cfg = Cfg::new(vec![suncrest()]);
        cfg.states = None;
        let mut out = Vec::new();
        assert!(matches!(
            register(&cfg, &mut out).expect_err("the slots must exist"),
            WebPlanError::NoState
        ));
        cfg.states = Some(Vec::new());
        let mut out = Vec::new();
        assert!(matches!(
            register(&cfg, &mut out).expect_err("one slot per website"),
            WebPlanError::StateCount {
                sites: 1,
                states: 0
            }
        ));
    }

    /// The digest is what re-renders the page when a website is renamed. A field the
    /// renderer reads and the digest does not is a page that keeps the old value until the
    /// daemon restarts.
    #[test]
    fn the_digest_follows_every_value_the_page_interpolates() {
        let base = suncrest();
        let digest = |s: &SiteView, v: &str| ConfigDigest::of(&digest_input(s, v));
        let d = digest(&base, "0.1.0");
        assert_eq!(d, digest(&base, "0.1.0"));
        assert_ne!(d, digest(&base, "0.2.0"), "the footer renders the version");
        /// One field's name and a mutation of it, which is what the sweep below is.
        type Mutation = (&'static str, Box<dyn Fn(&mut SiteView)>);
        let mutate: Vec<Mutation> = vec![
            ("name", Box::new(|s: &mut SiteView| s.name = "other".into())),
            (
                "hostname",
                Box::new(|s: &mut SiteView| s.hostname = "other.test".into()),
            ),
            (
                "page_title",
                Box::new(|s: &mut SiteView| s.page_title = "other".into()),
            ),
            (
                "apple_app_id",
                Box::new(|s: &mut SiteView| s.apple_app_id = "1".into()),
            ),
            ("is_portal", Box::new(|s: &mut SiteView| s.is_portal = true)),
            (
                "radar_enabled",
                Box::new(|s: &mut SiteView| s.radar_enabled = false),
            ),
            (
                "radar_token",
                Box::new(|s: &mut SiteView| s.radar_token = "t".into()),
            ),
            (
                "snow_enabled",
                Box::new(|s: &mut SiteView| s.snow_enabled = false),
            ),
            (
                "snow_device",
                Box::new(|s: &mut SiteView| s.snow_device = "g".into()),
            ),
            (
                "air_quality_enabled",
                Box::new(|s: &mut SiteView| s.air_quality_enabled = false),
            ),
            (
                "air_quality_device",
                Box::new(|s: &mut SiteView| s.air_quality_device = "a".into()),
            ),
            (
                "air_quality_device_id",
                Box::new(|s: &mut SiteView| s.air_quality_device_id = 9),
            ),
            (
                "primary_station_name",
                Box::new(|s: &mut SiteView| s.primary_station_name = "x".into()),
            ),
            (
                "primary_station_id",
                Box::new(|s: &mut SiteView| s.primary_station_id = 9),
            ),
            (
                "latitude",
                Box::new(|s: &mut SiteView| s.latitude = Some(1.0)),
            ),
            (
                "longitude",
                Box::new(|s: &mut SiteView| s.longitude = Some(1.0)),
            ),
            (
                "aeris_enabled",
                Box::new(|s: &mut SiteView| s.aeris_enabled = false),
            ),
            ("id", Box::new(|s: &mut SiteView| s.id = WebsiteId::new(9))),
            (
                "primary_station",
                Box::new(|s: &mut SiteView| s.primary_station = None),
            ),
        ];
        for (field, apply) in mutate {
            let mut changed = base.clone();
            apply(&mut changed);
            assert_ne!(
                d,
                digest(&changed, "0.1.0"),
                "`{field}` reaches the page and must reach the digest"
            );
        }
    }
}
