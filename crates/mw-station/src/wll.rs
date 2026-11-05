//! WeatherLink Live: two transports feeding one codec family, plus an outbound re-arm.
//!
//! W2a **D-44**, as three component bodies:
//!
//! ```text
//!  broadcast mode                                  poll mode
//!  ──────────────                                  ─────────
//!  station:wll:udp:22222   Service, UdpIn           station:wll:<key>   Every{>=30s}
//!    one socket, N units, identity from the           one GET of
//!    `did` in each datagram                           /v1/current_conditions
//!
//!  station:wll:<key>:rearm  Every{0.8 x duration}
//!    one GET of /v1/real_time, which is what makes the unit broadcast at all.
//!    Emits no readings.
//! ```
//!
//! # Three decisions this module makes that the design left open
//!
//! **1. The listener is one component per port, not per station.** W2a §4.1b names it
//! `station:wll:<id>` (broadcast) while §4a.5 gives the same component `Cardinality::Many`,
//! and those cannot both be right: `Many` means "the component is per **endpoint**, so a
//! second station is a configuration row rather than a second HTTP server" (§4a.4), and two
//! WeatherLink Live units on one LAN both broadcast to port 22222. Per-station would be two
//! components contending for one socket, which is the Ambient defect in a different coat —
//! the Go driver needs `SO_REUSEADDR` for exactly this reason and then has every daemon
//! filter by source IP. So the id is `station:wll:udp:<port>` and the cardinality rule is
//! the one that wins.
//!
//! **2. The listener holds a mapping per claimed serial, and that is not a second
//! registry.** Decoding needs the station's sensor mapping (`mw_codec::SensorMapping`) and
//! the station is only known from the payload, so the listener has to look the serial up
//! *before* a reading exists. The two lookups answer different questions and the second is
//! the one that authenticates: this one decides **how to decode**, and
//! `StationRegistry::resolve` inside `Ingress::admit` decides **whose reading it is**. A
//! serial in this map and not in the registry is refused by the registry and counted there;
//! a serial in the registry and not in this map is dropped here and counted as
//! `unknown_station`, which is §4a.6's "an unknown DSN is dropped and counted, never
//! registered".
//!
//! **3. The broadcast port is configuration, and the re-arm asserts it rather than
//! following it.** §4a.5 requires the re-arm to share no state with the listener, so it
//! cannot hand it a port. The Go driver takes the port from the API response and restarts
//! its receiver when it changes, which is a component reconfiguring another component at
//! run time. Here the listener binds `transport_port` and the re-arm *checks* that the unit
//! agrees, **logging** a disagreement and naming the column to change. A unit that reports
//! a different port is then a configuration error an operator can see, rather than a socket
//! that moved without anybody asking.
//!
//! It is logged and **not counted**, and that is a gap rather than a decision. §5.10's rule
//! is that an operator alerts on a number, and a silently-wrong broadcast port is exactly
//! the "station that reports nothing" this driver exists to make visible — so a counter is
//! the better surface. What stopped it being added here is that the one series this crate
//! owns is `mw_station_frames_total{component, outcome}` and the re-arm handles no frames,
//! so it would be a second series for an event that fires at most once per occurrence
//! period. The right home is a per-occurrence outcome on `ComponentControl`'s surface,
//! which is W5's to render; recorded here so the next reader does not take the absence for
//! a judgement.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_codec::{Codec, Frame, WllConditions};
use mw_component::{
    AdmitError, BoxFuture, BuildError, ComponentError, ComponentId, Cx, Ingress, Serve, Tick,
};
use mw_types::{Classify, DisplayChain, StationKey};
use secrecy::SecretString;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

use crate::endpoint::Endpoint;
use crate::http::{self, HttpError};
use crate::identity::StationIdentity;
use crate::metrics::{self as m, FrameOutcome};
use crate::transport::HttpMethod;

/// The `type` column value this driver serves.
pub const STATION_TYPE: &str = "weatherlink-live";

/// `GET /v1/current_conditions` — both transports' payload, over HTTP.
pub const CURRENT_CONDITIONS: &str = "/v1/current_conditions";

/// `GET /v1/real_time` — the call that makes the unit broadcast.
pub const REAL_TIME: &str = "/v1/real_time";

/// How long each re-arm asks the unit to broadcast for.
///
/// The Go driver's value, kept so that the two daemons ask for the same thing during the
/// side-by-side comparison (M5-2). The unit caps it at its own maximum and reports what it
/// actually granted, which is what the re-arm checks.
pub const REARM_DURATION_SECONDS: i64 = 3600;

/// The re-arm period as a fraction of the duration asked for: **0.8**, per W2a §4a.5.
///
/// Not the Go driver's 0.9. At 3600 s the difference is 360 s of margin, and the margin is
/// what covers a missed occurrence: `Trigger::Every` is a schedule the supervisor owns, so
/// one tick that exceeds its deadline is skipped and counted, and at 0.9 a single skip
/// leaves the broadcast lapsed for six minutes with nothing listening.
const REARM_FRACTION_NUMERATOR: i64 = 4;
const REARM_FRACTION_DENOMINATOR: i64 = 5;

/// The default HTTP poll period, and the floor W2a §4a.5 declares.
pub const DEFAULT_POLL: Duration = Duration::from_secs(60);
/// `Every { >= 30s }`: declared by this crate, enforced once in `plan()` (W2a Decision 56),
/// rather than clamped per occurrence as `runPollingMode` does.
pub const POLL_FLOOR: Duration = Duration::from_secs(30);

/// How long one request may take. Comfortably inside either component's deadline, and the
/// same ten seconds the Go `http.Client` uses.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The re-arm period for a requested duration.
pub fn rearm_period(duration_seconds: i64) -> Duration {
    let seconds = duration_seconds.max(1) * REARM_FRACTION_NUMERATOR / REARM_FRACTION_DENOMINATOR;
    Duration::from_secs(u64::try_from(seconds.max(1)).unwrap_or(1))
}

/// The component id of the broadcast listener for one port.
pub fn listener_id(port: u16) -> Result<ComponentId, mw_component::ComponentIdError> {
    ComponentId::new(format!("station:wll:udp:{port}"))
}

/// The component id of one station's poll.
pub fn poll_id(station_key: &str) -> Result<ComponentId, mw_component::ComponentIdError> {
    ComponentId::new(format!("station:wll:{station_key}"))
}

/// The component id of one station's broadcast re-arm.
pub fn rearm_id(station_key: &str) -> Result<ComponentId, mw_component::ComponentIdError> {
    ComponentId::new(format!("station:wll:{station_key}:rearm"))
}

/// One unit the broadcast listener will decode for.
#[derive(Debug, Clone)]
pub struct BroadcastUnit {
    /// The serial the unit puts in every datagram, as `config.stations.presented_key`
    /// holds it.
    pub serial: String,
    /// The station it is, for the log line. **Not** the identity: `Ingress::admit` resolves
    /// that from the claim, and this is only what to call the unit while saying something
    /// about it.
    pub station: StationKey,
    /// How to decode its records.
    pub codec: WllConditions,
}

/// The `UdpIn` service: one socket, N units, identity from each datagram's serial.
pub struct BroadcastListener {
    port: u16,
    group: Option<IpAddr>,
    /// Keyed by the lower-cased serial. A WeatherLink Live reports `001D0A700002` and an
    /// operator types `001d0a700002`; DNS went case-insensitive for the same reason
    /// (**DEV-2**) and a station that silently never resolves is worse here, because
    /// nothing fails — the datagrams simply stop counting as anything.
    units: BTreeMap<String, BroadcastUnit>,
    ingress: Arc<dyn Ingress>,
}

impl fmt::Debug for BroadcastListener {
    /// `Ingress` is a capability handle and not `Debug` -- a handle that printed itself
    /// would print a spool -- so the fields that identify this component are printed and
    /// the handle is elided.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BroadcastListener")
            .field("port", &self.port)
            .field("group", &self.group)
            .field("units", &self.units.len())
            .finish_non_exhaustive()
    }
}

impl BroadcastListener {
    /// The listener for one port and the units that broadcast to it.
    pub fn new(
        port: u16,
        group: Option<IpAddr>,
        units: impl IntoIterator<Item = BroadcastUnit>,
        ingress: Arc<dyn Ingress>,
    ) -> Self {
        Self {
            port,
            group,
            units: units
                .into_iter()
                .map(|u| (u.serial.to_ascii_lowercase(), u))
                .collect(),
            ingress,
        }
    }

    /// How many units this listener decodes for.
    pub fn units(&self) -> usize {
        self.units.len()
    }

    /// Open the receive socket.
    ///
    /// Separate from the loop so that a test can hand the loop a socket on an ephemeral
    /// port: port 0 is not a port a device broadcasts to, so there is no configuration that
    /// reaches [`BroadcastListener::serve_on`] with one, and the seam cannot be mistaken
    /// for a feature.
    pub async fn bind(&self) -> Result<UdpSocket, std::io::Error> {
        // `0.0.0.0`, because a broadcast datagram's destination is the subnet's broadcast
        // address and a socket bound to an interface address does not receive it.
        let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.port))).await?;
        if let Some(IpAddr::V4(group)) = self.group {
            socket.join_multicast_v4(group, Ipv4Addr::UNSPECIFIED)?;
        }
        if let Some(IpAddr::V6(group)) = self.group {
            socket.join_multicast_v6(&group, 0)?;
        }
        Ok(socket)
    }

    /// Run the receive loop on a socket somebody else bound.
    pub async fn serve_on(
        self: Box<Self>,
        socket: UdpSocket,
        cx: Cx,
    ) -> Result<(), ComponentError> {
        let id = cx.id().clone();
        m::arm(&id);
        info!(
            port = self.port,
            units = self.units.len(),
            "listening for WeatherLink Live broadcasts"
        );
        // One datagram is one frame, so the buffer is a frame's worth and a larger datagram
        // is a short read the decoder refuses -- rather than the Go receiver's silent
        // truncation at 4096, which decodes as a JSON parse error with no cause.
        let mut buf = vec![0u8; crate::session::MAX_FRAME_BYTES];
        loop {
            let received = tokio::select! {
                // Biased so that a cancelled component does not process one more datagram
                // before noticing: shutdown is ranked, and a reading admitted after the
                // spool's cursors have been told to stop is a reading nobody drains.
                biased;
                () = cx.cancelled() => {
                    debug!("the WeatherLink Live listener was cancelled");
                    return Ok(());
                }
                received = socket.recv_from(&mut buf) => received,
            };
            let (n, from) = match received {
                Ok(pair) => pair,
                // A receive error on a bound UDP socket is almost always ICMP feedback from
                // a previous send, which this socket never does. Counted as the loop
                // continuing rather than as a component failure: a driver that exited here
                // would stop collecting because something else on the host sent a port
                // unreachable.
                Err(e) => {
                    warn!(error = %e, "a WeatherLink Live datagram could not be received");
                    continue;
                }
            };
            self.on_datagram(&id, &buf[..n], from.ip(), Utc::now())
                .await;
        }
    }

    /// Decode one datagram and admit what it carried.
    ///
    /// Takes the arrival instant as an argument rather than reading a clock, for the reason
    /// `FrameMeta::received_at` gives: the honest answer to "when did we see this" is when
    /// the bytes landed, and a timestamp that comes in as data is a timestamp a test can
    /// pin.
    pub async fn on_datagram(
        &self,
        id: &ComponentId,
        bytes: &[u8],
        from: IpAddr,
        at: DateTime<Utc>,
    ) {
        let frame = Frame::new(bytes.to_vec());
        // Which unit this is, by the serial in the payload. Any codec will do for the
        // question -- `claimed_station` reads the document's `did` and not the mapping --
        // so the first configured unit's is used to ask it.
        let Some(any) = self.units.values().next() else {
            // A listener with no units is not plannable: `crate::plan` does not emit one.
            return;
        };
        let Some(claim) = any.codec.claimed_station(&frame) else {
            m::frame(id, FrameOutcome::Undecodable);
            warn!(
                %from,
                bytes = bytes.len(),
                "a datagram on the WeatherLink Live broadcast port carried no device serial"
            );
            return;
        };
        let Some(unit) = self.units.get(&claim.as_str().to_ascii_lowercase()) else {
            m::frame(id, FrameOutcome::UnknownStation);
            // W2a §4a.6: dropped and counted, never registered. At DEBUG because a shared
            // LAN legitimately carries a neighbour's unit, and at WARN it would be a line
            // every two seconds forever.
            debug!(
                %from,
                serial = claim.as_str(),
                "a WeatherLink Live broadcast is from a unit this node has no station for"
            );
            return;
        };
        self.admit(id, unit, &frame, Some(&claim), at).await;
    }

    async fn admit(
        &self,
        id: &ComponentId,
        unit: &BroadcastUnit,
        frame: &Frame,
        claim: Option<&StationKey>,
        at: DateTime<Utc>,
    ) {
        let batch = match unit.codec.decode(frame) {
            Ok(batch) => batch,
            Err(e) => {
                m::frame(id, FrameOutcome::Undecodable);
                warn!(
                    station = %unit.station,
                    error = %DisplayChain(&e),
                    "a WeatherLink Live frame did not decode"
                );
                return;
            }
        };
        let meta = StationIdentity::FromPayload.frame_meta(id, claim, at);
        match self.ingress.admit(&batch, &meta).await {
            Ok(admitted) => {
                m::frame(id, FrameOutcome::Decoded);
                debug!(
                    station = %unit.station,
                    accepted = admitted.accepted,
                    rejected = admitted.rejected,
                    "admitted a WeatherLink Live broadcast"
                );
            }
            Err(e) => {
                m::frame(id, FrameOutcome::Refused);
                report(&e, &unit.station);
            }
        }
    }
}

impl Serve for BroadcastListener {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            let port = self.port;
            let socket = self.bind().await.map_err(|source| {
                // A `BuildError::Resource` rather than a tick failure: a port that is
                // already held is the case `Backoff` exists for, and it reaches the
                // management API verbatim through `ComponentStatus::last_error`.
                ComponentError::Build(BuildError::Resource {
                    what: format!("the WeatherLink Live broadcast port {port}"),
                    source: Box::new(source),
                })
            })?;
            self.serve_on(socket, cx).await
        })
    }
}

/// The `HttpOut` poll: one `GET /v1/current_conditions` per occurrence.
pub struct Poll {
    endpoint: Endpoint,
    method: HttpMethod,
    auth: Option<SecretString>,
    station: StationKey,
    codec: WllConditions,
    ingress: Arc<dyn Ingress>,
}

impl fmt::Debug for Poll {
    /// The endpoint and the station, and neither the credential nor the handle. `auth` is
    /// a `SecretString`, which redacts itself; it is still elided, because a `Debug` that
    /// prints `Secret([REDACTED])` tells a reader nothing and invites the field being
    /// changed to a `String` later.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Poll")
            .field("endpoint", &self.endpoint)
            .field("method", &self.method)
            .field("station", &self.station)
            .finish_non_exhaustive()
    }
}

impl Poll {
    /// The poll for one station.
    pub fn new(
        endpoint: Endpoint,
        method: HttpMethod,
        auth: Option<SecretString>,
        station: StationKey,
        codec: WllConditions,
        ingress: Arc<dyn Ingress>,
    ) -> Self {
        Self {
            endpoint,
            method,
            auth,
            station,
            codec,
            ingress,
        }
    }

    /// Decode one response body and admit what it carried.
    ///
    /// The seam the transcript-level test drives: a recorded sequence of bodies, asserting
    /// that **every** one produces an admission. That assertion is the point — W3 **D-82**
    /// names the transcript level as the one that would have caught the Davis "stops polling
    /// after one batch" defect, and a driver that admitted the first body and then quietly
    /// stopped would pass every golden test the codec has.
    pub async fn on_body(
        &self,
        id: &ComponentId,
        frame: &Frame,
        at: DateTime<Utc>,
    ) -> Result<(), ComponentError> {
        let batch = match self.codec.decode(frame) {
            Ok(batch) => batch,
            Err(e) => {
                m::frame(id, FrameOutcome::Undecodable);
                // Returned rather than swallowed: a poll is an occurrence, and an
                // occurrence that read a body it could not decode is a failed occurrence
                // that `mw_component_ticks_total{outcome="failed"}` should show.
                return Err(ComponentError::tick(
                    "decoding a WeatherLink Live response",
                    e,
                ));
            }
        };
        // `Fixed`: configuration says which station, so the frame presents nothing and
        // `resolve` answers from the receiving component (`crate::identity`).
        let meta = StationIdentity::Fixed.frame_meta(id, None, at);
        match self.ingress.admit(&batch, &meta).await {
            Ok(admitted) => {
                m::frame(id, FrameOutcome::Decoded);
                debug!(
                    station = %self.station,
                    accepted = admitted.accepted,
                    rejected = admitted.rejected,
                    "admitted a WeatherLink Live poll"
                );
                Ok(())
            }
            Err(e) => {
                m::frame(id, FrameOutcome::Refused);
                report(&e, &self.station);
                // An `Ignorable` refusal is not a failed occurrence: a station pointed at
                // the wrong endpoint is a counted signal, not a failure of the component
                // that received it (§4.4). A `Fatal` spool failure is the binary's to
                // exit on and reaches it as a component error.
                match e.class() {
                    mw_types::Class::Ignorable => Ok(()),
                    _ => Err(ComponentError::tick(
                        "admitting a WeatherLink Live reading",
                        e,
                    )),
                }
            }
        }
    }
}

impl Tick for Poll {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            // Zero every outcome, so a station that has never polled successfully is
            // distinguishable from a station whose metrics are broken. Idempotent, and
            // cheaper than a once-cell for an occurrence that makes an HTTP request.
            m::arm(cx.id());
            let frame = http::request(
                &self.endpoint,
                self.method,
                CURRENT_CONDITIONS,
                self.auth.as_ref(),
                REQUEST_TIMEOUT,
            )
            .await
            .map_err(|e| ComponentError::tick("polling a WeatherLink Live", e))?;
            self.on_body(cx.id(), &frame, Utc::now()).await
        })
    }
}

/// The outbound control call that arms the unit's broadcast. Emits no readings.
#[derive(Debug)]
pub struct Rearm {
    endpoint: Endpoint,
    expected_port: u16,
    station: StationKey,
}

impl Rearm {
    /// The re-arm for one unit, which is expected to broadcast to `expected_port`.
    pub fn new(endpoint: Endpoint, expected_port: u16, station: StationKey) -> Self {
        Self {
            endpoint,
            expected_port,
            station,
        }
    }

    /// Ask the unit to broadcast, and check that it agrees about the port.
    pub async fn arm(&self) -> Result<mw_codec::Broadcast, ComponentError> {
        let path = format!("{REAL_TIME}?duration={REARM_DURATION_SECONDS}");
        let frame = http::request(
            &self.endpoint,
            HttpMethod::Get,
            &path,
            None,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e: HttpError| ComponentError::tick("arming a WeatherLink Live broadcast", e))?;
        let broadcast = mw_codec::real_time(&frame)
            .map_err(|e| ComponentError::tick("reading a WeatherLink Live arming response", e))?;

        if broadcast.port != self.expected_port {
            // The disagreement an operator has to act on, and the reason the re-arm does
            // not simply follow the unit: the listener is a different component and is not
            // this one's to reconfigure (see this module's third decision).
            warn!(
                station = %self.station,
                configured = self.expected_port,
                reported = broadcast.port,
                "a WeatherLink Live is broadcasting to a port this node is not listening \
                 on. Set the station's transport_port to the reported port."
            );
        }
        if broadcast.duration_seconds < rearm_period(REARM_DURATION_SECONDS).as_secs() as i64 {
            // The unit granted less than the schedule assumes, so the broadcast lapses
            // before the next occurrence. A warning rather than a reschedule: **D-25**
            // gives every clock to the supervisor, and a component that changed its own
            // period would be the one place that is not true.
            warn!(
                station = %self.station,
                granted_s = broadcast.duration_seconds,
                period_s = rearm_period(REARM_DURATION_SECONDS).as_secs(),
                "a WeatherLink Live granted a shorter broadcast than the re-arm period, so \
                 the broadcast will lapse between occurrences"
            );
        }
        debug!(
            station = %self.station,
            port = broadcast.port,
            duration_s = broadcast.duration_seconds,
            "armed a WeatherLink Live broadcast"
        );
        Ok(broadcast)
    }
}

impl Tick for Rearm {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move { self.arm().await.map(|_| ()) })
    }
}

/// Log an admission failure once, at the level its class deserves.
fn report(e: &AdmitError, station: &StationKey) {
    match e.class() {
        // The refusal is already counted by reason inside `admit`, and the registry's own
        // log line names the endpoint. A second WARN here would double every line.
        mw_types::Class::Ignorable => debug!(
            %station,
            error = %DisplayChain(e),
            "a WeatherLink Live reading was refused"
        ),
        mw_types::Class::Fatal => error!(
            %station,
            error = %DisplayChain(e),
            "the spool could not be written, so this node cannot durably accept readings"
        ),
        _ => warn!(
            %station,
            error = %DisplayChain(e),
            "a WeatherLink Live reading could not be admitted"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rearm_period_is_four_fifths_of_the_duration_asked_for() {
        assert_eq!(rearm_period(3600), Duration::from_secs(2880));
        assert_eq!(rearm_period(300), Duration::from_secs(240));
        // Never zero, whatever arithmetic a configuration produces: a zero period is a
        // timer that fires continuously.
        assert_eq!(rearm_period(1), Duration::from_secs(1));
        assert_eq!(rearm_period(0), Duration::from_secs(1));
        assert_eq!(rearm_period(-5), Duration::from_secs(1));
    }

    #[test]
    fn the_component_ids_are_the_ones_the_design_names() {
        assert_eq!(
            listener_id(22222).expect("a valid id").as_str(),
            "station:wll:udp:22222"
        );
        assert_eq!(
            poll_id("suncrest:roof").expect("a valid id").as_str(),
            "station:wll:suncrest:roof"
        );
        assert_eq!(
            rearm_id("suncrest:roof").expect("a valid id").as_str(),
            "station:wll:suncrest:roof:rearm"
        );
        // A station key that cannot be a metric label cannot be a component id either,
        // which is a plan error naming the key rather than a label nothing can scrape.
        assert!(poll_id("Roof Station").is_err());
    }
}
