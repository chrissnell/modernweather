//! `station:grpcrx` — `WeatherV1.SendWeatherReadings`, and the defect W2a §4a.6 names.
//!
//! The one inbound gRPC station driver: N satellites stream readings at one method,
//! identity is `FromRegistration`, and the trigger is `Service` (W2a §4a.5). It exists
//! because a `config.ingest` row with `listen = 'grpc'` does — "the only thing that makes
//! this node a collector: no flag, no mode" (W7 **D-44**).
//!
//! # The defect, which is the whole reason this file reads the way it does
//!
//! §4a.6 quotes the Go receiver:
//!
//! ```text
//!   internal/weatherstations/grpcreceiver/station.go:174-194
//!     internalReading := s.convertToInternalReading(reading)
//!     select { case s.ReadingDistributor <- internalReading:   // <- stored FIRST
//!     ...
//!     if reading.StationId != "" && s.registry != nil {        // <- checked SECOND
//!         if remoteStation == nil {
//!             s.logger.Debugf("Received reading from unregistered station ID: %s", ...)
//!         }
//!     }
//! ```
//!
//! So an unknown sender's reading is **stored** and then **debug-logged**. That ordering is
//! not expressible here, and not because this file is careful: `Ingress::admit` resolves
//! identity before an `Envelope` exists and nothing else produces a
//! [`StationKey`](mw_types::StationKey), so there is no reading to store until the gate has
//! answered. What this file does is hand the gate what the frame claimed; the refusal is
//! `mw_readings_rejected_total{reason}` and a `Status`, not a log line.
//!
//! # What landed with M6-4, and what this service still cannot do
//!
//! **The three admission layers of D-46 are the collector's** — `mw_collector::handshake`,
//! `mw_collector::service` and `mw_collector::admit`, over `collector.v1.Collector`. Layer
//! 1 is on the **socket**, so it covers this method too: a connection that reaches
//! `SendWeatherReadings` has completed the handshake `config.ingest.peer_auth` configures.
//! That is why [`crate::GrpcIngestConfig::ingest_tls_available`] exists and why
//! [`GrpcRxPlanError::InsecureIngest`] no longer fires on every row.
//!
//! Layers 2 and 3 are not reachable from here and cannot be: this service carries **bare
//! readings** rather than sealed envelopes, so there is no provenance to authorise and no
//! `(origin, seq)` to deduplicate against. Those are two of §9.2's three stated limitations,
//! and they are properties of the schema rather than of this file:
//!
//! | §9.2's limitation | Why it holds here |
//! |---|---|
//! | no resume, no cross-reconnect dedup | a `WeatherReading` has no `seq`, so `admit` mints one and a reconnect re-mints |
//! | no relay | a `WeatherReading` has no `path`, so there is nowhere to record a hop — relaying is **inexpressible**, not refused |
//! | field presence is lost | the committed Go proto's bare scalars make absent and `0.0` the same bytes (W3 §6.5) |
//!
//! The second one is worth being exact about, because §9.2 writes it as "`relay = true` on
//! a compat peer is a plan error" and this build does not have that check. It does not need
//! one and could not write one honestly: `plan()` cannot know which protocol a peer will
//! speak — `peers.software` is written by a `Hello` that has not happened yet — and the shim
//! re-originates every reading to **this** node through `admit`, so a forwarded path is not
//! something a compat peer can present at all. [`PEER_AUTH_NOTE`] states all three
//! limitations to the operator, once, at startup, and the removal condition is
//! `mw_peer_software_info` (W7 OQ-W7-6).
//!
//! **Registration.** `RegisterRemoteStation` still answers `Unimplemented`, and that is now
//! a decision rather than a deferral: its message carries the four plaintext secrets §5.2
//! and **D-67** delete, so decoding it would put them back into this tree. The supported
//! path is `collector.v1.Collector`'s `Hello`, which registers a peer's stations carrying
//! identity only — or an operator's own `config.stations` row, which the `Unimplemented`
//! message names.
//!
//! # Why `admit` and not `accept`
//!
//! **D-60**: `accept` preserves a peer's provenance and `admit` seals a reading this node
//! measured. A `WeatherReading` is a bare reading — no origin, no sequence number, no path
//! — so there is no provenance to preserve, and `admit` is the only one of the two that can
//! take it. The consequence is the shim's third limitation stated in another coat: a
//! reading forwarded through this method is re-originated to **this** node, so it is not
//! deduplicable against the same reading arriving later through the collector.
//! `collector.v1.Collector` is where a satellite's own `(origin, seq)` survives the hop.

use std::fmt;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use axum::Router;
use chrono::Utc;
use mw_component::{
    BoxFuture, ComponentError, ComponentId, ComponentIdError, ComponentSpec, ConfigDigest, Cx,
    Ingress, Methods, Need, Provision, Publisher, RouteEntry, RouteManifest, RouteProvider, Scope,
    Serve, Trigger,
};
use mw_station::identity::StationIdentity;
use mw_station::metrics::{self as m, FrameOutcome};
use mw_types::{Class, Classify, DisplayChain, PartialReading};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use crate::codec::ReadingCodec;
use crate::config::{GrpcIngestConfig, IngestRow};

/// The component's id. A constant because the spec, the log lines and a plan test name it.
pub const ID: &str = "station:grpcrx";

/// The method a satellite streams readings at.
///
/// **`remoteweather.v1`, not `modernweather.v1`**, and that is the compatibility: a Go-era
/// satellite's generated client sends this path, and a path is a wire contract. The
/// *message* on it is `modernweather.v1.Reading`, whose field numbers were bootstrapped from
/// the Go `WeatherReading` and are identical — which is what makes one schema serve both
/// (the registry's protobuf schema, and **D-21** on why a field number is never reused).
pub const SEND_WEATHER_READINGS: &str = "/remoteweather.v1.WeatherV1/SendWeatherReadings";

/// The Go-era registration method, which answers `Unimplemented` by decision -- see this
/// module's documentation and `route_table`'s message for why, and for the path that works.
pub const REGISTER_REMOTE_STATION: &str = "/remoteweather.v1.WeatherV1/RegisterRemoteStation";

/// What this build says, once, about what authenticates a reading on **this** method.
///
/// **Not "`peer_auth` is unenforced" any more**, which is what this note used to say.
/// Implementation-plan.md M6-4 landed D-46's three layers on the same socket, so a
/// connection that reaches this method has completed the handshake
/// `config.ingest.peer_auth` configures. What it has *not* done is prove which station it
/// may write for, and that is the shim's stated limitation rather than a gap: a
/// `WeatherReading` carries no origin, no sequence number and no path, so there is no
/// provenance for per-envelope authorisation to be checked against.
pub const PEER_AUTH_NOTE: &str =
    "`WeatherV1.SendWeatherReadings` is the GO COMPAT SHIM (docs/collector.md §9.2). The \
     connection is authenticated by `config.ingest.peer_auth` at the handshake, as every \
     connection to this socket is -- but a `WeatherReading` carries no provenance, so this \
     method cannot check WHICH station an authenticated peer may write for: any enrolled \
     peer can write readings for any station whose registered key it knows. It is also \
     at-most-once and cannot relay. A satellite that speaks `collector.v1.Collector` gets \
     all three back; the shim is removed when `mw_peer_software_info` shows no peer at \
     version \"unknown\" (W7 OQ-W7-6).";

/// What this build says, once, about the opt-out that let it start.
pub const INSECURE_NOTE: &str =
    "`config.ingest.allow_insecure = 1` is set and this node has no TLS certificate for \
     the gRPC socket, so it serves its reading receiver over plaintext with NO peer \
     authentication at all: anyone who can reach the gRPC port can write readings for any \
     station whose registered key they know. Decision 35 requires this to be said on every \
     start. Configure `listener.https_port` and a website certificate and clear \
     `allow_insecure`, or reach this port only over a private network or a tunnel.";

/// The component id, which is a constant and is still validated.
pub fn component_id() -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(ID)
}

/// Every path this service claims, as **D-51**'s pure declaration.
///
/// `Methods::POST`, because that is what a gRPC call is, and `Scope::Any`, because a
/// satellite sends whatever `Host` its channel was configured with.
pub fn manifest() -> RouteManifest {
    RouteManifest::new(
        Scope::Any,
        vec![
            RouteEntry::exact(SEND_WEATHER_READINGS, Methods::POST),
            RouteEntry::exact(REGISTER_REMOTE_STATION, Methods::POST),
        ],
    )
}

/// Everything one stream is answered from.
pub struct Handlers {
    ingress: Arc<dyn Ingress>,
    receiver: ComponentId,
}

impl fmt::Debug for Handlers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handlers")
            .field("receiver", &self.receiver)
            .finish_non_exhaustive()
    }
}

impl Handlers {
    /// The handlers for one receiver.
    pub fn new(receiver: ComponentId, ingress: Arc<dyn Ingress>) -> Self {
        Self { ingress, receiver }
    }

    /// Admit one reading a satellite sent.
    ///
    /// The seam the level-2 test drives: a wire `Reading` in, an admission out, with no
    /// socket and no stream. Returns the `Status` a satellite would see.
    pub async fn on_reading(&self, wire: &mw_proto::wire::Reading) -> Result<(), Status> {
        let id = &self.receiver;
        // **The claim, before anything is stored.** A satellite presents the key it was
        // issued at registration (`from_registration`); `station_id` is where a current
        // sender puts it and `stationName` is where a Go-era one does, so both are read and
        // the operator sets `config.stations.presented_key` to whichever their satellite
        // sends. Neither is a station until `resolve` says so.
        let claim = Some(wire.station_id.as_str())
            .filter(|key| !key.is_empty())
            .or(Some(wire.station_name.as_str()).filter(|name| !name.is_empty()));
        let Some(claim) = claim else {
            m::frame(id, FrameOutcome::UnknownStation);
            debug!("a streamed reading carried neither `station_id` nor `stationName`");
            return Err(Status::invalid_argument(
                "a reading has to carry `station_id` or `stationName`, which is the key \
                 this station was registered under",
            ));
        };

        let reading = match mw_types::Reading::try_from(wire) {
            Ok(reading) => reading,
            Err(e) => {
                m::frame(id, FrameOutcome::Undecodable);
                debug!(
                    claim,
                    error = %DisplayChain(&e),
                    "a streamed reading did not convert"
                );
                return Err(Status::invalid_argument(format!(
                    "the reading did not convert: {e}"
                )));
            }
        };
        // The satellite's own observation instant, which the wire message carries. A
        // reading with no timestamp is one admission substitutes the arrival time for,
        // which is what `PartialReading::observed_at` being an `Option` is for -- and the
        // conversion has already decided whether there was one.
        let observed_at = Some(reading.time).filter(|t| t.timestamp() != 0);
        let batch = [match observed_at {
            Some(at) => PartialReading::at(at, reading),
            None => PartialReading::new(reading),
        }];

        let meta = StationIdentity::FromRegistration.frame_meta(
            id,
            Some(&mw_types::StationKey::new(claim)),
            Utc::now(),
        );
        match self.ingress.admit(&batch, &meta).await {
            Ok(admitted) => {
                m::frame(id, FrameOutcome::Decoded);
                debug!(
                    claim,
                    accepted = admitted.accepted,
                    "admitted a streamed reading"
                );
                Ok(())
            }
            Err(e) => {
                m::frame(id, FrameOutcome::Refused);
                match e.class() {
                    // An unknown sender is a **counted rejection**, not a debug log, and
                    // the satellite is told. `PermissionDenied` rather than `NotFound`, for
                    // `mw_http`'s reason about one status for an unknown station and a bad
                    // secret: the status says neither. This method verifies no secret at
                    // all -- a `from_registration` station presents a key and nothing else
                    // -- so there is no second case here to be told apart from the first,
                    // and the timing note on `mw_component::UploadStatus::Unauthorised`
                    // does not apply to it.
                    Class::Ignorable => {
                        debug!(
                            claim,
                            error = %DisplayChain(&e),
                            "a streamed reading was refused"
                        );
                        Err(Status::permission_denied(
                            "this node collects no readings for that station key",
                        ))
                    }
                    Class::Fatal => {
                        error!(
                            claim,
                            error = %DisplayChain(&e),
                            "the spool could not be written, so this node cannot durably \
                             accept readings"
                        );
                        Err(Status::unavailable(
                            "this node cannot durably accept readings; please retry",
                        ))
                    }
                    _ => {
                        warn!(
                            claim,
                            error = %DisplayChain(&e),
                            "a streamed reading could not be admitted"
                        );
                        Err(Status::unavailable(
                            "this node could not accept the reading; please retry",
                        ))
                    }
                }
            }
        }
    }

    /// Read one satellite's stream to its end.
    ///
    /// **A refused reading ends the stream**, which is deliberate and is the opposite of
    /// `logger.Errorf` and carrying on: a satellite streaming readings for a station this
    /// node does not collect is a *configuration* error on the satellite, and a stream that
    /// absorbed them would leave it retrying for ever with nothing telling it why. The
    /// satellite reconnects and gets the same answer, which is a signal it can act on.
    async fn receive(
        &self,
        mut stream: tonic::Streaming<mw_proto::wire::Reading>,
    ) -> Result<Response<()>, Status> {
        let mut received = 0u64;
        loop {
            match stream.message().await {
                Ok(Some(reading)) => {
                    self.on_reading(&reading).await?;
                    received += 1;
                }
                // The satellite closed the stream, which is the `io.EOF` the Go receiver
                // answers with `SendAndClose(&pb.Empty{})`.
                Ok(None) => {
                    debug!(received, "a satellite's reading stream ended");
                    return Ok(Response::new(()));
                }
                Err(status) => {
                    m::frame(&self.receiver, FrameOutcome::Undecodable);
                    debug!(received, %status, "a satellite's reading stream failed");
                    return Err(status);
                }
            }
        }
    }
}

/// The slot the running component installs its [`Handlers`] into.
///
/// `mw_api_legacy::RouteState`'s pattern, for its reason: the route is built before `plan()`
/// runs because the listener needs it, and the component is what makes it answer — so "the
/// service is mounted" and "the component is `Running`" are the same fact, which is what
/// **D-48**'s invariant is stated over.
#[derive(Debug, Default)]
pub struct ServiceState {
    handlers: ArcSwapOption<Handlers>,
}

impl ServiceState {
    /// An empty slot, which is what the binary builds before `plan()` runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current handlers, or `None` while the component is not running.
    pub fn handlers(&self) -> Option<Arc<Handlers>> {
        self.handlers.load_full()
    }

    fn install(&self, handlers: Arc<Handlers>) {
        self.handlers.store(Some(handlers));
    }

    /// [`ServiceState::install`], for a test that drives the service without a supervisor.
    #[doc(hidden)]
    pub fn install_for_test(&self, handlers: Arc<Handlers>) {
        self.install(handlers);
    }

    fn clear(&self) {
        self.handlers.store(None);
    }
}

/// The route table the one listener mounts.
///
/// Built by the binary, over a slot the component fills — the same arrangement
/// `mw_api_legacy::route_table` uses, and for the same reason.
pub fn route_table(state: Arc<ServiceState>) -> Result<mw_http::RouteTable, ComponentIdError> {
    let owner = component_id()?;
    let send = {
        let state = Arc::clone(&state);
        axum::routing::post(move |request: axum::extract::Request| {
            let state = Arc::clone(&state);
            async move { send_weather_readings(state, request).await }
        })
    };
    let register = axum::routing::post(|| async move {
        // Named rather than absent: a satellite that tries to enrol gets a sentence and
        // somewhere to go, where a 404 would look like the wrong address.
        unimplemented_status(
            "`RegisterRemoteStation` is not implemented and will not be: its message \
             carries four plaintext secrets that docs/collector.md §5.2 and D-67 delete, \
             so decoding it would carry that exposure forward. Register through \
             `collector.v1.Collector/Hello`, which carries identity only.",
        )
    });
    Ok(mw_http::RouteTable::new(
        owner,
        manifest(),
        Router::new()
            .route(SEND_WEATHER_READINGS, send)
            .route(REGISTER_REMOTE_STATION, register),
    ))
}

/// One `SendWeatherReadings` call.
async fn send_weather_readings(
    state: Arc<ServiceState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let Some(handlers) = state.handlers() else {
        // The route exists and the component is not running, which is a temporary condition
        // the satellite's own retry is the right answer to.
        return unimplemented_or_unavailable();
    };
    // `tonic::server::Grpc` does the framing, the compression negotiation and the trailers;
    // what this crate supplies is the codec and the service body (`crate::codec` has why
    // the codec is not a generated one).
    let mut grpc = tonic::server::Grpc::new(ReadingCodec);
    let service = Receive { handlers };
    // `tonic` answers with its own body type and `axum` wants its own; both are
    // `http_body::Body` implementations, so the conversion is a wrap and not a copy.
    grpc.client_streaming(service, request)
        .await
        .map(axum::body::Body::new)
}

/// The `ClientStreamingService` `tonic` drives.
struct Receive {
    handlers: Arc<Handlers>,
}

impl tonic::server::ClientStreamingService<mw_proto::wire::Reading> for Receive {
    type Response = ();
    type Future = BoxFuture<'static, Result<Response<()>, Status>>;

    fn call(
        &mut self,
        request: Request<tonic::Streaming<mw_proto::wire::Reading>>,
    ) -> Self::Future {
        let handlers = Arc::clone(&self.handlers);
        Box::pin(async move { handlers.receive(request.into_inner()).await })
    }
}

/// A `Status` as a gRPC response, for the two methods that answer one without a stream.
fn unimplemented_status(message: &'static str) -> axum::response::Response {
    Status::unimplemented(message).into_http::<axum::body::Body>()
}

/// What the route answers while the component is not running.
fn unimplemented_or_unavailable() -> axum::response::Response {
    Status::unavailable("this node's reading receiver is not running; please retry")
        .into_http::<axum::body::Body>()
}

/// The `Serve` body: install, publish, wait, revoke, clear.
///
/// No loop and no socket — the service is a route on the one listener (§1.4), so what this
/// component does for its whole life is hold the handlers installed.
pub struct Receiver {
    state: Arc<ServiceState>,
    handlers: Arc<Handlers>,
    routes: Publisher<Arc<dyn RouteProvider>>,
    peer_auth: String,
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("peer_auth", &self.peer_auth)
            .field("handlers", &self.handlers)
            .finish_non_exhaustive()
    }
}

/// The marker this component publishes `HttpRoutes` as.
#[derive(Debug)]
struct Mounted;

impl RouteProvider for Mounted {}

impl Serve for Receiver {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            m::arm(cx.id());
            // Installed before the capability is published, so a consumer that reacts to the
            // publication cannot observe a mounted-but-empty service.
            self.state.install(Arc::clone(&self.handlers));
            self.routes
                .publish(Arc::new(Mounted) as Arc<dyn RouteProvider>);
            // Once, at startup, where an operator reading the boot log can act on it.
            // Both lines, because they are two different admissions: the first is a column
            // this build reads and does not honour, the second is the opt-out the operator
            // had to set to get here at all (decision 35: "logged at `WARN` on every
            // start").
            warn!(peer_auth = %self.peer_auth, "{PEER_AUTH_NOTE}");
            warn!("{INSECURE_NOTE}");
            info!(
                method = SEND_WEATHER_READINGS,
                "the reading receiver is mounted"
            );
            cx.cancelled().await;
            // Revoke before clearing, which is the ordering §3.6 asks of a provider.
            self.routes.revoke();
            self.state.clear();
            info!("the reading receiver is unmounted");
            Ok(())
        })
    }
}

/// Why a `config.ingest` row does not produce a receiver.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrpcRxPlanError {
    /// The component id is not one, which is a build error rather than a configuration one.
    #[error("`{ID}` is not a usable component id")]
    Id(#[from] ComponentIdError),

    /// `main` did not build the state slot.
    ///
    /// A wiring mistake rather than a configuration error: the listener and this component
    /// are assembled by the same function, so one existing without the other is a bug in
    /// the boot sequence.
    #[error(
        "the reading receiver's state slot was not built, so `{ID}` has nowhere to install \
         its handlers. `crate::boot` builds it beside the listener."
    )]
    NoServiceState,

    /// Plaintext ingest with no explicit opt-out, which **decision 35** makes a plan error.
    ///
    /// **The condition changed with implementation-plan.md M6-4 and the error did not.** It
    /// used to fire on every `config.ingest` row, because the gRPC socket had no TLS branch
    /// at all; it now fires when that socket *cannot* complete a handshake, which is when
    /// no `listener.https_port` and certificate are configured for the SNI resolver to
    /// answer from (`mw_collector::handshake`). A node with TLS no longer needs the opt-out,
    /// and that is what makes the two components mounted on this socket agree: refusing
    /// here while `ingest:grpc` planned happily would be a daemon that will not start for a
    /// configuration the design calls correct.
    ///
    /// Refusing at all rather than deferring: the alternative is a daemon that serves an
    /// unauthenticated write method while the operator's own opt-out sits unread, which is
    /// worse than one that will not start.
    #[error(
        "`config.ingest.listen = 'grpc'` serves `{SEND_WEATHER_READINGS}` over plaintext, \
         because this node has no TLS certificate for the gRPC socket to present -- so \
         `peer_auth = '{peer_auth}'` authenticates nobody. Configure \
         `listener.https_port` and a website certificate, or set \
         `config.ingest.allow_insecure = 1` to accept that (decision 35 requires it to be \
         explicit), or remove the row"
    )]
    InsecureIngest {
        /// What the row asked for, so the message names the setting being ignored.
        peer_auth: String,
    },
}

/// Plan `station:grpcrx`, or say why not.
///
/// **One condition**: a `config.ingest` row with `listen = 'grpc'`. A node with no such row
/// is a satellite and has no receiver, which is W7 **D-44** — "no flag, no mode" — and is
/// not an error.
///
/// A row that *is* present must also carry `allow_insecure = 1`, because the socket this
/// build serves it on is plaintext and `peer_auth` is not enforced: see
/// [`GrpcRxPlanError::InsecureIngest`].
pub fn register<C>(
    cfg: &C,
    state: Option<&Arc<ServiceState>>,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), GrpcRxPlanError>
where
    C: GrpcIngestConfig + ?Sized,
{
    // A listener this build has no driver for is said out loud rather than ignored. The
    // schema permits `rest` and `POST /v1/ingest` is M6-4's, so a node whose only `ingest`
    // row is that one boots, reports healthy and has **no ingest surface at all** -- which
    // is the state `mw_station::plan` refuses by name for a station type with no driver,
    // and which nothing else here would mention. A warning rather than a plan error
    // because the row is a legitimate thing to have configured ahead of M6-4.
    // **No warning for a `rest` row any more**, and the removal is the point: that row is
    // `mw_collector`'s, which serves `POST /v1/ingest` for it. The message that used to be
    // here said "only `listen = 'grpc'` is served until implementation-plan.md M6-4 lands
    // `POST /v1/ingest`" -- which became false on the documented collector shape of §8.3,
    // where both rows are present and both are served. What a `rest`-only node is told is
    // `mw_collector::service::register`'s to say, because that is the component that serves
    // it.
    let Some(row) = cfg
        .ingest()
        .iter()
        .find(|row| row.listen == IngestRow::GRPC)
    else {
        return Ok(());
    };
    // **Before anything is built**, because the point of a plan error is that the daemon
    // does not start: decision 35's "plaintext ingest is a plan-time error unless
    // `allow_insecure_ingest = true`". The TLS half of the condition is M6-4's and is the
    // same question `mw_collector::service::register` asks, so the two components mounted
    // on this socket cannot disagree about whether the configuration is acceptable.
    if !cfg.ingest_tls_available() && !row.allow_insecure {
        return Err(GrpcRxPlanError::InsecureIngest {
            peer_auth: row.peer_auth.clone(),
        });
    }
    // **No reading stream means no receiver, and not a refused configuration.** This is
    // the same shape `writer:timescale` is in and it gets the same answer, for the reason
    // `crate::boot`'s `Database` states: refusing the whole configuration for a node that
    // is serving but not ingesting is wrong. The stream exists only for a node with a
    // `node` row, and nothing in this build writes one -- enrolment is M6-4 -- so the
    // alternative was that any `config.ingest` row at all made the daemon refuse to start.
    // Said out loud, because a collector row with no receiver is not a state an operator
    // should have to infer.
    let Some(ingress) = cfg.ingress() else {
        warn!(
            "a `config.ingest` row makes this node a collector and it has no reading \
             stream, so no reading receiver is planned: there would be nothing to admit a \
             satellite's readings to. The stream arrives with the `config.node` row that \
             implementation-plan.md M6-4's enrolment writes."
        );
        return Ok(());
    };
    let ingress = Arc::clone(ingress);
    let state = Arc::clone(state.ok_or(GrpcRxPlanError::NoServiceState)?);
    let id = component_id()?;
    let receiver = id.clone();
    let peer_auth = row.peer_auth.clone();
    // Every value the builder closes over, so that changing `peer_auth` restarts the
    // component -- which is what makes the startup note above describe the row that is
    // actually in force.
    let digest = ConfigDigest::of(row);

    out.push(
        ComponentSpec::new(
            id,
            Trigger::Service,
            mw_component::Builder::serve(move |cx| {
                let state = Arc::clone(&state);
                let ingress = Arc::clone(&ingress);
                let receiver = receiver.clone();
                let peer_auth = peer_auth.clone();
                async move {
                    let routes = cx.publisher::<Arc<dyn RouteProvider>>()?;
                    Ok(Receiver {
                        state,
                        handlers: Arc::new(Handlers::new(receiver, ingress)),
                        routes,
                        peer_auth,
                    })
                }
            }),
        )
        .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
        .provides([Provision::routes(manifest())])
        .identity(mw_component::IdentityShape {
            // `GrpcIn` is not a `Transport` variant -- the schema's `transport_kind` list
            // deliberately omits it, because the gRPC receiver is an `ingest` row and never
            // a station's local transport (the column's own comment). So the shape is
            // declared here rather than derived from a transport: inbound, resolved from a
            // registration, N stations on one endpoint, and a stream.
            initiation: mw_component::Initiation::Inbound,
            source: mw_component::IdentitySource::FromRegistration,
            cardinality: mw_component::Cardinality::Many,
            continuity: mw_component::Continuity::Stream,
        })
        .config_digest(digest),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_station::testing::{IngressFailure, RecordingIngress};

    fn handlers(ingress: Arc<dyn Ingress>) -> Handlers {
        Handlers::new(component_id().expect("a legal component id"), ingress)
    }

    fn wire(station_id: &str, station_name: &str, barometer: f32) -> mw_proto::wire::Reading {
        mw_proto::wire::Reading {
            station_id: station_id.to_string(),
            station_name: station_name.to_string(),
            barometer: Some(barometer),
            reading_timestamp: Some(prost_timestamp(1_762_814_949)),
            ..Default::default()
        }
    }

    fn prost_timestamp(seconds: i64) -> mw_proto::wire::Timestamp {
        mw_proto::wire::Timestamp { seconds, nanos: 0 }
    }

    /// **The defect, as behaviour.** The claim reaches the gate and the reading reaches the
    /// spool only if the gate answered -- so a refused sender leaves nothing behind, where
    /// `station.go:174` stores first and consults the registry at `:194`.
    #[tokio::test]
    async fn a_readings_claim_reaches_the_gate_before_anything_is_stored() {
        let ingress = RecordingIngress::accepting();
        handlers(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_reading(&wire("satellite-a:backyard", "backyard", 29.92))
            .await
            .expect("an accepting gate");
        let admissions = ingress.admissions();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0].receiver, ID);
        assert_eq!(
            admissions[0].presented_key.as_deref(),
            Some("satellite-a:backyard"),
            "`station_id` is the key a satellite was registered under"
        );
        assert_eq!(admissions[0].batch[0].values.barometer, Some(29.92));
        // The satellite's own instant, which the wire message carries.
        assert_eq!(
            admissions[0].batch[0].observed_at.map(|at| at.timestamp()),
            Some(1_762_814_949)
        );
    }

    /// A Go-era satellite sets `stationName` and not `station_id`, so both are read.
    #[tokio::test]
    async fn a_go_era_sender_is_resolved_by_the_name_it_sends() {
        let ingress = RecordingIngress::accepting();
        handlers(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_reading(&wire("", "backyard", 29.92))
            .await
            .expect("an accepting gate");
        assert_eq!(
            ingress.admissions()[0].presented_key.as_deref(),
            Some("backyard")
        );
    }

    /// A reading carrying neither is refused by name, and nothing is stored.
    #[tokio::test]
    async fn a_reading_with_no_identity_at_all_is_refused() {
        let ingress = RecordingIngress::accepting();
        let status = handlers(Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_reading(&wire("", "", 29.92))
            .await
            .expect_err("nothing to resolve");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("registered under"), "{status}");
        assert!(ingress.admissions().is_empty());
    }

    /// An unknown sender is a **counted rejection and an answered one**, not a debug log.
    #[tokio::test]
    async fn an_unregistered_sender_is_told_and_stores_nothing() {
        for reason in [
            mw_component::Reject::UnknownStation,
            mw_component::Reject::Disabled,
        ] {
            let ingress = RecordingIngress::refusing(reason);
            let status = handlers(Arc::clone(&ingress) as Arc<dyn Ingress>)
                .on_reading(&wire("somebody-elses-station", "", 29.92))
                .await
                .expect_err("the gate refused it");
            assert_eq!(
                status.code(),
                tonic::Code::PermissionDenied,
                "{reason:?}: one answer for an unknown key and a disabled station"
            );
            assert!(ingress.admissions().is_empty(), "{reason:?}");
        }
    }

    /// Every refusal class, and the two answers they map to.
    ///
    /// The classification is the thing worth asserting -- `PermissionDenied` for a refusal
    /// the satellite caused and `Unavailable` for a condition on this node, which is what
    /// tells a satellite whether retrying can help. `Ignorable` was the only class a
    /// `RecordingIngress` could produce, so the other two arms went untested under a test
    /// named for the spool, which fed it an `Ignorable` reject and asserted
    /// `PermissionDenied`.
    #[tokio::test]
    async fn each_refusal_class_gets_the_answer_that_tells_the_satellite_what_to_do() {
        for (how, want) in [
            (
                IngressFailure::Rejected(mw_component::Reject::BatchTooLarge),
                tonic::Code::PermissionDenied,
            ),
            // The spool at its ceiling: the condition clears when a cursor catches up, so
            // retrying is exactly the right thing for the satellite to do.
            (IngressFailure::SpoolFull, tonic::Code::Unavailable),
            // And a spool that cannot be written is fatal for this process, which is still
            // `Unavailable` to the sender: it is this node's fault and not the frame's.
            (IngressFailure::Spool, tonic::Code::Unavailable),
        ] {
            let ingress = RecordingIngress::failing(how);
            let status = handlers(ingress as Arc<dyn Ingress>)
                .on_reading(&wire("satellite-a:backyard", "", 29.92))
                .await
                .unwrap_err();
            assert_eq!(status.code(), want, "{how:?}");
        }
    }

    #[test]
    fn the_declared_paths_are_the_wire_contract() {
        assert_eq!(
            SEND_WEATHER_READINGS,
            "/remoteweather.v1.WeatherV1/SendWeatherReadings"
        );
        assert_eq!(
            REGISTER_REMOTE_STATION,
            "/remoteweather.v1.WeatherV1/RegisterRemoteStation"
        );
        let manifest = manifest();
        assert_eq!(manifest.scope, Scope::Any);
        assert_eq!(
            manifest
                .entries
                .iter()
                .map(|e| e.pattern.as_str())
                .collect::<Vec<_>>(),
            vec![SEND_WEATHER_READINGS, REGISTER_REMOTE_STATION]
        );
        assert_eq!(
            component_id().expect("a legal id").as_str(),
            "station:grpcrx"
        );
    }

    /// A node with no `config.ingest` row is a satellite, which is not an error.
    #[test]
    fn a_node_with_no_ingest_row_has_no_receiver() {
        struct Cfg {
            rows: Vec<IngestRow>,
            ingress: Option<Arc<dyn Ingress>>,
        }
        impl GrpcIngestConfig for Cfg {
            fn ingest(&self) -> &[IngestRow] {
                &self.rows
            }
            fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
                self.ingress.as_ref()
            }
            /// No TLS, so decision 35's opt-out is what this test is about.
            fn ingest_tls_available(&self) -> bool {
                false
            }
        }

        let state = Arc::new(ServiceState::new());
        let mut out = Vec::new();
        let satellite = Cfg {
            rows: Vec::new(),
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
        };
        register(&satellite, Some(&state), &mut out).expect("a satellite plans");
        assert!(out.is_empty());

        // A REST ingest row is a collector and is not THIS receiver.
        let rest = Cfg {
            rows: vec![IngestRow {
                listen: "rest".into(),
                peer_auth: "mtls-ca".into(),
                ca_path: None,
                allow_insecure: false,
            }],
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
        };
        register(&rest, Some(&state), &mut out).expect("a REST collector plans");
        assert!(out.is_empty(), "`POST /v1/ingest` is M6-4's");

        // **Decision 35.** The same row without the opt-out does not plan: the socket is
        // plaintext in this build and `peer_auth` is not enforced, so the alternative is a
        // daemon serving an unauthenticated write method while the operator's own switch
        // sits unread. The message names the column to set.
        let plaintext = Cfg {
            rows: vec![IngestRow {
                listen: "grpc".into(),
                peer_auth: "mtls-ca".into(),
                ca_path: None,
                allow_insecure: false,
            }],
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
        };
        let e = register(&plaintext, Some(&state), &mut out)
            .expect_err("plaintext ingest with no opt-out");
        assert_eq!(
            e,
            GrpcRxPlanError::InsecureIngest {
                peer_auth: "mtls-ca".into()
            }
        );
        assert!(
            e.to_string().contains("allow_insecure"),
            "the refusal names the column that lifts it: {e}"
        );
        assert!(
            out.is_empty(),
            "and nothing was pushed before the refusal: a plan error is only a plan error \
             if the caller can discard the whole plan"
        );

        // And the gRPC row produces exactly one component, with the shape §4a.5 declares.
        let collector = Cfg {
            rows: vec![IngestRow {
                listen: "grpc".into(),
                peer_auth: "psk".into(),
                ca_path: None,
                allow_insecure: true,
            }],
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
        };
        register(&collector, Some(&state), &mut out).expect("a collector plans");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id.as_str(), ID);
        assert!(matches!(out[0].trigger, Trigger::Service));
        let shape = out[0].identity.as_ref().expect("a declared shape");
        assert_eq!(shape.initiation, mw_component::Initiation::Inbound);
        assert_eq!(shape.source, mw_component::IdentitySource::FromRegistration);
        assert_eq!(shape.cardinality, mw_component::Cardinality::Many);
        assert_eq!(shape.problem(&out[0].trigger), None);
        // The pure half of the provision, which is what makes a duplicate path a plan error
        // naming both owners (D-51).
        assert!(out[0]
            .provides
            .iter()
            .any(|p| p.capability == mw_component::Capability::HttpRoutes));

        // A collector with nowhere to put what it receives plans **nothing**, and does not
        // refuse the configuration. The same answer `writer:timescale` gets for the same
        // shape: the reading stream arrives with the `config.node` row that M6-4's
        // enrolment writes, nothing in this build writes one, and refusing the whole
        // configuration would make any `config.ingest` row an unstartable daemon.
        let nowhere = Cfg {
            rows: collector.rows.clone(),
            ingress: None,
        };
        let mut nothing = Vec::new();
        register(&nowhere, Some(&state), &mut nothing)
            .expect("a collector with no stream is not a refused configuration");
        assert!(nothing.is_empty(), "and no receiver is planned either");
        // ... and so is one whose state slot the boot sequence did not build.
        assert_eq!(
            register(&collector, None, &mut out),
            Err(GrpcRxPlanError::NoServiceState)
        );
    }

    /// The route answers while the component is not running, and says what is temporary.
    #[test]
    fn the_state_slot_is_empty_until_the_component_installs_into_it() {
        let state = ServiceState::new();
        assert!(state.handlers().is_none());
        state.install_for_test(Arc::new(handlers(
            RecordingIngress::accepting() as Arc<dyn Ingress>
        )));
        assert!(state.handlers().is_some());
        state.clear();
        assert!(state.handlers().is_none());
    }
}
