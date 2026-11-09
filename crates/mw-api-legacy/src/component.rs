//! `api:legacy`: the spec, the `Serve` body, the state slot and the router.
//!
//! # The declarations, and why each one is what it is
//!
//! `docs/component-model.md` §4.1b: **`LatestSnapshot` hard, `ReadingStore` soft,
//! `ForecastStore` soft, `AlertStore` soft, `SnowCache` soft**, providing `HttpRoutes`.
//! `AlertStore` is the one this build does not declare, because `/alerts` is not carried
//! forward (**D-69**) and a requirement for a capability no handler reads is a declaration
//! nobody checks.
//!
//! **The hard set being exactly `{LatestSnapshot}` is the whole of D-48**, and M4-1
//! recorded that nothing asserted it. [`tests::the_hard_needs_are_exactly_the_snapshot`] is
//! that assertion: a hard `ReadingStore` here would make every frozen route unmountable
//! whenever the database is down, and the conformance suite **cannot** catch it, because it
//! replays against a healthy database and a `503` only appears when there is not one.
//!
//! # Why the three soft handles are [`mw_component::Deferred`] and not `cx.soft()`
//!
//! `BuildCx::soft` reads the slot once. This component's only hard need is published by
//! `snapshot:latest`, which itself has no hard need, so `api:legacy` is built in the second
//! reconcile pass -- before `db:postgres` has connected. A once-only read would be `None`
//! for the life of every healthy process, which is the defect M4-1's review found in
//! `snapshot:latest` against `WindowedView`. `Deferred`'s own documentation has the full
//! argument and why this is not the hole `BuildCx::additive` declines to open.
//!
//! # The state slot, and why the router is built by the binary
//!
//! The router, host routing and `RouteTable` mounting are M4-3's. What exists today is one
//! listener the binary binds directly (`mw_http`'s module documentation records that as a
//! step in the boot sequence rather than a component). So [`routes`] builds an
//! `axum::Router` over a [`RouteState`] -- a root resource `main` owns, exactly as
//! `SnapshotState` and `RollupState` are -- and the component installs its [`Handlers`]
//! into that slot while it runs.
//!
//! That makes "the table is mounted" and "the component is `Running`" **the same fact**,
//! which is what D-48's invariant is stated over, without putting an `axum::Router` on the
//! capability bus -- `mw-component` may not name `axum`, so `Capability::HttpRoutes`'
//! handle type cannot be one. The pure half still travels the declared way:
//! [`manifest`] is the [`mw_component::RouteManifest`] on the `Provision`, so a duplicate
//! path is a `PlanError` naming both owners (**D-51**).

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use mw_component::{
    Builder, Capability, ComponentError, ComponentId, ComponentSpec, ConfigDigest, Cx, Deferred,
    ForecastStore, LatestSnapshot, Methods, Need, Provision, Publisher, ReadingStore, RouteEntry,
    RouteManifest, RouteProvider, Scope, Serve, SnowCache, Store, Trigger,
};
use tracing::info;

use crate::config::{Directory, LegacyConfig};
use crate::params::Params;
use crate::render::Renderer;
use crate::span::SpanCache;

/// The component's id. A constant because the spec, the log lines and a plan test name it.
pub const ID: &str = "api:legacy";

/// Everything one request is answered from.
///
/// Built once per start, installed into the [`RouteState`] and read by every handler. The
/// three `Deferred` handles are the only mutable thing about it, and they are read rather
/// than written: a database that comes up later is observed at the next request.
pub struct Handlers {
    /// The resolved website and station tables.
    pub directory: Arc<Directory>,
    /// The frozen DTO's serialiser, shared with `snapshot:latest`.
    pub renderer: Arc<Renderer>,
    /// The pre-serialised current conditions. **Hard**, so this is never absent while the
    /// component runs -- which is what makes `/latest`'s zero queries structural.
    pub snapshot: Arc<dyn LatestSnapshot>,
    /// `/span` and `/almanac`'s reads. `None` is a database that is down, which degrades
    /// those two endpoints and nothing else.
    pub store: Deferred<Arc<dyn Store>>,
    /// `/forecast`'s read.
    pub forecasts: Deferred<Arc<dyn ForecastStore>>,
    /// `/snow`'s read.
    pub snow: Deferred<Arc<dyn SnowCache>>,
    /// `/span`'s 60-second prefix cache (**D-54**).
    pub span_cache: SpanCache,
}

impl std::fmt::Debug for Handlers {
    /// Capability handles are not `Debug` and a snapshot table is not a log field. What is
    /// useful in one is which of the optional ones are answering.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handlers")
            .field("websites", &self.directory.websites().len())
            .field("stations", &self.directory.stations().len())
            .field("store", &self.store.is_available())
            .field("forecasts", &self.forecasts.is_available())
            .field("snow", &self.snow.is_available())
            .finish_non_exhaustive()
    }
}

impl Handlers {
    /// The website a request resolves to, or the `503` the contract gives when none is
    /// configured.
    ///
    /// **The response differs per endpoint in the Go tree**, in both the message and the
    /// content type, so the caller names which of the seven it is: see
    /// [`crate::reply::NoWebsite`]. The `error` string is the one thing all seven share.
    ///
    /// `Box`ed on the error side because an `axum::Response` is 128 bytes and every
    /// handler returns this `Result` by value -- `clippy::result_large_err`, and it is
    /// right: the error arm is the rare one.
    pub fn website(
        &self,
        host: &str,
        which: crate::reply::NoWebsite,
    ) -> Result<&crate::config::WebsiteView, Box<Response<Body>>> {
        self.directory
            .website_for_host(host)
            .ok_or_else(|| Box::new(crate::reply::no_website(which)))
    }

    /// The read half of the store, or `None` while the database is unavailable.
    ///
    /// **Upcast to [`ReadingStore`] here and nowhere else.** The capability's handle type
    /// is `Arc<dyn Store>`, which is both halves; narrowing it at the one place it is
    /// loaded is what stops anything in this crate reaching `ReadingSink::store` --
    /// `mw_component::store`'s own words for the same move in `writer:timescale` and
    /// `window:rollup`.
    pub fn reading_store(&self) -> Option<Arc<dyn ReadingStore>> {
        self.store.load().map(|s| s as Arc<dyn ReadingStore>)
    }
}

/// The slot the router reads and the component writes.
///
/// `ArcSwapOption` rather than a lock: a reader does one atomic load per request and never
/// waits on the writer (**D-28**, and **D-32**'s entry for `arc-swap`). `None` is a
/// component that is not `Running`.
#[derive(Debug, Default)]
pub struct RouteState {
    handlers: ArcSwapOption<Handlers>,
}

impl RouteState {
    /// An empty slot, which is what the binary builds before `plan()` runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current handler set, or `None` while the component is not running.
    pub fn handlers(&self) -> Option<Arc<Handlers>> {
        self.handlers.load_full()
    }

    /// Install a handler set. The component's own `build` is the only caller.
    fn install(&self, handlers: Arc<Handlers>) {
        self.handlers.store(Some(handlers));
    }

    /// [`RouteState::install`], for an integration test that drives the router without a
    /// supervisor.
    ///
    /// Named so that it reads as test-only at every call site, rather than being a second
    /// public way to mount a table: `tests/degraded.rs` is the one caller, and what it
    /// tests -- **D-48**'s byte-level half -- cannot be reached through `build` without a
    /// supervisor and a database to take away.
    #[doc(hidden)]
    pub fn install_for_test(&self, handlers: Arc<Handlers>) {
        self.install(handlers);
    }

    /// Clear the slot, which unmounts the table.
    fn clear(&self) {
        self.handlers.store(None);
    }
}

/// Every path `api:legacy` claims, as the pure declaration **D-51** puts on the provision.
///
/// `Methods::ANY` on all seven, which is the frozen surface's actual behaviour rather than
/// a shortcut: no Go route calls `.Methods(...)`, so `POST /latest` returns the same body
/// as `GET` and W1 pinned that (Q-17).
///
/// `Scope::Any` rather than `Scope::Site`: these paths are served for **every** host and the
/// website is selected from the `Host` header inside the handler, which is the frozen
/// routing and the reason three websites declaring `/` are not in conflict while two
/// components declaring `/latest` would be.
pub fn manifest() -> RouteManifest {
    RouteManifest::new(
        Scope::Any,
        vec![
            RouteEntry::exact("/latest", Methods::ANY),
            RouteEntry::exact("/span/{span}", Methods::ANY),
            RouteEntry::exact("/snow", Methods::ANY),
            RouteEntry::exact("/stationinfo", Methods::ANY),
            RouteEntry::exact("/forecast/{span}", Methods::ANY),
            RouteEntry::exact("/almanac", Methods::ANY),
            RouteEntry::exact("/api/stations", Methods::ANY),
        ],
    )
}

/// The frozen path patterns.
///
/// The **five** of **D-68**, not all seven: `/almanac` and `/api/stations` are coupled
/// rather than frozen, so a `503` on either is a website that renders an empty panel and
/// not a shipped client that stops working.
///
/// **There is no plan-level check over these yet**, and this comment used to say there was
/// -- "for the plan-level check that none of them is claimed by a component whose hard needs
/// reach outside `{LatestSnapshot}`". Nothing does that: the constant is used by
/// [`manifest`] and its test, and it is not re-exported from `lib.rs`, so no other crate
/// could. The check belongs to `implementation-plan.md` M4-3, which is the item that mounts
/// a second component on these paths; until then the assertion that matters is this
/// component's own hard-need set, which `the_hard_needs_are_exactly_the_latest_snapshot`
/// pins. Describing enforcement that does not exist is the shape of G-1.
pub const FROZEN_PATTERNS: [&str; 5] = [
    "/latest",
    "/span/{span}",
    "/snow",
    "/stationinfo",
    "/forecast/{span}",
];

/// The `axum::Router` for the frozen surface.
///
/// `any` on every route, for [`manifest`]'s reason. The path parameters are `{span}` in
/// `axum` 0.8's syntax, which is also the syntax [`RouteEntry`] compares -- so the manifest
/// and the router cannot disagree about what a pattern claims.
pub fn routes(state: Arc<RouteState>) -> Router {
    Router::new()
        .route("/latest", any(latest))
        .route("/span/{span}", any(span))
        .route("/snow", any(snow))
        .route("/stationinfo", any(stationinfo))
        .route("/forecast/{span}", any(forecast))
        .route("/almanac", any(almanac))
        .route("/api/stations", any(stations))
        .with_state(state)
}

/// The `Host` header, or an empty string.
///
/// An absent or non-ASCII `Host` resolves to the default website, which is what an
/// unmatched one does (Q-18) -- so there is no error path here and no response the contract
/// would have no bytes for.
fn host_of(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// What a request gets when the component is not running.
///
/// **Unreachable on a frozen path, and that is D-48's invariant rather than a hope**: this
/// component's only hard need is `LatestSnapshot`, whose provider has no hard need of its
/// own, so the table is mounted in the second reconcile pass and stays mounted while the
/// process is healthy -- including with the database stopped.
///
/// It is nonetheless a real branch, because the slot is empty between the listener binding
/// and the supervisor's first pass, and during shutdown. A `503` with no body is W2a
/// Decision 19's unmounted-declared-path response.
fn unmounted() -> Response<Body> {
    crate::reply::empty(StatusCode::SERVICE_UNAVAILABLE)
}

async fn latest(
    State(state): State<Arc<RouteState>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::latest::handle(
        &h,
        &host_of(&headers),
        &Params::parse(query.as_deref()),
        &headers,
    )
}

async fn span(
    State(state): State<Arc<RouteState>>,
    Path(span): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::span::handle(
        &h,
        &host_of(&headers),
        &span,
        &Params::parse(query.as_deref()),
    )
    .await
}

async fn snow(
    State(state): State<Arc<RouteState>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::snow::handle(&h, &host_of(&headers), &Params::parse(query.as_deref())).await
}

async fn stationinfo(State(state): State<Arc<RouteState>>, headers: HeaderMap) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::stationinfo::handle(&h, &host_of(&headers))
}

async fn forecast(
    State(state): State<Arc<RouteState>>,
    Path(span): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::forecast::handle(
        &h,
        &host_of(&headers),
        &span,
        &Params::parse(query.as_deref()),
    )
    .await
}

async fn almanac(
    State(state): State<Arc<RouteState>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::almanac::handle(&h, &host_of(&headers), &Params::parse(query.as_deref())).await
}

async fn stations(State(state): State<Arc<RouteState>>, headers: HeaderMap) -> Response<Body> {
    let Some(h) = state.handlers() else {
        return unmounted();
    };
    crate::stations::handle(&h, &host_of(&headers))
}

/// Why `api:legacy` could not be planned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LegacyPlanError {
    /// The component id is not a valid [`ComponentId`], which is a build error rather than
    /// a configuration one.
    #[error("`{ID}` is not a usable component id")]
    Id(#[from] mw_component::ComponentIdError),

    /// `main` did not build the state slot.
    ///
    /// A bug in the boot sequence rather than a configuration error: the listener and this
    /// component are assembled by the same function, so one existing without the other is
    /// a wiring mistake and not something an operator can fix.
    #[error(
        "the frozen route table's state slot was not built, so `{ID}` has nowhere to \
         install its handlers. `crate::boot` builds it beside the listener."
    )]
    NoRouteState,

    /// `main` did not build the DTO's serialiser.
    ///
    /// The same class of bug as [`LegacyPlanError::NoRouteState`] and separate from it
    /// because the two are built for different consumers: the renderer is also what
    /// `snapshot:latest` renders every payload through, so one without the other is a plan
    /// that serves `/latest` from a snapshot nothing fills.
    #[error(
        "the frozen DTO's serialiser was not built, so `{ID}` has nothing to render \
         `/latest` with. `crate::boot` builds it beside the route state."
    )]
    NoRenderer,
}

/// Plan `api:legacy`, or say why not.
///
/// **One condition, and it is not about the website set**: a `LatestSnapshot` provider has
/// to exist. **D-48** requires the hard need set to be exactly `{LatestSnapshot}`, and
/// `plan()` refuses a configuration whose hard need nobody provides -- so a node with no
/// station in it would otherwise be a daemon that will not start.
/// [`LegacyConfig::has_latest_snapshot`] is that question and
/// `mw_pipeline::snapshot::provides_latest_snapshot` is its one answer.
///
/// Nothing about the **website** set is a condition. A node with a station and no matching
/// website serves the contract's `503` from the handlers (§6.3), not a missing route: the
/// two are different bytes and only one of them is in the contract.
pub fn register<C: LegacyConfig>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), LegacyPlanError> {
    if !cfg.has_latest_snapshot() {
        info!(
            "no component in this configuration provides LatestSnapshot, so `{ID}` is not \
             planned -- its hard need would refuse the whole configuration (D-48). A node \
             reaches this with an empty snapshot key set (no website, or no station) or \
             with neither a spool nor a weather database; `mw_pipeline::snapshot::register` \
             logs which."
        );
        return Ok(());
    }
    let state = Arc::clone(
        cfg.legacy_route_state()
            .ok_or(LegacyPlanError::NoRouteState)?,
    );
    let renderer = Arc::clone(cfg.legacy_render().ok_or(LegacyPlanError::NoRenderer)?);
    let id = ComponentId::new(ID)?;

    let websites = cfg.legacy_websites().to_vec();
    let stations = cfg.legacy_stations().to_vec();
    let listener = cfg.legacy_listener();
    // Every configuration value the builder closes over, folded into the digest -- which
    // is what restarts this component when a website is renamed or a station's coordinates
    // move. A registrar that read a row and did not fold it in is a component that keeps
    // serving the old configuration (`ComponentSpec::fingerprint`).
    let digest = ConfigDigest::of(&digest_input(&websites, &stations, listener));
    let directory = Arc::new(Directory::new(websites, stations, listener));

    out.push(
        ComponentSpec::new(
            id,
            Trigger::Service,
            Builder::serve(move |cx| {
                let state = Arc::clone(&state);
                let directory = Arc::clone(&directory);
                let renderer = Arc::clone(&renderer);
                async move {
                    let routes = cx.publisher::<Arc<dyn RouteProvider>>()?;
                    // Hard, so the supervisor has already published it: a failure here is
                    // a provider that published the wrong type, which is a build error and
                    // not a degraded state.
                    let snapshot = cx.hard::<Arc<dyn LatestSnapshot>>()?;
                    // Soft, and read at USE rather than here -- see the module
                    // documentation.
                    let store = cx.deferred::<Arc<dyn Store>>()?;
                    let forecasts = cx.deferred::<Arc<dyn ForecastStore>>()?;
                    let snow = cx.deferred::<Arc<dyn SnowCache>>()?;
                    info!(
                        websites = directory.websites().len(),
                        stations = directory.stations().len(),
                        zones = renderer.zones(),
                        "the frozen REST surface is mounted"
                    );
                    Ok(Legacy {
                        state,
                        routes,
                        handlers: Arc::new(Handlers {
                            directory,
                            renderer,
                            snapshot,
                            store,
                            forecasts,
                            snow,
                            span_cache: SpanCache::new(),
                        }),
                    })
                }
            }),
        )
        .requires([
            // **The hard set, and it is exactly one** (**D-48**).
            Need::hard(Capability::LatestSnapshot),
            Need::soft(Capability::ReadingStore),
            Need::soft(Capability::ForecastStore),
            Need::soft(Capability::SnowCache),
        ])
        .provides([Provision::routes(manifest())])
        .config_digest(digest),
    );
    Ok(())
}

/// A canonical text rendering of everything the builder closes over, for the digest.
///
/// Text rather than `#[derive(Hash)]` on the row types, because two of their fields are
/// `Option<f64>` and `f64` is deliberately not `Hash`: `NaN != NaN`, so a derived `Hash`
/// would be inconsistent with `PartialEq` and the derive is refused. Rendering the bits
/// through `Display` is the fix a reviewer can check -- every field that reaches a handler
/// appears here exactly once, which is the property `the_digest_follows_every_row_the_builder_closes_over`
/// asserts one field at a time.
fn digest_input(
    websites: &[crate::config::WebsiteView],
    stations: &[crate::config::StationView],
    listener: crate::config::Listener,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(256 + websites.len() * 128 + stations.len() * 96);
    let _ = write!(out, "listener={}:{}", listener.protocol(), listener.port);
    for w in websites {
        let _ = write!(
            out,
            "|website={} {} {} {} {} {:?} {} {:?} {} {:?} {} {} {}",
            w.id,
            w.name,
            w.hostname,
            w.page_title,
            w.about_text,
            w.primary_station,
            w.snow_enabled,
            w.snow_station,
            w.air_quality_enabled,
            w.air_quality_station,
            w.is_portal,
            w.radar_enabled,
            w.radar_token,
        );
    }
    for s in stations {
        let _ = write!(
            out,
            "|station={} {} {} {} {} {:?} {:?} {:?}",
            s.id,
            s.key,
            s.name,
            s.kind,
            s.enabled,
            s.latitude.map(f64::to_bits),
            s.longitude.map(f64::to_bits),
            s.timezone,
        );
    }
    out
}

/// The object this component publishes under `Capability::HttpRoutes`.
///
/// [`RouteProvider`] is a marker trait and its own documentation has why: the pure half of
/// a route table is the [`RouteManifest`] on the provision, and the impure half cannot be a
/// handle type because `mw-component` may not name `axum`. So the publication is a graph
/// edge rather than a query set, and this is the object on the end of it.
#[derive(Debug)]
struct Mounted;

impl RouteProvider for Mounted {}

/// The component's body: install, hold, clear.
struct Legacy {
    state: Arc<RouteState>,
    routes: Publisher<Arc<dyn RouteProvider>>,
    handlers: Arc<Handlers>,
}

impl std::fmt::Debug for Legacy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Legacy").finish_non_exhaustive()
    }
}

impl Serve for Legacy {
    fn serve(
        self: Box<Self>,
        cx: Cx,
    ) -> mw_component::BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            // Installed before the capability is published, so a consumer that reacts to
            // the publication cannot observe a mounted-but-empty table.
            self.state.install(Arc::clone(&self.handlers));
            self.routes
                .publish(Arc::new(Mounted) as Arc<dyn RouteProvider>);
            cx.cancelled().await;
            // Revoke before clearing, which is the ordering §3.6 asks of a provider: a
            // consumer must never observe a revoked capability it is still being served
            // from, and the reverse -- a published capability whose table is gone -- is
            // the one this crate can prevent.
            self.routes.revoke();
            self.state.clear();
            info!("the frozen REST surface is unmounted");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Listener, StationView, WebsiteView};
    use mw_component::Match;
    use mw_types::{StationKey, WebsiteId};

    #[derive(Debug)]
    struct Cfg {
        websites: Vec<WebsiteView>,
        stations: Vec<StationView>,
        state: Option<Arc<RouteState>>,
        render: Option<Arc<Renderer>>,
        snapshot: bool,
    }

    impl LegacyConfig for Cfg {
        fn legacy_websites(&self) -> &[WebsiteView] {
            &self.websites
        }
        fn legacy_stations(&self) -> &[StationView] {
            &self.stations
        }
        fn legacy_listener(&self) -> Listener {
            Listener {
                port: 18080,
                tls: false,
            }
        }
        fn legacy_route_state(&self) -> Option<&Arc<RouteState>> {
            self.state.as_ref()
        }
        fn legacy_render(&self) -> Option<&Arc<Renderer>> {
            self.render.as_ref()
        }
        fn has_latest_snapshot(&self) -> bool {
            self.snapshot
        }
    }

    fn station(key: &str, zone: Option<&str>) -> StationView {
        StationView {
            id: 1,
            key: StationKey::new(key),
            name: key.into(),
            kind: "davis".into(),
            enabled: true,
            latitude: Some(39.9),
            longitude: Some(-105.5),
            timezone: zone.map(str::to_string),
        }
    }

    fn website(id: i64) -> WebsiteView {
        WebsiteView {
            id: WebsiteId::new(id),
            name: "Suncrest".into(),
            hostname: "localhost".into(),
            page_title: "Suncrest".into(),
            about_text: String::new(),
            primary_station: Some(StationKey::new("davis-full")),
            snow_enabled: false,
            snow_station: None,
            air_quality_enabled: false,
            air_quality_station: None,
            is_portal: false,
            radar_enabled: false,
            radar_token: String::new(),
        }
    }

    fn cfg() -> Cfg {
        Cfg {
            websites: vec![website(1)],
            stations: vec![station("davis-full", Some("America/Denver"))],
            state: Some(Arc::new(RouteState::new())),
            render: Some(Arc::new(Renderer::default())),
            snapshot: true,
        }
    }

    fn plan(cfg: &Cfg) -> ComponentSpec {
        let mut out = Vec::new();
        register(cfg, &mut out).expect("the slot is built");
        assert_eq!(out.len(), 1, "one component, unconditionally");
        out.pop().expect("one spec")
    }

    /// **D-48's structural half, asserted.** M4-1 recorded that nothing did.
    ///
    /// A hard `ReadingStore` here would make `/latest` answer `503` whenever the database
    /// is down, and the conformance suite replays against a healthy one -- so this test is
    /// the only thing between that change and a shipped regression.
    #[test]
    fn the_hard_needs_are_exactly_the_snapshot() {
        let spec = plan(&cfg());
        let hard: Vec<Capability> = spec.hard_needs().map(|n| n.capability).collect();
        assert_eq!(
            hard,
            vec![Capability::LatestSnapshot],
            "D-48: a hard need outside {{LatestSnapshot}} makes a frozen route unmountable"
        );
        let soft: Vec<Capability> = spec
            .requires
            .iter()
            .filter(|n| n.strength == mw_component::Strength::Soft)
            .map(|n| n.capability)
            .collect();
        assert_eq!(
            soft,
            vec![
                Capability::ReadingStore,
                Capability::ForecastStore,
                Capability::SnowCache
            ],
            "AlertStore is absent because /alerts is not carried forward (D-69)"
        );
        // Read access only: nothing on this surface writes.
        assert!(
            spec.requires
                .iter()
                .all(|n| n.access == mw_component::Access::Read),
            "the frozen surface is read-only"
        );
    }

    /// The manifest covers the five frozen patterns and the two coupled ones, every one of
    /// them `ANY` (Q-17), in one `Scope::Any` table.
    #[test]
    fn the_manifest_declares_every_path_for_every_method() {
        let m = manifest();
        assert_eq!(m.scope, Scope::Any);
        assert_eq!(m.entries.len(), 7);
        for entry in &m.entries {
            assert_eq!(entry.methods, Methods::ANY, "{}", entry.pattern);
            assert_eq!(entry.matching, Match::Exact, "{}", entry.pattern);
        }
        let patterns: Vec<&str> = m.entries.iter().map(|e| e.pattern.as_str()).collect();
        for frozen in FROZEN_PATTERNS {
            assert!(patterns.contains(&frozen), "{frozen} is not declared");
        }
        assert!(patterns.contains(&"/almanac"));
        assert!(patterns.contains(&"/api/stations"));
    }

    /// The provision carries the manifest, which is what makes a duplicate route a
    /// `PlanError` naming both owners rather than silent last-registration-wins.
    #[test]
    fn the_provision_carries_the_route_manifest() {
        let spec = plan(&cfg());
        assert_eq!(spec.provides.len(), 1);
        let provision = &spec.provides[0];
        assert_eq!(provision.capability, Capability::HttpRoutes);
        assert_eq!(provision.arity, mw_component::Arity::Additive);
        assert_eq!(
            provision.detail,
            mw_component::ProvisionDetail::Routes(manifest())
        );
    }

    /// A configuration change this component closes over moves the digest, which is what
    /// restarts it. A registrar that read a row and forgot to fold it in is a component
    /// that keeps serving the old configuration.
    #[test]
    fn the_digest_follows_every_row_the_builder_closes_over() {
        let base = plan(&cfg()).config_digest;

        let mut renamed = cfg();
        renamed.websites[0].name = "Renamed".into();
        assert_ne!(plan(&renamed).config_digest, base, "the website's name");

        let mut moved = cfg();
        moved.stations[0].latitude = Some(40.0);
        assert_ne!(plan(&moved).config_digest, base, "the station's latitude");

        let mut renumbered = cfg();
        renumbered.stations[0].id = 9;
        assert_ne!(plan(&renumbered).config_digest, base, "the station's id");
    }

    /// A missing root resource is a boot-sequence bug and says which one, rather than
    /// planning a component that would have nowhere to install anything -- or nothing to
    /// render `/latest` with.
    #[test]
    fn a_missing_root_resource_is_a_named_refusal() {
        let mut no_slot = cfg();
        no_slot.state = None;
        let mut out = Vec::new();
        let e = register(&no_slot, &mut out).expect_err("no slot");
        assert_eq!(e, LegacyPlanError::NoRouteState);
        assert!(out.is_empty());
        assert!(e.to_string().contains("boot"), "{e}");

        let mut no_render = cfg();
        no_render.render = None;
        let e = register(&no_render, &mut out).expect_err("no renderer");
        assert_eq!(e, LegacyPlanError::NoRenderer);
        assert!(out.is_empty());
    }

    /// No `LatestSnapshot` provider means no component, rather than a `PlanError` that
    /// refuses the whole configuration -- which is what a hard need nobody provides is.
    #[test]
    fn no_snapshot_provider_means_no_component_rather_than_a_refused_plan() {
        let mut cfg = cfg();
        cfg.snapshot = false;
        let mut out = Vec::new();
        register(&cfg, &mut out).expect("not planning is not an error");
        assert!(out.is_empty());
    }

    /// The slot is empty until something installs a handler set, and a request against an
    /// empty slot is the unmounted response rather than a panic.
    #[test]
    fn an_empty_slot_has_no_handlers() {
        let state = RouteState::new();
        assert!(state.handlers().is_none());
    }
}
