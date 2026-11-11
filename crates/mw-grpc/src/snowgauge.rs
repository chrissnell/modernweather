//! The snow gauge driver's body. Its **names** are `mw_station::snowgauge`'s and its codec
//! is `mw_codec::SnowGaugeProto`'s; what is here is the one thing neither of those may
//! contain, which is `tonic`.
//!
//! W2a §4a.5 maps the gauge as `GrpcOut { streaming: true }`, `Fixed`, `Service`: we dial
//! it, it streams `snowgauge.Reading` messages, and the wire is the credential. So the
//! component is an outbound stream and the whole of its loop is
//!
//! ```text
//!   build   Channel::connect              -> BuildError::Resource, so Backoff (Decision 3)
//!           StreamReading(StreamRequest)  -> the server stream
//!   serve   loop { message -> codec.decode -> ingress.admit }
//!           return on cancellation, or on a stream failure -> Backoff
//! ```
//!
//! # Where this disagrees with the Go driver
//!
//! **1. There is no reconnect loop here**, for `mw_station::driver`'s reason: a connection
//! is a *build*, so a gauge that is unplugged is `Backoff { until }` with the reason in
//! `ComponentStatus::last_error` and the retry rate is the supervisor's one
//! exponential-backoff-with-jitter implementation (**D-27**). `ConnectToGauge` is a loop
//! with its own doubling delay capped at thirty seconds, called from two places, and
//! `logger.Fatalf` if the hostname or port is missing — which is a configuration mistake
//! that takes the whole daemon down rather than one station.
//!
//! **2. The discard is the codec's, and it is counted.** `snowgauge/station.go:171` drops a
//! distance greater than the configured base distance at the device and logs it;
//! `mw_codec::SnowGaugeProto` refuses it as `CodecError::Implausible`, which the driver
//! counts as `mw_station_frames_total{outcome="implausible"}`. **DEV-9** makes that discard
//! this item's inherited constraint: M3-3 moved `snowdepth` to ingest, so without it such a
//! reading stores a **negative** depth into a column with `min`/`mean`/`max` aggregates
//! over it.
//!
//! **3. A gauge with no configured base distance reports its distance** rather than nothing.
//! `config.DeviceData.BaseSnowDistance` is a non-nullable `float64` in Go, so an
//! unconfigured gauge has a base of zero and *every* reading is discarded —
//! `mw_codec::SnowGaugeProto::new` has the whole of that.
//!
//! # The keepalive is the Go driver's, and it is the one thing kept verbatim
//!
//! Two minutes with a twenty-second timeout and `permit_without_stream`
//! (`snowgauge/station.go:92`). A snow gauge reports when the distance *changes*, which in
//! settled weather is not for hours, so without a keepalive a NAT or a firewall drops the
//! connection and neither end notices until the next reading never arrives.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mw_codec::{Codec as _, CodecError, Frame, SnowGaugeProto, STREAM_READING};
use mw_component::{BoxFuture, BuildError, ComponentError, ComponentId, Cx, Ingress, Serve};
use mw_station::endpoint::Endpoint;
use mw_station::identity::StationIdentity;
use mw_station::metrics::{self as m, FrameOutcome};
use mw_types::{Class, Classify, DisplayChain, StationKey};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::codec::BytesCodec;

/// How long to wait for the TCP connection and the HTTP/2 handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The keepalive interval, which is the Go driver's.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(120);

/// How long a keepalive ping may go unanswered, which is the Go driver's.
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why the gauge could not be dialled.
#[derive(Debug, thiserror::Error)]
pub enum DialError {
    /// The configured address is not one `tonic` will dial.
    ///
    /// Reachable only through a `transport_url` that `mw_station::Endpoint` accepted and
    /// `http::Uri` did not, which is a disagreement between two parsers rather than an
    /// operator's mistake. The cause is carried as **text** rather than as a `#[source]`,
    /// because what `Channel::from_shared` returns is an `http::uri::InvalidUri` and the
    /// variant would otherwise be the one place this enum names a type from two crates away
    /// for a branch no configuration reaches.
    #[error("`{authority}` is not an address this build can dial: {why}")]
    Address {
        /// The authority that was tried.
        authority: String,
        /// What the URI parser said.
        why: String,
    },

    /// The gauge did not accept a connection.
    #[error("the snow gauge at {authority} could not be reached")]
    Connect {
        /// The authority that was tried.
        authority: String,
        /// Why.
        #[source]
        source: tonic::transport::Error,
    },

    /// The connection attempt exceeded [`CONNECT_TIMEOUT`].
    #[error("the snow gauge at {authority} did not answer within {seconds}s")]
    Timeout {
        /// The authority that was tried.
        authority: String,
        /// The window.
        seconds: u64,
    },

    /// The stream could not be opened over a connection that was accepted.
    ///
    /// Separate from [`DialError::Connect`] because the two mean different things: this is a
    /// gauge whose software is not answering `StreamReading`, where the other is a gauge
    /// that is not there at all.
    #[error("the snow gauge at {authority} refused `{STREAM_READING}`")]
    Stream {
        /// The authority that was tried.
        authority: String,
        /// Why.
        #[source]
        source: tonic::Status,
    },
}

impl DialError {
    /// This failure as a [`BuildError`], which is how it reaches the supervisor.
    pub fn build(self, station: &StationKey) -> BuildError {
        BuildError::Resource {
            what: format!("the snow gauge stream for station `{station}`"),
            source: Box::new(self),
        }
    }
}

impl Classify for DialError {
    /// Retryable, every one. A gauge on a battery that has gone flat is a gauge somebody is
    /// about to charge, and `Backoff` caps the retry rate either way (**D-27**).
    fn class(&self) -> Class {
        Class::RETRYABLE
    }
}

/// One connected gauge, mid-stream.
pub struct SnowGauge {
    stream: tonic::codec::Streaming<Vec<u8>>,
    codec: SnowGaugeProto,
    station: StationKey,
    ingress: Arc<dyn Ingress>,
    authority: String,
}

impl fmt::Debug for SnowGauge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnowGauge")
            .field("station", &self.station)
            .field("authority", &self.authority)
            .field("base_distance_mm", &self.codec.base_distance_mm())
            .finish_non_exhaustive()
    }
}

impl SnowGauge {
    /// Dial the gauge and open its stream.
    ///
    /// This is the station's **build**: a gauge that is not answering is
    /// `BuildError::Resource` and therefore `Backoff` with a readable reason, rather than a
    /// startup abort (W2a Decision 3) -- which is what `logger.Fatalf("SnowGauge station
    /// [%s] must define a hostname and port")` is in the Go driver.
    pub async fn connect(
        endpoint: &Endpoint,
        codec: SnowGaugeProto,
        station: StationKey,
        ingress: Arc<dyn Ingress>,
    ) -> Result<Self, DialError> {
        let authority = endpoint.authority().to_string();
        // Through the endpoint's own rendering rather than by formatting the parts again:
        // `mw_station::Endpoint` is what `transport_url` was validated by, and a second
        // spelling here is a second answer to "which port".
        let uri = endpoint.to_string();
        let channel = Channel::from_shared(uri)
            .map_err(|e| DialError::Address {
                authority: authority.clone(),
                why: e.to_string(),
            })?
            .connect_timeout(CONNECT_TIMEOUT)
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT);

        let connected = match tokio::time::timeout(CONNECT_TIMEOUT, channel.connect()).await {
            Ok(Ok(channel)) => channel,
            Ok(Err(source)) => return Err(DialError::Connect { authority, source }),
            Err(_) => {
                return Err(DialError::Timeout {
                    authority,
                    seconds: CONNECT_TIMEOUT.as_secs(),
                })
            }
        };

        let path = STREAM_READING
            .parse::<http::uri::PathAndQuery>()
            .expect("`mw_codec::STREAM_READING` is a literal path");
        // An empty `StreamRequest`: the message has one optional field and this build sets
        // none of it, so it encodes to no bytes (`crate::codec`).
        let request = tonic::Request::new(Vec::new());
        let stream = tonic::client::Grpc::new(connected)
            .server_streaming(request, path, BytesCodec)
            .await
            .map_err(|source| DialError::Stream {
                authority: authority.clone(),
                source,
            })?
            .into_inner();

        Ok(Self {
            stream,
            codec,
            station,
            ingress,
            authority,
        })
    }

    /// Decode one message and admit what it carried.
    async fn on_message(&self, id: &ComponentId, bytes: Vec<u8>) -> Result<(), ComponentError> {
        admit_message(id, &self.codec, &self.station, &self.ingress, bytes).await
    }

    /// Read the stream until it ends.
    pub async fn run(mut self: Box<Self>, cx: Cx) -> Result<(), ComponentError> {
        let id = cx.id().clone();
        m::arm(&id);
        info!(
            station = %self.station,
            authority = %self.authority,
            base_distance_mm = ?self.codec.base_distance_mm(),
            "a snow gauge stream is open"
        );
        loop {
            let next = tokio::select! {
                // Biased for `mw_station::driver`'s reason: a reading admitted after the
                // spool's cursors have been told to stop is a reading nobody drains.
                biased;
                () = cx.cancelled() => {
                    debug!(station = %self.station, "a snow gauge stream was cancelled");
                    return Ok(());
                }
                next = self.stream.message() => next,
            };
            match next {
                Ok(Some(bytes)) => self.on_message(&id, bytes).await?,
                // The gauge closed the stream cleanly. **Still a failure**, because a
                // station that has stopped reporting is not a station that has finished:
                // `mw_supervisor`'s own rule is that "a service that returns on its own has
                // stopped serving, however calmly it was spelled".
                Ok(None) => {
                    warn!(
                        station = %self.station,
                        "a snow gauge closed its stream; the supervisor will reconnect"
                    );
                    return Err(ComponentError::tick(
                        "reading a snow gauge stream",
                        StreamEnded,
                    ));
                }
                Err(source) => {
                    warn!(
                        station = %self.station,
                        status = %source,
                        "a snow gauge stream failed; the supervisor will reconnect"
                    );
                    return Err(ComponentError::tick(
                        "reading a snow gauge stream",
                        StreamFailed { source },
                    ));
                }
            }
        }
    }
}

/// Decode one message and admit what it carried.
///
/// A free function rather than a method, so that the level-2 test can drive the seam with
/// recorded wire bytes and no gauge: `tonic::codec::Streaming` has no public constructor,
/// so a `SnowGauge` cannot be built without one. The stream is the only impure part of this
/// driver and everything below it is a function over a byte string, which is what this
/// signature says. `mw_station::driver::admit` is the same split for the same reason.
pub async fn admit_message(
    id: &ComponentId,
    codec: &SnowGaugeProto,
    station: &StationKey,
    ingress: &Arc<dyn Ingress>,
    bytes: Vec<u8>,
) -> Result<(), ComponentError> {
    let frame = Frame::new(bytes);
    let batch = match codec.decode(&frame) {
        Ok(batch) => batch,
        Err(e) => {
            let outcome = match &e {
                // DEV-9's discard: the gauge is working and this reading is a fault its
                // own configuration says it cannot be. A separate label, because a
                // gauge whose base distance is set two metres short otherwise reports
                // nothing and counts nothing under a name an operator would look at.
                CodecError::Implausible { .. } => FrameOutcome::Implausible,
                _ => FrameOutcome::Undecodable,
            };
            m::frame(id, outcome);
            // At DEBUG for the discard and WARN for the rest: a gauge reporting past
            // its base distance in a storm is an event, and at WARN it would be a line
            // per reading.
            match outcome {
                FrameOutcome::Implausible => debug!(
                    station = %station,
                    error = %DisplayChain(&e),
                    "a snow gauge reading was discarded as a fault"
                ),
                _ => warn!(
                    station = %station,
                    error = %DisplayChain(&e),
                    "a snow gauge message did not decode"
                ),
            }
            return Ok(());
        }
    };
    // `Fixed`: configuration says which station, so the frame presents nothing and
    // `StationRegistry::resolve` answers from the receiving component.
    let meta = StationIdentity::Fixed.frame_meta(id, None, Utc::now());
    match ingress.admit(&batch, &meta).await {
        Ok(admitted) => {
            m::frame(id, FrameOutcome::Decoded);
            debug!(
                station = %station,
                accepted = admitted.accepted,
                "admitted a snow gauge reading"
            );
            Ok(())
        }
        Err(e) => {
            m::frame(id, FrameOutcome::Refused);
            match e.class() {
                Class::Ignorable => {
                    debug!(
                        station = %station,
                        error = %DisplayChain(&e),
                        "a snow gauge reading was refused"
                    );
                    Ok(())
                }
                Class::Fatal => {
                    error!(
                        station = %station,
                        error = %DisplayChain(&e),
                        "the spool could not be written, so this node cannot durably \
                         accept readings"
                    );
                    Err(ComponentError::tick("admitting a snow gauge reading", e))
                }
                _ => Err(ComponentError::tick("admitting a snow gauge reading", e)),
            }
        }
    }
}

impl Serve for SnowGauge {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(self.run(cx))
    }
}

/// The gauge closed its stream.
#[derive(Debug, thiserror::Error)]
#[error("the snow gauge closed its stream")]
pub struct StreamEnded;

impl Classify for StreamEnded {
    fn class(&self) -> Class {
        Class::RETRYABLE
    }
}

/// The gauge's stream failed.
#[derive(Debug, thiserror::Error)]
#[error("the snow gauge's stream failed")]
pub struct StreamFailed {
    /// What `tonic` reported.
    #[source]
    pub source: tonic::Status,
}

impl Classify for StreamFailed {
    fn class(&self) -> Class {
        Class::RETRYABLE
    }
}

/// Why a snow gauge row does not produce a component.
///
/// Short, because almost every way a gauge row can be wrong is refused by
/// `mw_station::plan` — which validates the row's shape and mints its component id, so that
/// a malformed gauge is one error from one function with the same sentences as every other
/// station's (`mw_station::snowgauge`). What is left here is the part only this crate knows.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnowGaugePlanError {
    /// The station key cannot name a component.
    ///
    /// Refused by `mw_station::plan` too, and still checked here: the two functions mint the
    /// **same** id from the same function, and a correctness that depends on one of them
    /// having run first is one a later edit breaks silently.
    #[error("station `{station}`: its key cannot name a component: {source}")]
    Key {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: mw_component::ComponentIdError,
    },

    /// A `transport_url` that is not an address.
    #[error("station `{station}`: `transport_url` is not an address: {source}")]
    Address {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: mw_station::EndpointError,
    },

    /// This node has a gauge to collect from and nowhere to put its readings.
    #[error(
        "station `{station}` would collect readings and this process has no reading \
         stream, so there is nothing to admit them to"
    )]
    NoIngress {
        /// Which station.
        station: String,
    },
}

/// Derive one component per configured snow gauge.
///
/// **The row's shape is `mw_station::plan`'s to check and this function does not re-check
/// it.** That is the split `mw_station::snowgauge` records: a `snowgauge` row is validated
/// there — `grpc_out`, streaming, `Fixed` identity, a usable key — and produces no spec,
/// because `ci/layers.toml` forbids `mw-station` from naming `tonic`. So a configuration
/// that reaches here has already been refused or accepted once, by one function, with one
/// set of sentences; what is left is building the body.
pub fn register<C>(
    cfg: &C,
    out: &mut Vec<mw_component::ComponentSpec>,
) -> Result<(), SnowGaugePlanError>
where
    C: crate::config::SnowGaugeConfig + ?Sized,
{
    let rows: Vec<&mw_station::StationRow> = cfg
        .stations()
        .iter()
        .filter(|row| row.is_collected_here() && row.kind == mw_station::snowgauge::STATION_TYPE)
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let ingress = cfg.ingress().ok_or_else(|| SnowGaugePlanError::NoIngress {
        station: rows[0].key.clone(),
    })?;

    for row in rows {
        let id = mw_station::snowgauge::station_id(&row.key).map_err(|source| {
            SnowGaugePlanError::Key {
                station: row.key.clone(),
                source,
            }
        })?;
        // `is_collected_here` is what guarantees the transport is present, and
        // `mw_station::plan` is what guarantees it is a streaming `grpc_out`.
        let columns = row.transport.as_ref().expect("a collected station");
        let endpoint =
            Endpoint::parse(columns.url.as_deref().unwrap_or_default()).map_err(|source| {
                SnowGaugePlanError::Address {
                    station: row.key.clone(),
                    source,
                }
            })?;
        let station = StationKey::new(row.key.as_str());
        let codec = SnowGaugeProto::new(row.snow_base_distance_mm());
        let ingress = Arc::clone(ingress);
        // One parse, then the shape: `Transport::shape` is what `plan()` checks the trigger
        // against, and deriving it from the same `Endpoint` the body dials means the
        // declaration and the connection cannot disagree about the address.
        let shape = mw_station::Transport::GrpcOut {
            endpoint: endpoint.clone(),
            streaming: true,
        }
        .shape(&StationIdentity::Fixed);
        out.push(
            mw_component::ComponentSpec::new(
                id,
                mw_component::Trigger::Service,
                mw_component::Builder::serve(move |_cx| {
                    let endpoint = endpoint.clone();
                    let station = station.clone();
                    let ingress = Arc::clone(&ingress);
                    async move {
                        // The dial is the BUILD, so a gauge that is not answering is
                        // `Backoff { until }` with a readable reason (W2a Decision 3).
                        let gauge = SnowGauge::connect(&endpoint, codec, station.clone(), ingress)
                            .await
                            .map_err(|e| e.build(&station))?;
                        Ok(gauge)
                    }
                }),
            )
            .requires([mw_component::Need::hard_write(
                mw_component::Capability::ReadingStream,
            )])
            .identity(shape)
            .config_digest(mw_component::ConfigDigest::of(row)),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_station::testing::RecordingIngress;
    use mw_station::{StationRow, TransportColumns};

    fn id() -> ComponentId {
        mw_station::snowgauge::station_id("suncrest:gauge").expect("a legal component id")
    }

    /// One `snowgauge.Reading` on the wire: field 1 `stationName`, field 2 `distance`.
    ///
    /// Hand-built rather than encoded through a generated type, because there is no
    /// generated type -- `mw_codec::SnowGaugeProto` is the schema, and the whole point of
    /// `crate::codec::BytesCodec` is that these bytes reach it untouched.
    fn message(name: &str, distance: i32) -> Vec<u8> {
        let mut bytes = vec![0x0a, u8::try_from(name.len()).expect("a short name")];
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0x10);
        let mut value = i64::from(distance) as u64;
        loop {
            let byte = u8::try_from(value & 0x7f).expect("seven bits");
            value >>= 7;
            if value == 0 {
                bytes.push(byte);
                break;
            }
            bytes.push(byte | 0x80);
        }
        bytes
    }

    /// The level-2 seam: a recorded message in, an admission out, with no stream.
    ///
    /// [`admit_message`] is a free function precisely so that this is possible --
    /// `tonic::codec::Streaming` has no public constructor, so a `SnowGauge` cannot be
    /// built without a real gRPC call, and what the loop around this adds is `tonic`'s
    /// framing and cancellation. Those are the emulator level's to exercise.
    struct Gauge {
        codec: SnowGaugeProto,
        station: StationKey,
        ingress: Arc<dyn Ingress>,
    }

    impl Gauge {
        async fn on_message(&self, bytes: Vec<u8>) -> Result<(), ComponentError> {
            admit_message(&id(), &self.codec, &self.station, &self.ingress, bytes).await
        }
    }

    fn gauge(base: Option<f32>, ingress: Arc<dyn Ingress>) -> Gauge {
        Gauge {
            codec: SnowGaugeProto::new(base),
            station: StationKey::new("suncrest:gauge"),
            ingress,
        }
    }

    #[tokio::test]
    async fn a_reading_is_admitted_with_the_distance_the_gauge_sent() {
        let ingress = RecordingIngress::accepting();
        gauge(Some(1500.0), Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_message(message("roof-gauge", 1200))
            .await
            .expect("admitted");
        let admissions = ingress.admissions();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0].receiver, "station:snowgauge:suncrest:gauge");
        assert_eq!(admissions[0].presented_key, None, "a Fixed identity");
        assert_eq!(admissions[0].batch[0].values.snow_distance, Some(1200.0));
        assert_eq!(
            admissions[0].batch[0].values.snow_depth, None,
            "the depth is the enrich stage's subtraction (DEV-9)"
        );
    }

    /// **DEV-9's discard**, which is this item's inherited constraint: the reading is
    /// dropped rather than stored, and the conversation continues.
    #[tokio::test]
    async fn a_distance_past_the_base_is_discarded_and_the_stream_continues() {
        let ingress = RecordingIngress::accepting();
        let gauge = gauge(Some(1500.0), Arc::clone(&ingress) as Arc<dyn Ingress>);
        gauge
            .on_message(message("roof-gauge", 1600))
            .await
            .expect("a discard is not a component failure");
        assert!(ingress.admissions().is_empty());
        // ... and the next good reading is admitted, which is what makes it a counted drop
        // rather than an outage.
        gauge
            .on_message(message("roof-gauge", 1200))
            .await
            .expect("admitted");
        assert_eq!(ingress.readings(), 1);
    }

    #[tokio::test]
    async fn bytes_that_are_not_a_reading_are_a_counted_drop() {
        let ingress = RecordingIngress::accepting();
        gauge(Some(1500.0), Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_message(vec![0x0a, 0x40, 0x01])
            .await
            .expect("a bad message is not a component failure");
        assert!(ingress.admissions().is_empty());
    }

    /// An unknown station is a counted refusal and not a failed component (§4.4).
    #[tokio::test]
    async fn an_unknown_station_does_not_end_the_stream() {
        let ingress = RecordingIngress::refusing(mw_component::Reject::UnknownStation);
        gauge(Some(1500.0), Arc::clone(&ingress) as Arc<dyn Ingress>)
            .on_message(message("roof-gauge", 1200))
            .await
            .expect("an Ignorable refusal is not a component failure");
    }

    fn row(key: &str, url: &str, base: Option<f64>) -> StationRow {
        StationRow {
            key: key.into(),
            name: key.into(),
            kind: mw_station::snowgauge::STATION_TYPE.into(),
            enabled: true,
            identity_kind: "fixed".into(),
            snow_base_distance_mm: base,
            transport: Some(TransportColumns {
                kind: "grpc_out".into(),
                url: Some(url.into()),
                streaming: Some(true),
                ..TransportColumns::default()
            }),
            ..StationRow::default()
        }
    }

    struct Cfg {
        rows: Vec<StationRow>,
        ingress: Option<Arc<dyn Ingress>>,
    }

    impl crate::config::SnowGaugeConfig for Cfg {
        fn stations(&self) -> &[StationRow] {
            &self.rows
        }
        fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
            self.ingress.as_ref()
        }
    }

    fn cfg(rows: Vec<StationRow>) -> Cfg {
        Cfg {
            rows,
            ingress: Some(RecordingIngress::accepting() as Arc<dyn Ingress>),
        }
    }

    #[test]
    fn one_component_per_gauge_with_the_shape_the_design_declares() {
        let mut out = Vec::new();
        register(
            &cfg(vec![
                row("suncrest:gauge", "http://gauge.lan:50051", Some(1500.0)),
                row("suncrest:barn", "http://barn-gauge.lan:50051", None),
            ]),
            &mut out,
        )
        .expect("a valid plan");
        let mut ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "station:snowgauge:suncrest:barn",
                "station:snowgauge:suncrest:gauge",
            ]
        );
        for spec in &out {
            assert!(matches!(spec.trigger, mw_component::Trigger::Service));
            let shape = spec.identity.as_ref().expect("a declared shape");
            assert_eq!(shape.initiation, mw_component::Initiation::Outbound);
            assert_eq!(shape.source, mw_component::IdentitySource::Fixed);
            assert_eq!(shape.cardinality, mw_component::Cardinality::One);
            assert_eq!(shape.continuity, mw_component::Continuity::Stream);
            assert_eq!(shape.problem(&spec.trigger), None);
        }
    }

    /// Only this crate's rows. A node with other stations and no gauge plans nothing here,
    /// which is what keeps the two registrars from each needing the other's filters.
    #[test]
    fn a_configuration_with_no_gauge_plans_nothing() {
        let mut davis = row("suncrest:backyard", "http://console.lan:50051", None);
        davis.kind = "davis".into();
        let mut out = Vec::new();
        register(&cfg(vec![davis]), &mut out).expect("a valid plan");
        assert!(out.is_empty());

        // A disabled gauge, and a gauge received from a peer: neither is collected here.
        let mut off = row("suncrest:gauge", "http://gauge.lan:50051", Some(1500.0));
        off.enabled = false;
        register(&cfg(vec![off]), &mut out).expect("a valid plan");
        assert!(out.is_empty());
    }

    #[test]
    fn a_gauge_on_a_node_with_no_reading_stream_is_refused() {
        let mut out = Vec::new();
        let e = register(
            &Cfg {
                rows: vec![row(
                    "suncrest:gauge",
                    "http://gauge.lan:50051",
                    Some(1500.0),
                )],
                ingress: None,
            },
            &mut out,
        )
        .expect_err("nowhere to admit to");
        assert!(matches!(e, SnowGaugePlanError::NoIngress { .. }), "{e}");
    }

    #[test]
    fn the_keepalive_is_the_go_drivers() {
        assert_eq!(KEEPALIVE_INTERVAL, Duration::from_secs(120));
        assert_eq!(KEEPALIVE_TIMEOUT, Duration::from_secs(20));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_gauge_that_is_not_answering_is_a_build_failure_naming_the_station() {
        // Port 1 on the loopback: privileged, unbound, refused immediately.
        let endpoint = Endpoint::parse("http://127.0.0.1:1").expect("a literal address");
        let e = SnowGauge::connect(
            &endpoint,
            SnowGaugeProto::new(Some(1500.0)),
            StationKey::new("suncrest:gauge"),
            RecordingIngress::accepting() as Arc<dyn Ingress>,
        )
        .await
        .expect_err("nothing is listening");
        assert_eq!(e.class(), Class::RETRYABLE);
        let rendered = DisplayChain(&e.build(&StationKey::new("suncrest:gauge"))).to_string();
        assert!(rendered.contains("suncrest:gauge"), "{rendered}");
        assert!(rendered.contains("127.0.0.1:1"), "{rendered}");
    }
}
