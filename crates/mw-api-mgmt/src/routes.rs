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
//! # Four dispositions this module makes visible by not having a route
//!
//! Recorded here rather than left as an absence, because a reader counting routes against
//! W5 §1 will look for them:
//!
//! - **`/api/v1/auth/*`** is implementation-plan.md **M7-2**'s: Argon2id at m=64 MiB/t=3/p=4,
//!   sessions that are never the API credential, the `X-MW-Request` header, and the setup
//!   code that goes to stderr only. Until it lands, [`router`] refuses to serve on anything
//!   but loopback -- see its own documentation, which is where that refusal is enforced.
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

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use mw_component::{ComponentControl, ComponentId, ControlError, LogControlError};

use crate::admin::{Applied, Commit, ConfigAdmin};
use crate::change::{
    Change, ForecastsInput, ListenerInput, PairingInput, RadarInput, ServiceInput, StationInput,
    StorageInput, TransportInput, WebsiteInput,
};
use crate::display::{Badge, Display, Section, Vocabulary};
use crate::error::{ApiError, Body, ErrorBody, FieldError};
use crate::host::{
    LogEntry, LogField, LogLevelInput, LogLevelsView, LogSource, LogsView, Machine, SerialPort,
    SerialPortsView, Stream, SystemInfoView,
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
        }
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
/// **It carries no authentication**, and that is M7-2's work rather than an omission here:
/// §3 is a whole work item -- Argon2id parameters, hashed session secrets, CSRF as
/// `SameSite=Strict` plus a required header, the single-use setup code. What this item owes
/// that one is a surface to authenticate, which is this.
///
/// The consequence is handled where it is enforceable rather than left to a reader: the
/// binary binds this router to the configured `management_listen_addr` and **refuses a
/// non-loopback address while the instance is unclaimed**, which is M7-2's own
/// loopback-until-claimed rule applied early. The schema's default for that column is
/// already `127.0.0.1` (**A-W5-3**).
pub fn router(state: Arc<Mgmt>) -> Router {
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
    ),
    components(schemas(
        Applied,
        Badge,
        Change,
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
        NodeView,
        PairingInput,
        PairingView,
        RadarInput,
        RevertInput,
        Section,
        SerialPort,
        SerialPortsView,
        ServiceInput,
        ServiceView,
        ServicesView,
        StationInput,
        StationView,
        StationsView,
        StorageInput,
        StorageView,
        SummaryView,
        SystemInfoView,
        TransportInput,
        WebsiteInput,
        WebsiteView,
        WebsitesView,
    )),
    tags(
        (name = "config", description = "The configuration, the save preview and one step back"),
        (name = "stations", description = "My weather stations"),
        (name = "websites", description = "My website"),
        (name = "storage", description = "Data storage"),
        (name = "services", description = "Share my data"),
        (name = "runtime", description = "Configured versus running"),
        (name = "system", description = "System"),
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
