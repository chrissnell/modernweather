//! The handler set, the router, and the schema that is derived from them.
//!
//! Every route is `/api/v1/*` (**D-49**), and the versioning is on the **management** API
//! only: the frozen public contract is unversioned by definition and is not touched
//! (**D-37**, §4.4).
//!
//! # The schema is the handlers
//!
//! [`ApiDoc`] is `utoipa`'s derive over the `#[utoipa::path]` attributes below, so the
//! handler *is* the schema — §4.1's requirement, and the reason the gate that checks it
//! (`mgmt-schema-drift`, **D-57**) is separate from `codegen-drift`: that check should keep
//! meaning precisely "the generated tree matches `metrics/registry.toml`" and nothing else.
//! `cargo xtask mgmt-schema` is the emitter and the committed `openapi/management.json` is
//! the artefact.
//!
//! (The paths are written without their leading directory on purpose: `ci/layers.toml`'s
//! `only-mw-types-and-the-proto-build-script-mention-generated` rule scans this file's text,
//! and the point of that rule -- a hand-written module must not reach a generated type -- is
//! worth more than the convenience of spelling a path in a comment.)
//!
//! # Three dispositions this module makes visible by not having a route
//!
//! Recorded here rather than left as an absence, because a reader counting routes against
//! W5 §1 will look for them:
//!
//! - **`/api/v1/events`** is the SSE endpoint of §5 and belongs with the client that
//!   consumes its channels (**M7-3**). There is no polling to replace yet.
//! - **`/api/v1/checks/*`** -- all seven -- are `check:<name>` components with
//!   `Trigger::OnDemand` run through `ComponentControl::run_now` (W2a §4.4). The *route* is
//!   two lines; the seven components are not, and no work item creates them. Writing the
//!   routes against components that do not exist would be seven endpoints that answer
//!   `NoSuchComponent`.
//! - **`/api/v1/discovery/weatherlink/*`** needs the in-process mDNS probe to be driven from
//!   a request rather than from a station's build (M3-1's constraint), which is a change to
//!   `mw-station`'s surface rather than a handler.
//!
//! Adding each of them is additive under §4.4 and needs no version bump, which is why they
//! are absent rather than stubbed.
//!
//! The fourth entry in that list used to be `/api/v1/auth/*`. **M7-2 has landed it** -- the
//! eight operations at the bottom of this file -- so what was an absence is now a section.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::response::IntoResponse as _;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use mw_component::{ComponentControl, ComponentId, ControlError, LogControlError};
use secrecy::SecretString;

use crate::admin::{Applied, Commit, ConfigAdmin};
use crate::change::{
    Change, ForecastsInput, ListenerInput, PairingInput, RadarInput, ServiceInput, StationInput,
    StorageInput, TransportInput, WebsiteInput,
};
use crate::display::{Badge, Display, Section, Vocabulary};
use crate::error::{ApiError, Body, ErrorBody, FieldError};
use crate::events::{self, ChannelsInput, ChannelsView, Live};
use crate::guard::{Current, Guard, Peer};
use crate::host::{
    EnrolmentToken, EnrolmentView, Fleet, FleetView, LatestReading, LogEntry, LogField,
    LogLevelInput, LogLevelsView, LogSource, LogsView, Machine, PeerView, SerialPort,
    SerialPortsView, SpoolView, Stream, SystemInfoView,
};
use crate::runtime::{self, ComponentView, ComponentsView, SummaryView};
use crate::view::{
    ConfigView, Credential, ForecastsView, ListenerView, NodeView, PairingView, ServiceView,
    StationView, StorageView, WebsiteView,
};

/// Everything a management request is answered from.
///
/// Four handles, and three of them are traits this crate declares rather than
/// implementations: [`ConfigAdmin`] and [`crate::host`]'s two. The fourth,
/// `ComponentControl`, is `mw-component`'s and is published by the supervisor as a **root**
/// capability -- which is half of how W2a §2.5 eliminates the `AppReloader` cycle: the
/// management API no longer reaches back into the application object that owns the manager
/// that constructs every other component.
pub struct Mgmt {
    admin: Arc<dyn ConfigAdmin>,
    control: Arc<dyn ComponentControl>,
    logs: Arc<dyn LogSource>,
    machine: Arc<dyn Machine>,
    /// §5's live stream. `None` in a build with no runtime behind it -- every `oneshot`
    /// test -- and the route then answers `501` rather than this process carrying a tick
    /// nothing reads.
    live: Option<Arc<Live>>,
    /// **N-11**'s fleet numbers. `None` on a node with no spool and no peers.
    fleet: Option<Arc<dyn Fleet>>,
}

impl std::fmt::Debug for Mgmt {
    /// Capability handles are not `Debug`, and what is useful about this in a log line is
    /// the generation it would answer with.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mgmt")
            .field("generation", &self.admin.generation())
            .finish_non_exhaustive()
    }
}

impl Mgmt {
    /// Assemble the state. The binary is the only caller outside tests.
    pub fn new(
        admin: Arc<dyn ConfigAdmin>,
        control: Arc<dyn ComponentControl>,
        logs: Arc<dyn LogSource>,
        machine: Arc<dyn Machine>,
    ) -> Self {
        Self {
            admin,
            control,
            logs,
            machine,
            live: None,
            fleet: None,
        }
    }

    /// Attach §5's live stream. Separate from [`Mgmt::new`] because a router without one
    /// is a valid router: forty of the forty-three operations do not need it.
    #[must_use]
    pub fn with_live(mut self, live: Arc<Live>) -> Self {
        self.live = Some(live);
        self
    }

    /// Attach **N-11**'s fleet numbers.
    #[must_use]
    pub fn with_fleet(mut self, fleet: Arc<dyn Fleet>) -> Self {
        self.fleet = Some(fleet);
        self
    }
}

/// How many log entries one read returns when the request does not say.
///
/// The ring holds more; a default of everything would make the first paint of
/// `System → Activity` the largest response this API can produce.
const LOG_PAGE: usize = 200;

/// The largest a caller may ask for, so that one request cannot serialise the whole ring.
const LOG_PAGE_MAX: usize = 2000;

/// The management API's router.
///
/// **Authenticated, since implementation-plan.md M7-2.** Two sub-routers with different
/// state -- [`authentication`] over the [`Guard`] and the configuration surface over
/// [`Mgmt`] -- merged and then wrapped in **one** `crate::guard::enforce` layer, so there is
/// a single place a request is admitted and a single list of the three routes that do not
/// need a session (`crate::guard::UNAUTHENTICATED`).
///
/// M7-1 shipped this surface with no authentication and relied on the binary narrowing its
/// bind address to loopback to make that safe. That narrowing is now conditional on the
/// instance being **unclaimed** (W5 §3.4) rather than unconditional, which is the column
/// `listener.management_listen_addr` meaning what it says for the first time; the schema's
/// default for it is `127.0.0.1` either way (**A-W5-3**).
pub fn router(state: Arc<Mgmt>, guard: Arc<Guard>, assets: crate::assets::Assets) -> Router {
    let api = configured(state)
        .merge(authentication(Arc::clone(&guard)))
        .layer(axum::middleware::from_fn_with_state(
            guard,
            crate::guard::enforce,
        ));
    // **The bundle is served outside the guard, and that is the decision rather than an
    // oversight.** The sign-in page is part of the bundle, so a guard in front of it would
    // refuse the very document whose job is to collect the credentials -- a redirect loop
    // with no way out. What the bundle contains is the same for every installation and is
    // compiled into a binary anybody can download, so there is nothing in it to protect;
    // every byte that is specific to *this* installation comes through the API behind the
    // guard. `crate::assets::serve` refuses `/api/` so the guard's own 404 and 401 still
    // reach the client (§8.8's signed-out state depends on the 401 arriving).
    api.fallback(
        move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
            let assets = assets.clone();
            async move { crate::assets::serve(&assets, &uri, &headers) }
        },
    )
}

/// The configuration and runtime surface — M7-1's forty operations.
fn configured(state: Arc<Mgmt>) -> Router {
    Router::new()
        // §1.10 -- the whole configuration, under System → Advanced only.
        .route("/api/v1/config", get(get_config))
        // §2.4 -- the pure `plan()` dry run, and the one step back.
        .route("/api/v1/config/preview", post(post_preview))
        .route("/api/v1/config/revert", post(post_revert))
        // §1.2 -- my weather stations.
        .route("/api/v1/stations", get(get_stations).post(post_station))
        .route(
            "/api/v1/stations/{key}",
            get(get_station).put(put_station).delete(delete_station),
        )
        .route(
            "/api/v1/stations/{key}/secret",
            delete(delete_station_secret),
        )
        // §1.5 -- my website.
        .route("/api/v1/websites", get(get_websites).post(post_website))
        .route(
            "/api/v1/websites/{id}",
            get(get_website).put(put_website).delete(delete_website),
        )
        .route(
            "/api/v1/websites/{id}/radar",
            post(post_radar).delete(delete_radar),
        )
        // §1.3 -- data storage.
        .route("/api/v1/storage", get(get_storage).post(post_storage))
        .route(
            "/api/v1/storage/{id}",
            get(get_storage_by_id)
                .put(put_storage)
                .delete(delete_storage),
        )
        // §1.4 -- share my data.
        .route("/api/v1/services", get(get_services))
        .route(
            "/api/v1/services/{service}",
            get(get_service).put(put_service).delete(delete_service),
        )
        .route(
            "/api/v1/forecasts",
            get(get_forecasts)
                .put(put_forecasts)
                .delete(delete_forecasts),
        )
        // §1.4 -- the `rest` controller's page, which is also System → Advanced's.
        .route(
            "/api/v1/website-server",
            get(get_website_server).put(put_website_server),
        )
        // §2.3, §2.5, §2.6 -- configured versus running.
        .route("/api/v1/runtime/summary", get(get_summary))
        .route("/api/v1/runtime/components", get(get_components))
        .route("/api/v1/runtime/components/{id}", get(get_component))
        .route(
            "/api/v1/runtime/components/{id}/run-now",
            post(post_run_now),
        )
        .route(
            "/api/v1/runtime/components/{id}/restart",
            post(post_restart),
        )
        // §5.5 -- runtime log levels.
        .route("/api/v1/runtime/log-levels", get(get_log_levels))
        .route("/api/v1/runtime/log-level", post(post_log_level))
        // §1.8 -- the two rings, one presentation.
        .route("/api/v1/logs", get(get_logs))
        // §1.9 -- System → This machine, and the serial-port picker.
        .route("/api/v1/system/info", get(get_system_info))
        .route("/api/v1/system/serial-ports", get(get_serial_ports))
        // §5 -- the one live connection, and its in-stream channel change.
        .route("/api/v1/events", get(get_events))
        .route(
            "/api/v1/events/{stream_id}/channels",
            post(post_event_channels),
        )
        // N-11 -- W7's fleet numbers, on the two pages W5 already designed.
        .route("/api/v1/fleet", get(get_fleet))
        .with_state(state)
}

/// The derived schema. `cargo xtask mgmt-schema` serialises this.
#[derive(Debug, utoipa::OpenApi)]
#[openapi(
    info(
        title = "ModernWeather management API",
        description = "The management API over ConfigAdmin and ComponentControl. \
                       GENERATED from the Rust handlers in crates/mw-api-mgmt by \
                       `cargo xtask mgmt-schema`. DO NOT EDIT. Every route is /api/v1/* \
                       (DESIGN.md D-49); the frozen public contract is unversioned and is \
                       not described here (D-37)."
    ),
    paths(
        get_config,
        post_preview,
        post_revert,
        get_stations,
        post_station,
        get_station,
        put_station,
        delete_station,
        delete_station_secret,
        get_websites,
        post_website,
        get_website,
        put_website,
        delete_website,
        post_radar,
        delete_radar,
        get_storage,
        get_storage_by_id,
        post_storage,
        put_storage,
        delete_storage,
        get_services,
        get_service,
        put_service,
        delete_service,
        get_forecasts,
        put_forecasts,
        delete_forecasts,
        get_website_server,
        put_website_server,
        get_summary,
        get_components,
        get_component,
        post_run_now,
        post_restart,
        get_log_levels,
        post_log_level,
        get_logs,
        get_system_info,
        get_serial_ports,
        get_events,
        post_event_channels,
        get_fleet,
        post_login,
        post_claim,
        get_auth_state,
        delete_session,
        get_sessions,
        delete_one_session,
        delete_all_sessions,
        post_password,
    ),
    components(schemas(
        Applied,
        AuthStateView,
        Badge,
        Change,
        ClaimInput,
        ComponentView,
        ComponentsView,
        ConfigView,
        Credential,
        Display,
        ErrorBody,
        FieldError,
        ForecastsInput,
        ForecastsView,
        ListenerInput,
        ListenerView,
        LogEntry,
        LogField,
        LogLevelInput,
        LogLevelsView,
        LogsView,
        LoginInput,
        NodeView,
        PasswordInput,
        PasswordPolicyView,
        PairingInput,
        PairingView,
        RadarInput,
        RevertInput,
        Section,
        SerialPort,
        SessionView,
        SessionsView,
        SerialPortsView,
        ServiceInput,
        ServiceView,
        ServicesView,
        SignedInView,
        StationInput,
        StationView,
        StationsView,
        StorageInput,
        StorageView,
        SummaryView,
        SystemInfoView,
        ChannelsInput,
        ChannelsView,
        EnrolmentToken,
        EnrolmentView,
        FleetView,
        LatestReading,
        PeerView,
        SpoolView,
        TransportInput,
        WebsiteInput,
        WebsiteView,
        WebsitesView,
    )),
    tags(
        (name = "auth", description = "Signing in, sessions, and the first-run claim"),
        (name = "config", description = "The configuration, the save preview and one step back"),
        (name = "stations", description = "My weather stations"),
        (name = "websites", description = "My website"),
        (name = "storage", description = "Data storage"),
        (name = "services", description = "Share my data"),
        (name = "runtime", description = "Configured versus running"),
        (name = "system", description = "System"),
        (name = "events", description = "Live data: one endpoint, several channels"),
        (name = "fleet", description = "Unsent readings, peers and enrolment"),
    )
)]
pub struct ApiDoc;

// ---------------------------------------------------------------------------------------
// Resource wrappers
//
// §4.4: "every new resource is a JSON object with named fields, never a bare array or a
// scalar", so a field can be added without a version bump. Each list also carries the
// generation, because a screen that shows a list and a status badge has to compare one
// integer rather than two reads' worth.
// ---------------------------------------------------------------------------------------

/// `GET /api/v1/stations`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StationsView {
    /// The newest committed configuration generation.
    pub generation: u64,
    /// Every station, including the disabled ones.
    pub stations: Vec<StationView>,
}

/// `GET /api/v1/websites`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct WebsitesView {
    /// The newest committed configuration generation.
    pub generation: u64,
    /// Every website, in id order.
    pub websites: Vec<WebsiteView>,
}

/// `GET /api/v1/services`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ServicesView {
    /// The newest committed configuration generation.
    pub generation: u64,
    /// Every upload service that has an account.
    pub services: Vec<ServiceView>,
}

/// `POST /api/v1/config/revert`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RevertInput {
    /// Which generation to go back to. One step only, and only within this process's life.
    pub to_generation: u64,
}

/// How many log entries to return, and from which ring.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct LogQuery {
    /// `app`, `access`, or absent for both merged (§1.8).
    #[serde(default)]
    pub stream: Option<String>,
    /// How many entries. Clamped to [`LOG_PAGE_MAX`].
    ///
    /// `i64` rather than `usize` so that `?limit=-1` is a **number this API refuses** rather
    /// than a `serde` rejection, which `axum` answers with a plain-text `400` that has none
    /// of this surface's one error shape.
    #[serde(default)]
    pub limit: Option<i64>,
}

type Reply<T> = Result<Json<T>, ApiError>;

// ---------------------------------------------------------------------------------------
// The configuration, the preview and the revert
// ---------------------------------------------------------------------------------------

/// The whole configuration, with every secret omitted.
#[utoipa::path(
    get,
    path = "/api/v1/config",
    tag = "config",
    responses(
        (status = 200, description = "The configuration, secrets omitted", body = ConfigView),
        (status = 500, description = "The configuration could not be read", body = ErrorBody),
    )
)]
async fn get_config(State(s): State<Arc<Mgmt>>) -> Reply<ConfigView> {
    Ok(Json(s.admin.read().await?))
}

/// Try a change: run `plan()` against it and report the diff without committing.
#[utoipa::path(
    post,
    path = "/api/v1/config/preview",
    tag = "config",
    request_body = Change,
    responses(
        (status = 200, description = "What this change would do", body = Applied),
        (status = 404, description = "It names something that is not configured", body = ErrorBody),
        (status = 422, description = "The plan refuses it, with the field that caused it", body = ErrorBody),
    )
)]
async fn post_preview(State(s): State<Arc<Mgmt>>, Body(change): Body<Change>) -> Reply<Applied> {
    // **The same field-level pass the save runs**, and that is the point rather than
    // defence in depth: a preview that accepted a body the save refuses would disagree with
    // it about the one thing this endpoint exists to agree on. `try_change` is what both
    // call.
    Ok(Json(try_change(&s, change, Commit::Preview).await?))
}

/// Put the previous committed configuration back — §2.4's **Undo save**.
#[utoipa::path(
    post,
    path = "/api/v1/config/revert",
    tag = "config",
    request_body = RevertInput,
    responses(
        (status = 200, description = "The previous configuration is back", body = Applied),
        (status = 409, description = "That generation is not the one step this process kept", body = ErrorBody),
    )
)]
async fn post_revert(State(s): State<Arc<Mgmt>>, Body(input): Body<RevertInput>) -> Reply<Applied> {
    Ok(Json(s.admin.revert(input.to_generation).await?))
}

// ---------------------------------------------------------------------------------------
// Weather stations
// ---------------------------------------------------------------------------------------

/// Every weather station.
#[utoipa::path(
    get,
    path = "/api/v1/stations",
    tag = "stations",
    responses((status = 200, description = "Every station", body = StationsView))
)]
async fn get_stations(State(s): State<Arc<Mgmt>>) -> Reply<StationsView> {
    let cfg = s.admin.read().await?;
    Ok(Json(StationsView {
        generation: cfg.generation,
        stations: cfg.stations,
    }))
}

/// Add a weather station.
#[utoipa::path(
    post,
    path = "/api/v1/stations",
    tag = "stations",
    request_body = StationInput,
    responses(
        (status = 200, description = "Added, with what restarted", body = Applied),
        (status = 422, description = "The form has problems, by field", body = ErrorBody),
    )
)]
async fn post_station(
    State(s): State<Arc<Mgmt>>,
    Body(station): Body<StationInput>,
) -> Reply<Applied> {
    // Writing a station by key is an upsert in the store, because one-step-back has to put
    // a deleted station back under the key its history is stored under. So "add" and "edit"
    // are told apart here: a `POST` whose key already exists would otherwise silently
    // replace a live station's every column and answer `200 Added`.
    if let Some(key) = station.key.as_deref() {
        if s.admin.read().await?.station(key).is_some() {
            return Err(ApiError::Conflict {
                summary: format!("There is already a weather station called `{key}`."),
                detail: Some(
                    "Open that station to change it, or choose a different identifier.".to_string(),
                ),
            });
        }
    }
    save(&s, Change::PutStation { key: None, station }).await
}

/// One weather station.
#[utoipa::path(
    get,
    path = "/api/v1/stations/{key}",
    tag = "stations",
    params(("key" = String, Path, description = "The station's identifier")),
    responses(
        (status = 200, description = "The station", body = StationView),
        (status = 404, description = "No station has that identifier", body = ErrorBody),
    )
)]
async fn get_station(State(s): State<Arc<Mgmt>>, Path(key): Path<String>) -> Reply<StationView> {
    let cfg = s.admin.read().await?;
    cfg.station(&key)
        .cloned()
        .map(Json)
        .ok_or(ApiError::NotFound {
            what: "weather station",
            id: key,
        })
}

/// Change a weather station's settings.
#[utoipa::path(
    put,
    path = "/api/v1/stations/{key}",
    tag = "stations",
    params(("key" = String, Path, description = "The station's identifier")),
    request_body = StationInput,
    responses(
        (status = 200, description = "Saved, with what restarts", body = Applied),
        (status = 404, description = "No station has that identifier", body = ErrorBody),
        (status = 422, description = "The form has problems, by field", body = ErrorBody),
    )
)]
async fn put_station(
    State(s): State<Arc<Mgmt>>,
    Path(key): Path<String>,
    Body(station): Body<StationInput>,
) -> Reply<Applied> {
    // The route refuses an unknown key, not the statement — `put_website`'s reason exactly.
    if s.admin.read().await?.station(&key).is_none() {
        return Err(ApiError::NotFound {
            what: "weather station",
            id: key,
        });
    }
    save(
        &s,
        Change::PutStation {
            key: Some(key),
            station,
        },
    )
    .await
}

/// Delete a weather station.
///
/// The consequences the confirmation dialog shows (§8.7) come from
/// `POST /api/v1/config/preview` with the same change, so they cannot drift from what this
/// does.
#[utoipa::path(
    delete,
    path = "/api/v1/stations/{key}",
    tag = "stations",
    params(("key" = String, Path, description = "The station's identifier")),
    responses(
        (status = 200, description = "Deleted, with what stops", body = Applied),
        (status = 404, description = "No station has that identifier", body = ErrorBody),
    )
)]
async fn delete_station(State(s): State<Arc<Mgmt>>, Path(key): Path<String>) -> Reply<Applied> {
    save(&s, Change::DeleteStation { key }).await
}

/// Forget a station's stored protocol secret.
///
/// Its own route rather than a field on the form, because omitting a credential means "keep
/// the stored one" everywhere else in this API -- no response returns a secret to retype, so
/// treating an absent field as a clear would erase a working credential on every save.
#[utoipa::path(
    delete,
    path = "/api/v1/stations/{key}/secret",
    tag = "stations",
    params(("key" = String, Path, description = "The station's identifier")),
    responses(
        (status = 200, description = "Forgotten", body = Applied),
        (status = 404, description = "No station has that identifier", body = ErrorBody),
    )
)]
async fn delete_station_secret(
    State(s): State<Arc<Mgmt>>,
    Path(key): Path<String>,
) -> Reply<Applied> {
    save(&s, Change::ClearStationSecret { key }).await
}

// ---------------------------------------------------------------------------------------
// Websites
// ---------------------------------------------------------------------------------------

/// Every website.
#[utoipa::path(
    get,
    path = "/api/v1/websites",
    tag = "websites",
    responses((status = 200, description = "Every website", body = WebsitesView))
)]
async fn get_websites(State(s): State<Arc<Mgmt>>) -> Reply<WebsitesView> {
    let cfg = s.admin.read().await?;
    Ok(Json(WebsitesView {
        generation: cfg.generation,
        websites: cfg.websites,
    }))
}

/// Add a website.
#[utoipa::path(
    post,
    path = "/api/v1/websites",
    tag = "websites",
    request_body = WebsiteInput,
    responses(
        (status = 200, description = "Added, with what starts", body = Applied),
        (status = 422, description = "The form has problems, by field", body = ErrorBody),
    )
)]
async fn post_website(
    State(s): State<Arc<Mgmt>>,
    Body(website): Body<WebsiteInput>,
) -> Reply<Applied> {
    save(&s, Change::PutWebsite { id: None, website }).await
}

/// One website.
#[utoipa::path(
    get,
    path = "/api/v1/websites/{id}",
    tag = "websites",
    params(("id" = i64, Path, description = "The website's id")),
    responses(
        (status = 200, description = "The website", body = WebsiteView),
        (status = 404, description = "No website has that id", body = ErrorBody),
    )
)]
async fn get_website(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<WebsiteView> {
    let id = website_id(&id)?;
    let cfg = s.admin.read().await?;
    cfg.website(id)
        .cloned()
        .map(Json)
        .ok_or(ApiError::NotFound {
            what: "website",
            id: id.to_string(),
        })
}

/// Change a website's settings.
#[utoipa::path(
    put,
    path = "/api/v1/websites/{id}",
    tag = "websites",
    params(("id" = i64, Path, description = "The website's id")),
    request_body = WebsiteInput,
    responses(
        (status = 200, description = "Saved, with what restarts", body = Applied),
        (status = 404, description = "No website has that id", body = ErrorBody),
        (status = 422, description = "The form has problems, by field", body = ErrorBody),
    )
)]
async fn put_website(
    State(s): State<Arc<Mgmt>>,
    Path(id): Path<String>,
    Body(website): Body<WebsiteInput>,
) -> Reply<Applied> {
    let id = website_id(&id)?;
    // **The route is what refuses an unknown id, not the statement.** Writing a website by
    // id is an upsert in the store, because `ConfigAdmin`'s one-step-back has to put a
    // deleted row back under the id the **frozen** contract returns (`/stationinfo` and the
    // rendered page carry it). "Edit this website" and "put this website back" are different
    // intentions, and only the second may invent a row -- so the first checks first.
    if s.admin.read().await?.website(id).is_none() {
        return Err(ApiError::NotFound {
            what: "website",
            id: id.to_string(),
        });
    }
    save(
        &s,
        Change::PutWebsite {
            id: Some(id),
            website,
        },
    )
    .await
}

/// Delete a website.
#[utoipa::path(
    delete,
    path = "/api/v1/websites/{id}",
    tag = "websites",
    params(("id" = i64, Path, description = "The website's id")),
    responses(
        (status = 200, description = "Deleted, with what stops", body = Applied),
        (status = 404, description = "No website has that id", body = ErrorBody),
    )
)]
async fn delete_website(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<Applied> {
    let id = website_id(&id)?;
    save(&s, Change::DeleteWebsite { id }).await
}

/// Turn on the weather radar map by recording its registration.
#[utoipa::path(
    post,
    path = "/api/v1/websites/{id}/radar",
    tag = "websites",
    params(("id" = i64, Path, description = "The website's id")),
    request_body = RadarInput,
    responses(
        (status = 200, description = "Registered", body = Applied),
        (status = 404, description = "No website has that id", body = ErrorBody),
        (status = 422, description = "The registration is empty", body = ErrorBody),
    )
)]
async fn post_radar(
    State(s): State<Arc<Mgmt>>,
    Path(id): Path<String>,
    Body(radar): Body<RadarInput>,
) -> Reply<Applied> {
    let id = website_id(&id)?;
    save(&s, Change::RegisterRadar { id, radar }).await
}

/// Turn off the weather radar map and forget its registration.
#[utoipa::path(
    delete,
    path = "/api/v1/websites/{id}/radar",
    tag = "websites",
    params(("id" = i64, Path, description = "The website's id")),
    responses(
        (status = 200, description = "Withdrawn", body = Applied),
        (status = 404, description = "No website has that id", body = ErrorBody),
    )
)]
async fn delete_radar(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<Applied> {
    let id = website_id(&id)?;
    save(&s, Change::UnregisterRadar { id }).await
}

// ---------------------------------------------------------------------------------------
// Data storage
// ---------------------------------------------------------------------------------------

/// Where readings are kept.
#[utoipa::path(
    get,
    path = "/api/v1/storage",
    tag = "storage",
    responses((status = 200, description = "The weather database, if one is configured", body = StorageView)),
)]
async fn get_storage(State(s): State<Arc<Mgmt>>) -> Reply<StorageView> {
    let cfg = s.admin.read().await?;
    // **A node with no weather database is a legitimate configuration**, not an error: it
    // has no `db:postgres` and therefore no `writer:timescale` (D-44). It is a `404` so that
    // the screen's empty state is reached by the same branch a missing resource always is,
    // rather than by a `200` carrying a null the client has to special-case.
    cfg.storage.map(Json).ok_or(ApiError::NotFound {
        what: "weather database",
        id: "1".to_string(),
    })
}

/// The weather database, by id.
///
/// §1.3 notes that the Go surface has no by-id read for storage while weather stations and
/// websites have one, and that the new surface removes the inconsistency. It removes it by
/// **having** the route rather than by pretending there can be two.
#[utoipa::path(
    get,
    path = "/api/v1/storage/{id}",
    tag = "storage",
    params(("id" = i64, Path, description = "Always 1 — there is one weather database")),
    responses(
        (status = 200, description = "The weather database", body = StorageView),
        (status = 404, description = "There is no weather database with that id", body = ErrorBody),
    )
)]
async fn get_storage_by_id(
    State(s): State<Arc<Mgmt>>,
    Path(id): Path<String>,
) -> Reply<StorageView> {
    only_the_one_database(storage_id(&id)?)?;
    get_storage(State(s)).await
}

/// Set up the weather database.
#[utoipa::path(
    post,
    path = "/api/v1/storage",
    tag = "storage",
    request_body = StorageInput,
    responses(
        (status = 200, description = "Set, with what starts", body = Applied),
        (status = 422, description = "The connection address is not usable", body = ErrorBody),
    )
)]
async fn post_storage(
    State(s): State<Arc<Mgmt>>,
    Body(storage): Body<StorageInput>,
) -> Reply<Applied> {
    save(&s, Change::PutStorage { storage }).await
}

/// Change the weather database's connection.
///
/// A connection the database **rejects** is still a legitimate save (§7.2): the database may
/// be down or about to be reconfigured, so the save succeeds, `db:postgres` moves to
/// `Backoff`, and §2.4's banner ⑤ reports it. Conflating "I can't reach it" with "that input
/// is invalid" is how a UI ends up refusing to let an operator fix a configuration while the
/// thing it configures is down.
#[utoipa::path(
    put,
    path = "/api/v1/storage/{id}",
    tag = "storage",
    params(("id" = i64, Path, description = "Always 1 — there is one weather database")),
    request_body = StorageInput,
    responses(
        (status = 200, description = "Saved, with what restarts", body = Applied),
        (status = 404, description = "There is no weather database with that id", body = ErrorBody),
        (status = 422, description = "The connection address is not usable", body = ErrorBody),
    )
)]
async fn put_storage(
    State(s): State<Arc<Mgmt>>,
    Path(id): Path<String>,
    Body(storage): Body<StorageInput>,
) -> Reply<Applied> {
    only_the_one_database(storage_id(&id)?)?;
    configured_database(&s).await?;
    save(&s, Change::PutStorage { storage }).await
}

/// Stop saving readings to a weather database.
#[utoipa::path(
    delete,
    path = "/api/v1/storage/{id}",
    tag = "storage",
    params(("id" = i64, Path, description = "Always 1 — there is one weather database")),
    responses(
        (status = 200, description = "Removed, with what stops", body = Applied),
        (status = 404, description = "There is no weather database with that id", body = ErrorBody),
    )
)]
async fn delete_storage(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<Applied> {
    let id = storage_id(&id)?;
    only_the_one_database(id)?;
    configured_database(&s).await?;
    save(&s, Change::DeleteStorage).await
}

/// `weather_database` is `CHECK (id = 1)`: one row, enforced by a constraint rather than by
/// a convention.
///
/// The by-id routes exist because §1.3 notes that the Go surface has no by-id read for
/// storage while stations and websites do, and this surface removes the inconsistency. It
/// removes it by *having* the route, not by pretending there can be two.
fn only_the_one_database(id: i64) -> Result<(), ApiError> {
    if id != 1 {
        return Err(ApiError::NotFound {
            what: "weather database",
            id: id.to_string(),
        });
    }
    Ok(())
}

/// Refuse a by-id write on a node that has no weather database.
///
/// `PutWeatherDatabase` is an upsert and `DeleteWeatherDatabase` matches zero rows happily,
/// so without this a `PUT /api/v1/storage/1` would **create** the row that `POST` exists for
/// and a `DELETE` would answer `200` having deleted nothing — which is the "a 200 for a
/// DELETE that deleted nothing tells an operator their change took effect" lie that
/// `mw_store::config_admin`'s own `missing()` exists to prevent.
async fn configured_database(s: &Arc<Mgmt>) -> Result<(), ApiError> {
    if s.admin.read().await?.storage.is_none() {
        return Err(ApiError::NotFound {
            what: "weather database",
            id: "1".to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Share my data
// ---------------------------------------------------------------------------------------

/// Every upload service.
#[utoipa::path(
    get,
    path = "/api/v1/services",
    tag = "services",
    responses((status = 200, description = "Every upload service", body = ServicesView))
)]
async fn get_services(State(s): State<Arc<Mgmt>>) -> Reply<ServicesView> {
    let cfg = s.admin.read().await?;
    Ok(Json(ServicesView {
        generation: cfg.generation,
        services: cfg.services,
    }))
}

/// One upload service.
#[utoipa::path(
    get,
    path = "/api/v1/services/{service}",
    tag = "services",
    params(("service" = String, Path, description = "wunderground, pwsweather or aprs")),
    responses(
        (status = 200, description = "The service", body = ServiceView),
        (status = 404, description = "That service has no account configured", body = ErrorBody),
    )
)]
async fn get_service(
    State(s): State<Arc<Mgmt>>,
    Path(service): Path<String>,
) -> Reply<ServiceView> {
    let cfg = s.admin.read().await?;
    cfg.service(&service)
        .cloned()
        .map(Json)
        .ok_or(ApiError::NotFound {
            what: "service",
            id: service,
        })
}

/// Turn an upload service on, off, or change its settings.
///
/// There is no "add a controller": creating a service **is** this `PUT` (§1.4).
#[utoipa::path(
    put,
    path = "/api/v1/services/{service}",
    tag = "services",
    params(("service" = String, Path, description = "wunderground, pwsweather or aprs")),
    request_body = ServiceInput,
    responses(
        (status = 200, description = "Saved, with what starts or restarts", body = Applied),
        (status = 422, description = "The page has problems, by field", body = ErrorBody),
    )
)]
async fn put_service(
    State(s): State<Arc<Mgmt>>,
    Path(service): Path<String>,
    Body(settings): Body<ServiceInput>,
) -> Reply<Applied> {
    save(&s, Change::PutService { service, settings }).await
}

/// Stop sharing with an upload service, and forget its account.
#[utoipa::path(
    delete,
    path = "/api/v1/services/{service}",
    tag = "services",
    params(("service" = String, Path, description = "wunderground, pwsweather or aprs")),
    responses(
        (status = 200, description = "Stopped, with what stops", body = Applied),
        (status = 404, description = "That service has no account configured", body = ErrorBody),
    )
)]
async fn delete_service(State(s): State<Arc<Mgmt>>, Path(service): Path<String>) -> Reply<Applied> {
    save(&s, Change::DeleteService { service }).await
}

/// Forecasts and alerts.
///
/// Its own resource rather than `/services/aerisweather`, and the reason is §4.4's: an
/// account with a client-id pair and a station list has no field in common with an upload
/// account's interval, endpoint and APRS symbols, so one union would be an object with every
/// field optional. §1.4's mapping is honoured by the *page*, which is one page under "Share
/// my data" either way.
#[utoipa::path(
    get,
    path = "/api/v1/forecasts",
    tag = "services",
    responses(
        (status = 200, description = "The forecast account", body = ForecastsView),
        (status = 404, description = "No forecast account is configured", body = ErrorBody),
    )
)]
async fn get_forecasts(State(s): State<Arc<Mgmt>>) -> Reply<ForecastsView> {
    let cfg = s.admin.read().await?;
    cfg.forecasts.map(Json).ok_or(ApiError::NotFound {
        what: "forecast account",
        id: "1".to_string(),
    })
}

/// Turn forecasts and alerts on, off, or change their settings.
#[utoipa::path(
    put,
    path = "/api/v1/forecasts",
    tag = "services",
    request_body = ForecastsInput,
    responses(
        (status = 200, description = "Saved, with what starts or restarts", body = Applied),
        (status = 422, description = "The page has problems, by field", body = ErrorBody),
    )
)]
async fn put_forecasts(
    State(s): State<Arc<Mgmt>>,
    Body(forecasts): Body<ForecastsInput>,
) -> Reply<Applied> {
    save(&s, Change::PutForecasts { forecasts }).await
}

/// Stop fetching forecasts and forget the account.
#[utoipa::path(
    delete,
    path = "/api/v1/forecasts",
    tag = "services",
    responses((status = 200, description = "Stopped, with what stops", body = Applied))
)]
async fn delete_forecasts(State(s): State<Arc<Mgmt>>) -> Reply<Applied> {
    save(&s, Change::DeleteForecasts).await
}

// ---------------------------------------------------------------------------------------
// Website & API, which is also System → Advanced
// ---------------------------------------------------------------------------------------

/// Where this node listens.
///
/// One resource for both pages that edit it, because it is one row. §1.4 splits the *screen*
/// -- the public ports are "Website & API" and the management address, port and certificate
/// are `System → Advanced` -- and two routes writing one row would mean one page's save
/// silently rewriting the other page's fields with whatever it last read.
#[utoipa::path(
    get,
    path = "/api/v1/website-server",
    tag = "system",
    responses((status = 200, description = "Where this node listens", body = ListenerView))
)]
async fn get_website_server(State(s): State<Arc<Mgmt>>) -> Reply<ListenerView> {
    Ok(Json(s.admin.read().await?.listener))
}

/// Change where this node listens.
#[utoipa::path(
    put,
    path = "/api/v1/website-server",
    tag = "system",
    request_body = ListenerInput,
    responses(
        (status = 200, description = "Saved, with what restarts", body = Applied),
        (status = 422, description = "The form has problems, by field", body = ErrorBody),
    )
)]
async fn put_website_server(
    State(s): State<Arc<Mgmt>>,
    Body(listener): Body<ListenerInput>,
) -> Reply<Applied> {
    save(&s, Change::PutListener { listener }).await
}

// ---------------------------------------------------------------------------------------
// Configured versus running
// ---------------------------------------------------------------------------------------

/// One root cause, not five outages.
#[utoipa::path(
    get,
    path = "/api/v1/runtime/summary",
    tag = "runtime",
    responses((status = 200, description = "The root-cause rollup Home renders", body = SummaryView))
)]
async fn get_summary(State(s): State<Arc<Mgmt>>) -> Reply<SummaryView> {
    let vocab = Vocabulary::new(&s.admin.read().await?);
    Ok(Json(runtime::summary(
        &s.control.components(),
        s.admin.generation().get(),
        &s.admin.awaiting(),
        &vocab,
        chrono::Utc::now(),
    )))
}

/// Every component's status.
#[utoipa::path(
    get,
    path = "/api/v1/runtime/components",
    tag = "runtime",
    responses((status = 200, description = "Every component", body = ComponentsView))
)]
async fn get_components(State(s): State<Arc<Mgmt>>) -> Reply<ComponentsView> {
    let vocab = Vocabulary::new(&s.admin.read().await?);
    Ok(Json(runtime::components(
        &s.control.components(),
        s.admin.generation().get(),
        &s.admin.awaiting(),
        &vocab,
        chrono::Utc::now(),
    )))
}

/// One component's status.
#[utoipa::path(
    get,
    path = "/api/v1/runtime/components/{id}",
    tag = "runtime",
    params(("id" = String, Path, description = "The component's internal id")),
    responses(
        (status = 200, description = "The component", body = ComponentView),
        (status = 404, description = "The plan has no component with that id", body = ErrorBody),
    )
)]
async fn get_component(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<ComponentView> {
    let component = component_id(&id)?;
    let status = s.control.component(&component).ok_or(ApiError::NotFound {
        what: "component",
        id: id.clone(),
    })?;
    let vocab = Vocabulary::new(&s.admin.read().await?);
    Ok(Json(runtime::component(
        &status,
        &s.admin.awaiting(),
        &vocab,
        chrono::Utc::now(),
    )))
}

/// Run a scheduled component now, in addition to its schedule.
///
/// Deduplicated by occurrence key, so a double-click runs it once.
#[utoipa::path(
    post,
    path = "/api/v1/runtime/components/{id}/run-now",
    tag = "runtime",
    params(("id" = String, Path, description = "The component's internal id")),
    responses(
        (status = 200, description = "Queued, with the component's state", body = ComponentView),
        (status = 404, description = "The plan has no component with that id", body = ErrorBody),
        (status = 409, description = "It has no occurrence to run, or it is not live", body = ErrorBody),
    )
)]
async fn post_run_now(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<ComponentView> {
    let component = component_id(&id)?;
    s.control.run_now(&component).map_err(control_refusal)?;
    look(&s, &component, &id).await
}

/// Cancel a component and create it again.
///
/// §2.6: `run-now` and `restart` are safe and reversible, and a general "stop this
/// component" button is **not** offered -- it would create a running state no configuration
/// describes (W5 Decision 22). Turning something off is a configuration change, on the page
/// that owns it.
#[utoipa::path(
    post,
    path = "/api/v1/runtime/components/{id}/restart",
    tag = "runtime",
    params(("id" = String, Path, description = "The component's internal id")),
    responses(
        (status = 200, description = "Restarting, with the component's state", body = ComponentView),
        (status = 404, description = "The plan has no component with that id", body = ErrorBody),
        (status = 409, description = "The supervisor is shutting down", body = ErrorBody),
    )
)]
async fn post_restart(State(s): State<Arc<Mgmt>>, Path(id): Path<String>) -> Reply<ComponentView> {
    let component = component_id(&id)?;
    // Cancel, then create: `ComponentControl` has no `restart`, because the supervisor's own
    // restart is a plan diff. Two commands rather than one is not a race -- the supervisor
    // handles them in order on its single command channel, and `start` on a component that
    // is still `Stopping` is what its lifecycle already does after a `Backoff`.
    s.control.stop(&component).map_err(control_refusal)?;
    s.control.start(&component).map_err(control_refusal)?;
    look(&s, &component, &id).await
}

/// Read one component back after steering it.
async fn look(s: &Arc<Mgmt>, component: &ComponentId, id: &str) -> Reply<ComponentView> {
    let status = s.control.component(component).ok_or(ApiError::NotFound {
        what: "component",
        id: id.to_string(),
    })?;
    let vocab = Vocabulary::new(&s.admin.read().await?);
    Ok(Json(runtime::component(
        &status,
        &s.admin.awaiting(),
        &vocab,
        chrono::Utc::now(),
    )))
}

/// The standing log-level override.
#[utoipa::path(
    get,
    path = "/api/v1/runtime/log-levels",
    tag = "system",
    responses((status = 200, description = "The filter in force", body = LogLevelsView))
)]
async fn get_log_levels(State(s): State<Arc<Mgmt>>) -> Reply<LogLevelsView> {
    Ok(Json(LogLevelsView {
        directive: s.control.log_control().current(),
    }))
}

/// Change the log level at run time, or put it back (**D-52**).
#[utoipa::path(
    post,
    path = "/api/v1/runtime/log-level",
    tag = "system",
    request_body = LogLevelInput,
    responses(
        (status = 200, description = "The filter now in force", body = LogLevelsView),
        (status = 422, description = "That is not a valid filter", body = ErrorBody),
        (status = 409, description = "This build's logging cannot be reconfigured", body = ErrorBody),
    )
)]
async fn post_log_level(
    State(s): State<Arc<Mgmt>>,
    Body(input): Body<LogLevelInput>,
) -> Reply<LogLevelsView> {
    let log = s.control.log_control();
    match input.directive.as_deref() {
        Some(directive) => log.set(directive).map_err(log_refusal)?,
        None => log.reset().map_err(log_refusal)?,
    }
    Ok(Json(LogLevelsView {
        directive: log.current(),
    }))
}

/// The log rings.
#[utoipa::path(
    get,
    path = "/api/v1/logs",
    tag = "system",
    params(
        ("stream" = Option<String>, Query, description = "app, access, or omitted for both merged"),
        ("limit" = Option<i64>, Query, description = "How many entries; capped at 2000"),
    ),
    responses((status = 200, description = "The newest entries, oldest first", body = LogsView))
)]
async fn get_logs(State(s): State<Arc<Mgmt>>, Query(q): Query<LogQuery>) -> Reply<LogsView> {
    let stream = Stream::parse(q.stream.as_deref()).ok_or_else(|| {
        ApiError::field(
            "That is not a log this daemon keeps.",
            "stream",
            "Choose `app`, `access`, or leave it out for both.",
        )
    })?;
    let limit = match q.limit {
        None => LOG_PAGE,
        Some(n) if n >= 1 => usize::try_from(n).unwrap_or(LOG_PAGE_MAX).min(LOG_PAGE_MAX),
        Some(_) => {
            return Err(ApiError::field(
                "That is not a number of entries.",
                "limit",
                "Ask for at least one entry.",
            ))
        }
    };
    Ok(Json(s.logs.read(stream, limit)))
}

/// This process and this host.
#[utoipa::path(
    get,
    path = "/api/v1/system/info",
    tag = "system",
    responses((status = 200, description = "This machine", body = SystemInfoView))
)]
async fn get_system_info(State(s): State<Arc<Mgmt>>) -> Reply<SystemInfoView> {
    Ok(Json(s.machine.info()))
}

/// Every serial port a station could be plugged into.
#[utoipa::path(
    get,
    path = "/api/v1/system/serial-ports",
    tag = "system",
    responses(
        (status = 200, description = "Every serial port found", body = SerialPortsView),
        (status = 500, description = "The devices could not be listed", body = ErrorBody),
    )
)]
async fn get_serial_ports(State(s): State<Arc<Mgmt>>) -> Reply<SerialPortsView> {
    Ok(Json(s.machine.serial_ports().await?))
}

// ---------------------------------------------------------------------------------------
// §5 -- live data, and N-11's fleet numbers
// ---------------------------------------------------------------------------------------

/// What `GET /api/v1/events` accepts.
#[derive(Debug, Clone, serde::Deserialize)]
struct EventsQuery {
    /// `components`, `logs`, `readings`, comma-separated. Absent is `components`.
    #[serde(default)]
    channels: Option<String>,
    /// Which log ring to backfill and follow: `app`, `access` or both.
    #[serde(default)]
    log_stream: Option<String>,
    /// The resume point, for a client that re-creates the connection itself.
    ///
    /// Accepted **as well as** the `Last-Event-ID` header, not instead of it: a browser's
    /// own `EventSource` reconnect sends the header and cannot be made to send a query
    /// parameter, and §5.2's backoff schedule requires the client to reconnect on its own
    /// behalf -- at which point the header is gone. Supporting both is what makes the
    /// resume rules identical either way.
    #[serde(default)]
    last_event_id: Option<String>,
}

/// §5's one live connection.
///
/// Not declared with a response body: the body is an unbounded `text/event-stream`, and
/// describing it as a schema would invite a generated client to `await` it as JSON. The
/// frame shapes are in `crate::events`' module documentation, which is where a client
/// author should read them.
#[utoipa::path(
    get,
    path = "/api/v1/events",
    tag = "events",
    params(
        ("channels" = Option<String>, Query,
         description = "Comma-separated: components, logs, readings. Default components."),
        ("log_stream" = Option<String>, Query, description = "app, access, or both."),
        ("last_event_id" = Option<String>, Query, description = "Resume point; see also Last-Event-ID."),
    ),
    responses(
        (status = 200, description = "An event stream, typed by the SSE event field",
         content_type = "text/event-stream"),
        (status = 422, description = "A channel or stream this build does not know", body = ErrorBody),
        (status = 501, description = "This build serves no live data", body = ErrorBody),
    )
)]
async fn get_events(
    State(s): State<Arc<Mgmt>>,
    Query(q): Query<EventsQuery>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let Some(live) = s.live.clone() else {
        return Err(ApiError::Unsupported {
            summary: "This build of RemoteWeather does not send live updates.".to_string(),
        });
    };
    let channels = crate::events::Channel::parse_list(q.channels.as_deref().unwrap_or(""))
        .map_err(|name| ApiError::Invalid {
            summary: format!("There is no live channel called \u{201c}{name}\u{201d}."),
            errors: vec![FieldError {
                field: "channels".to_string(),
                message: "Ask for components, logs or readings.".to_string(),
            }],
        })?;
    let log_stream = Stream::parse(q.log_stream.as_deref()).ok_or_else(|| ApiError::Invalid {
        summary: "There is no log called that.".to_string(),
        errors: vec![FieldError {
            field: "log_stream".to_string(),
            message: "Ask for app, access, or leave it out for both.".to_string(),
        }],
    })?;
    // The header wins over the query parameter: a browser reconnecting on its own behalf
    // sends the header, and it is the more recent of the two by construction -- the query
    // parameter was written when the URL was built.
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or(q.last_event_id)
        .and_then(|v| v.trim().parse::<u64>().ok());

    let rendering = events::Rendering::gather(s.admin.as_ref()).await?;
    let request = events::Request {
        channels,
        log_stream,
        last_event_id,
    };
    Ok(events::connect(&live, s.control.as_ref(), &rendering, request).into_response())
}

/// §5.1's in-stream channel change, so a route change does not drop the connection.
#[utoipa::path(
    post,
    path = "/api/v1/events/{stream_id}/channels",
    tag = "events",
    params(("stream_id" = String, Path, description = "The id the stream's `hello` frame carried")),
    request_body = ChannelsInput,
    responses(
        (status = 200, description = "The channels now in force", body = ChannelsView),
        (status = 404, description = "No such connection", body = ErrorBody),
        (status = 422, description = "A channel this build does not know", body = ErrorBody),
        (status = 501, description = "This build serves no live data", body = ErrorBody),
    )
)]
async fn post_event_channels(
    State(s): State<Arc<Mgmt>>,
    Path(stream_id): Path<String>,
    Json(input): Json<ChannelsInput>,
) -> Reply<ChannelsView> {
    let Some(live) = s.live.clone() else {
        return Err(ApiError::Unsupported {
            summary: "This build of RemoteWeather does not send live updates.".to_string(),
        });
    };
    match live.hub.set_channels(&stream_id, &input.channels) {
        Ok(channels) => Ok(Json(ChannelsView { channels })),
        // The empty string is the hub's "no such connection", which is a stale client
        // rather than a bad request: it reconnects and gets a new id.
        Err(name) if name.is_empty() => Err(ApiError::NotFound {
            what: "live connection",
            id: stream_id,
        }),
        Err(name) => Err(ApiError::Invalid {
            summary: format!("There is no live channel called \u{201c}{name}\u{201d}."),
            errors: vec![FieldError {
                field: "channels".to_string(),
                message: "Ask for components, logs or readings.".to_string(),
            }],
        }),
    }
}

/// **N-11**: the unsent-readings buffer, the peers, and what enrolment is open.
#[utoipa::path(
    get,
    path = "/api/v1/fleet",
    tag = "fleet",
    responses(
        (status = 200, description = "Unsent readings, peers and enrolment", body = FleetView),
        (status = 500, description = "The peer rows could not be read", body = ErrorBody),
        (status = 501, description = "This node has no spool and no peers", body = ErrorBody),
    )
)]
async fn get_fleet(State(s): State<Arc<Mgmt>>) -> Reply<FleetView> {
    let Some(fleet) = s.fleet.clone() else {
        return Err(ApiError::Unsupported {
            summary: "This node keeps no unsent readings and has no peers.".to_string(),
        });
    };
    Ok(Json(fleet.view().await?))
}

// ---------------------------------------------------------------------------------------
// The shared halves

// ---------------------------------------------------------------------------------------

/// Validate a change in the operator's words, then commit it.
///
/// The field-level pass happens **before** the transaction so that an empty required field
/// does not reach SQLite and come back as a constraint name. The two checks after it --
/// `plan()`'s and the schema's -- are inside [`ConfigAdmin::apply`], where the transaction
/// is.
async fn save(s: &Arc<Mgmt>, change: Change) -> Reply<Applied> {
    Ok(Json(try_change(s, change, Commit::Save).await?))
}

/// Validate in the operator's words, then try or make the change.
///
/// One function for both verbs, because every difference between them is one more way for
/// a preview and the save behind it to disagree — and "a save's preview names exactly the
/// components that then restart" is the claim this whole surface is built on.
async fn try_change(s: &Arc<Mgmt>, change: Change, commit: Commit) -> Result<Applied, ApiError> {
    let problems = change.problems();
    if !problems.is_empty() {
        return Err(ApiError::Invalid {
            summary: "Check the highlighted fields.".to_string(),
            errors: problems,
        });
    }
    s.admin.apply(change, commit).await
}

/// A path segment that is not a usable component id is a `404` rather than a `422`.
///
/// The segment is an **identifier**, not a submitted value: there is no field to put a
/// message beside, and "there is no component called `wu backyard`" is the whole diagnosis.
/// A website or storage id, as a `404` rather than as `axum`'s plain-text `400`.
///
/// The path segment is an **identifier**, not a submitted value: there is no field to put a
/// message beside, and `Path<i64>` would answer a rejection that has none of this surface's
/// one error shape — the thing `crate::error` exists to make true of every response.
fn website_id(id: &str) -> Result<i64, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound {
        what: "website",
        id: id.to_string(),
    })
}

/// The same, for the one weather database.
fn storage_id(id: &str) -> Result<i64, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound {
        what: "weather database",
        id: id.to_string(),
    })
}

fn component_id(id: &str) -> Result<ComponentId, ApiError> {
    ComponentId::new(id).map_err(|_| ApiError::NotFound {
        what: "component",
        id: id.to_string(),
    })
}

/// `ControlError`, as a status an operator can act on.
///
/// `NoSuchComponent` is a `404`; the other three are `409`, because nothing the operator
/// types differently fixes them -- which is the distinction `ControlError`'s own
/// classification already makes (`mw_types::Classify`).
fn control_refusal(e: ControlError) -> ApiError {
    match &e {
        ControlError::NoSuchComponent { id } => ApiError::NotFound {
            what: "component",
            id: id.as_str().to_string(),
        },
        ControlError::NotRunnable { .. } | ControlError::NotLive { .. } => ApiError::Conflict {
            // Plain, because this one reaches a button the operator just pressed. The
            // technical form -- which trigger, which state -- is the detail.
            summary: "There is nothing to run just now.".to_string(),
            detail: Some(mw_types::DisplayChain(&e).to_string()),
        },
        ControlError::ShuttingDown => ApiError::Conflict {
            summary: "The daemon is shutting down.".to_string(),
            detail: Some(mw_types::DisplayChain(&e).to_string()),
        },
    }
}

/// `LogControlError`, as a status.
///
/// A bad directive is the operator's to fix and lands on the field; a subscriber with no
/// reload handle is this build's and is a `409`, because retrying will not install one.
fn log_refusal(e: LogControlError) -> ApiError {
    match &e {
        LogControlError::FilterParse { reason, .. } => ApiError::field(
            "That is not a filter this daemon understands.",
            "directive",
            format!("Check the filter syntax: {reason}"),
        ),
        LogControlError::Unavailable => ApiError::Conflict {
            summary: "This daemon's logging cannot be changed while it runs.".to_string(),
            detail: Some(mw_types::DisplayChain(&e).to_string()),
        },
    }
}

// ---------------------------------------------------------------------------------------
// Authentication — implementation-plan.md M7-2
//
// W5 §1.1's three registrations, plus the four §3.3 and §3.4 need that the Go surface had
// no equivalent of: the setup claim, the session list, the two revocations and the password
// change. Eight operations over the one `admin_credential` row and the `admin_session`
// table, with every secret comparison in `crate::auth` and every statement in the binary.
//
// **None of these is a configuration change.** They write `admin_credential` and
// `admin_session`, which are not in `Config` and not in any plan, so nothing here bumps the
// generation or restarts a component -- the single exception being the one widening the
// binary performs when an instance is first claimed, which is the listener's bind address
// and is handled there (`crate::admin`), not here.
// ---------------------------------------------------------------------------------------

/// What `POST /api/v1/auth/login` submits.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct LoginInput {
    /// The administrator's username.
    pub username: String,
    /// Their password.
    ///
    /// A `SecretString` from the moment it is deserialised (**D-33**, §3.2), so a `{:?}` on
    /// this struct anywhere downstream cannot print it. `value_type = String` because that
    /// is what is on the wire; the schema describes the JSON, not the Rust.
    #[schema(value_type = String)]
    pub password: SecretString,
}

/// What `POST /api/v1/auth/claim` submits — §3.4's one-time setup claim.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct ClaimInput {
    /// The code printed to the console at first start.
    ///
    /// Secret for the same reason the password is, and for one more: this is the value
    /// §3.5's "nothing secret in logs" rule makes the single deliberate exception for, and
    /// the exception is the **banner**, not a `{:?}`.
    #[schema(value_type = String)]
    pub setup_code: SecretString,
    /// The username to create.
    pub username: String,
    /// The password to set.
    #[schema(value_type = String)]
    pub password: SecretString,
    /// The same password again (§3.4 step 3: "a password (twice)").
    #[schema(value_type = String)]
    pub password_again: SecretString,
}

/// What `POST /api/v1/auth/password` submits.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct PasswordInput {
    /// The current password. **Required** (§3.5): a session is not sufficient authority to
    /// replace the credential, because the session is what a borrowed laptop has.
    #[schema(value_type = String)]
    pub current_password: SecretString,
    /// The new password.
    #[schema(value_type = String)]
    pub new_password: SecretString,
    /// The new password again.
    #[schema(value_type = String)]
    pub new_password_again: SecretString,
}

/// One session, as the session list shows it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SessionView {
    /// The public handle, hex. What `DELETE /api/v1/auth/sessions/{id}` names.
    pub id: String,
    /// When it was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it was last used.
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    /// When it stops working.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// The browser that created it, if it said — §3.3's "which browser is this?".
    pub user_agent: Option<String>,
    /// Where it was created from.
    pub created_ip: Option<String>,
    /// Whether this is the session making the request, so the list can say "this browser"
    /// rather than offering to sign the operator out of the page they are looking at.
    pub current: bool,
}

/// `GET /api/v1/auth/sessions`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SessionsView {
    /// Every live session, newest first.
    pub sessions: Vec<SessionView>,
}

/// The password rule, so the form states it rather than restating it.
///
/// §0.5's inline-help standard applied to the one rule there is: a client that hardcoded
/// "at least 12 characters" would be a second copy of [`crate::auth::POLICY_MIN_CHARS`],
/// and the two would disagree the first time the policy moved.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PasswordPolicyView {
    /// The fewest characters accepted.
    pub minimum_characters: usize,
    /// The most bytes accepted.
    pub maximum_bytes: usize,
}

impl PasswordPolicyView {
    /// The policy as the server enforces it.
    fn current() -> Self {
        Self {
            minimum_characters: crate::auth::POLICY_MIN_CHARS,
            maximum_bytes: crate::auth::POLICY_MAX_BYTES,
        }
    }
}

/// `GET /api/v1/auth/session` — the one unauthenticated read, and the first request the UI
/// makes.
///
/// Three states in one object, which is what §1.1 means by "also reports whether the
/// instance has been claimed, which drives the first-run redirect":
///
/// | `claimed` | `authenticated` | the client shows |
/// |---|---|---|
/// | `false` | `false` | §0.7's "Create your administrator account" |
/// | `true` | `false` | the login screen (§8.8's signed-out state) |
/// | `true` | `true` | the application |
///
/// `claimed = false, authenticated = true` is unrepresentable in practice and is not
/// special-cased: the claim creates the credential and the session in one transaction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct AuthStateView {
    /// Whether an administrator account exists.
    pub claimed: bool,
    /// Whether this request carried a live session.
    pub authenticated: bool,
    /// Who, when `authenticated`.
    pub username: Option<String>,
    /// This request's session, when `authenticated`.
    pub session: Option<SessionView>,
    /// The password rule the forms should state.
    pub password_policy: PasswordPolicyView,
}

/// What a successful login, claim or password change answers with.
///
/// One type for all three because the client does the same thing with all three: it is
/// signed in, as this person, on this session, with these things worth knowing. `warnings`
/// is §8.5's non-blocking slot and is where **O-10**'s breached-password notice lands --
/// which is the whole reason that decision is "warn" rather than "refuse".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SignedInView {
    /// The administrator's username.
    pub username: String,
    /// The session this request is now on.
    pub session: SessionView,
    /// Consequences that do not block.
    pub warnings: Vec<String>,
}

/// The authentication routes.
///
/// A separate sub-router from the configuration one because they hold different state --
/// these need the [`Guard`], those need [`Mgmt`] -- and because keeping them apart is what
/// lets this crate's own tests exercise the gate over the real handler set without
/// assembling a `ConfigAdmin`, a `ComponentControl`, a `LogSource` and a `Machine` first.
///
/// `crate::guard::enforce` wraps **both**, in [`router`], so there is still exactly one gate.
fn authentication(guard: Arc<Guard>) -> Router {
    Router::new()
        // §1.1 -- the three registrations, rewritten.
        .route("/api/v1/auth/login", post(post_login))
        .route(
            "/api/v1/auth/session",
            get(get_auth_state).delete(delete_session),
        )
        // §3.4 -- the one-time setup claim.
        .route("/api/v1/auth/claim", post(post_claim))
        // §3.3 -- individually revocable sessions, and "sign out everywhere".
        .route(
            "/api/v1/auth/sessions",
            get(get_sessions).delete(delete_all_sessions),
        )
        .route("/api/v1/auth/sessions/{id}", delete(delete_one_session))
        // §3.5 -- changing the password requires the current one.
        .route("/api/v1/auth/password", post(post_password))
        .with_state(guard)
}

/// Render a session for the wire.
fn session_view(session: &crate::auth::Session, current: Option<&[u8; 16]>) -> SessionView {
    SessionView {
        id: crate::auth::session_id(&session.id),
        created_at: session.created_at,
        last_seen_at: session.last_seen_at,
        expires_at: session.expires_at,
        user_agent: session.user_agent.clone(),
        created_ip: session.created_ip.clone(),
        current: current == Some(&session.id),
    }
}

/// The `User-Agent`, truncated.
///
/// Truncated rather than rejected, because a long one is a browser quirk and not an attack,
/// and this column is shown in a list: 256 bytes is more than any real agent string and
/// bounds what one request can write into the configuration database.
fn user_agent(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(256).collect())
}

/// Sign in.
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = LoginInput,
    responses(
        (status = 200, description = "Signed in; the session is in a Set-Cookie", body = SignedInView),
        (status = 401, description = "The username and password do not match", body = ErrorBody),
        (status = 409, description = "The instance has not been claimed yet", body = ErrorBody),
        (status = 429, description = "The progressive delay is still holding this off", body = ErrorBody),
    )
)]
async fn post_login(
    State(guard): State<Arc<Guard>>,
    Extension(peer): Extension<Peer>,
    headers: axum::http::HeaderMap,
    Body(input): Body<LoginInput>,
) -> Result<axum::response::Response, ApiError> {
    let store = guard.store();
    let Some(credential) = store.credential().await? else {
        return Err(ApiError::Conflict {
            summary: "This daemon has not been set up yet. Create the administrator account \
                      with the code printed on its console."
                .to_string(),
            detail: None,
        });
    };
    let now = chrono::Utc::now().timestamp();

    // **Before the hash, not after** (§3.5): applying the delay after verification would
    // make its presence an oracle for whether the password was right.
    if let Some(seconds) =
        crate::auth::retry_after(credential.failed_attempts, credential.last_failed_at, now)
    {
        tracing::warn!(
            target: crate::auth::AUDIT,
            outcome = "throttled",
            username = %input.username,
            source_ip = peer.0.as_deref().unwrap_or("-"),
            retry_after_seconds = seconds,
            "login refused"
        );
        return Err(ApiError::Throttled {
            summary: format!("Too many attempts. Try again in {seconds} seconds."),
            retry_after_seconds: seconds,
        });
    }

    // Both halves evaluated regardless, so the time this takes does not depend on whether
    // the username was right. `&` rather than `&&` would not be enough on its own -- the
    // verification is the expensive half and it has to actually run.
    let verified = crate::auth::verify_password(&credential.password_phc, &input.password);
    let named = credential.username == input.username;
    if !verified.ok || !named {
        store.record_failure().await?;
        tracing::warn!(
            target: crate::auth::AUDIT,
            outcome = "refused",
            username = %input.username,
            source_ip = peer.0.as_deref().unwrap_or("-"),
            "login refused"
        );
        // One message for both halves. Saying which was wrong tells an unauthenticated
        // caller the administrator's username.
        return Err(ApiError::Unauthenticated {
            summary: "That username and password do not match.".to_string(),
        });
    }

    let rehash = if verified.needs_rehash {
        // §3.2's transparent upgrade, from the plaintext that was just verified. The store
        // writes it in the same transaction as the session, so a failure leaves the old
        // hash rather than no hash.
        Some(crate::auth::hash_password(&input.password)?)
    } else {
        None
    };
    let (set_cookie, view) = open(guard.as_ref(), &credential, &headers, &peer, rehash).await?;
    tracing::info!(
        target: crate::auth::AUDIT,
        outcome = "ok",
        username = %credential.username,
        source_ip = peer.0.as_deref().unwrap_or("-"),
        session_id = %view.session.id,
        rehashed = verified.needs_rehash,
        "signed in"
    );
    Ok(signed_in(&set_cookie, view))
}

/// Create the administrator account — §3.4's one-time claim.
#[utoipa::path(
    post,
    path = "/api/v1/auth/claim",
    tag = "auth",
    request_body = ClaimInput,
    responses(
        (status = 200, description = "The account exists and you are signed in", body = SignedInView),
        (status = 409, description = "Already claimed, or there is no outstanding code", body = ErrorBody),
        (status = 422, description = "The code or the password was refused", body = ErrorBody),
    )
)]
async fn post_claim(
    State(guard): State<Arc<Guard>>,
    Extension(peer): Extension<Peer>,
    headers: axum::http::HeaderMap,
    Body(input): Body<ClaimInput>,
) -> Result<axum::response::Response, ApiError> {
    use secrecy::ExposeSecret as _;

    let store = guard.store();
    // **Before anything expensive.** The authoritative check is inside `claim`'s own
    // transaction -- that is what makes the race resolve to exactly one claimant -- but this
    // route is unauthenticated, and a caller who loops it against an instance that is
    // *already* claimed would otherwise spend a 64 MiB Argon2id hash per request before
    // being refused. Hashing is serialised process-wide (`auth::hash_password`), so that is a
    // queue in front of every legitimate login rather than just wasted work.
    if store.claimed().await? {
        return Err(claim_refusal(crate::auth::ClaimRefusal::AlreadyClaimed));
    }
    let mut errors = crate::auth::password_problems(&input.password, "password");
    if input.password.expose_secret() != input.password_again.expose_secret() {
        errors.push(FieldError::new(
            "password_again",
            "Those two passwords are not the same.",
        ));
    }
    if input.username.trim().is_empty() {
        errors.push(FieldError::new("username", "Choose a username."));
    }
    if !errors.is_empty() {
        return Err(ApiError::Invalid {
            summary: "That account cannot be created.".to_string(),
            errors,
        });
    }

    let password_phc = crate::auth::hash_password(&input.password)?;
    let username = input.username.trim().to_string();
    let minted = crate::auth::Minted::new();
    // Fourteen days, which is `admin_credential`'s own default and cannot be read off a row
    // that does not exist yet -- this request is what creates it.
    let lifetime = std::time::Duration::from_secs(crate::auth::DEFAULT_ABSOLUTE_SECONDS);
    let set_cookie = crate::auth::set_cookie(&minted, lifetime, guard.secure());
    let created_at = chrono::Utc::now().timestamp();
    let new_session = crate::auth::NewSession {
        minted,
        created_at,
        expires_at: created_at
            + i64::try_from(crate::auth::DEFAULT_ABSOLUTE_SECONDS).unwrap_or(i64::MAX),
        user_agent: user_agent(&headers),
        created_ip: peer.0.clone(),
    };
    let session = new_session.as_session().ok_or_else(|| ApiError::Internal {
        summary: "This machine's clock is outside the range this daemon can record.".to_string(),
        detail: None,
    })?;
    let outcome = store
        .claim(crate::auth::NewAccount {
            typed_code: crate::auth::typed_setup_code(input.setup_code.expose_secret()),
            username: username.clone(),
            password_phc,
            session: new_session,
        })
        .await?;
    if let Err(refusal) = outcome {
        tracing::warn!(
            target: crate::auth::AUDIT,
            outcome = "refused",
            username = %username,
            source_ip = peer.0.as_deref().unwrap_or("-"),
            reason = refusal.as_str(),
            "setup claim refused"
        );
        return Err(claim_refusal(refusal));
    }
    // §3.4 step 5: the claim is audited with its source IP. The code itself is not here and
    // never will be -- the only place it is rendered is the console banner.
    tracing::info!(
        target: crate::auth::AUDIT,
        outcome = "ok",
        username = %username,
        source_ip = peer.0.as_deref().unwrap_or("-"),
        session_id = %crate::auth::session_id(&session.id),
        "administrator account created"
    );
    Ok(signed_in(
        &set_cookie,
        SignedInView {
            username,
            session: session_view(&session, Some(&session.id)),
            warnings: crate::auth::password_warnings(&input.password),
        },
    ))
}

/// Whether this instance is claimed, and whether this request is signed in.
#[utoipa::path(
    get,
    path = "/api/v1/auth/session",
    tag = "auth",
    responses((status = 200, description = "The three states of §1.1's table", body = AuthStateView))
)]
async fn get_auth_state(
    State(guard): State<Arc<Guard>>,
    current: Option<Extension<Current>>,
) -> Reply<AuthStateView> {
    let current = current.map(|Extension(c)| c);
    Ok(Json(AuthStateView {
        claimed: guard.store().claimed().await?,
        authenticated: current.is_some(),
        username: current.as_ref().map(|c| c.username.clone()),
        session: current
            .as_ref()
            .map(|c| session_view(&c.session, Some(&c.session.id))),
        password_policy: PasswordPolicyView::current(),
    }))
}

/// Sign out of this browser — W5 §1.1's `POST /logout`, which now actually revokes.
#[utoipa::path(
    delete,
    path = "/api/v1/auth/session",
    tag = "auth",
    responses((status = 204, description = "Signed out; the session row is gone"))
)]
async fn delete_session(
    State(guard): State<Arc<Guard>>,
    Extension(current): Extension<Current>,
) -> Result<axum::response::Response, ApiError> {
    guard.store().revoke(current.session.id).await?;
    tracing::info!(
        target: crate::auth::AUDIT,
        outcome = "ok",
        username = %current.username,
        session_id = %crate::auth::session_id(&current.session.id),
        "signed out"
    );
    Ok(signed_out())
}

/// Every live session (§3.3).
#[utoipa::path(
    get,
    path = "/api/v1/auth/sessions",
    tag = "auth",
    responses((status = 200, description = "Every live session, newest first", body = SessionsView))
)]
async fn get_sessions(
    State(guard): State<Arc<Guard>>,
    Extension(current): Extension<Current>,
) -> Reply<SessionsView> {
    let sessions = guard.store().sessions().await?;
    Ok(Json(SessionsView {
        sessions: sessions
            .iter()
            .map(|s| session_view(s, Some(&current.session.id)))
            .collect(),
    }))
}

/// Revoke one session (§3.3).
#[utoipa::path(
    delete,
    path = "/api/v1/auth/sessions/{id}",
    tag = "auth",
    params(("id" = String, Path, description = "The session's public handle")),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, description = "There is no such session", body = ErrorBody),
    )
)]
async fn delete_one_session(
    State(guard): State<Arc<Guard>>,
    Extension(current): Extension<Current>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let Some(target) = crate::auth::parse_session_id(&id) else {
        return Err(ApiError::NotFound {
            what: "session",
            id,
        });
    };
    if !guard.store().revoke(target).await? {
        return Err(ApiError::NotFound {
            what: "session",
            id,
        });
    }
    tracing::info!(
        target: crate::auth::AUDIT,
        outcome = "ok",
        username = %current.username,
        session_id = %id,
        "session revoked"
    );
    // Revoking the session the request arrived on is a legitimate thing to do from the
    // session list, and it has to clear the cookie or the browser keeps presenting a handle
    // that no longer resolves.
    if target == current.session.id {
        return Ok(signed_out());
    }
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// Sign out everywhere (§3.3) — **including** the session making the request.
#[utoipa::path(
    delete,
    path = "/api/v1/auth/sessions",
    tag = "auth",
    responses((status = 204, description = "Every session is gone, this one included"))
)]
async fn delete_all_sessions(
    State(guard): State<Arc<Guard>>,
    Extension(current): Extension<Current>,
) -> Result<axum::response::Response, ApiError> {
    let revoked = guard.store().revoke_all().await?;
    tracing::info!(
        target: crate::auth::AUDIT,
        outcome = "ok",
        username = %current.username,
        revoked,
        "signed out everywhere"
    );
    Ok(signed_out())
}

/// Change the password (§3.3, §3.5).
///
/// **Revokes every other session and keeps this one**, which is implementation-plan.md
/// **M7-2**'s done-criterion and what a person expects after "my laptop was stolen". The
/// cookie is therefore *not* cleared and no new session is minted: the operator stays on
/// the page they were on.
#[utoipa::path(
    post,
    path = "/api/v1/auth/password",
    tag = "auth",
    request_body = PasswordInput,
    responses(
        (status = 200, description = "Changed; every other session is revoked", body = SignedInView),
        (status = 422, description = "The current password was wrong, or the new one was refused", body = ErrorBody),
        (status = 429, description = "The progressive delay is still holding this off", body = ErrorBody),
    )
)]
async fn post_password(
    State(guard): State<Arc<Guard>>,
    Extension(current): Extension<Current>,
    Extension(peer): Extension<Peer>,
    Body(input): Body<PasswordInput>,
) -> Reply<SignedInView> {
    use secrecy::ExposeSecret as _;

    let store = guard.store();
    let credential = store
        .credential()
        .await?
        .ok_or_else(|| ApiError::Internal {
            summary: "The administrator account could not be read.".to_string(),
            detail: None,
        })?;
    let now = chrono::Utc::now().timestamp();
    // §3.5: "rate-limited on the same schedule as login". The same counter, because the
    // thing being guessed is the same secret.
    if let Some(seconds) =
        crate::auth::retry_after(credential.failed_attempts, credential.last_failed_at, now)
    {
        return Err(ApiError::Throttled {
            summary: format!("Too many attempts. Try again in {seconds} seconds."),
            retry_after_seconds: seconds,
        });
    }
    if !crate::auth::verify_password(&credential.password_phc, &input.current_password).ok {
        store.record_failure().await?;
        tracing::warn!(
            target: crate::auth::AUDIT,
            outcome = "refused",
            username = %credential.username,
            source_ip = peer.0.as_deref().unwrap_or("-"),
            session_id = %crate::auth::session_id(&current.session.id),
            "password change refused"
        );
        return Err(ApiError::field(
            "That password change was refused.",
            "current_password",
            "That is not the current password.",
        ));
    }
    let mut errors = crate::auth::password_problems(&input.new_password, "new_password");
    if input.new_password.expose_secret() != input.new_password_again.expose_secret() {
        errors.push(FieldError::new(
            "new_password_again",
            "Those two passwords are not the same.",
        ));
    }
    if !errors.is_empty() {
        return Err(ApiError::Invalid {
            summary: "That password change was refused.".to_string(),
            errors,
        });
    }

    let phc = crate::auth::hash_password(&input.new_password)?;
    store.change_password(phc, current.session.id).await?;
    tracing::info!(
        target: crate::auth::AUDIT,
        outcome = "ok",
        username = %credential.username,
        source_ip = peer.0.as_deref().unwrap_or("-"),
        session_id = %crate::auth::session_id(&current.session.id),
        "password changed; every other session revoked"
    );
    Ok(Json(SignedInView {
        username: credential.username,
        session: session_view(&current.session, Some(&current.session.id)),
        warnings: crate::auth::password_warnings(&input.new_password),
    }))
}

/// Mint a session, write it, and build the `200` that carries it.
///
/// Shared by login and nothing else today; it is a function rather than inline because the
/// ordering matters and is worth stating once: the row is written **before** the response is
/// built, so a store failure is a `500` with no cookie rather than a cookie the server does
/// not recognise.
async fn open(
    guard: &Guard,
    credential: &crate::auth::StoredCredential,
    headers: &axum::http::HeaderMap,
    peer: &Peer,
    rehash: Option<String>,
) -> Result<(String, SignedInView), ApiError> {
    let minted = crate::auth::Minted::new();
    let lifetime = std::time::Duration::from_secs(
        u64::try_from(credential.session_absolute_seconds).unwrap_or(0),
    );
    let set_cookie = crate::auth::set_cookie(&minted, lifetime, guard.secure());
    let created_at = chrono::Utc::now().timestamp();
    let new_session = crate::auth::NewSession {
        minted,
        created_at,
        expires_at: created_at + credential.session_absolute_seconds,
        user_agent: user_agent(headers),
        created_ip: peer.0.clone(),
    };
    let session = new_session.as_session().ok_or_else(|| ApiError::Internal {
        summary: "This machine's clock is outside the range this daemon can record.".to_string(),
        detail: None,
    })?;
    guard.store().open_session(new_session, rehash).await?;
    Ok((
        set_cookie,
        SignedInView {
            username: credential.username.clone(),
            session: session_view(&session, Some(&session.id)),
            warnings: Vec::new(),
        },
    ))
}

/// A `200` with the session cookie on it.
fn signed_in(set_cookie: &str, view: SignedInView) -> axum::response::Response {
    let mut response = Json(view).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(set_cookie) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    response
}

/// A `204` that clears the cookie under **both** names.
///
/// Both, because a daemon that has just had a management certificate configured will be
/// presented the plaintext cookie a browser still holds, and clearing only the name this
/// socket would set would leave it there.
fn signed_out() -> axum::response::Response {
    let mut response = axum::http::StatusCode::NO_CONTENT.into_response();
    for clear in [
        crate::auth::clear_cookie(true),
        crate::auth::clear_cookie(false),
    ] {
        if let Ok(value) = axum::http::HeaderValue::from_str(&clear) {
            response
                .headers_mut()
                .append(axum::http::header::SET_COOKIE, value);
        }
    }
    response
}

/// `ClaimRefusal`, as a status.
///
/// A wrong code is `422` on the field the operator typed it into; the other two are `409`,
/// because nothing they type differently fixes an instance that is already claimed or has no
/// outstanding code (`modernweather reset-setup-token` does, and the message says so).
fn claim_refusal(refusal: crate::auth::ClaimRefusal) -> ApiError {
    match refusal {
        crate::auth::ClaimRefusal::WrongCode => ApiError::field(
            "That setup code was refused.",
            "setup_code",
            "Check the code printed on this machine's console. It is shown once, at startup.",
        ),
        crate::auth::ClaimRefusal::AlreadyClaimed => ApiError::Conflict {
            summary: "This daemon already has an administrator account. Sign in, or run \
                      `modernweather reset-password` on the machine itself."
                .to_string(),
            detail: None,
        },
        crate::auth::ClaimRefusal::NoCodeOutstanding => ApiError::Conflict {
            summary: "There is no setup code outstanding. Run `modernweather \
                      reset-setup-token` on the machine itself to issue a new one."
                .to_string(),
            detail: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body as AxumBody;
    use axum::http::{header, Request as HttpRequest, StatusCode};
    use serde_json::json;
    use tower::ServiceExt as _;

    use super::*;
    use crate::auth::fake::Store;

    /// The authentication half of [`router`] with the real gate over it.
    ///
    /// The configuration half needs a `ConfigAdmin`, a `ComponentControl`, a `LogSource` and
    /// a `Machine`, none of which has anything to do with what is under test here -- and the
    /// property that the gate covers *those* routes too is asserted against the real binary
    /// in `crates/modernweather/tests/auth.rs`, where the router really is `router`.
    fn app(store: Arc<Store>, origins: &[&str]) -> Router {
        let guard = Arc::new(Guard::new(
            store as Arc<dyn crate::auth::AuthStore>,
            origins.iter().map(|o| (*o).to_string()).collect(),
            false,
        ));
        authentication(Arc::clone(&guard)).layer(axum::middleware::from_fn_with_state(
            guard,
            crate::guard::enforce,
        ))
    }

    /// One request, as a status, the parsed body and the response headers.
    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        csrf: bool,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value, axum::http::HeaderMap) {
        let mut request = HttpRequest::builder().method(method).uri(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if csrf {
            request = request.header("x-mw-request", "1");
        }
        let request = match body {
            Some(json) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(AxumBody::from(json.to_string()))
                .expect("the request is buildable"),
            None => request
                .body(AxumBody::empty())
                .expect("the request is buildable"),
        };
        let response = app
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("the body is readable");
        let parsed = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, parsed, headers)
    }

    /// The `mw_session=<id>.<secret>` cookie out of a `Set-Cookie`.
    fn cookie_from(headers: &axum::http::HeaderMap) -> String {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.split(';').next())
            .expect("a Set-Cookie with a value")
            .to_string()
    }

    /// **The gate.** Every route but the three is a `401` with no cookie, and the three
    /// answer.
    #[tokio::test]
    async fn without_a_session_only_three_routes_answer() {
        let app = app(Arc::new(Store::unclaimed()), &[]);
        for (method, path, csrf) in [
            ("DELETE", "/api/v1/auth/session", true),
            ("GET", "/api/v1/auth/sessions", false),
            ("DELETE", "/api/v1/auth/sessions", true),
            (
                "DELETE",
                "/api/v1/auth/sessions/00000000000000000000000000000000",
                true,
            ),
            ("POST", "/api/v1/auth/password", true),
        ] {
            let (status, body, _) = call(&app, method, path, None, csrf, Some(json!({}))).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
        }
        // And the one unauthenticated read answers with the state the UI branches on.
        let (status, body, _) = call(&app, "GET", "/api/v1/auth/session", None, false, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["claimed"], false);
        assert_eq!(body["authenticated"], false);
        assert_eq!(body["password_policy"]["minimum_characters"], 12);
    }

    /// §3.3's CSRF pair: the header is required on every write, **including** the two writes
    /// a caller with no session may make.
    #[tokio::test]
    async fn a_write_without_the_csrf_header_is_refused_before_anything_reads_a_cookie() {
        let app = app(
            Arc::new(Store::claimed(
                "chris",
                "correct horse battery staple",
                None,
            )),
            &[],
        );
        let credentials = json!({"username": "chris", "password": "correct horse battery staple"});
        let (status, body, headers) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            false,
            Some(credentials.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            headers.get_all(header::SET_COOKIE).iter().next().is_none(),
            "a refused request must not mint a session"
        );
        // The same request with the header signs in.
        let (status, _, headers) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            true,
            Some(credentials),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get_all(header::SET_COOKIE).iter().next().is_some());
    }

    /// Sign in, be recognised, and sign out — and the cookie's shape on the way through.
    #[tokio::test]
    async fn a_login_mints_a_session_that_a_logout_revokes() {
        let store = Arc::new(Store::claimed(
            "chris",
            "correct horse battery staple",
            None,
        ));
        let app = app(Arc::clone(&store), &[]);

        let (status, body, headers) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            true,
            Some(json!({"username": "chris", "password": "correct horse battery staple"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["username"], "chris");
        assert_eq!(body["session"]["current"], true);
        assert_eq!(store.session_count(), 1);
        let set_cookie = headers[header::SET_COOKIE]
            .to_str()
            .expect("the cookie is text")
            .to_string();
        for attribute in ["HttpOnly", "SameSite=Strict", "Path=/"] {
            assert!(set_cookie.contains(attribute), "{set_cookie}");
        }
        // **The cookie is not the credential.** Nothing in it is derived from the password,
        // and the body never echoes either.
        assert!(!set_cookie.contains("correct"), "{set_cookie}");
        assert!(!body.to_string().contains("argon2"), "{body}");

        let cookie = cookie_from(&headers);
        let (status, body, _) = call(
            &app,
            "GET",
            "/api/v1/auth/session",
            Some(&cookie),
            false,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["authenticated"], true);
        assert_eq!(body["username"], "chris");

        let (status, _, headers) = call(
            &app,
            "DELETE",
            "/api/v1/auth/session",
            Some(&cookie),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            store.session_count(),
            0,
            "the row is gone, not just the cookie"
        );
        // Both names are cleared, so a certificate added since the login does not leave one
        // behind in the browser.
        let cleared: Vec<String> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        assert_eq!(cleared.len(), 2, "{cleared:?}");
        assert!(cleared.iter().any(|c| c.starts_with("mw_session=;")));
        assert!(cleared.iter().any(|c| c.starts_with("__Host-mw_session=;")));

        // And the cookie no longer resolves, which is the whole of §3.1's last defect: today
        // a browser session cannot be revoked without rotating the API credential.
        let (status, body, _) = call(
            &app,
            "GET",
            "/api/v1/auth/sessions",
            Some(&cookie),
            false,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }

    /// A wrong password and a wrong username are the same answer, and both count a failure.
    #[tokio::test]
    async fn a_refused_login_says_nothing_about_which_half_was_wrong() {
        let store = Arc::new(Store::claimed(
            "chris",
            "correct horse battery staple",
            None,
        ));
        let app = app(Arc::clone(&store), &[]);
        let mut answers = Vec::new();
        for body in [
            json!({"username": "chris", "password": "wrong password here"}),
            json!({"username": "someone", "password": "correct horse battery staple"}),
            json!({"username": "someone", "password": "wrong password here"}),
        ] {
            let (status, parsed, _) =
                call(&app, "POST", "/api/v1/auth/login", None, true, Some(body)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{parsed}");
            answers.push(parsed["summary"].as_str().unwrap_or_default().to_string());
        }
        assert_eq!(answers[0], answers[1], "the two halves answer differently");
        assert_eq!(answers[1], answers[2]);
        assert_eq!(store.failures(), 3);
        assert_eq!(store.session_count(), 0);
    }

    /// §3.5's progressive delay reaches the wire as a `429` with a `Retry-After`, and it is
    /// applied **before** the hash — so a correct password is refused too.
    #[tokio::test]
    async fn the_progressive_delay_is_a_429_with_a_retry_after() {
        let store = Arc::new(Store::claimed(
            "chris",
            "correct horse battery staple",
            None,
        ));
        // Deep enough to be at the 30-second ceiling, which is what makes the assertion on
        // the header a range rather than a value: this test shares eight cores with every
        // other Argon2id hash in the suite, so the wait has visibly counted down by the time
        // the request lands. §3.5's schedule itself is asserted exactly, without a clock, in
        // `auth::tests::the_progressive_delay_is_free_three_times_then_doubles_to_a_ceiling`.
        store.set_failures(12, chrono::Utc::now().timestamp());
        let app = app(Arc::clone(&store), &[]);
        let (status, body, headers) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            true,
            Some(json!({"username": "chris", "password": "correct horse battery staple"})),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let retry: u64 = headers["retry-after"]
            .to_str()
            .expect("the header is text")
            .parse()
            .expect("the header is a number of seconds");
        assert!((1..=30).contains(&retry), "retry-after was {retry}");
        // The right password did not get through, and the attempt is not counted twice:
        // the delay short-circuits before the verification.
        assert_eq!(store.session_count(), 0);
        assert_eq!(store.failures(), 12);
    }

    /// §3.2's transparent upgrade, end to end: a hash stored below policy is replaced by the
    /// login that verified against it.
    #[tokio::test]
    async fn a_login_rehashes_a_credential_stored_below_policy() {
        let store = Arc::new(Store::claimed(
            "chris",
            "correct horse battery staple",
            None,
        ));
        let weak = {
            use argon2::password_hash::rand_core::OsRng;
            use argon2::password_hash::{PasswordHasher as _, SaltString};
            argon2::Argon2::new(
                argon2::Algorithm::Argon2id,
                argon2::Version::V0x13,
                argon2::Params::new(19_456, 2, 1, None).expect("in range"),
            )
            .hash_password(
                b"correct horse battery staple",
                &SaltString::generate(&mut OsRng),
            )
            .expect("hashes")
            .to_string()
        };
        store.set_phc(weak.clone());
        let app = app(Arc::clone(&store), &[]);
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            true,
            Some(json!({"username": "chris", "password": "correct horse battery staple"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let after = store.phc().expect("a credential");
        assert_ne!(after, weak, "the weak hash survived a successful login");
        assert!(
            after.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"),
            "{after}"
        );
    }

    /// The claim: the code is required, the password rule applies, and the breached list
    /// **warns without blocking** — which is **O-10**'s answer to OQ-W5-2.
    #[tokio::test]
    async fn the_setup_claim_requires_the_code_and_warns_rather_than_blocking() {
        let code = crate::auth::SetupCode::issue();
        let store = Arc::new(Store::unclaimed().with_setup(code.token_hash()));
        let app = app(Arc::clone(&store), &[]);

        // A wrong code lands on the field the operator typed it into.
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": "0000-0000-0000",
                "username": "chris",
                "password": "correct horse battery staple",
                "password_again": "correct horse battery staple",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["errors"][0]["field"], "setup_code");
        assert!(!store.claimed_now(), "a refused claim created an account");

        // A short password, with the right code: refused on `password`, and the code is
        // still outstanding afterwards.
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": code.display(),
                "username": "chris",
                "password": "short",
                "password_again": "short",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["errors"][0]["field"], "password");

        // Two different passwords: refused on the confirmation.
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": code.display(),
                "username": "chris",
                "password": "correct horse battery staple",
                "password_again": "correct horse battery stapler",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["errors"][0]["field"], "password_again");

        // A breached password: accepted, with a warning. The claim succeeds and the
        // operator is signed in.
        let (status, body, headers) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": code.display().to_lowercase(),
                "username": "  chris  ",
                "password": "qwerty123456",
                "password_again": "qwerty123456",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["username"], "chris", "the username is trimmed");
        assert_eq!(body["warnings"].as_array().map(Vec::len), Some(1), "{body}");
        assert!(store.claimed_now());
        assert_eq!(store.session_count(), 1);

        // **Single-use**: the same code again is a `409`, because the account now exists.
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": code.display(),
                "username": "someone-else",
                "password": "another good passphrase",
                "password_again": "another good passphrase",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // And the refusal comes **before** the password is validated or hashed, which is
        // what keeps this unauthenticated route from being a 64 MiB Argon2id per request on
        // an instance that is already claimed. A body whose password the policy would refuse
        // still answers `409` rather than `422`: nothing looked at it.
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": code.display(),
                "username": "",
                "password": "short",
                "password_again": "different",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // And the claim's own session works.
        let cookie = cookie_from(&headers);
        let (status, body, _) = call(
            &app,
            "GET",
            "/api/v1/auth/session",
            Some(&cookie),
            false,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["claimed"], true);
        assert_eq!(body["authenticated"], true);
    }

    /// **M7-2's done-criterion, at the handler level**: changing the password revokes every
    /// other session and keeps the current one.
    #[tokio::test]
    async fn changing_the_password_revokes_every_other_session_and_keeps_this_one() {
        let store = Arc::new(Store::claimed(
            "chris",
            "correct horse battery staple",
            None,
        ));
        let app = app(Arc::clone(&store), &[]);
        let sign_in = || async {
            let (status, _, headers) = call(
                &app,
                "POST",
                "/api/v1/auth/login",
                None,
                true,
                Some(json!({"username": "chris", "password": "correct horse battery staple"})),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            cookie_from(&headers)
        };
        // Three browsers.
        let laptop = sign_in().await;
        let phone = sign_in().await;
        let desktop = sign_in().await;
        assert_eq!(store.session_count(), 3);

        // The wrong current password changes nothing.
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/password",
            Some(&desktop),
            true,
            Some(json!({
                "current_password": "not the current one",
                "new_password": "a brand new passphrase",
                "new_password_again": "a brand new passphrase",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["errors"][0]["field"], "current_password");
        assert_eq!(store.session_count(), 3);

        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/password",
            Some(&desktop),
            true,
            Some(json!({
                "current_password": "correct horse battery staple",
                "new_password": "a brand new passphrase",
                "new_password_again": "a brand new passphrase",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(store.session_count(), 1, "the other two survived");

        // This browser is still signed in -- the operator stays on the page they were on.
        let (status, _, _) = call(
            &app,
            "GET",
            "/api/v1/auth/sessions",
            Some(&desktop),
            false,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The stolen laptop and the phone are not.
        for gone in [&laptop, &phone] {
            let (status, _, _) = call(
                &app,
                "GET",
                "/api/v1/auth/sessions",
                Some(gone),
                false,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        // And the new password is the one that works now.
        let (status, _, _) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            true,
            Some(json!({"username": "chris", "password": "a brand new passphrase"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The session list, one revocation, and "sign out everywhere".
    #[tokio::test]
    async fn sessions_are_individually_revocable_and_sign_out_everywhere_takes_this_one_too() {
        let store = Arc::new(Store::claimed(
            "chris",
            "correct horse battery staple",
            None,
        ));
        let app = app(Arc::clone(&store), &[]);
        let sign_in = || async {
            let (_, _, headers) = call(
                &app,
                "POST",
                "/api/v1/auth/login",
                None,
                true,
                Some(json!({"username": "chris", "password": "correct horse battery staple"})),
            )
            .await;
            cookie_from(&headers)
        };
        let laptop = sign_in().await;
        let phone = sign_in().await;

        let (status, body, _) = call(
            &app,
            "GET",
            "/api/v1/auth/sessions",
            Some(&laptop),
            false,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sessions = body["sessions"].as_array().expect("a list");
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions.iter().filter(|s| s["current"] == true).count(),
            1,
            "exactly one session is this browser's"
        );
        let other = sessions
            .iter()
            .find(|s| s["current"] == false)
            .and_then(|s| s["id"].as_str())
            .expect("the other session's handle")
            .to_string();

        // A handle that is not one is a 404 rather than a panic.
        let (status, _, _) = call(
            &app,
            "DELETE",
            "/api/v1/auth/sessions/not-a-handle",
            Some(&laptop),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _, _) = call(
            &app,
            "DELETE",
            &format!("/api/v1/auth/sessions/{other}"),
            Some(&laptop),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(store.session_count(), 1);
        let (status, _, _) = call(
            &app,
            "GET",
            "/api/v1/auth/sessions",
            Some(&phone),
            false,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the revoked cookie is dead"
        );
        // Revoking it again is a 404, not a silent success.
        let (status, _, _) = call(
            &app,
            "DELETE",
            &format!("/api/v1/auth/sessions/{other}"),
            Some(&laptop),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Sign out everywhere takes this one too, and clears the cookie.
        let (status, _, headers) = call(
            &app,
            "DELETE",
            "/api/v1/auth/sessions",
            Some(&laptop),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(store.session_count(), 0);
        assert_eq!(headers.get_all(header::SET_COOKIE).iter().count(), 2);
    }

    /// The CORS allowlist on the wire: an allowlisted origin gets a preflight and the echo,
    /// an unknown one gets neither — and the security headers are on both.
    #[tokio::test]
    async fn cors_is_an_exact_allowlist_and_the_security_headers_are_unconditional() {
        let app = app(Arc::new(Store::unclaimed()), &["https://ui.example.com"]);

        let preflight = |origin: &'static str| {
            let app = app.clone();
            async move {
                let request = HttpRequest::builder()
                    .method("OPTIONS")
                    .uri("/api/v1/auth/login")
                    .header(header::ORIGIN, origin)
                    .header("access-control-request-method", "POST")
                    .body(AxumBody::empty())
                    .expect("buildable");
                app.oneshot(request).await.expect("the router answers")
            }
        };

        let allowed = preflight("https://ui.example.com").await;
        assert_eq!(allowed.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            allowed.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://ui.example.com"
        );
        assert_eq!(
            allowed.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
            "true"
        );
        assert!(allowed.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .expect("text")
            .contains("x-mw-request"));

        let refused = preflight("https://ui.example.com.evil.test").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(!refused
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        assert_eq!(refused.headers()[header::REFERRER_POLICY], "no-referrer");

        // And on an ordinary answer.
        let (_, _, headers) = call(&app, "GET", "/api/v1/auth/session", None, false, None).await;
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    /// Login on an unclaimed instance is a `409` that says what to do, not a `401` that
    /// leaves the operator guessing at a username they never set.
    #[tokio::test]
    async fn logging_in_before_the_claim_says_so() {
        let app = app(Arc::new(Store::unclaimed()), &[]);
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            true,
            Some(json!({"username": "chris", "password": "correct horse battery staple"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("not been set up"),
            "{body}"
        );
    }

    /// Claiming an instance that has no outstanding code points at the CLI that issues one.
    #[tokio::test]
    async fn claiming_with_no_outstanding_code_names_the_command_that_issues_one() {
        let app = app(Arc::new(Store::unclaimed()), &[]);
        let (status, body, _) = call(
            &app,
            "POST",
            "/api/v1/auth/claim",
            None,
            true,
            Some(json!({
                "setup_code": "0000-0000-0000",
                "username": "chris",
                "password": "correct horse battery staple",
                "password_again": "correct horse battery staple",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("reset-setup-token"),
            "{body}"
        );
    }
}
