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
    /// The sending node is not a peer, or has no grant for this origin (W7 §5.4).
    UnauthorizedPeer,
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
    /// The metric label and log field for this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Reject::UnknownStation => "unknown_station",
            Reject::BadSecret => "bad_secret",
            Reject::Disabled => "disabled",
            Reject::UnauthorizedPeer => "unauthorized_peer",
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
/// **The query set is deliberately one method.** A handle trait is the query set its
/// consumer writes (this crate's module doc), and what `Ingress::admit` needs from the
/// station registry is "turn this frame into a station or refuse it". Coordinates, the
/// callsign, the snow base distance and the registration path are the same capability's
/// surface and they arrive with the items that read them — M3-1's drivers, M6-1's APRS,
/// M6-4's enrolment.
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
            12,
            "a new Reject variant needs a line in ALL"
        );
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
