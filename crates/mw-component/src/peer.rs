//! The peer-facing accessor set: who may speak to this node, and for whom.
//!
//! **Not a new capability** (W7 §5.1). `PeerRegistry` is `Capability::StationRegistry`
//! seen from the collector's side — the same provider, the same published snapshot, a
//! different query set. W2a Decision 45 already settled the shape: there is no
//! `RemoteStationRegistry`, there is `StationRegistry`'s `FromRegistration` path, and a
//! peer is a *node* that has stations rather than a station. That is why registration
//! writes two kinds of row and why `Write` on this capability belongs to `ingest:*` alone:
//! registration mints station keys and nothing else may.
//!
//! **A separate trait for the same capability, and that is deliberate.** This crate's
//! doctrine is that a handle trait is the query set its *consumer* writes
//! ([`crate::StationRegistry`]'s own documentation). `Ingress::admit` asks "turn this frame
//! into a station"; the collector asks "is this node a peer, what may it speak for, and is
//! it still trusted". Widening `StationRegistry` with three peer methods would put them on
//! `admit`'s hot-path trait and on the four test doubles that implement it, for a question
//! none of them has. Same capability, same object, two accessor sets.
//!
//! # The three layers this type is layer 3 of (W7 **D-34**)
//!
//! | Layer | Where | What it establishes |
//! |---|---|---|
//! | 1 | the TLS handshake | this connection presents material that matches a `peers` row |
//! | 2 | a per-RPC check, before any method body runs | that row is resolvable and not revoked |
//! | 3 | per envelope | the peer may speak **for that origin** |
//!
//! [`PeerEntry::may_speak_for`] is layer 3 and nothing else. `relay = false` is the default
//! (W7 **D-36**), so a peer may speak only for itself and relaying is an explicit per-peer
//! grant — which is why a compromised leaf cannot inject history for another station, a
//! property the Go design cannot express because it has no notion of who may say what.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use mw_types::{OriginId, StationKey};
use secrecy::SecretString;

use crate::body::BoxFuture;
use crate::capability::Capability;
use crate::cx::CapabilityHandle;

/// The operator-facing name of a peer — `peers.name`.
///
/// The `peer` label on every metric in W7 §7's collector table, which is what makes it a
/// newtype rather than a `String`: §5.10's cardinality rule requires a label whose values
/// come from configuration and not from a request, and a name is the configured one where
/// a `NodeId` is whatever the connection claimed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerName(Box<str>);

impl PeerName {
    /// Wrap a name read from `peers.name`.
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self(name.into())
    }

    /// The name as text, for a log field or a metric label.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a peer proves it is itself — `peers.auth_kind` and `peers.auth_material`.
///
/// Three kinds because the operator's situation differs, not because the protocol does:
/// under a shared CA the chain authenticates and the row only says who is *allowed*; an
/// operator who does not want to run a CA pins an SPKI; and a PSK exists for a peer that
/// cannot hold a certificate at all. **TLS is required in all three** (W7 **D-35**): a PSK
/// on a plaintext socket is a credential in the clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAuth {
    /// Chain validation against the collector's peer CA. No material on the row, and that
    /// is the common case rather than an exception — the certificate does the
    /// authenticating.
    MtlsCa,
    /// The presented certificate's SubjectPublicKeyInfo must hash to exactly this.
    ///
    /// SHA-256 of the **SPKI**, not of the certificate: an SPKI pin survives a certificate
    /// renewal that keeps the keypair, which is the renewal an operator with no CA actually
    /// performs.
    MtlsPinned {
        /// The pinned digest, as 32 raw bytes.
        spki_sha256: [u8; 32],
    },
    /// An Argon2id PHC string, compared against a secret the peer presents in gRPC
    /// metadata or `Authorization: Bearer` (W7 §5.4).
    Psk {
        /// The stored hash. Never the secret: §5.4 authenticates a peer, it does not need
        /// to reproduce its credential.
        phc: Box<str>,
    },
}

impl PeerAuth {
    /// The `peers.auth_kind` spelling, for a log field and for `EnrolGrant`.
    pub const fn kind(&self) -> &'static str {
        match self {
            PeerAuth::MtlsCa => "mtls-ca",
            PeerAuth::MtlsPinned { .. } => "mtls-pinned",
            PeerAuth::Psk { .. } => "psk",
        }
    }
}

/// One `peers` row, joined to its `peer_origins`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEntry {
    /// `peers.node_id` — the peer's own `NodeId`.
    pub node: OriginId,
    /// `peers.name`.
    pub name: PeerName,
    /// How it authenticates.
    pub auth: PeerAuth,
    /// `peers.relay`. **False by default** (W7 **D-36**).
    pub relay: bool,
    /// `peer_origins.origin_node_id` — which origins this peer may deliver.
    ///
    /// Read only when [`PeerEntry::relay`] is set: with `relay = false` the one legal
    /// origin is the peer itself, and §8.5's `PlanError::RelayNotGranted` is what stops a
    /// row from carrying a list it cannot use.
    pub origins: BTreeSet<OriginId>,
    /// `peers.revoked_at`. Revocation is a timestamp rather than a deletion, because "this
    /// peer was trusted until Tuesday" is what an operator needs to read off the fleet
    /// page.
    pub revoked_at: Option<DateTime<Utc>>,
    /// `peers.software` — `NodeIdentity.software` as last seen.
    ///
    /// The `version` label of `peer_software_info`, which is what W7 OQ-W7-6 gates removal
    /// of the Go compat shim on.
    pub software: Option<String>,
}

impl PeerEntry {
    /// Whether this peer is still trusted.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// **Layer 3**: whether this peer may deliver an envelope that claims `origin`.
    ///
    /// ```text
    /// relay = false (the default):  origin must be the peer itself
    /// relay = true:                 origin must be listed in peer_origins
    /// ```
    ///
    /// The path's own clauses are not re-checked here, because they are already proved
    /// elsewhere and proving them twice would be two places to keep in step: `path[0] ==
    /// origin` is enforced by [`mw_types::Provenance::sealed`] at the decode, and
    /// "`path[len - 2]` is the sending peer" by the receive check. Together with
    /// `relay = false` those give `path == [peer, self]` exactly — §5.4's wording — without
    /// this method having to know what a path is.
    pub fn may_speak_for(&self, origin: OriginId) -> bool {
        if origin == self.node {
            // A peer may always speak for itself, with or without the grant, which is what
            // makes §8.5's `PeerCannotSpeakForItself` a plan-time tidiness check rather
            // than something admission depends on.
            return true;
        }
        self.relay && self.origins.contains(&origin)
    }
}

/// One station a peer says it owns — `NodeIdentity.stations`, in the domain's vocabulary.
///
/// Not the wire message: this crate may not name `mw-proto`'s types (it does not depend on
/// it), and the conversion happening in `mw-collector` is what keeps the registration path
/// a function of a *decoded* claim rather than of a protobuf struct. Every field is what
/// §5.2's `StationMeta` carries, and the four plaintext secrets of
/// `RemoteStationConfig` are absent here for the same reason they are absent there.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerStation {
    /// The key the peer resolved at its own end (W2a §4a.6). There is no envelope without
    /// one.
    pub station: StationKey,
    /// Operator-facing name.
    pub name: String,
    /// The peer's `station_type`, stored as given.
    ///
    /// **Not validated against this build's driver list**, which is W7 §5.2's point: a
    /// newer peer running a driver this collector has never heard of must still be able to
    /// register, or an upgrade order becomes a fleet-wide enrolment failure.
    pub kind: String,
    /// Degrees north.
    pub latitude: Option<f64>,
    /// Degrees east.
    pub longitude: Option<f64>,
    /// Metres.
    pub altitude_m: Option<f64>,
    /// An IANA zone name (W2a Decision 11).
    pub timezone: Option<String>,
    /// Public identity, never a credential (W7 **D-39**).
    pub aprs_callsign: Option<String>,
}

/// Why the registration path refused.
#[derive(Debug, thiserror::Error)]
pub enum PeerWriteError {
    /// The configuration database refused or could not be reached.
    ///
    /// Carried as text rather than as a typed source: this crate may not name `sqlx`, and
    /// the implementation's own error type is already classified beside its definition.
    #[error("the peer registry could not be written: {reason}")]
    Backend {
        /// The flattened chain from the implementation.
        reason: String,
        /// How the implementation classified it, which is what decides whether the caller
        /// retries.
        class: mw_types::Class,
    },

    /// No unexpired, unconsumed enrolment token exists, so `Enroll` is **closed**.
    ///
    /// Distinguishable from "wrong address" and "bad token" on purpose (W7 **D-33**): an
    /// operator debugging a join needs to tell those three apart.
    #[error("enrolment is closed: no unexpired enrolment token exists on this node")]
    EnrolmentClosed,

    /// The presented token did not match any open token.
    #[error("the enrolment token was not accepted")]
    BadToken,

    /// The request carried nothing to pin and no PSK to hash.
    #[error("the enrolment request carries no key material to record")]
    NoMaterial,

    /// Registering these stations would take one node past the number it may own.
    ///
    /// A bound rather than a truncation: `NodeIdentity.stations` is a repeated field and a
    /// `Hello` can be re-sent, so without a limit a peer inventing a fresh key per
    /// connection grows a table and a resident snapshot for ever. Refused whole, because
    /// registering some of a satellite's stations and silently refusing envelopes for the
    /// rest is the worse failure.
    #[error("this node already owns {owned} stations, and the limit is {limit}")]
    TooManyStations {
        /// How many it owns now.
        owned: i64,
        /// The limit.
        limit: i64,
    },

    /// `auth_kind = 'mtls-ca'` asks this node to **sign** a certificate.
    ///
    /// Refused by name rather than half-implemented. §5.3 offers two outcomes — sign the
    /// CSR with the collector's peer CA, or record the SPKI fingerprint — and only the
    /// second is expressible with the dependency set `DESIGN.md` **D-32** fixes: there is
    /// no X.509 issuer in it and adding one is an amendment, not a manifest edit. §5.3's
    /// own last paragraph is the supported path for a CA fleet: "a node may also be
    /// enrolled entirely out of band — the operator copies a certificate and inserts the
    /// `peers` row".
    #[error(
        "this node cannot issue a certificate: enrolment under a shared CA is an \
         out-of-band operation (docs/collector.md §5.3), so insert the `peers` row and \
         hand the peer a certificate your CA signed. `mtls-pinned` and `psk` enrol here."
    )]
    CannotIssueCertificates,
}

impl mw_types::Classify for PeerWriteError {
    fn class(&self) -> mw_types::Class {
        match self {
            PeerWriteError::Backend { class, .. } => *class,
            // Every other variant is the request's fault and re-sending the same bytes
            // cannot change the answer.
            _ => mw_types::Class::Ignorable,
        }
    }
}

/// What one accepted enrolment recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// The row as it now stands.
    pub peer: PeerEntry,
    /// The SPKI digest recorded, when one was.
    pub pinned_spki_sha256: Option<[u8; 32]>,
}

/// What a peer presents in order to be enrolled.
#[derive(Debug)]
pub struct Enrolment {
    /// The operator-minted single-use token. A [`SecretString`] (**D-33**), so it is
    /// redacted in every `Debug` on the way to a constant-time comparison.
    pub token: SecretString,
    /// The peer's own `NodeId`.
    pub node: OriginId,
    /// `NodeIdentity.node_name`, which becomes `peers.name`.
    pub name: String,
    /// `NodeIdentity.software`.
    pub software: String,
    /// SHA-256 of the DER `SubjectPublicKeyInfo` the peer generated.
    ///
    /// **Already hashed, by the caller.** The digest has to be computed where a hash
    /// implementation exists — `mw-collector` has one, because `rustls`'s `ring` provider
    /// is already in the binary for TLS — and a registry that took the raw SPKI would need
    /// a second digest crate for one function.
    pub spki_sha256: [u8; 32],
    /// The stations it declares, written by the same transaction.
    pub stations: Vec<PeerStation>,
}

/// The peer registry: `Capability::StationRegistry`, as the collector queries it.
///
/// **Reads are synchronous and writes are not**, and the split is the same one
/// [`crate::StationRegistry`] makes for the same reason. A read is on the admission path,
/// which W3 **D-87** allows no I/O on but the spool append, so it is a pointer load over a
/// published snapshot and the signature is what says so. A write happens once per
/// connection (`Hello`) or once per peer in the life of a fleet (`Enroll`), is a
/// transaction, and must be awaited.
pub trait PeerRegistry: Send + Sync + 'static {
    /// The peer this `NodeId` names, revoked or not.
    ///
    /// **Revoked rows are returned rather than hidden**, because layer 2 has to tell
    /// `Reject::Revoked` from `Reject::UnauthorizedPeer`: the first needs an operator who
    /// knows they revoked it, the second means someone is pointing an unknown node at this
    /// port. A registry that filtered revoked rows would make those one answer.
    fn peer(&self, node: OriginId) -> Option<PeerEntry>;

    /// The peer whose pinned SPKI digest is this, for layer 1's `mtls-pinned` verifier.
    ///
    /// Keyed by the digest rather than by the node id because at the handshake there is no
    /// node id yet — the certificate is all there is, and refusing an unknown key there is
    /// what "refused at the TLS layer, before any frame is decoded" means.
    fn peer_by_spki(&self, spki_sha256: &[u8; 32]) -> Option<PeerEntry>;

    /// Every peer, for the startup log line and for `peer_software_info`.
    fn peers(&self) -> Vec<PeerEntry>;

    /// Record a `Hello`: `peers.last_seen`, `peers.software`, and the peer's stations.
    ///
    /// **This is the registration path** (W7 §5.1, W2a Decision 45) and it is why
    /// `ingest:*` holds `Write`: without it a satellite's station keys never reach
    /// `config.stations`, so every envelope it sends fails §4.4's step 5 and a fleet that
    /// authenticates perfectly stores nothing. Identity only — the four plaintext secrets
    /// that used to ride along have no successor (§5.2, **D-67**).
    fn record_hello<'a>(
        &'a self,
        node: OriginId,
        software: &'a str,
        stations: &'a [PeerStation],
    ) -> BoxFuture<'a, Result<(), PeerWriteError>>;

    /// Whether any unexpired, unconsumed enrolment token exists.
    ///
    /// Asynchronous unlike the other reads: a token's expiry is a *clock* fact against a
    /// table that changes between snapshots, and answering it from a published snapshot
    /// would mean `Enroll` stayed open for as long as the snapshot was stale.
    fn enrolment_open(&self) -> BoxFuture<'_, Result<bool, PeerWriteError>>;

    /// Verify a token, burn it, and insert the `peers`, `peer_origins` and
    /// `config.stations` rows — in one transaction (W7 §5.3).
    fn enrol<'a>(
        &'a self,
        request: &'a Enrolment,
    ) -> BoxFuture<'a, Result<Enrolled, PeerWriteError>>;
}

impl CapabilityHandle for std::sync::Arc<dyn PeerRegistry> {
    /// The same capability the station registry is, because it is the same provider seen
    /// from the collector's side (W7 §5.1). `Access::Write` is what distinguishes the
    /// collector's need from a reader's, exactly as it does for
    /// [`crate::Capability::ReadingStream`].
    const CAPABILITY: Capability = Capability::StationRegistry;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(b: u8) -> OriginId {
        OriginId::new([b; 16])
    }

    fn entry(relay: bool, origins: &[u8]) -> PeerEntry {
        PeerEntry {
            node: node(1),
            name: PeerName::new("satellite-a"),
            auth: PeerAuth::MtlsCa,
            relay,
            origins: origins.iter().copied().map(node).collect(),
            revoked_at: None,
            software: None,
        }
    }

    /// W7 **D-36**: the default is that a peer speaks only for itself, and a grant it was
    /// not given does not become one by being listed.
    #[test]
    fn without_relay_a_peer_speaks_only_for_itself() {
        let peer = entry(false, &[2, 3]);
        assert!(peer.may_speak_for(node(1)));
        assert!(
            !peer.may_speak_for(node(2)),
            "`relay = false` means the origins list is not a grant"
        );
    }

    #[test]
    fn with_relay_a_peer_speaks_for_its_granted_origins_and_no_others() {
        let peer = entry(true, &[2, 3]);
        assert!(peer.may_speak_for(node(1)), "itself, always");
        assert!(peer.may_speak_for(node(2)));
        assert!(peer.may_speak_for(node(3)));
        assert!(!peer.may_speak_for(node(4)), "not granted");
    }

    #[test]
    fn revocation_is_a_timestamp_and_not_a_deletion() {
        let mut peer = entry(false, &[]);
        assert!(!peer.is_revoked());
        peer.revoked_at = Some(DateTime::from_timestamp_millis(1).expect("an instant"));
        assert!(peer.is_revoked());
        assert!(
            peer.may_speak_for(node(1)),
            "layer 2 refuses a revoked peer; layer 3 answers a different question and \
             must not silently also answer that one"
        );
    }

    /// The PHC never reaches a `Display`, and the kind is what a log line gets.
    #[test]
    fn the_auth_kind_spells_the_column() {
        assert_eq!(PeerAuth::MtlsCa.kind(), "mtls-ca");
        assert_eq!(
            PeerAuth::MtlsPinned {
                spki_sha256: [0; 32]
            }
            .kind(),
            "mtls-pinned"
        );
        assert_eq!(
            PeerAuth::Psk {
                phc: "$argon2id$v=19$m=65536,t=3,p=4$c2FsdA$aGFzaA".into()
            }
            .kind(),
            "psk"
        );
    }
}
