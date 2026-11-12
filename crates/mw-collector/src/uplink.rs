//! `uplink:<peer>` — the satellite side: read the spool, forward, wait for the ack.
//!
//! # The state machine (§4.3), and who owns each edge
//!
//! ```text
//!   Disconnected ──connect──▶ Connecting ──mTLS──▶ Hello ──HelloAck──▶ Streaming
//!        ▲                         │                 │                    │
//!        └──────── the supervisor's Backoff ─────────┴────────────────────┘
//! ```
//!
//! **`Disconnected → Connecting` is the supervisor's, not this component's** (W7 **D-25**).
//! There is no `go c.reconnect(ctx)` here: a failed delivery returns `Err`, the supervisor
//! moves the component `Failed → Backoff → Starting` (W2a Decisions 28 and 29), and the
//! backoff is exponential with full jitter, 1 s base, 60 s cap (W3 §3.4). That is the whole
//! of defect 2's unbounded-goroutine half — the current code spawns a fresh `reconnect`
//! goroutine per failed send (`grpcstream/client.go:362-367`) **and** on every
//! connection-state transition (`:426`), guarded only by a `reconnecting bool` read without
//! the mutex that protects it.
//!
//! **`Draining` is the supervisor's too.** W3 §3.6 gives an uplink a 15 s grace; what this
//! component does on cancellation is stop reading new batches and return, and the unacked
//! batches are still in the spool and replay on the next start. Nothing is lost by shutting
//! down mid-flight, which is the property the current implementation cannot claim.
//!
//! # Why this is `OnReading` and not a `SpooledSink`
//!
//! §4.3 describes the uplink as a `SpooledSink`, and `mw_pipeline::Sink::deliver` takes
//! `&[Envelope]` — which is not enough. `IngestBatch.through` is the **sender's spool
//! `seq`** (W7 **D-22**), and for a relay that is a number no envelope in the batch
//! carries, because a relay's log interleaves origins. [`mw_component::ReadingBatch`] is
//! what carries it, so this implements `OnReading` directly and the partial-ack arithmetic
//! `SpooledSink` exists for is unnecessary: one batch is one `IngestBatch` is one atom, so
//! the only two outcomes are "the collector acked the whole batch" and "`Err`".
//!
//! # Resume, end to end, and why a crash and a clean restart are the same path
//!
//! ```text
//! acked_seq from the cursor file  →  Hello  →  skip the leading run known_through covers
//!                                           →  stream from acked_seq + 1
//! ```
//!
//! The satellite remembers **nothing** about the stream. The cursor is the only durable
//! state and the supervisor advances it only on a batch this component returned `Ok` for,
//! so an ack that was lost when the link dropped costs a deduplicated resend and never a
//! gap. `HelloAck.known_through` is advisory on top of that: the sender may skip a leading
//! run the collector demonstrably already has, and ignoring it entirely is correct.
//!
//! # The filter is an optimisation; `forward` is the invariant (§1.4)
//!
//! The component declares `ReadingFilter::NotFrom(peer)` — **`NotFrom(peer)`, not
//! `NotFrom(self)`** (W7 **D-47**): `admit` sets `path = [self]`, so every envelope in this
//! node's spool carries `self` and `NotFrom(self)` would match nothing and send nothing.
//! The subscription applies that filter and advances the cursor over what it dropped
//! (W7 **D-9**), so a mutual uplink presents as `uplink_filtered_total` rising rather than
//! as a disk filling up.
//!
//! What remains is [`Envelope::forward`], which **refuses** rather than truncating. A cycle
//! is not detected here, it is unrepresentable. `uplink_loop_refused_total` moving means
//! the filter missed and the invariant caught it, which per W7 **D-41** is a bug in this
//! code rather than a configuration problem.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mw_component::{
    BoxFuture, BuildError, ComponentError, ComponentId, ComponentIdError, ComponentSpec,
    ConfigDigest, Cx, OnReading, PeerName, ReadingBatch, ReadingFilter, ReadingPolicy, Trigger,
};
use mw_proto::wire::collector::v1 as pb;
use mw_types::{Class, Classify, DisplayChain, Envelope, ForwardRefused, OriginId, Seq};
use secrecy::{ExposeSecret, SecretString};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::{debug, error, info, warn};

use crate::metrics::{self as m, Degradation, UplinkState};
use crate::wire;

/// The component id for an uplink to `peer`.
///
/// W7 **D-2**: `uplink:<peer>`, where the Go-era spelling was
/// `uplink:collector:<peer>` — cosmetic, and this is the one the design settles on.
pub fn id(peer: &str) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("uplink:{peer}"))
}

/// The per-batch deadline (W3 §3.6's 10 s for an uplink).
pub const DEADLINE: Duration = Duration::from_secs(10);

/// How long shutdown waits for outstanding acks (W3 §3.6).
pub const GRACE: Duration = Duration::from_secs(15);

/// How long to wait for the TCP connection and the HTTP/2 handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The keepalive interval, which is the snow gauge driver's and for the same reason: a
/// satellite in settled weather sends a reading every couple of seconds, but a satellite
/// whose stations are all offline sends nothing for hours, and without a keepalive a NAT or
/// a firewall drops the connection and neither end notices.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(120);

/// How long a keepalive ping may go unanswered.
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why an uplink could not be built or could not deliver.
#[derive(Debug, thiserror::Error)]
pub enum UplinkError {
    /// The configured endpoint is not one `tonic` will dial.
    ///
    /// A configuration mistake rather than a transient failure, and carried as **text**
    /// rather than as a `#[source]` for `mw_grpc::snowgauge::DialError::Address`'s reason:
    /// what `Endpoint::from_shared` returns is an `http::uri::InvalidUri`, and the variant
    /// would otherwise be the one place this enum names a type from two crates away.
    #[error("`{endpoint}` is not an address this build can dial: {why}")]
    Address {
        /// What was configured.
        endpoint: String,
        /// What the URI parser said.
        why: String,
    },

    /// A certificate or key named by the `uplink` row could not be read.
    #[error("the uplink's own `{what}` at {path} could not be read")]
    Credential {
        /// Which file.
        what: &'static str,
        /// Its path.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// `rustls` refused the client configuration.
    #[error("the uplink's TLS configuration was refused")]
    Tls {
        /// Why.
        #[source]
        source: tonic::transport::Error,
    },

    /// The collector did not accept a connection.
    #[error("the collector at {endpoint} could not be reached")]
    Connect {
        /// What was tried.
        endpoint: String,
        /// Why.
        #[source]
        source: tonic::transport::Error,
    },

    /// The connection attempt exceeded [`CONNECT_TIMEOUT`].
    #[error("the collector at {endpoint} did not answer within {seconds}s")]
    Timeout {
        /// What was tried.
        endpoint: String,
        /// The window.
        seconds: u64,
    },

    /// `Hello` was refused. Terminal for the attempt and distinguishable by code.
    #[error("the collector refused `Hello`: {status}")]
    Hello {
        /// What it said.
        status: Box<tonic::Status>,
    },

    /// `Ingest` failed mid-stream.
    #[error("the collector's ingest stream failed: {status}")]
    Stream {
        /// What it said.
        status: Box<tonic::Status>,
    },

    /// The collector closed the stream without acknowledging a batch it was sent.
    #[error("the collector ended the stream without acknowledging {envelopes} envelopes")]
    Unacknowledged {
        /// How many are still unacked, and therefore still in the spool.
        envelopes: usize,
    },

    /// The collector's spool is at its ceiling (`ACK_RESOURCE_EXHAUSTED`).
    ///
    /// **Not an error on this side** in the sense that matters: the cursor does not advance
    /// and the batch replays. It is reported as `Retryable` so the supervisor backs the
    /// component off, which is §4.3's "the collector's spool being full is the collector's
    /// `Failed`, not the satellite's".
    #[error("the collector cannot durably accept more envelopes yet")]
    CollectorFull,

    /// The batch carried no local position, which is a wiring mistake rather than a
    /// runtime condition.
    #[error(
        "a batch reached the uplink with no spool position, so there is no `through` to \
         send: an uplink must declare `ReadingPolicy::Durable`"
    )]
    NoPosition,
}

impl Classify for UplinkError {
    /// **What this classification reaches, and what it does not.**
    ///
    /// `Fatal` here does not make the supervisor stop retrying, and saying so matters
    /// because the first version of this comment claimed it did.
    /// `ComponentError::Tick` is `Class::RETRYABLE` unconditionally and its own
    /// documentation explains why — the boxed source's `Classify` is not reachable through
    /// `dyn Error`, and the supervisor's answer to a failed occurrence is the same either
    /// way: record it, count it, and the next occurrence *is* the retry (W2a Decision 29).
    ///
    /// So this impl earns its keep where the concrete type is still known: the log line, a
    /// test, and the two errors below that are produced at **build** time, where
    /// `BuildError` does carry a classification. A malformed endpoint is one of those —
    /// which is why it is parsed in [`Uplink::build`] and not on the connect path.
    fn class(&self) -> Class {
        match self {
            // A configuration mistake no retry fixes. Reachable only from `build`, where
            // the classification is read.
            UplinkError::Address { .. } | UplinkError::NoPosition => Class::Fatal,
            // A credential that cannot be read is a `Backoff`, not a `Fatal`: the usual
            // cause is a certificate being rotated under the process.
            _ => Class::RETRYABLE,
        }
    }
}

impl From<UplinkError> for ComponentError {
    fn from(e: UplinkError) -> Self {
        ComponentError::tick("deliver a batch to a collector", e)
    }
}

/// One `uplink` row, in the shape this crate plans from.
///
/// `Clone`, because a `Builder` runs **per start attempt** (W2a Decision 3) and so must be
/// a `Fn` over values it can copy. That is also why the secret is behind an `Arc`:
/// `secrecy::SecretString` is deliberately not `Clone` — a secret that can be duplicated is
/// a secret with more copies to zeroise — so the sharing is explicit here rather than a
/// second plaintext copy per restart.
#[derive(Debug, Clone)]
pub struct UplinkRow {
    /// `peer_name` — which peer this sends to, and the `peer` metric label.
    pub peer_name: String,
    /// The peer's `NodeId`, from its `peers` row.
    ///
    /// **Resolved once, at plan time, from the peer registry** (§1.4 property 1): the
    /// filter and `forward` both need it per envelope, and the Go version's
    /// `getGRPCReceiverDevices()` — a full `LoadConfig()` plus a device scan, per reading —
    /// is what having it here exists against.
    pub peer_node: OriginId,
    /// `endpoint`, e.g. `https://suncrest.example.net:50051`.
    pub endpoint: String,
    /// `auth_kind` — `mtls` or `psk`.
    pub auth_kind: String,
    /// The client certificate, for `mtls`.
    pub tls_cert_path: Option<String>,
    /// Its key.
    pub tls_key_path: Option<String>,
    /// The CA the collector's own certificate is verified against.
    pub tls_ca_path: Option<String>,
    /// The pre-shared secret, for `psk`.
    pub psk: Option<Arc<SecretString>>,
    /// Whether a collector with no ack support is a hard failure (§9.3).
    ///
    /// `false` during the transition, `true` after. `[[uplink]].require_ack` has no column
    /// yet — the schema's `uplink` table predates §9.3 — so the binary passes `false` and
    /// the field exists so that adding the column is a migration rather than a redesign.
    pub require_ack: bool,
}

/// Everything about this node that a `Hello` announces.
#[derive(Debug, Clone)]
pub struct Identity {
    /// This node's `NodeId`.
    pub node: OriginId,
    /// `node.node_name`.
    pub name: String,
    /// `"modernweather <version>"`.
    pub software: String,
    /// Every station this node has a `config.stations` row for, local **and received**.
    ///
    /// Both, deliberately. A relay's `config.stations` contains the stations it received
    /// from the nodes behind it, as inventory, and a collector two hops up needs those rows
    /// to exist or §4.4's step 5 refuses everything the relay forwards. "A node has
    /// stations" (§5.1) is about the node's inventory and not only about its own sensors.
    pub stations: Vec<pb::StationMeta>,
}

/// The `OnReading` body: one connection, held across batches.
pub struct Uplink {
    peer: PeerName,
    peer_node: OriginId,
    /// The endpoint as configured, for every message that names it.
    endpoint: String,
    /// The same endpoint, **parsed once at build** rather than per connect.
    ///
    /// A URL `tonic` will not dial is a configuration mistake no retry fixes, and `build`
    /// is the one place a classification is read (`Classify for UplinkError`) — so parsing
    /// here turns it into a `BuildError::Resource` with the reason in
    /// `ComponentStatus::last_error`, which is where an unreadable certificate already
    /// lands. On the connect path it would have been a `Tick` failure indistinguishable
    /// from a collector that was merely down.
    dial: Endpoint,
    credentials: Credentials,
    identity: Identity,
    require_ack: bool,
    /// The spool, for `IngestBatch.sender_depth` alone (§7).
    spool: Arc<mw_pipeline::Spool>,
    cursor: ComponentId,
    /// The open connection, or `None` before the first batch.
    ///
    /// Built lazily rather than in `build`, so that a collector that is down does not stop
    /// the component from starting: a satellite whose link is down must still admit and
    /// spool its own readings, which is the entire point of the spool.
    session: Option<Link>,
    /// `known_through` from the most recent `HelloAck`, for the fast-resume skip (§4.2).
    known_through: Vec<(OriginId, Seq)>,
}

impl fmt::Debug for Uplink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Uplink")
            .field("peer", &self.peer)
            .field("endpoint", &self.endpoint)
            .field("connected", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

/// The credentials an uplink presents, with the secret held as one (**D-33**).
enum Credentials {
    /// A client certificate and key, plus the CA the collector is verified against.
    Mtls {
        cert: Vec<u8>,
        key: Vec<u8>,
        ca: Vec<u8>,
    },
    /// A pre-shared secret, over server-authenticated TLS.
    Psk { secret: Arc<SecretString> },
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credentials::Mtls { .. } => f.write_str("Mtls { .. }"),
            Credentials::Psk { .. } => f.write_str("Psk { .. }"),
        }
    }
}

/// One open connection, in one of the two shapes a collector can be.
///
/// An enum rather than a flag, because the two shapes have **nothing in common**: one holds
/// a bidirectional stream and waits for acks, and the other opens a fresh client-streaming
/// call per batch and has no ack to wait for. A `bool` beside one set of fields is how a
/// fallback comes to share a send path it cannot use -- which is exactly the defect this
/// enum replaced: the first version set `acked = false` and then opened `Ingest` anyway, a
/// method the collector that had just answered `UNIMPLEMENTED` for `Hello` does not
/// implement either.
enum Link {
    /// A collector that speaks `collector.v1.Collector`: one `Ingest` stream for the life
    /// of the connection, and `through` acknowledged per batch.
    Acked {
        /// The sender half of the request stream.
        batches: tokio::sync::mpsc::Sender<pb::IngestBatch>,
        /// The ack stream.
        acks: tonic::Streaming<pb::IngestAck>,
    },
    /// A **Go-era** collector (§9.3): `WeatherV1.SendWeatherReadings`, one call per batch.
    ///
    /// At-most-once, and the cursor advances when the call returns rather than when
    /// anything is durable -- because there is nothing on that wire that says it is. The
    /// `Empty` the method answers with is confirmation that the stream was accepted and
    /// nothing more, which is still better than the fire-and-forget the Go satellite does
    /// today and is the most the frozen method can offer.
    Compat {
        /// The channel, re-used across batches. Each batch is its own RPC on it.
        channel: Channel,
    },
}

impl Link {
    /// Whether this link acknowledges durability.
    const fn is_acked(&self) -> bool {
        matches!(self, Link::Acked { .. })
    }
}

impl Uplink {
    /// Read the row's credentials and build the body. Called per start attempt.
    ///
    /// `pub` so that an integration test can drive the component over a real socket without
    /// standing up a supervisor, which is `ServiceState::install_for_test`'s reason on the
    /// other side of the same protocol.
    pub fn build(
        row: &UplinkRow,
        identity: Identity,
        spool: Arc<mw_pipeline::Spool>,
        cursor: ComponentId,
    ) -> Result<Self, UplinkError> {
        let credentials = match row.auth_kind.as_str() {
            "psk" => Credentials::Psk {
                secret: row
                    .psk
                    .clone()
                    .unwrap_or_else(|| Arc::new(SecretString::from(String::new()))),
            },
            // `mtls`, and anything the schema's `CHECK` would not have allowed. The three
            // paths are `NOT NULL` under that `CHECK` when `auth_kind = 'mtls'`, so a
            // missing one is this build disagreeing with its own schema; an empty path
            // reads as an unreadable file, which is the message an operator can act on.
            _ => Credentials::Mtls {
                cert: read("tls_cert_path", row.tls_cert_path.as_deref())?,
                key: read("tls_key_path", row.tls_key_path.as_deref())?,
                ca: read("tls_ca_path", row.tls_ca_path.as_deref())?,
            },
        };
        let mut dial = Endpoint::from_shared(row.endpoint.clone())
            .map_err(|e| UplinkError::Address {
                endpoint: row.endpoint.clone(),
                why: e.to_string(),
            })?
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT)
            // Without this a keepalive is only sent while a stream is open, and the gap it
            // exists to close is precisely a satellite with nothing to send.
            .keep_alive_while_idle(true);
        if let Credentials::Mtls { cert, key, ca } = &credentials {
            let tls = ClientTlsConfig::new()
                .ca_certificate(tonic::transport::Certificate::from_pem(ca.clone()))
                .identity(tonic::transport::Identity::from_pem(
                    cert.clone(),
                    key.clone(),
                ));
            dial = dial
                .tls_config(tls)
                .map_err(|source| UplinkError::Tls { source })?;
        }
        let peer = PeerName::new(row.peer_name.as_str());
        m::arm_uplink(&peer);
        m::uplink_state(&peer, UplinkState::Disconnected);
        Ok(Self {
            peer,
            peer_node: row.peer_node,
            endpoint: row.endpoint.clone(),
            dial,
            credentials,
            identity,
            require_ack: row.require_ack,
            spool,
            cursor,
            session: None,
            known_through: Vec::new(),
        })
    }

    /// Dial, handshake and `Hello`. The three states before `Streaming`.
    async fn connect(&mut self) -> Result<(), UplinkError> {
        m::uplink_state(&self.peer, UplinkState::Connecting);
        let channel = tokio::time::timeout(CONNECT_TIMEOUT, self.dial.connect())
            .await
            .map_err(|_| UplinkError::Timeout {
                endpoint: self.endpoint.clone(),
                seconds: CONNECT_TIMEOUT.as_secs(),
            })?
            .map_err(|source| UplinkError::Connect {
                endpoint: self.endpoint.clone(),
                source,
            })?;

        m::uplink_state(&self.peer, UplinkState::Hello);
        let link = if self.hello(channel.clone()).await? {
            self.open_stream(channel).await?
        } else {
            // §9.3: a collector with no `Hello` has no `Ingest` either, so there is no
            // stream to open. The channel is kept and each batch is its own RPC on it.
            Link::Compat { channel }
        };
        self.session = Some(link);
        m::reconnected(&self.peer);
        m::uplink_state(&self.peer, UplinkState::Streaming);
        Ok(())
    }

    /// One `Hello` per connection (§4.3). Returns whether the collector acks at all.
    async fn hello(&mut self, channel: Channel) -> Result<bool, UplinkError> {
        let mut grpc = tonic::client::Grpc::new(channel);
        grpc.ready().await.map_err(|source| UplinkError::Connect {
            endpoint: self.endpoint.clone(),
            source,
        })?;
        let request = self.request(pb::NodeIdentity {
            node_id: self.identity.node.as_bytes().to_vec(),
            node_name: self.identity.name.clone(),
            software: self.identity.software.clone(),
            stations: self.identity.stations.clone(),
        });
        let path = http::uri::PathAndQuery::from_static(wire::HELLO);
        match grpc
            .unary::<_, pb::HelloAck, _>(request, path, wire::HelloClientCodec::default())
            .await
        {
            Ok(response) => {
                let ack = response.into_inner();
                let sent_at = Utc::now();
                // §4.6: a drifting clock is a number before it is a rejection.
                #[allow(clippy::cast_precision_loss)]
                let skew = (sent_at.timestamp_millis() - ack.server_time) as f64 / 1000.0;
                m::clock_skew(&self.peer, skew);
                m::degraded(&self.peer, Degradation::NoAckSupport, false);
                self.known_through = ack
                    .known_through
                    .iter()
                    .filter_map(|high| {
                        <[u8; 16]>::try_from(high.origin.as_slice())
                            .ok()
                            .map(|o| (OriginId::new(o), Seq::new(high.seq)))
                    })
                    .collect();
                info!(
                    peer = %self.peer,
                    accepted = ack.accepted_stations.len(),
                    known_origins = self.known_through.len(),
                    clock_skew_s = skew,
                    "the collector said hello"
                );
                Ok(true)
            }
            // §9.3: a Go-era collector has no `Hello`. The uplink falls back to the compat
            // method and is at-most-once, which is **logged on every connection and
            // exported** rather than being a silent downgrade.
            //
            // **`UNIMPLEMENTED` and nothing else.** That is what a gRPC server answers for
            // a method it does not register -- `grpc-go`'s does, and so does `tonic`'s --
            // and it is the only signal that means "this is a collector, and it is older
            // than me". A bare HTTP `404` is deliberately **not** treated the same way:
            // it is what an endpoint pointed at the wrong server answers, and reporting a
            // wrong address as a degraded link would hide a configuration mistake behind a
            // gauge that says "working, slowly".
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                if self.require_ack {
                    return Err(UplinkError::Hello {
                        status: Box::new(status),
                    });
                }
                m::degraded(&self.peer, Degradation::NoAckSupport, true);
                warn!(
                    peer = %self.peer,
                    "this collector does not implement `collector.v1.Collector`, so it is \
                     a Go-era node: there is no acknowledgement to wait for and this \
                     uplink is AT-MOST-ONCE until the collector is upgraded. Set \
                     `require_ack` to make this a hard failure instead."
                );
                Ok(false)
            }
            Err(status) => {
                // Distinguishable by code, because the two need different people:
                // `PERMISSION_DENIED` is not enrolled or revoked and needs an operator,
                // `UNAVAILABLE` is a collector starting and needs patience.
                match status.code() {
                    tonic::Code::PermissionDenied | tonic::Code::NotFound => error!(
                        peer = %self.peer,
                        code = %status.code(),
                        "this collector does not recognise this node: it is not enrolled, \
                         or its enrolment was revoked. An operator has to enrol it."
                    ),
                    _ => warn!(
                        peer = %self.peer,
                        code = %status.code(),
                        message = status.message(),
                        "`Hello` was refused"
                    ),
                }
                Err(UplinkError::Hello {
                    status: Box::new(status),
                })
            }
        }
    }

    /// Open the `Ingest` stream over a connection that has said hello.
    async fn open_stream(&self, channel: Channel) -> Result<Link, UplinkError> {
        // **At most 8 batches in flight unacked** (W7 **D-24**), which is what bounds the
        // memory a slow collector can cost a satellite -- the backpressure the current
        // unbounded `stream.Send` lacks. The channel's own capacity is that bound.
        let (tx, rx) = tokio::sync::mpsc::channel::<pb::IngestBatch>(REQUEST_CHANNEL);
        let mut grpc = tonic::client::Grpc::new(channel);
        grpc.ready().await.map_err(|source| UplinkError::Connect {
            endpoint: self.endpoint.clone(),
            source,
        })?;
        let path = http::uri::PathAndQuery::from_static(wire::INGEST);
        let acks = grpc
            .streaming(
                self.request(BatchStream { rx }),
                path,
                wire::IngestClientCodec::default(),
            )
            .await
            .map_err(|status| UplinkError::Stream {
                status: Box::new(status),
            })?
            .into_inner();
        Ok(Link::Acked { batches: tx, acks })
    }

    /// One request, carrying the metadata layer 2 resolves a `psk` peer from.
    fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        if let Credentials::Psk { secret } = &self.credentials {
            // `x-mw-node-id` and `Authorization: Bearer`, which is what
            // `crate::service::NODE_ID_HEADER` and §5.4 ask for. Both are set only on the
            // `psk` path: under mTLS the certificate is the identity and sending a claim
            // beside it would be a second, weaker one.
            if let Ok(value) = tonic::metadata::MetadataValue::try_from(crate::handshake::hex(
                self.identity.node.as_bytes(),
            )) {
                request
                    .metadata_mut()
                    .insert(crate::service::NODE_ID_HEADER, value);
            }
            if let Ok(value) = tonic::metadata::MetadataValue::try_from(format!(
                "Bearer {}",
                secret.expose_secret()
            )) {
                request.metadata_mut().insert("authorization", value);
            }
        }
        request
    }

    /// One batch: filter, forward, send, wait for the ack.
    async fn deliver(&mut self, batch: &ReadingBatch) -> Result<(), UplinkError> {
        let through = batch.through().ok_or(UplinkError::NoPosition)?;
        // **Staged before the connection is dialled**, and the order is W7 **D-9**: a batch
        // in which every envelope is for this peer already has nothing to send, so it must
        // advance the cursor *whether or not the peer is reachable*. Dialling first made
        // that advance depend on a collector that is down -- which is precisely the stall
        // D-9 exists to forbid, reached from the one direction nobody looks at.
        let staged = self.stage(batch.envelopes());
        if staged.filtered > 0 {
            m::filtered(&self.peer, staged.filtered);
        }
        if staged.skipped > 0 {
            // §4.2's advisory fast resume: the collector's `known_through` said it already
            // had these. Counted as duplicates-avoided rather than as filtered, because
            // they were for this peer and would have been deduplicated at the far end.
            m::resume_skipped(&self.peer, staged.skipped);
        }
        if staged.path_full > 0 {
            // W7 **D-8**'s counted rejection, and **not** D-41's bug signal: a path at
            // `MAX_PATH` is a legitimate eight-hop topology refusing a ninth, which is the
            // bound working. Counted separately so that `uplink_loop_refused_total` keeps
            // the one meaning it is documented to have.
            m::path_full(&self.peer, staged.path_full);
            warn!(
                peer = %self.peer,
                refused = staged.path_full,
                max_hops = mw_types::PathSet::MAX_HOPS,
                "`forward` refused envelopes already at the hop bound, so they are not \
                 forwarded to this peer. The cursor advances over them; a topology this \
                 deep needs a shorter route rather than a larger bound."
            );
        }
        if staged.loop_refused > 0 {
            // **Non-zero is a bug in this code** (W7 **D-41**): the filter should have
            // caught it and the invariant did. Said at `ERROR` as well as counted, because
            // the counter's whole meaning is that it does not move.
            m::loop_refused(&self.peer, staged.loop_refused);
            error!(
                peer = %self.peer,
                refused = staged.loop_refused,
                "`Envelope::forward` refused envelopes the `NotFrom(peer)` filter should \
                 have skipped. This is a bug in the uplink, not a configuration problem \
                 (W7 D-41): the cursor advances over them so nothing stalls, but the two \
                 guards disagree and one of them is wrong."
            );
        }
        if staged.envelopes.is_empty() {
            // Nothing to send. **The cursor still advances**, which is W7 **D-9** and is the
            // part that is easy to get wrong: a cursor that stalls on an envelope it will
            // never send stalls for ever, the spool never reclaims, and `max_lag` eventually
            // abandons the cursor -- so a mutual uplink would present as a disk filling up.
            // Returning `Ok` is what advances it.
            debug!(
                peer = %self.peer,
                through = through.get(),
                filtered = staged.filtered,
                skipped = staged.skipped,
                "this batch had nothing to send to this peer; the cursor advances over it"
            );
            return Ok(());
        }

        if self.session.is_none() {
            self.connect().await?;
        }
        let envelopes = staged.envelopes.len();
        match self.session.as_ref().map(Link::is_acked) {
            Some(true) => self.deliver_acked(staged.envelopes, through).await,
            // §9.3, and the cost is stated where it is paid: at-most-once.
            Some(false) => self.deliver_compat(staged.envelopes).await,
            None => Err(UplinkError::Unacknowledged { envelopes }),
        }
    }

    /// The ordinary path: one `IngestBatch` on the open stream, then its ack.
    async fn deliver_acked(
        &mut self,
        envelopes: Vec<pb::Envelope>,
        through: Seq,
    ) -> Result<(), UplinkError> {
        let count = envelopes.len();
        let message = pb::IngestBatch {
            envelopes,
            through: through.get(),
            sender_depth: self.depth(),
        };
        let Some(Link::Acked { batches, .. }) = self.session.as_mut() else {
            return Err(UplinkError::Unacknowledged { envelopes: count });
        };
        if batches.send(message).await.is_err() {
            // The request stream was closed under us, which means the connection is gone.
            self.drop_session();
            return Err(UplinkError::Unacknowledged { envelopes: count });
        }
        m::sent(&self.peer, count as u64);
        self.await_ack(count, through).await
    }

    /// §9.3: one `WeatherV1.SendWeatherReadings` call against a Go-era collector.
    ///
    /// **At-most-once, and the cursor advances when the call returns.** There is no ack on
    /// that wire -- the method answers one `Empty` at end of stream and says nothing about
    /// durability -- so what returning `Ok` here means is "the collector accepted the
    /// stream", which is the most the frozen method can offer. `uplink_degraded` is already
    /// set for the life of the connection, so the downgrade is a number and not only a log
    /// line.
    ///
    /// One call per batch rather than one for the life of the connection, deliberately: a
    /// single long-lived call would make the end-of-stream `Empty` arrive once, at
    /// shutdown, and every batch before it would be fire-and-forget with no confirmation at
    /// all. Per batch, a collector that has gone away is noticed on the next batch rather
    /// than at the next restart.
    async fn deliver_compat(&mut self, envelopes: Vec<pb::Envelope>) -> Result<(), UplinkError> {
        let count = envelopes.len();
        let Some(Link::Compat { channel }) = self.session.as_ref() else {
            return Err(UplinkError::Unacknowledged { envelopes: count });
        };
        // Decoded back to the domain type and re-encoded as the Go message. The round trip
        // is not free and is not avoidable: the two schemas are different messages, and the
        // envelope on the spool is the collector one (W7 **D-11**).
        let mut readings = Vec::with_capacity(count);
        for wire in &envelopes {
            match mw_types::Envelope::try_from(wire) {
                Ok(envelope) => readings.push(crate::wire::compat_reading(&envelope)),
                Err(e) => {
                    // An envelope this node staged itself and cannot decode is a bug here,
                    // not a peer's malformed input -- so it is loud, and it is dropped
                    // rather than failing the batch, because the alternative is a cursor
                    // that never moves past it.
                    error!(
                        peer = %self.peer,
                        error = %DisplayChain(&e),
                        "an envelope this uplink staged did not re-decode, so it is not \
                         forwarded. This is a bug in this code."
                    );
                }
            }
        }
        let mut grpc = tonic::client::Grpc::new(channel.clone());
        grpc.ready().await.map_err(|source| UplinkError::Connect {
            endpoint: self.endpoint.clone(),
            source,
        })?;
        let path = http::uri::PathAndQuery::from_static(crate::wire::SEND_WEATHER_READINGS);
        let sent = readings.len();
        grpc.client_streaming(
            self.request(ReadingStream {
                readings: readings.into_iter(),
            }),
            path,
            crate::wire::CompatCodec,
        )
        .await
        .map_err(|status| {
            self.drop_session();
            UplinkError::Stream {
                status: Box::new(status),
            }
        })?;
        m::sent(&self.peer, sent as u64);
        // Nothing is outstanding, because nothing was ever promised.
        m::unacked(&self.peer, 0);
        m::ack_lag(&self.peer, 0.0);
        Ok(())
    }

    /// Wait for the ack that covers `through`.
    ///
    /// **Only `ACK_OK` and `ACK_REJECTED` advance anything**, which is `IngestAck.through`'s
    /// own contract: on any other status the field carries the last position that *was*
    /// acknowledged on this stream, and taking it as a cursor position would rewind the
    /// sender rather than advance it.
    async fn await_ack(&mut self, envelopes: usize, through: Seq) -> Result<(), UplinkError> {
        loop {
            let Some(Link::Acked { acks, .. }) = self.session.as_mut() else {
                return Err(UplinkError::Unacknowledged { envelopes });
            };
            match acks.message().await {
                Ok(Some(ack)) => {
                    if ack.duplicates > 0 {
                        m::duplicates_acked(&self.peer, u64::from(ack.duplicates));
                    }
                    if ack.rejected > 0 {
                        // **On every status, not only on `ACK_REJECTED`.** A batch of 256
                        // where 255 were spooled and one was refused answers `ACK_OK`, and
                        // without this the one that was refused is lost at the collector
                        // with nothing here saying so.
                        m::rejected_by_collector(&self.peer, u64::from(ack.rejected));
                        warn!(
                            peer = %self.peer,
                            rejected = ack.rejected,
                            status = ack.status,
                            "the collector refused {} envelope(s) in a batch. Its \
                             `mw_ingest_rejected_total{{peer, reason}}` says which clause; \
                             the usual causes are a station it has no row for and two \
                             clocks that disagree.",
                            ack.rejected
                        );
                    }
                    let status = ack.status;
                    if status == pb::AckStatus::AckResourceExhausted as i32 {
                        // **The cursor does not advance.** Returning `Err` is what leaves
                        // it where it is, and the batch replays on the next attempt. Two
                        // conditions share this status and both are the collector's own
                        // state: its spool at the ceiling, or one of these envelopes
                        // refused for a reason it can stop refusing (a clock, a station it
                        // has no row for, an origin grant nobody wrote).
                        warn!(
                            peer = %self.peer,
                            rejected = ack.rejected,
                            "the collector could not take this batch and asked for it \
                             again, so the cursor stays where it is and nothing is lost"
                        );
                        self.drop_session();
                        return Err(UplinkError::CollectorFull);
                    }
                    if status == pb::AckStatus::AckRejected as i32 {
                        // Every envelope was refused **permanently** -- a forged path, a
                        // repeated hop, a reading with no metric -- so the cursor must
                        // advance or it stalls for ever (W7 D-9). Loud, because the cause
                        // is always this node's own encoder or configuration.
                        error!(
                            peer = %self.peer,
                            rejected = ack.rejected,
                            through = ack.through,
                            "the collector refused every envelope in a batch as \
                             PERMANENTLY unacceptable. These readings are lost at that \
                             collector and will NOT be re-sent."
                        );
                    } else if status != pb::AckStatus::AckOk as i32 {
                        // `ACK_UNSPECIFIED`, or a status from a newer collector. Advancing
                        // on a status this build does not understand is the one thing that
                        // cannot be undone, so it does not.
                        error!(
                            peer = %self.peer,
                            status,
                            "the collector answered with an acknowledgement status this \
                             build does not understand, so the batch is treated as \
                             undelivered. The cursor stays where it is."
                        );
                        self.drop_session();
                        return Err(UplinkError::Unacknowledged { envelopes });
                    }
                    if ack.through >= through.get() {
                        m::unacked(&self.peer, 0);
                        m::ack_lag(&self.peer, 0.0);
                        return Ok(());
                    }
                    // A cumulative ack below our position: the collector is acknowledging
                    // an earlier batch, which happens when more than one is in flight.
                    m::unacked(&self.peer, through.get().saturating_sub(ack.through));
                }
                // The collector closed the stream without acking. Nothing is lost -- the
                // cursor did not advance -- and the batch replays.
                Ok(None) => {
                    self.drop_session();
                    return Err(UplinkError::Unacknowledged { envelopes });
                }
                Err(status) => {
                    self.drop_session();
                    return Err(UplinkError::Stream {
                        status: Box::new(status),
                    });
                }
            }
        }
    }

    /// The filter, the fast-resume skip, the invariant, and the encode. §1.4's three lines
    /// plus §4.2's one.
    fn stage(&self, envelopes: &[Envelope]) -> Staged {
        let mut staged = Staged {
            envelopes: Vec::with_capacity(envelopes.len()),
            filtered: 0,
            skipped: 0,
            loop_refused: 0,
            path_full: 0,
        };
        for envelope in envelopes {
            // The filter, as an optimisation. The subscription already applies
            // `NotFrom(peer)`, so this fires only for an envelope that reached here by
            // another route -- and it is cheaper than the `forward` that would refuse it.
            if envelope.provenance.has_visited(self.peer_node) {
                staged.filtered += 1;
                continue;
            }
            // **§4.2's advisory fast resume.** `HelloAck.known_through` is the collector's
            // per-origin high water, so an envelope at or below it is one the collector
            // demonstrably already has -- the common case being an ack that was lost when
            // the link dropped. Skipping it is an optimisation and nothing more: ignoring
            // the vector entirely is correct (**D-23**) and costs only deduplicated
            // resends, which is why this is a `<=` against a number the collector offered
            // rather than any kind of state this node keeps.
            if self.already_delivered(envelope) {
                staged.skipped += 1;
                continue;
            }
            // The invariant. `forward` inserts the **destination**, so the path the
            // collector receives already contains it -- which is what makes the receive
            // check's "addressed to me" clause exact (§2.1).
            match envelope.clone().forward(self.peer_node) {
                Ok(forwarded) => staged.envelopes.push(pb::Envelope::from(&forwarded)),
                // The two refusals mean different things and must not share a counter: a
                // cycle is D-41's "a bug in this code", and a full path is D-8's counted
                // bound doing its job.
                Err(ForwardRefused::Loop { .. }) => staged.loop_refused += 1,
                Err(ForwardRefused::PathFull { .. }) => staged.path_full += 1,
            }
        }
        staged
    }

    /// Whether the collector's own `known_through` already covers this envelope (§4.2).
    fn already_delivered(&self, envelope: &Envelope) -> bool {
        let origin = envelope.provenance.origin();
        self.known_through
            .iter()
            .find(|(known, _)| *known == origin)
            .is_some_and(|(_, high)| envelope.provenance.seq() <= *high)
    }

    /// `IngestBatch.sender_depth` — envelopes still unsent in this node's spool (§7).
    ///
    /// `tail − acked` on this component's own cursor. A **bound** rather than an exact
    /// count, because gaps in `seq` are visible and permitted (W7 **D-4**: a batch refused
    /// at the ceiling leaves one) — and the field is peer-reported and advisory, which is
    /// what makes a bound the right shape for it. The collector's own authoritative view is
    /// `peer_last_seen_seconds`.
    fn depth(&self) -> u64 {
        let tail = self.spool.tail_seq().get();
        let acked = self
            .spool
            .cursor_position(&self.cursor)
            .map_or(tail, |position| position.acked_seq.get());
        tail.saturating_sub(acked)
    }

    /// Forget the connection, so the next batch reconnects.
    fn drop_session(&mut self) {
        self.session = None;
        m::uplink_state(&self.peer, UplinkState::Disconnected);
    }
}

/// What one call to [`Uplink::stage`] produced.
///
/// Five numbers rather than a tuple, because four of them are *different kinds of
/// not-sent* and each has its own counter and its own meaning to an operator — a tuple of
/// `u64`s is how `path_full` came to be counted as a loop refusal, on the one counter whose
/// documented meaning is "there is a bug here".
struct Staged {
    /// Ready to send, already addressed at the peer.
    envelopes: Vec<pb::Envelope>,
    /// Dropped by the loop filter: this peer is already on the path (§1.4).
    filtered: u64,
    /// Dropped by the fast-resume skip: the collector said it already has them (§4.2).
    skipped: u64,
    /// `forward` refused because the hop is already on the path. **A bug here** (D-41).
    loop_refused: u64,
    /// `forward` refused because the path is at `MAX_PATH`. D-8's bound, not a bug.
    path_full: u64,
}

/// The request half of the compat call: the readings, once each.
struct ReadingStream {
    readings: std::vec::IntoIter<crate::wire::pb_reading::Reading>,
}

impl tonic::codegen::tokio_stream::Stream for ReadingStream {
    type Item = crate::wire::pb_reading::Reading;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Ready immediately and for ever: the whole batch is in memory before the call
        // opens, so there is nothing for this stream to wait on. A `tokio_stream::iter`
        // would be the same thing from a feature of `tokio-stream` that `tonic` does not
        // enable (`crate::service`'s `AckStream` has the same note).
        std::task::Poll::Ready(self.readings.next())
    }
}

/// The protocol's bound on batches in flight unacked (W7 **D-24**).
///
/// **This implementation stays strictly inside it, at one.** `Trigger::OnReading` is driven
/// by `mw_supervisor::task::run_on_reading`, which calls `readings` for one batch, waits
/// for it to return, and only then advances the cursor and asks the subscription for the
/// next — so a second batch cannot be in flight while the first is unacked, whatever this
/// crate does. D-24's 8/2048 is therefore a ceiling this uplink is far below rather than a
/// window it fills, and the pipelining it would buy a satellite draining a backlog does not
/// exist here. Recorded rather than papered over: closing the gap needs
/// `ReadingSubscription` to be able to acknowledge a *prefix*, which is `mw-component`'s
/// trait and `mw-supervisor`'s driver — the same missing `ack_through(n)` that
/// `mw_pipeline::sink`'s module documentation already records as a known gap.
pub const MAX_IN_FLIGHT_BATCHES: usize = 8;

/// The request stream's channel capacity.
///
/// **One**, which is what is actually in flight (see [`MAX_IN_FLIGHT_BATCHES`]). A capacity
/// of 8 would have been a buffer nothing ever puts a second item in, and a buffer that is
/// never exercised is a place for a bug to hide: a `send` that succeeded into it would have
/// made `deliver` return before the batch reached the socket.
const REQUEST_CHANNEL: usize = 1;

/// Read one of the `uplink` row's credential files.
fn read(what: &'static str, path: Option<&str>) -> Result<Vec<u8>, UplinkError> {
    let path = path.unwrap_or_default();
    std::fs::read(path).map_err(|source| UplinkError::Credential {
        what,
        path: path.to_string(),
        source,
    })
}

/// The request half of the `Ingest` stream: whatever the uplink hands the channel.
struct BatchStream {
    rx: tokio::sync::mpsc::Receiver<pb::IngestBatch>,
}

impl tonic::codegen::tokio_stream::Stream for BatchStream {
    type Item = pb::IngestBatch;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl OnReading for Uplink {
    fn readings<'a>(
        &'a mut self,
        batch: ReadingBatch,
        _cx: &'a Cx,
    ) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let result = self.deliver(&batch).await;
            if let Err(e) = &result {
                m::ack_lag(&self.peer, started.elapsed().as_secs_f64());
                warn!(
                    peer = %self.peer,
                    envelopes = batch.len(),
                    error = %DisplayChain(e),
                    "a batch could not be delivered, so the cursor stays where it is and \
                     it will be re-sent"
                );
            }
            result.map_err(Into::into)
        })
    }
}

/// Why an `uplink` row cannot be planned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UplinkPlanError {
    /// The component id derived from the peer's name is not one.
    #[error("`uplink:{peer}` is not a usable component id")]
    Id {
        /// The peer's name.
        peer: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },

    /// §8.5: an `[[uplink]]` whose peer is this node itself.
    ///
    /// **Its place in the table is earned**: a node uplinking to itself is the degenerate
    /// loop, and making it a configuration error means the loop invariant never has to be
    /// the thing that catches it.
    #[error(
        "`uplink` names the peer `{peer}`, whose node id is this node's own: a node \
         cannot uplink to itself"
    )]
    SelfUplink {
        /// The peer's name.
        peer: String,
    },

    /// §8.5: two `[[uplink]]`s to the same peer.
    #[error("two `uplink` rows name the peer `{peer}`, and one peer is one uplink")]
    DuplicateUplink {
        /// The peer's name.
        peer: String,
    },

    /// An `uplink` row naming a peer with no `peers` row.
    ///
    /// Not in §8.5's table and it belongs there: the uplink needs the peer's `NodeId` for
    /// the filter and for `forward`, and there is nowhere else to get one. Without this the
    /// component would have to invent an id, which is a loop guard comparing against the
    /// wrong node.
    ///
    /// **This is also the one check that makes §8.2's rendering incomplete**, and that is
    /// worth being explicit about: that section's satellite is `[node]` + `[[station]]` +
    /// `[[uplink]]` with no `[[peer]]` block, because a satellite accepts nobody. But the
    /// uplink still needs its collector's `NodeId`, and the only place a node id lives is
    /// a `peers` row — so a satellite needs one for its collector even though nothing will
    /// ever connect *to* it. §5.3's `mw-admin uplink join` is what writes both rows in
    /// practice; recorded as a proposed amendment to §8.2 in this item's PR.
    #[error(
        "`uplink` names the peer `{peer}`, which has no `peers` row -- and an uplink needs \
         that peer's node id for the loop filter and for `Envelope::forward`, so there is \
         nothing to plan. Insert the `peers` row for `{peer}` (its `node_id` is the \
         `collector_node_id` that collector reports at enrolment), or remove the `uplink` \
         row"
    )]
    UnknownPeer {
        /// The peer's name.
        peer: String,
    },
}

/// What this crate needs from the configuration to plan the uplinks.
pub trait UplinkConfig {
    /// Every `uplink` row.
    ///
    /// **Its emptiness is meaningful**: a node with no row sends to nobody, which is the
    /// other half of W7 **D-44**.
    fn uplinks(&self) -> &[UplinkRow];

    /// The `uplink` rows whose peer this node has no `peers` row for.
    ///
    /// Separate from [`UplinkConfig::uplinks`] because they cannot be the same type: a row
    /// without its peer's `NodeId` is not an `UplinkRow`, and the join that produces one is
    /// the binary's. Carrying the names is what makes
    /// [`UplinkPlanError::UnknownPeer`] reachable — before this existed the rows were
    /// filtered out by the join and nothing anywhere mentioned them.
    fn unjoinable_peers(&self) -> &[String];

    /// What a `Hello` announces about this node.
    fn identity(&self) -> Option<&Identity>;

    /// The spool, for `sender_depth`.
    fn spool(&self) -> Option<&Arc<mw_pipeline::Spool>>;
}

/// Plan one `uplink:<peer>` per `uplink` row.
pub fn register<C>(cfg: &C, out: &mut Vec<ComponentSpec>) -> Result<(), UplinkPlanError>
where
    C: UplinkConfig + ?Sized,
{
    // **Before anything else**, because an `uplink` row naming a peer this node has no
    // row for cannot be planned at all: the component needs that peer's `NodeId` for the
    // loop filter and for `forward`, and inventing one would make the loop guard compare
    // against the wrong node and the collector refuse every envelope as
    // `NotAddressedHere`.
    //
    // **A refusal and not a warning**, because the alternative was measured: dropping the
    // row planned no uplink, said nothing, and left the node spooling readings it would
    // never forward until `max_lag` abandoned the cursor. A satellite configured by
    // `docs/collector.md` §8.2 -- which has an `[[uplink]]` and no `[[peer]]` -- is exactly
    // that shape, so this is the error an operator following the document gets, and its
    // message is what tells them the row they are missing. The satellite half of §5.3's
    // enrolment is what writes both rows; until that command exists the `peers` row is an
    // operator's own insert.
    if let Some(peer) = cfg.unjoinable_peers().first() {
        return Err(UplinkPlanError::UnknownPeer { peer: peer.clone() });
    }
    let rows = cfg.uplinks();
    if rows.is_empty() {
        return Ok(());
    }
    let (Some(identity), Some(spool)) = (cfg.identity(), cfg.spool()) else {
        // No node row means no spool and no identity, so there is nothing to forward and
        // nothing to forward it as. Said out loud rather than silently planning nothing.
        warn!(
            uplinks = rows.len(),
            "this node has `uplink` rows and no spool, so no uplink is planned: there \
             would be nothing to read batches from"
        );
        return Ok(());
    };
    let mut seen: Vec<&str> = Vec::with_capacity(rows.len());
    for row in rows {
        if seen.contains(&row.peer_name.as_str()) {
            return Err(UplinkPlanError::DuplicateUplink {
                peer: row.peer_name.clone(),
            });
        }
        seen.push(row.peer_name.as_str());
        if row.peer_node == identity.node {
            return Err(UplinkPlanError::SelfUplink {
                peer: row.peer_name.clone(),
            });
        }
        let id = id(&row.peer_name).map_err(|source| UplinkPlanError::Id {
            peer: row.peer_name.clone(),
            source,
        })?;
        let digest = ConfigDigest::of(&(
            row.peer_name.as_str(),
            row.endpoint.as_str(),
            row.auth_kind.as_str(),
            row.tls_cert_path.as_deref(),
            row.tls_key_path.as_deref(),
            row.tls_ca_path.as_deref(),
            row.require_ack,
        ));
        let built = row.clone();
        let identity = identity.clone();
        let spool = Arc::clone(spool);
        let cursor = id.clone();
        out.push(
            ComponentSpec::new(
                id,
                Trigger::OnReading {
                    // **`NotFrom(peer)`, not `NotFrom(self)`** (W7 **D-47**): every
                    // envelope in this node's spool carries `self` on its path, so
                    // `NotFrom(self)` would match nothing and this uplink would send
                    // nothing.
                    filter: ReadingFilter::NotFrom(row.peer_node),
                    policy: ReadingPolicy::Durable,
                    deadline: DEADLINE,
                },
                mw_component::Builder::on_reading(move |_cx| {
                    let row = built.clone();
                    let identity = identity.clone();
                    let spool = Arc::clone(&spool);
                    let cursor = cursor.clone();
                    async move {
                        Uplink::build(&row, identity, spool, cursor).map_err(|e| {
                            BuildError::Resource {
                                what: "the uplink's credentials".to_string(),
                                source: Box::new(e),
                            }
                        })
                    }
                }),
            )
            .requires([mw_component::Need::hard(
                mw_component::Capability::ReadingStream,
            )])
            .grace(GRACE)
            .config_digest(digest),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(b: u8) -> OriginId {
        OriginId::new([b; 16])
    }

    fn identity() -> Identity {
        Identity {
            node: node(1),
            name: "satellite-a".to_string(),
            software: "modernweather 0.1.0".to_string(),
            stations: Vec::new(),
        }
    }

    fn row(peer: &str, peer_node: u8) -> UplinkRow {
        UplinkRow {
            peer_name: peer.to_string(),
            peer_node: node(peer_node),
            endpoint: "https://suncrest.example.net:50051".to_string(),
            auth_kind: "mtls".to_string(),
            tls_cert_path: Some("/etc/modernweather/satellite-a.pem".to_string()),
            tls_key_path: Some("/etc/modernweather/satellite-a-key.pem".to_string()),
            tls_ca_path: Some("/etc/modernweather/peers-ca.pem".to_string()),
            psk: None,
            require_ack: false,
        }
    }

    struct Cfg {
        rows: Vec<UplinkRow>,
        unjoinable: Vec<String>,
        identity: Option<Identity>,
        spool: Option<Arc<mw_pipeline::Spool>>,
    }

    impl UplinkConfig for Cfg {
        fn uplinks(&self) -> &[UplinkRow] {
            &self.rows
        }
        fn unjoinable_peers(&self) -> &[String] {
            &self.unjoinable
        }
        fn identity(&self) -> Option<&Identity> {
            self.identity.as_ref()
        }
        fn spool(&self) -> Option<&Arc<mw_pipeline::Spool>> {
            self.spool.as_ref()
        }
    }

    fn cfg(rows: Vec<UplinkRow>) -> Cfg {
        Cfg {
            rows,
            unjoinable: Vec::new(),
            identity: Some(identity()),
            spool: Some(crate::testing::null_spool()),
        }
    }

    #[test]
    fn the_component_id_is_the_renamed_one() {
        // W7 **D-2**: `uplink:<peer>`, not `uplink:collector:<peer>`.
        assert_eq!(
            id("suncrest").expect("a legal id").as_str(),
            "uplink:suncrest"
        );
    }

    /// §8.2's satellite -- an `[[uplink]]` with no `[[peer]]` -- used to plan **nothing**
    /// and say nothing. It is now a refusal that names the row to add.
    #[test]
    fn an_uplink_whose_peer_has_no_row_is_a_plan_error() {
        let mut out = Vec::new();
        let err = register(
            &Cfg {
                rows: Vec::new(),
                unjoinable: vec!["suncrest".to_string()],
                identity: Some(identity()),
                spool: Some(crate::testing::null_spool()),
            },
            &mut out,
        )
        .expect_err("an uplink with no peer id cannot be planned");
        assert!(matches!(err, UplinkPlanError::UnknownPeer { .. }), "{err}");
        let message = err.to_string();
        assert!(message.contains("suncrest"), "{message}");
        assert!(
            message.contains("peers"),
            "the message has to name the row to insert: {message}"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_node_with_no_uplink_row_sends_to_nobody() {
        let mut out = Vec::new();
        register(&cfg(Vec::new()), &mut out).expect("a collector plans no uplink");
        assert!(out.is_empty());
    }

    /// §8.5: the degenerate loop is a configuration error, so the invariant never has to
    /// be the thing that catches it.
    #[test]
    fn an_uplink_to_this_node_is_a_plan_error() {
        let mut out = Vec::new();
        let err = register(&cfg(vec![row("myself", 1)]), &mut out)
            .expect_err("a node cannot uplink to itself");
        assert!(matches!(err, UplinkPlanError::SelfUplink { .. }), "{err}");
    }

    #[test]
    fn two_uplinks_to_one_peer_are_a_plan_error() {
        let mut out = Vec::new();
        let err = register(&cfg(vec![row("suncrest", 9), row("suncrest", 9)]), &mut out)
            .expect_err("one peer is one uplink");
        assert!(
            matches!(err, UplinkPlanError::DuplicateUplink { .. }),
            "{err}"
        );
    }

    /// **D-47**: the filter has to be `NotFrom(peer)`. With `NotFrom(self)` the
    /// subscription matches nothing and the uplink sends nothing — a silent failure, which
    /// is why this is asserted on the spec rather than left to the module doc.
    #[test]
    fn the_filter_is_not_from_the_peer() {
        let mut out = Vec::new();
        register(&cfg(vec![row("suncrest", 9)]), &mut out).expect("one uplink");
        let spec = out.first().expect("one spec");
        match &spec.trigger {
            Trigger::OnReading { filter, policy, .. } => {
                assert_eq!(*filter, ReadingFilter::NotFrom(node(9)));
                assert_eq!(
                    *policy,
                    ReadingPolicy::Durable,
                    "an uplink must not lose readings, and only `Durable` carries a \
                     cursor position for `IngestBatch.through`"
                );
            }
            other => panic!("an uplink is an OnReading component, not {other:?}"),
        }
    }

    #[test]
    fn a_node_with_no_spool_plans_no_uplink_and_does_not_refuse_the_configuration() {
        let mut out = Vec::new();
        register(
            &Cfg {
                rows: vec![row("suncrest", 9)],
                unjoinable: Vec::new(),
                identity: Some(identity()),
                spool: None,
            },
            &mut out,
        )
        .expect("a node that serves but does not ingest is not a refused configuration");
        assert!(out.is_empty());
    }

    /// Changing where an uplink points has to restart it, or the component keeps the old
    /// connection.
    #[test]
    fn the_digest_follows_the_endpoint_and_the_credentials() {
        let mut a = Vec::new();
        register(&cfg(vec![row("suncrest", 9)]), &mut a).expect("one uplink");
        let mut moved = row("suncrest", 9);
        moved.endpoint = "https://suncrest.example.net:50052".to_string();
        let mut b = Vec::new();
        register(&cfg(vec![moved]), &mut b).expect("one uplink");
        assert_ne!(
            a[0].config_digest, b[0].config_digest,
            "a moved endpoint is a restart"
        );
        let mut rotated = row("suncrest", 9);
        rotated.tls_cert_path = Some("/etc/modernweather/satellite-a-2026.pem".to_string());
        let mut c = Vec::new();
        register(&cfg(vec![rotated]), &mut c).expect("one uplink");
        assert_ne!(
            a[0].config_digest, c[0].config_digest,
            "a rotated certificate is a restart"
        );
    }

    #[test]
    fn an_unreadable_credential_is_a_build_failure_naming_the_file() {
        let err = Uplink::build(
            &row("suncrest", 9),
            identity(),
            crate::testing::null_spool(),
            id("suncrest").expect("a legal id"),
        )
        .expect_err("the test's certificate paths do not exist");
        assert!(matches!(err, UplinkError::Credential { .. }), "{err}");
        let message = err.to_string();
        assert!(message.contains("satellite-a.pem"), "{message}");
        assert_eq!(
            err.class(),
            Class::RETRYABLE,
            "a certificate being rotated under the process is a Backoff, not a Fatal"
        );
    }

    #[test]
    fn a_malformed_endpoint_is_fatal_rather_than_retried() {
        let e = UplinkError::Address {
            endpoint: "not a url".to_string(),
            why: "invalid".to_string(),
        };
        assert_eq!(
            e.class(),
            Class::Fatal,
            "no retry makes a URL parse, and an operator has to see it"
        );
    }

    /// The in-flight bound is W7 **D-24**'s, and the two numbers have to agree: 8 batches
    /// of at most 256 envelopes is the 2048 that decision writes.
    #[test]
    fn the_in_flight_window_is_eight_batches_and_two_thousand_envelopes() {
        assert_eq!(MAX_IN_FLIGHT_BATCHES, 8);
        assert_eq!(
            MAX_IN_FLIGHT_BATCHES * mw_pipeline::MAX_BATCH_ENVELOPES,
            2048
        );
        assert_eq!(
            mw_pipeline::READ_AHEAD,
            mw_pipeline::MAX_BATCH_ENVELOPES,
            "W3 §2.4's read-ahead lines up with exactly one IngestBatch"
        );
    }
}
