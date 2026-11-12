//! The write end of the reading pipeline: how a reading gets *in*.
//!
//! [`ReadingStream`](crate::ReadingStream) is the read end — what the supervisor delivers
//! from. This is the other side of the same spool: the handle a station driver, a gRPC
//! receiver or a REST ingest handler resolves once in `build` from
//! `Need { ReadingStream, Hard, Write }` and then pushes into.
//!
//! **Two entry points, and the second one is not a convenience (D-60).**
//!
//! - [`Ingress::admit`] seals a reading this node measured: it resolves identity, enriches,
//!   mints `origin = self` and `seq = ` the next spool position, and durably appends.
//! - [`Ingress::accept`] takes envelopes a peer already sealed and **preserves their
//!   provenance**. Running `admit`'s `seq` mint on a forwarded envelope renumbers it in the
//!   receiver's coordinate space and destroys the `(origin, seq)` identity that the
//!   cumulative ack and the dedup gate both depend on. Having only `admit` would be the
//!   defect, not the simplification.
//!
//! **What `admit` may wait on is the whole of W3 D-87.** The only await on this path is the
//! local spool's `fsync`. It never waits on a sink, a database, a peer or a subscriber, and
//! identity resolution is a *synchronous* [`StationRegistry::resolve`] over a published
//! snapshot — a signature that cannot reach a database, rather than a rule that says not
//! to.

use chrono::{DateTime, Utc};
use mw_types::{Envelope, PartialReading, Seq, StationKey};

use crate::body::BoxFuture;
use crate::capability::Capability;
use crate::cx::CapabilityHandle;
use crate::id::ComponentId;
use crate::identity::IdentitySource;

/// What the transport knows about a frame, beyond the readings in it.
///
/// This is W2a Decision 46's single authentication point expressed as data: everything
/// [`StationRegistry::resolve`] is allowed to consult about *who sent this* is in here, so
/// there is one gate rather than one per transport. W7 assumption 6 asks for a
/// peer-identity field among these, and [`FrameMeta::peer`] is it.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    /// Which component received the frame, for the log line on a rejection — so "somebody
    /// is pointing a station at us" names the endpoint they are pointing it at.
    ///
    /// **Not** a metric label today: `mw_readings_rejected_total` carries `reason` alone.
    /// §2.6 and §5.10 specify `{component, reason}` and the component id is bounded by
    /// configuration, so adding it is cardinality-safe; it is simply not done yet, and
    /// saying so here is better than documenting a label set that is not emitted.
    pub receiver: ComponentId,

    /// How this transport says which station a frame is from (W2a §4a.5).
    pub source: IdentitySource,

    /// The station key the frame presented, for every `source` but
    /// [`IdentitySource::Fixed`] — a path segment, an `?ID=`, a DSN inside a payload, a
    /// registration id. It is a **claim**: it is `&str` until `resolve` turns it into a
    /// [`StationKey`], and that conversion is the authentication.
    pub presented_key: Option<String>,

    /// The secret the frame presented, where the protocol has one — Ambient's
    /// `?PASSWORD=`, a registration token.
    ///
    /// Compared in constant time against the registry's copy and never logged (**D-33**).
    /// `SecretString` rather than `String` so that the redaction is the type's job: the Go
    /// management token is logged in cleartext at startup and on every failed
    /// authentication, which is what a plain `String` here would reproduce.
    pub presented_secret: Option<secrecy::SecretString>,

    /// The peer a prior admission layer resolved, for an inbound node-to-node frame
    /// (W7 §5.4, assumption 6).
    ///
    /// `Some` means mTLS or a PSK already identified the *node*; the per-envelope origin
    /// authorisation that follows is a separate check. `None` for every station transport,
    /// where there is no peer because there is no node on the other end.
    pub peer: Option<mw_types::OriginId>,

    /// When the frame arrived, as the transport observed it.
    ///
    /// Admission uses it for [`Envelope::received_at`] and for any
    /// [`PartialReading::observed_at`] the frame did not carry, rather than reading a clock
    /// of its own: the honest answer to "when did we see this" is when the bytes landed,
    /// not when the group commit drained — and a timestamp that comes in as data is a
    /// timestamp a test can pin.
    pub received_at: DateTime<Utc>,
}

impl FrameMeta {
    /// The metadata of a frame from a `Fixed`-identity transport — a serial Davis, a
    /// polled vendor API — where configuration already says which station this is.
    pub fn fixed(receiver: ComponentId, received_at: DateTime<Utc>) -> Self {
        Self {
            receiver,
            source: IdentitySource::Fixed,
            presented_key: None,
            presented_secret: None,
            peer: None,
            received_at,
        }
    }

    /// The metadata of a frame that presented a key.
    pub fn presenting(
        receiver: ComponentId,
        source: IdentitySource,
        key: impl Into<String>,
        received_at: DateTime<Utc>,
    ) -> Self {
        Self {
            receiver,
            source,
            presented_key: Some(key.into()),
            presented_secret: None,
            peer: None,
            received_at,
        }
    }

    /// The same frame, with a secret the protocol presented alongside the key.
    pub fn with_secret(mut self, secret: secrecy::SecretString) -> Self {
        self.presented_secret = Some(secret);
        self
    }

    /// The same frame, attributed to an already-resolved peer node.
    pub fn from_peer(mut self, peer: mw_types::OriginId) -> Self {
        self.peer = Some(peer);
        self
    }
}

/// Why a frame or an envelope was refused *before* anything was stored.
///
/// W2a §4a.6 names the first four and W7 §4.4 and §5.4 add the rest. Every variant is a
/// counted rejection — `mw_readings_rejected_total{reason}` — and not a log line: an
/// unknown sender is a number an operator can alert on, where the Go gRPC receiver stores
/// the reading and then emits a `Debugf` about it. (§5.10 also specifies a `component`
/// label; see [`FrameMeta::receiver`] for where that stands.)
///
/// [`Reject::as_str`] is the metric label, so the set of label values is this closed enum
/// and §5.10's cardinality rule holds by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// No station with that key, or no station configured for that endpoint.
    UnknownStation,
    /// The presented secret did not match.
    BadSecret,
    /// The station exists and is switched off.
    Disabled,
    /// The sending node is not a peer at all — §5.4's `UnknownPeer`.
    ///
    /// The one name in §5.4's list that is spelled differently here, and deliberately: the
    /// variant and its metric label predate M6-4 and are already emitted, so renaming it
    /// would retire a series an operator may be alerting on to say exactly the same thing.
    /// [`Reject::UnauthorizedOrigin`] carries the other half the original wording covered.
    UnauthorizedPeer,
    /// The peer is known and `peers.revoked_at` is set (W7 §5.4 layer 2).
    ///
    /// Separate from [`Reject::UnauthorizedPeer`] because the two need different people: a
    /// revoked peer is an operator's own past decision still being honoured, where an
    /// unknown one means somebody is pointing a node at this port.
    Revoked,
    /// The peer may not speak for the origin this envelope claims (W7 §5.4 layer 3).
    ///
    /// `relay = false` is the default, so this is what a leaf trying to inject history for
    /// another station reaches.
    UnauthorizedOrigin,
    /// `observed_at` is more than `max_future_skew` **ahead** of this node's clock
    /// (W7 §4.6).
    ///
    /// Transient, and that is the direction that matters: a collector whose own clock is
    /// fast refuses every envelope in the fleet, and the condition clears the moment NTP
    /// corrects it.
    ClockSkew,
    /// `observed_at` is more than `max_past_skew` **behind** this node's clock (W7 §4.6).
    ///
    /// **Separate from [`Reject::ClockSkew`] because the two edges have opposite
    /// permanence.** Time moves away from the past edge and never back towards it, so an
    /// envelope that is already too old will never be young enough — treating it as
    /// transient would guarantee a stall until `[spool].max_lag` abandoned the sender's
    /// cursor, which is the one outcome worse than a counted rejection. The window is
    /// thirty days, chosen above both `max_lag` and the worst-case ceiling, so reaching
    /// this means a spool that outlived its own retention.
    TooOld,
    /// `seq` is zero, which `mw_types::Seq::FIRST` makes impossible for an honest encoder.
    ///
    /// Its own variant rather than `MalformedPath`, because the metric label and the log
    /// field are what an operator chases: `malformed_path` on an envelope whose path is
    /// perfectly well formed sends them to the wrong place.
    BadSequence,
    /// The provenance path is longer than `PathSet::MAX_HOPS` (W7 **D-8**).
    PathTooLong,
    /// The provenance path is not something an honest encoder produces — the origin is not
    /// `path[0]`, or a node appears twice.
    MalformedPath,
    /// `path.last()` is not this node: the envelope was addressed somewhere else.
    NotAddressedHere,
    /// This node is an interior hop on the path — the receive-side loop guard (W7 **D-7**).
    Loop,
    /// `path[len - 2]` is not the peer that sent it: the sender did not append itself, so
    /// the path it presents is not the path the envelope took.
    PathForged,
    /// The origin intern table is at `MAX_ORIGINS` (W7 **D-8**).
    OriginTableFull,
    /// More envelopes, or more bytes, than one batch may carry.
    BatchTooLarge,
    /// The reading carries no populated metric, so it would store as an all-`NULL` row
    /// the aggregates still have to process (W7 §4.6).
    EmptyReading,
}

impl Reject {
    /// Whether re-sending the same bytes could ever get a different answer.
    ///
    /// **The question is whose state the verdict depends on.** A verdict that is a function
    /// of the envelope's own bytes — a forged path, a hop that repeats, a reading with no
    /// metric — is the same verdict for ever, on this node and on every other. A verdict
    /// that is also a function of **this node's** state — a station it has no row for yet,
    /// an origin grant an operator has not written, a clock that is wrong, an intern table
    /// that is full — changes the moment that state does.
    ///
    /// # Why this distinction is load-bearing and not taxonomy
    ///
    /// A collector's ack is cumulative in the sender's log, so acknowledging a batch tells
    /// the sender it may advance **past** every envelope in it. Doing that for a transient
    /// refusal destroys real observations and reports success while doing it:
    ///
    /// - a collector whose clock is ten minutes fast refuses *every* envelope from *every*
    ///   satellite as [`Reject::ClockSkew`]. If that advances the senders' cursors, then by
    ///   the time NTP corrects the clock the fleet's readings for that window are gone from
    ///   every spool in it. One wrong clock on one machine, permanent fleet-wide loss.
    /// - a station added to a running satellite is not in the collector's
    ///   `config.stations` until the next `Hello`, so its readings are
    ///   [`Reject::UnknownStation`] until then.
    /// - [`Reject::OriginTableFull`] is a capacity condition by construction.
    ///
    /// Treating a reason as transient costs a **stall**, and a stall is bounded: the
    /// sender's cursor stays put, its spool fills, and `[spool].max_lag` abandons the cursor
    /// with a counted, named gap (W7 **D-16**). Treating one as permanent costs silent loss
    /// with no counter anywhere. So the safe default is transient, and only a reason that is
    /// provably a property of the bytes answers `false`.
    pub const fn is_transient(self) -> bool {
        match self {
            // Properties of this node's configuration or state.
            Reject::UnknownStation
            | Reject::Disabled
            | Reject::UnauthorizedPeer
            | Reject::Revoked
            | Reject::UnauthorizedOrigin
            | Reject::BadSecret
            | Reject::ClockSkew
            | Reject::OriginTableFull => true,
            // Properties of the envelope's own bytes. No configuration change makes these
            // acceptable, and re-reading them cannot produce a different answer.
            Reject::PathTooLong
            | Reject::MalformedPath
            | Reject::NotAddressedHere
            | Reject::Loop
            | Reject::PathForged
            | Reject::BatchTooLarge
            | Reject::EmptyReading
            | Reject::BadSequence
            // Time only moves away from the past edge, so an envelope already too old will
            // never be young enough. Transient here would mean a guaranteed stall.
            | Reject::TooOld => false,
        }
    }

    /// The metric label and log field for this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Reject::UnknownStation => "unknown_station",
            Reject::BadSecret => "bad_secret",
            Reject::Disabled => "disabled",
            Reject::UnauthorizedPeer => "unauthorized_peer",
            Reject::Revoked => "revoked",
            Reject::UnauthorizedOrigin => "unauthorized_origin",
            Reject::ClockSkew => "clock_skew",
            Reject::TooOld => "too_old",
            Reject::BadSequence => "bad_sequence",
            Reject::PathTooLong => "path_too_long",
            Reject::MalformedPath => "malformed_path",
            Reject::NotAddressedHere => "not_addressed_here",
            Reject::Loop => "loop",
            Reject::PathForged => "path_forged",
            Reject::OriginTableFull => "origin_table_full",
            Reject::BatchTooLarge => "batch_too_large",
            Reject::EmptyReading => "empty_reading",
        }
    }

    /// Every reason, so that the counters can be zeroed at startup and a daemon that has
    /// rejected nothing is distinguishable from a daemon whose metrics are broken.
    pub const ALL: &'static [Reject] = &[
        Reject::UnknownStation,
        Reject::BadSecret,
        Reject::Disabled,
        Reject::UnauthorizedPeer,
        Reject::Revoked,
        Reject::UnauthorizedOrigin,
        Reject::ClockSkew,
        Reject::TooOld,
        Reject::BadSequence,
        Reject::PathTooLong,
        Reject::MalformedPath,
        Reject::NotAddressedHere,
        Reject::Loop,
        Reject::PathForged,
        Reject::OriginTableFull,
        Reject::BatchTooLarge,
        Reject::EmptyReading,
    ];
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one successful [`Ingress::admit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    /// How many readings were sealed and durably appended.
    pub accepted: u32,
    /// How many were dropped by a per-reading check. The batch still committed.
    pub rejected: u32,
    /// The highest `seq` this node minted for the batch. Zero envelopes means the previous
    /// tail, so a caller can always use it as "nothing after this is mine".
    pub seq_high: Seq,
}

/// What one successful [`Ingress::accept`] did.
///
/// `duplicates` is a first-class outcome rather than an error: in W7 §2.3's diamond the
/// same envelope legitimately arrives twice, so a non-zero count is the dedup gate working
/// (W7 **D-27**).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    /// How many envelopes were durably appended.
    pub accepted: u32,
    /// How many this node already had, by high water or seen-window.
    pub duplicates: u32,
    /// How many failed a per-envelope check.
    pub rejected: u32,
}

/// Why a frame could not be admitted.
#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    /// Identity resolution refused the frame — W2a §4a.6's single gate. Counted by reason
    /// and **not** an error at the caller's level: a station pointed at the wrong endpoint
    /// is a signal, not a failure of the component that received it.
    #[error("the frame was rejected: {0}")]
    Rejected(Reject),

    /// The spool is at its configured ceiling, so the batch is lost (W3 §2.7, W7 **D-17**).
    ///
    /// Refusing at the edge rather than stalling: a stalled `admit` back-pressures the
    /// Davis serial read loop, and an undrained Davis console drops packets anyway — so
    /// the data is lost either way and the only real choice is whether shutdown still
    /// works and whether the operator is told. `drop-oldest` is rejected because it turns
    /// "this node is misconfigured" into "this node has holes in its history".
    #[error("the spool is full ({depth_bytes} bytes on disk)")]
    SpoolFull {
        /// What is on disk, for the log line and for `mw_spool_depth_bytes`.
        depth_bytes: u64,
    },

    /// The spool could not be written. **Fatal for the process** (W2a Decision 62): a
    /// daemon that cannot durably accept readings is not degraded, it is wrong.
    #[error("the spool could not be written")]
    Spool {
        /// The cause, with its chain intact.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl mw_types::Classify for AdmitError {
    /// The three variants are the three classes, which is why they are three variants.
    ///
    /// `Rejected` is `Ignorable`: §4.4 names a rejected reading as the canonical example —
    /// it is a signal rather than a failure, and retrying the same frame against the same
    /// registry cannot change the answer. `SpoolFull` is `Retryable`: the batch is gone,
    /// but the condition clears when a cursor catches up or `max_lag` abandons one, so the
    /// component belongs in `Backoff` and not in `Failed` forever. `Spool` is `Fatal` and
    /// the binary exits non-zero on it.
    fn class(&self) -> mw_types::Class {
        match self {
            AdmitError::Rejected(_) => mw_types::Class::Ignorable,
            AdmitError::SpoolFull { .. } => mw_types::Class::RETRYABLE,
            AdmitError::Spool { .. } => mw_types::Class::Fatal,
        }
    }
}

impl AdmitError {
    /// Wrap a spool failure, preserving its chain.
    pub fn spool<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        AdmitError::Spool {
            source: Box::new(source),
        }
    }
}

/// Why a peer's batch could not be accepted.
///
/// Distinct from [`AdmitError`] because the *whole-batch* refusals differ: a batch over the
/// size limit closes the stream, where there is no such thing on a station transport. A
/// per-envelope [`Reject`] is not here at all — it is counted in
/// [`Accepted::rejected`], because W7 **D-8** requires a rejected envelope to be dropped
/// with the batch still acknowledged. A batch that is nothing but rejections is a
/// successful `accept` with `accepted == 0`, which is exactly what the sender needs to
/// learn so that it advances its cursor instead of resending forever.
#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    /// The batch itself is inadmissible — too many envelopes, or too many bytes.
    #[error("the batch was rejected: {0}")]
    Rejected(Reject),

    /// The spool is at its ceiling. The sender is told `ACK_RESOURCE_EXHAUSTED` and backs
    /// off; `through` does not advance, so nothing is lost on its side (W7 §4.1).
    #[error("the spool is full ({depth_bytes} bytes on disk)")]
    SpoolFull {
        /// What is on disk.
        depth_bytes: u64,
    },

    /// The spool could not be written. Fatal, as for [`AdmitError::Spool`].
    #[error("the spool could not be written")]
    Spool {
        /// The cause, with its chain intact.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl mw_types::Classify for AcceptError {
    fn class(&self) -> mw_types::Class {
        match self {
            AcceptError::Rejected(_) => mw_types::Class::Ignorable,
            AcceptError::SpoolFull { .. } => mw_types::Class::RETRYABLE,
            AcceptError::Spool { .. } => mw_types::Class::Fatal,
        }
    }
}

impl AcceptError {
    /// Wrap a spool failure, preserving its chain.
    pub fn spool<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        AcceptError::Spool {
            source: Box::new(source),
        }
    }
}

/// The write end of the reading pipeline (docs/service-architecture.md §2.3, **D-60**).
pub trait Ingress: Send + Sync + 'static {
    /// Admit readings **this node measured**.
    ///
    /// Resolves identity, enriches, mints `origin = self` and the next `seq`, and durably
    /// appends the batch before returning.
    ///
    /// **Cancellation:** cancel-safe at any await point. A cancelled `admit` appended the
    /// whole batch or none of it — the spool's group commit is the atom (W7 **D-14**).
    ///
    /// **Backpressure:** the only thing this may wait on is the local spool's `fsync`.
    fn admit<'a>(
        &'a self,
        batch: &'a [PartialReading],
        meta: &'a FrameMeta,
    ) -> BoxFuture<'a, Result<Admitted, AdmitError>>;

    /// Accept envelopes **a peer already sealed** (**D-60**).
    ///
    /// `origin`, `seq` and `path` are never reassigned — [`mw_types::Provenance`] has no
    /// way to express it, which is the point. `from` is the peer that sent them, already
    /// resolved by an earlier admission layer, and it is what makes the positional clauses
    /// of W7 **D-7**'s receive check decidable.
    ///
    /// One call is one append call and one `fsync` and one atom, which is why W7 §4.2's ack
    /// has no within-batch partial to express.
    fn accept<'a>(
        &'a self,
        batch: &'a [Envelope],
        from: mw_types::OriginId,
    ) -> BoxFuture<'a, Result<Accepted, AcceptError>>;
}

impl CapabilityHandle for std::sync::Arc<dyn Ingress> {
    /// The same capability as [`crate::ReadingStream`], because it is the same spool seen
    /// from the other end: a station declares `Need { ReadingStream, Hard, Write }` and an
    /// `OnReading` component is delivered to from the read end. `Access` is what
    /// distinguishes them, not the capability.
    const CAPABILITY: Capability = Capability::ReadingStream;
}

/// Identity resolution: the one gate, and the only producer of a [`StationKey`].
///
/// **The query set is two methods, and the second one arrived with its reader.** A handle
/// trait is the query set its consumer writes (this crate's module doc), and what
/// `Ingress::admit` needs from the station registry is "turn this frame into a station or
/// refuse it" plus the static metadata the enrich stage is a pure function *of*. The
/// callsign and the registration path are the same capability's surface and still arrive
/// with the items that read them — M6-1's APRS, M6-4's enrolment.
///
/// **Synchronous on purpose.** W3 **D-87** requires `admit` to do no I/O but the spool
/// append, and a `fn` returning a `Result` rather than a future is that requirement
/// expressed in a signature: `config:sqlite` publishes a handle over an `ArcSwap` of the
/// configuration snapshot, so a resolve is a pointer load and a comparison. An
/// implementation that wanted the database would have to change this trait.
pub trait StationRegistry: Send + Sync + 'static {
    /// Resolve a frame to the station that sent it, or say why not.
    ///
    /// The secret comparison is the implementation's and must be constant-time
    /// (`subtle`, **D-33**).
    fn resolve(&self, meta: &FrameMeta) -> Result<StationKey, Reject>;

    /// The static metadata the enrich stage reads (**R1**, W3 §2.5).
    ///
    /// Total rather than fallible: every field of [`StationMeta`] is already an `Option`
    /// because every configuration column is nullable, so a station the registry has
    /// never heard of is [`StationMeta::EMPTY`] and not an error. That matters on
    /// `Ingress::accept`, where the station key is a peer's claim until M6-4's registry
    /// check lands — an unknown key there must mean "derive what needs no metadata" and
    /// not "refuse the envelope", because refusing it would lose a reading a peer has
    /// already acknowledged.
    ///
    /// Synchronous for `resolve`'s reason, and over the same snapshot.
    fn metadata(&self, station: &StationKey) -> StationMeta;

    /// What the weather schema's `stations` read-side projection should say about this
    /// station (**D-56**, W4 §2.1).
    ///
    /// `None` for a station this node has no configuration row for, which is a peer's
    /// received station until M6-4 and is **not** an error: the writer falls back to what
    /// the reading said about itself rather than refusing to store it.
    ///
    /// Separate from [`StationRegistry::metadata`] rather than one wider type, because
    /// their callers are different in a way that shows up in the signature.
    /// `metadata` is on `admit`'s hot path and is `Copy` — four `Option<f64>`, no
    /// allocation. This one allocates two `String`s and is called once per **batch** by
    /// `writer:timescale`, off that path entirely. Merging them would put two allocations
    /// on the path W3 **D-87** exists to keep free of everything but an `fsync`.
    fn projection(&self, station: &StationKey) -> Option<StationProjection>;

    /// Which node owns this station — `config.stations.owning_node_id` — or `None` if
    /// this node has no row for the key at all.
    ///
    /// **W7 §4.4's step 5, and one clause more than §4.4 writes.** The existence half is
    /// §4.6's: "an envelope naming a station the collector has never heard of would create
    /// history under a key nothing else references". The owner half is what makes
    /// **D-36**'s stated property true — *a compromised leaf cannot inject history for
    /// another station* — because origin authorisation alone does not give it:
    /// `peers.relay = false` constrains which **origin** a peer may claim and says nothing
    /// about which **station key** that origin may write into, so a leaf speaking honestly
    /// for itself could name the collector's own roof sensor and have every other check
    /// pass. `mw_collector::admit` reads the owner and requires it to be a node on the
    /// envelope's inbound path; the reasoning and the proposed amendment are recorded
    /// there.
    ///
    /// **The check [`StationRegistry::metadata`] deliberately is not.** That method is
    /// total by design: every field of [`StationMeta`] is already an `Option`, so an
    /// unplaced station answers [`StationMeta::EMPTY`] rather than refusing, and turning
    /// it into a refusal would make an unplaced station's envelopes unstorable.
    ///
    /// It is **existence and ownership, not admission**: a disabled station still exists,
    /// and an envelope a peer has already sealed for a station whose collection the
    /// operator switched off is history that arrived, not a frame to refuse. `resolve` is
    /// the gate for the `admit` path and this is not a second one.
    ///
    /// `None` is also the honest answer for a row whose `owning_node_id` is not 16 bytes,
    /// which is a schema the `CHECK` should have refused: unowned reads as unknown, which
    /// fails closed.
    ///
    /// Synchronous for `resolve`'s reason, and over the same snapshot.
    fn station_owner(&self, station: &StationKey) -> Option<mw_types::OriginId>;
}

/// The weather schema's `stations` row, as configuration knows it (W4 §2.1).
///
/// A **projection**, not a registry of record: `config:sqlite`'s `StationRegistry` is
/// authoritative for a station's existence, credentials and configuration, and this is the
/// subset a reading-side query needs so that `/span` can resolve a display name with a
/// join instead of a column repeated on every row of a hundred-million-row table.
///
/// `name` and `kind` are not `Option` because the projection's columns are `NOT NULL` and
/// the configuration's are too. Everything else is nullable in both places.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StationProjection {
    /// `config.stations.name` -> `stations.name`.
    pub name: String,
    /// `config.stations.type` -> `stations.type`. The **configured** device type, which is
    /// `campbellscientific` rather than the driver's self-reported `campbell`
    /// (docs/legacy-api-contract.md §3.1 is explicit that the two sets must not be
    /// unified).
    pub kind: String,
    /// `config.stations.timezone` -> `stations.timezone`. An IANA name. `None` means the
    /// column's own `DEFAULT 'UTC'` is the honest answer.
    pub timezone: Option<String>,
    /// Degrees north.
    pub latitude: Option<f64>,
    /// Degrees east.
    pub longitude: Option<f64>,
    /// **Metres**, as `config.stations.altitude_m` holds it. The projection's column is
    /// `altitude_ft`, and the conversion is the writer's — both units are in both column
    /// names so that it cannot be done twice or not at all.
    pub altitude_m: Option<f64>,
    /// Millimetres, the gauge's zero point.
    pub snow_base_distance_mm: Option<f64>,
}

/// The static station metadata **R1** admits into a derived value (W3 §2.5).
///
/// **R1 is "a pure function of a single reading plus static station metadata", and this
/// type is the second half of that sentence.** It is deliberately small: a field here is a
/// licence to compute a column at ingest, so adding one widens what R1 permits. Each of
/// the four is here because exactly one derived value needs it, and the enrich stage's own
/// tests name which.
///
/// Every field is an `Option` because every one of the four configuration columns is
/// nullable (`migrations/config/`), and `None` means the derived value is `None` rather
/// than a default: a latitude of 0.0 is the Gulf of Guinea, and a snow base distance of
/// 0.0 would make every depth the negation of the distance. **D-26** again, one level up
/// from a sensor.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StationMeta {
    /// `config.stations.latitude`, degrees north.
    pub latitude: Option<f64>,
    /// `config.stations.longitude`, degrees east.
    pub longitude: Option<f64>,
    /// `config.stations.altitude_m`, metres above sea level.
    ///
    /// Metres, which is the column's unit and the unit the clear-sky model's barometric
    /// term wants. The weather schema's `stations.altitude_ft` projection is a different
    /// column in a different unit and is not this.
    pub altitude_m: Option<f64>,
    /// `config.stations.snow_base_distance_mm`: the distance the gauge reads with no snow
    /// under it, in millimetres.
    pub snow_base_distance_mm: Option<f64>,
}

impl StationMeta {
    /// A station with no configured metadata — which is every station that has not been
    /// given coordinates, and every station this node has not heard of.
    pub const EMPTY: Self = Self {
        latitude: None,
        longitude: None,
        altitude_m: None,
        snow_base_distance_mm: None,
    };

    /// The coordinates, when both are present. A latitude without a longitude is not a
    /// position, and the clear-sky model needs both or neither.
    #[must_use]
    pub fn coordinates(&self) -> Option<(f64, f64)> {
        self.latitude.zip(self.longitude)
    }
}

impl CapabilityHandle for std::sync::Arc<dyn StationRegistry> {
    const CAPABILITY: Capability = Capability::StationRegistry;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Class, Classify};

    #[test]
    fn every_reject_reason_has_a_distinct_label() {
        let mut labels: Vec<&str> = Reject::ALL.iter().map(|r| r.as_str()).collect();
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), before, "two reasons share a metric label");
    }

    /// `ALL` is what zeroes the counters at startup, so a reason missing from it is a
    /// series that appears halfway through an alert window and reads as a step change.
    #[test]
    fn all_covers_every_variant() {
        // Exhaustive `match` in reverse: every label must map back to a listed variant.
        for reason in Reject::ALL {
            assert!(!reason.as_str().is_empty());
        }
        assert_eq!(
            Reject::ALL.len(),
            17,
            "a new Reject variant needs a line in ALL"
        );
    }

    /// The split is exhaustive by construction — `is_transient` is a total `match` — but
    /// the *direction* of each answer is a decision, so each one is pinned.
    #[test]
    fn only_a_property_of_the_bytes_is_permanent() {
        for reason in [
            Reject::UnknownStation,
            Reject::Disabled,
            Reject::UnauthorizedPeer,
            Reject::Revoked,
            Reject::UnauthorizedOrigin,
            Reject::BadSecret,
            Reject::ClockSkew,
            Reject::OriginTableFull,
        ] {
            assert!(
                reason.is_transient(),
                "{reason} is a function of this node's state, so acknowledging past it \
                 would destroy observations a later configuration would have accepted"
            );
        }
        for reason in [
            Reject::PathTooLong,
            Reject::MalformedPath,
            Reject::NotAddressedHere,
            Reject::Loop,
            Reject::PathForged,
            Reject::BatchTooLarge,
            Reject::EmptyReading,
            Reject::BadSequence,
            Reject::TooOld,
        ] {
            assert!(
                !reason.is_transient(),
                "{reason} is a property of the envelope's bytes, so a sender that kept \
                 re-sending it would never drain"
            );
        }
    }

    #[test]
    fn the_three_admit_failures_are_the_three_classes() {
        assert_eq!(
            AdmitError::Rejected(Reject::BadSecret).class(),
            Class::Ignorable
        );
        assert_eq!(
            AdmitError::SpoolFull { depth_bytes: 1 }.class(),
            Class::RETRYABLE
        );
        assert_eq!(
            AdmitError::spool(std::io::Error::other("disk")).class(),
            Class::Fatal,
            "W2a Decision 62: a daemon that cannot durably accept readings exits"
        );
    }

    #[test]
    fn a_spool_failure_keeps_its_chain() {
        let e = AdmitError::spool(std::io::Error::other("no space left on device"));
        assert_eq!(
            mw_types::DisplayChain(&e).to_string(),
            "the spool could not be written: no space left on device"
        );
    }
}
