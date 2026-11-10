//! The Ambient Weather driver: **N stations on one endpoint**.
//!
//! This is the structural one. W2a §4a.5's Ambient row is
//! `HttpIn { /ingest/ambient }`, `FromQuery { id: "ID", secret: Some("PASSWORD") }`,
//! `Cardinality::Many` — one route on the one listener, identity from `?ID=` resolved
//! against `StationRegistry`, and the secret from `?PASSWORD=` compared in constant time
//! (**D-33**). The Go driver is the other shape entirely:
//!
//! | | Go | Here |
//! |---|---|---|
//! | A second Ambient station | a second `http.Server` on a second port | one `config.stations` row |
//! | Listeners | N | 0 — it shares `http:listener` |
//! | Port configuration | one per device, opened by the device's own component | none |
//! | Station identity | **the port the request arrived on** | `?ID=`, against the registry |
//! | Authentication | none: any request on that port is trusted as that station | `?PASSWORD=`, constant-time against the stored hash |
//!
//! The identity row is the defect. `ambientcustomized/station.go:131` builds a reading with
//! `StationName: s.config.Name` — the station is whichever component's listener received
//! the request — and `handleWeatherUpdate` reads `?ID=` and `?PASSWORD=` only to check that
//! they are **non-empty**. So anything that can reach the port can write readings for that
//! station under any identifier at all. Here the claim is a claim until
//! `StationRegistry::resolve` turns it into a `StationKey` (`crate::identity`), which is the
//! only producer of one.
//!
//! # Two lookups, and only one of them authenticates
//!
//! The same shape as the WeatherLink Live listener's, for the same reason. Decoding needs
//! the station's **vane calibration** and the station is only known from the query, so this
//! endpoint holds a map from claimed `?ID=` to codec and looks it up *before* a reading
//! exists. That map decides **how to decode**; `resolve` inside `Ingress::admit` decides
//! **whose reading it is**. A claim in this map and not in the registry is refused by the
//! registry and counted there; a claim in the registry and not in this map is refused here
//! and counted as `unknown_station`.
//!
//! # Where the secret is compared, and where it is not
//!
//! Not here. [`Upload`] carries `?PASSWORD=` into
//! [`FrameMeta::presented_secret`](mw_component::FrameMeta::presented_secret) and
//! `StationRegistry::resolve` is the single gate (W2a §4a.6). This endpoint never sees the
//! stored hash and cannot compare anything, which is the property §4a.6 is stated over —
//! a driver that verified the secret itself would be a second gate, and the two would
//! disagree the first time one of them was changed.
//!
//! # `totalrainin` is the one piece of per-station state a driver holds
//!
//! The console's all-time rainfall counter, which the Go driver turns into an increment by
//! remembering the previous upload (`station.go:216-230`) and overwriting whatever `rainin`
//! put in `rainincremental`. A codec cannot remember anything — that is what makes it
//! golden-testable — so the arithmetic is `mw_codec::rain_since` and the memory is
//! [`Handler::totals`]. It is keyed by the **claim** rather than by the station, because the
//! claim is what this endpoint has before the registry has answered.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use chrono::{DateTime, Utc};
use mw_codec::{AmbientQuery, Codec, Frame, QueryParams};
use mw_component::{
    BoxFuture, ComponentError, ComponentId, ComponentIdError, Cx, Ingress, Methods, Publisher,
    RouteEntry, RouteManifest, RouteProvider, Scope, Serve, Upload, UploadEndpoint, UploadReply,
    UploadStatus,
};
use mw_types::{Class, Classify, DisplayChain, StationKey};
use secrecy::SecretString;
use tracing::{debug, error, info, warn};

use crate::identity::StationIdentity;
use crate::metrics::{self as m, FrameOutcome};

/// The `type` column value this driver serves.
///
/// The frozen contract's spelling, which `docs/legacy-api-contract.md` §3.1 freezes and the
/// schema's own `CHECK` lists. The driver name is the codec's, `ambient-query`.
pub const STATION_TYPE: &str = "ambient-customized";

/// The component id. **One component, however many stations** — §4a.5's own name for it.
pub const ID: &str = "station:ambient";

/// Where the route mounts, as W2a §4a.5 writes it.
pub const DEFAULT_MOUNT: &str = "/ingest/ambient";

/// What an accepted upload answers.
///
/// The Go handler's body (`station.go:148`). An Ambient console does not read it, and
/// keeping it means a `curl` against the endpoint says the same thing on both
/// implementations.
pub const ACCEPTED_BODY: &str = "success";

/// What a malformed upload answers.
pub const MALFORMED_BODY: &str = "missing or malformed parameters\n";

/// What an upload from a station this node does not collect for answers.
pub const UNAUTHORISED_BODY: &str = "unauthorized\n";

/// What an upload answers when the reading could not be durably accepted.
pub const UNAVAILABLE_BODY: &str = "the reading could not be stored; please retry\n";

/// The component id, which is a constant and is still validated.
pub fn endpoint_id() -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(ID)
}

/// Every path this endpoint claims, as **D-51**'s pure declaration.
///
/// `Scope::Any`: an ingest endpoint is not a website's, and a console sends whatever `Host`
/// its firmware puts in the request — usually an IP address, which matches no website.
pub fn manifest(mount: &str) -> RouteManifest {
    RouteManifest::new(Scope::Any, vec![RouteEntry::exact(mount, Methods::READ)])
}

/// One station this endpoint will decode for.
#[derive(Debug, Clone)]
pub struct AmbientUnit {
    /// The `?ID=` this console sends, as `config.stations.presented_key` holds it.
    pub claim: String,
    /// The station it is, for the log line. **Not** the identity: `Ingress::admit` resolves
    /// that from the claim, and this is only what to call the console while saying
    /// something about it.
    pub station: StationKey,
    /// How to decode its uploads — which is the vane calibration and nothing else.
    pub codec: AmbientQuery,
}

/// Everything one upload is answered from.
pub struct Handler {
    /// Keyed by the lower-cased claim. An Ambient console sends whatever the operator typed
    /// into its web interface and an operator types a station key in whatever case they
    /// like; `ConfigRegistry::resolve` lower-cases its own side for the same reason
    /// (**DEV-2**), so the two agree.
    units: BTreeMap<String, AmbientUnit>,
    ingress: Arc<dyn Ingress>,
    /// The previous `?totalrainin=` per claim. See this module's last section.
    totals: Mutex<BTreeMap<String, f32>>,
    receiver: ComponentId,
}

impl fmt::Debug for Handler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handler")
            .field("receiver", &self.receiver)
            .field("units", &self.units.len())
            .finish_non_exhaustive()
    }
}

impl Handler {
    /// The handler for a set of stations on one endpoint.
    pub fn new(
        receiver: ComponentId,
        units: impl IntoIterator<Item = AmbientUnit>,
        ingress: Arc<dyn Ingress>,
    ) -> Self {
        Self {
            units: units
                .into_iter()
                .map(|u| (u.claim.to_ascii_lowercase(), u))
                .collect(),
            ingress,
            totals: Mutex::new(BTreeMap::new()),
            receiver,
        }
    }

    /// How many stations this endpoint decodes for.
    pub fn units(&self) -> usize {
        self.units.len()
    }

    /// Handle one upload.
    ///
    /// Takes the arrival instant as an argument rather than reading a clock, for
    /// `FrameMeta::received_at`'s reason — and here it is only a fallback, because an
    /// Ambient upload carries `?dateutc=` and that is the observation instant.
    pub async fn upload(&self, query: &str, at: DateTime<Utc>) -> UploadReply {
        let id = &self.receiver;
        let params = QueryParams::parse(query);
        if params.skipped() > 0 {
            // `net/url` records the same number in an error its caller throws away.
            // Logged rather than refused, because one mangled parameter out of a hundred
            // and thirty is not a reason to lose the other hundred and twenty-nine.
            debug!(
                skipped = params.skipped(),
                "an Ambient upload carried parameters with invalid escapes"
            );
        }

        // The protocol's own required set, which is the Go handler's check
        // (`station.go:117`) minus the part that treated a non-empty password as
        // authentication.
        if params.get(mw_codec::ACTION_PARAM) != Some(mw_codec::ACTION_UPDATERAW) {
            m::frame(id, FrameOutcome::Undecodable);
            debug!("an Ambient upload carried no `action=updateraw`");
            return UploadReply::refused(UploadStatus::Malformed, MALFORMED_BODY);
        }
        let Some(claim) = params.non_empty(mw_codec::ID_PARAM) else {
            m::frame(id, FrameOutcome::Undecodable);
            debug!("an Ambient upload carried no `ID`");
            return UploadReply::refused(UploadStatus::Malformed, MALFORMED_BODY);
        };
        let Some(secret) = params.non_empty(mw_codec::PASSWORD_PARAM) else {
            m::frame(id, FrameOutcome::Undecodable);
            debug!(claim, "an Ambient upload carried no `PASSWORD`");
            return UploadReply::refused(UploadStatus::Malformed, MALFORMED_BODY);
        };

        // The **decode** lookup, which is not the authentication one.
        let Some(unit) = self.units.get(&claim.to_ascii_lowercase()) else {
            m::frame(id, FrameOutcome::UnknownStation);
            // At DEBUG: an endpoint on the public internet legitimately receives
            // somebody else's console, and at WARN this would be a line per upload
            // forever. The counter is what an operator alerts on (§5.10).
            debug!(
                claim,
                "an Ambient upload claims a station this node has no row for"
            );
            return UploadReply::refused(UploadStatus::Unauthorised, UNAUTHORISED_BODY);
        };

        let frame = Frame::new(query.as_bytes().to_vec());
        let mut batch = match unit.codec.decode(&frame) {
            Ok(batch) => batch,
            Err(e) => {
                m::frame(id, FrameOutcome::Undecodable);
                debug!(
                    station = %unit.station,
                    error = %DisplayChain(&e),
                    "an Ambient upload did not decode"
                );
                return UploadReply::refused(UploadStatus::Malformed, MALFORMED_BODY);
            }
        };
        self.apply_rain_counter(&params, &claim.to_ascii_lowercase(), &mut batch);

        let meta = StationIdentity::FromQuery
            .frame_meta(id, Some(&StationKey::new(claim)), at)
            .with_secret(SecretString::from(secret.to_string()));
        match self.ingress.admit(&batch, &meta).await {
            Ok(admitted) => {
                m::frame(id, FrameOutcome::Decoded);
                debug!(
                    station = %unit.station,
                    accepted = admitted.accepted,
                    rejected = admitted.rejected,
                    "admitted an Ambient upload"
                );
                UploadReply::accepted(ACCEPTED_BODY)
            }
            Err(e) => {
                m::frame(id, FrameOutcome::Refused);
                self.report(&unit.station, &e)
            }
        }
    }

    /// Turn the console's all-time counter into this upload's increment.
    ///
    /// Overwrites whatever `rainin` put in `rainincremental`, which is the Go ordering
    /// (`station.go:216` runs after `parseWeatherData`) — `rainin` is Ambient's **hourly**
    /// rainfall and the increment is what the column means, so the counter wins wherever
    /// the console sends one.
    fn apply_rain_counter(
        &self,
        params: &QueryParams,
        claim: &str,
        batch: &mut [mw_types::PartialReading],
    ) {
        let Some(total) = mw_codec::total_rain(params) else {
            return;
        };
        // The lock is taken and released **before** the admission below: `admit` awaits the
        // spool's `fsync`, and a `std::sync::MutexGuard` held across an await would make
        // two concurrent uploads from the same console a deadlock risk rather than a
        // serialisation.
        let previous = {
            let mut totals = self
                .totals
                .lock()
                .expect("the rain counters are not poisoned");
            totals.insert(claim.to_string(), total)
        };
        let increment = mw_codec::rain_since(previous, total);
        for reading in batch {
            reading.values.rain_incremental = increment;
        }
    }

    /// One admission failure as a reply, at the log level its class deserves.
    fn report(&self, station: &StationKey, e: &mw_component::AdmitError) -> UploadReply {
        match e.class() {
            // The refusal is already counted by reason inside `admit`, and the registry's
            // own log line names the endpoint.
            Class::Ignorable => {
                debug!(
                    %station,
                    error = %DisplayChain(e),
                    "an Ambient reading was refused"
                );
                UploadReply::refused(UploadStatus::Unauthorised, UNAUTHORISED_BODY)
            }
            Class::Fatal => {
                error!(
                    %station,
                    error = %DisplayChain(e),
                    "the spool could not be written, so this node cannot durably accept \
                     readings"
                );
                UploadReply::refused(UploadStatus::Unavailable, UNAVAILABLE_BODY)
            }
            _ => {
                warn!(
                    %station,
                    error = %DisplayChain(e),
                    "an Ambient reading could not be admitted"
                );
                UploadReply::refused(UploadStatus::Unavailable, UNAVAILABLE_BODY)
            }
        }
    }
}

/// The slot the running component installs its [`Handler`] into.
///
/// A root resource the binary owns, exactly as `mw_api_legacy::RouteState`,
/// `SnapshotState` and `RollupState` are — and for the same reason: `mw-http` builds the
/// `axum` route and this crate may not name `axum`, so what travels is the closure
/// [`UploadState::endpoint`] returns rather than a router. "The route is mounted" and
/// "the component is `Running`" are then the same fact, which is what **D-48**'s invariant
/// is stated over.
#[derive(Debug, Default)]
pub struct UploadState {
    handler: ArcSwapOption<Handler>,
}

impl UploadState {
    /// An empty slot, which is what the binary builds before `plan()` runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current handler, or `None` while the component is not running.
    pub fn handler(&self) -> Option<Arc<Handler>> {
        self.handler.load_full()
    }

    /// The endpoint the one listener calls, over this slot.
    ///
    /// An upload that arrives while the component is not running is
    /// [`UploadStatus::Unavailable`] rather than a `404`: the route exists, the station is
    /// configured, and the condition is temporary — so the console's own retry is the right
    /// answer, which a `404` would stop.
    pub fn endpoint(self: &Arc<Self>) -> UploadEndpoint {
        let state = Arc::clone(self);
        Arc::new(move |upload: Upload| {
            let state = Arc::clone(&state);
            Box::pin(async move {
                let Some(handler) = state.handler() else {
                    return UploadReply::refused(UploadStatus::Unavailable, UNAVAILABLE_BODY);
                };
                handler.upload(&upload.query, Utc::now()).await
            })
        })
    }

    /// Install a handler. The component's own `serve` is the only caller.
    fn install(&self, handler: Arc<Handler>) {
        self.handler.store(Some(handler));
    }

    /// [`UploadState::install`], for a test that drives the endpoint without a supervisor.
    ///
    /// Named so that it reads as test-only at every call site, which is
    /// `mw_api_legacy::RouteState::install_for_test`'s reason.
    #[doc(hidden)]
    pub fn install_for_test(&self, handler: Arc<Handler>) {
        self.install(handler);
    }

    /// Clear the slot, which unmounts the endpoint.
    fn clear(&self) {
        self.handler.store(None);
    }
}

/// The `Serve` body: install, publish, wait, revoke, clear.
///
/// There is no loop, and no socket. The endpoint *is* a route on the one listener
/// (§1.4), so what this component does for its whole life is hold the handler installed —
/// which is what makes "the route answers" and "the component is `Running`" one fact.
pub struct Endpoint {
    state: Arc<UploadState>,
    handler: Arc<Handler>,
    routes: Publisher<Arc<dyn RouteProvider>>,
    mount: String,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("mount", &self.mount)
            .field("handler", &self.handler)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// The component body for one endpoint.
    pub fn new(
        state: Arc<UploadState>,
        handler: Arc<Handler>,
        routes: Publisher<Arc<dyn RouteProvider>>,
        mount: String,
    ) -> Self {
        Self {
            state,
            handler,
            routes,
            mount,
        }
    }
}

/// The marker this component publishes `HttpRoutes` as.
///
/// `RouteProvider` has no methods: the pure half of a provision is the `RouteManifest` and
/// the impure half is a closure the binary already holds, so what the publication carries
/// is the **graph edge** (`mw_component::RouteProvider`).
#[derive(Debug)]
struct Mounted;

impl RouteProvider for Mounted {}

impl Serve for Endpoint {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            m::arm(cx.id());
            // Installed before the capability is published, so a consumer that reacts to
            // the publication cannot observe a mounted-but-empty endpoint.
            self.state.install(Arc::clone(&self.handler));
            self.routes
                .publish(Arc::new(Mounted) as Arc<dyn RouteProvider>);
            info!(
                mount = %self.mount,
                stations = self.handler.units(),
                "the Ambient ingest endpoint is mounted"
            );
            cx.cancelled().await;
            // Revoke before clearing, which is the ordering §3.6 asks of a provider.
            self.routes.revoke();
            self.state.clear();
            info!(mount = %self.mount, "the Ambient ingest endpoint is unmounted");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
