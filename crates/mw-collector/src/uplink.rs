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
use mw_types::{Class, Classify, DisplayChain, Envelope, OriginId, Seq};
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
    fn class(&self) -> Class {
        match self {
            // A configuration mistake. `Fatal` so the component stays `Failed` and an
            // operator sees it, rather than retrying a URL that will never parse.
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
    endpoint: String,
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
    session: Option<Session>,
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

/// One open connection: the ingest stream, and what the collector said at `Hello`.
struct Session {
    /// The sender half of the request stream.
    batches: tokio::sync::mpsc::Sender<pb::IngestBatch>,
    /// The ack stream.
    acks: tonic::Streaming<pb::IngestAck>,
    /// Whether the collector supports `Hello` at all (§9.3).
    acked: bool,
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
        let peer = PeerName::new(row.peer_name.as_str());
        m::arm_uplink(&peer);
        m::uplink_state(&peer, UplinkState::Disconnected);
        Ok(Self {
            peer,
            peer_node: row.peer_node,
            endpoint: row.endpoint.clone(),
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
        let mut endpoint = Endpoint::from_shared(self.endpoint.clone())
            .map_err(|e| UplinkError::Address {
                endpoint: self.endpoint.clone(),
                why: e.to_string(),
            })?
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT)
            // Without this a keepalive is only sent while a stream is open, and the gap
            // this exists to close is precisely a satellite with nothing to send.
            .keep_alive_while_idle(true);
        if let Credentials::Mtls { cert, key, ca } = &self.credentials {
            let tls = ClientTlsConfig::new()
                .ca_certificate(tonic::transport::Certificate::from_pem(ca.clone()))
                .identity(tonic::transport::Identity::from_pem(
                    cert.clone(),
                    key.clone(),
                ));
            endpoint = endpoint
                .tls_config(tls)
                .map_err(|source| UplinkError::Tls { source })?;
        }
        let channel = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect())
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
        let acked = self.hello(channel.clone()).await?;
        let session = self.open_stream(channel, acked).await?;
        self.session = Some(session);
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
    async fn open_stream(&self, channel: Channel, acked: bool) -> Result<Session, UplinkError> {
        // **At most 8 batches in flight unacked** (W7 **D-24**), which is what bounds the
        // memory a slow collector can cost a satellite -- the backpressure the current
        // unbounded `stream.Send` lacks. The channel's own capacity is that bound.
        let (tx, rx) = tokio::sync::mpsc::channel::<pb::IngestBatch>(MAX_IN_FLIGHT_BATCHES);
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
        Ok(Session {
            batches: tx,
            acks,
            acked,
        })
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
        if self.session.is_none() {
            self.connect().await?;
        }
        let (staged, filtered, refused) = self.stage(batch.envelopes());
        if filtered > 0 {
            m::filtered(&self.peer, filtered);
        }
        if refused > 0 {
            // **Non-zero is a bug in this code** (W7 **D-41**): the filter should have
            // caught it and the invariant did. Said at `ERROR` as well as counted, because
            // the counter's whole meaning is that it does not move.
            m::loop_refused(&self.peer, refused);
            error!(
                peer = %self.peer,
                refused,
                "`Envelope::forward` refused envelopes the `NotFrom(peer)` filter should \
                 have skipped. This is a bug in the uplink, not a configuration problem \
                 (W7 D-41): the cursor advances over them so nothing stalls, but the two \
                 guards disagree and one of them is wrong."
            );
        }
        if staged.is_empty() {
            // Every envelope was filtered or refused. **The cursor still advances**, which
            // is W7 **D-9** and is the part that is easy to get wrong: a cursor that stalls
            // on an envelope it will never send stalls for ever, the spool never reclaims,
            // and `max_lag` eventually abandons the cursor -- so a mutual uplink would
            // present as a disk filling up. Returning `Ok` is what advances it.
            debug!(
                peer = %self.peer,
                through = through.get(),
                "every envelope in this batch was for this peer already; the cursor \
                 advances over them without sending"
            );
            return Ok(());
        }

        let envelopes = staged.len();
        let message = pb::IngestBatch {
            envelopes: staged,
            through: through.get(),
            sender_depth: self.depth(),
        };
        let session = self
            .session
            .as_mut()
            .expect("a session was established above");
        if session.batches.send(message).await.is_err() {
            // The request stream was closed under us, which means the connection is gone.
            self.drop_session();
            return Err(UplinkError::Unacknowledged { envelopes });
        }
        m::sent(&self.peer, envelopes as u64);
        if !session.acked {
            // §9.3: there is no ack to wait for, so the cursor advances **on send** and
            // this uplink is at-most-once -- exactly as the Go satellite is today.
            m::unacked(&self.peer, 0);
            return Ok(());
        }
        self.await_ack(envelopes, through).await
    }

    /// Wait for the ack that covers `through`.
    async fn await_ack(&mut self, envelopes: usize, through: Seq) -> Result<(), UplinkError> {
        let session = self
            .session
            .as_mut()
            .expect("a session was established by the caller");
        loop {
            match session.acks.message().await {
                Ok(Some(ack)) => {
                    if ack.duplicates > 0 {
                        m::duplicates_acked(&self.peer, u64::from(ack.duplicates));
                    }
                    if ack.status == pb::AckStatus::AckResourceExhausted as i32 {
                        // The cursor does **not** advance: returning `Err` is what leaves
                        // it where it is, and the batch replays on the next attempt.
                        warn!(
                            peer = %self.peer,
                            "the collector's spool is at its ceiling, so this batch was \
                             not spooled and will be re-sent"
                        );
                        self.drop_session();
                        return Err(UplinkError::CollectorFull);
                    }
                    if ack.status == pb::AckStatus::AckRejected as i32 {
                        // `through` has advanced on the collector's side and these
                        // envelopes will never be accepted, so the cursor must advance too
                        // (W7 D-9). Loud, because the cause is always configuration: an
                        // unknown station, a path this node forged, a clock out of the
                        // window.
                        error!(
                            peer = %self.peer,
                            rejected = ack.rejected,
                            through = ack.through,
                            "the collector REFUSED every envelope in a batch. These \
                             readings are lost at that collector and will not be re-sent: \
                             check that it has a station row for each key this node sends \
                             and that the two clocks agree."
                        );
                    }
                    if ack.through >= through.get() {
                        m::unacked(&self.peer, 0);
                        m::ack_lag(&self.peer, 0.0);
                        return Ok(());
                    }
                    // A cumulative ack below our position: the collector is acknowledging
                    // an earlier batch, which happens when more than one is in flight.
                    #[allow(clippy::cast_precision_loss)]
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

    /// The filter, the invariant, and the encode. §1.4's three lines.
    fn stage(&self, envelopes: &[Envelope]) -> (Vec<pb::Envelope>, u64, u64) {
        let mut staged = Vec::with_capacity(envelopes.len());
        let mut filtered = 0u64;
        let mut refused = 0u64;
        for envelope in envelopes {
            // The filter, as an optimisation. The subscription already applies
            // `NotFrom(peer)`, so this fires only for an envelope that reached here by
            // another route -- and it is cheaper than the `forward` that would refuse it.
            if envelope.provenance.has_visited(self.peer_node) {
                filtered += 1;
                continue;
            }
            // The invariant. `forward` inserts the **destination**, so the path the
            // collector receives already contains it -- which is what makes the receive
            // check's "addressed to me" clause exact (§2.1).
            match envelope.clone().forward(self.peer_node) {
                Ok(forwarded) => staged.push(pb::Envelope::from(&forwarded)),
                Err(e) => {
                    refused += 1;
                    debug!(
                        peer = %self.peer,
                        error = %DisplayChain(&e),
                        "`forward` refused an envelope"
                    );
                }
            }
        }
        (staged, filtered, refused)
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

/// At most 8 batches / 2048 envelopes in flight unacked (W7 **D-24**).
pub const MAX_IN_FLIGHT_BATCHES: usize = 8;

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
    #[error(
        "`uplink` names the peer `{peer}`, which has no `peers` row: the uplink needs that \
         peer's node id for the loop filter, so enrol it or insert the row"
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
        identity: Option<Identity>,
        spool: Option<Arc<mw_pipeline::Spool>>,
    }

    impl UplinkConfig for Cfg {
        fn uplinks(&self) -> &[UplinkRow] {
            &self.rows
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
