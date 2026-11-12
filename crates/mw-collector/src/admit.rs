//! **Layers 2 and 3**, and §4.6's boundary validation: who is speaking, and whether what
//! they said is storable.
//!
//! > **The gate is W2a Decision 46's single authentication point.** An unknown peer is
//! > **rejected**, not stored with a debug log.
//!
//! The Go receiver pushes the reading into the distributor at `station.go:175` and consults
//! the registry at `:182`, with an unknown sender getting a `Debugf`. Everything here runs
//! before `Ingress::accept` is called, and `accept` is the only thing that can append to
//! the spool — so an envelope that fails any check below never reaches the spool, and
//! therefore never reaches the reading stream, the downstream peers, the snapshot or the
//! database. That ordering is the structural form of W7 §4.4's "steps 2–7 run before step
//! 9".
//!
//! # Where each of §4.4's eleven steps actually runs
//!
//! | Step | Where | Why there |
//! |---|---|---|
//! | 0 the peer is resolved | [`resolve`] | layer 2: the stream would not exist otherwise |
//! | 1 batch bounds | `mw_pipeline::ingress` | one limit, enforced where the append is |
//! | 2 the six-clause receive check | `mw_pipeline::ingress::receive_check` + `Provenance::sealed` | three clauses are a property of the decode |
//! | **3 `origin ∈ peer.origins`** | [`Gate::check`] | needs the peer registry |
//! | **4 the clock-skew window** | [`Gate::check`] | needs a clock |
//! | **5 `station_key` resolves** | [`Gate::check`] | needs the station registry, **and the owner** |
//! | 6–7 interning and dedup | `mw_pipeline::spool` | inside the append's critical section |
//! | 8 enrich | `mw_pipeline::ingress` | idempotent, `None`-only |
//! | 9 the append | `mw_pipeline::spool` | the atom |
//! | 10 notifications | `mw_pipeline::spool` | after the commit, never before |
//! | 11 the ack | [`crate::service`] | the sender's coordinate space |
//!
//! Steps 3, 4 and 5 are the three `mw_pipeline::ingress`'s module documentation records as
//! this item's, and they are **here** rather than inside `accept` for the reason the split
//! is there at all: `accept` holds the spool and the station registry and has no idea what
//! a peer is, and giving it one would put the peer registry on `admit`'s hot-path type.
//! What `accept` keeps is everything that must be inside one critical section.
//!
//! # Step 5 is one clause wider than §4.4 writes it
//!
//! §4.4 step 5 is "`station_key` resolves in `StationRegistry`". That is necessary and it is
//! not sufficient, and the gap is worth stating because **D-36**'s whole point falls into
//! it: `relay = false` constrains which **origin** a peer may claim and says nothing about
//! which **station key** that origin may write into. A satellite speaking honestly for
//! itself can name the collector's own roof sensor; every other check passes, and the
//! envelope lands in that station's history.
//!
//! So the check is: **the station's owner must be the envelope's origin or the sending
//! peer.** That is exactly as permissive as the topologies §8 documents and no more:
//!
//! ```text
//! leaf    path [A, me]         owned by A                   ✓ owner == origin == peer
//! relay   path [B, R, me]      owned by B, the measurer      ✓ owner == origin
//!                              owned by R, its inventory     ✓ owner == peer
//! attack  path [A, me]         owned by me                   ✗ neither
//! attack  path [B, X, R, me]   owned by X                    ✗ neither
//! ```
//!
//! **Two nodes and not "anywhere on the path", which is where the first version was
//! wrong.** A path's interior hops are chosen by whoever built it — nothing constrains them
//! beyond distinctness, the length bound, `me` not being among them, `path[0] == origin`
//! and `path[len - 2] == peer` — so "the owner is on the inbound path" was satisfiable by
//! inventing a hop, and a relay-granted peer could reach any station it liked. The two
//! nodes this clause does accept are the two the other checks have already proved:
//! `origin` is `path[0]`, which `Provenance::sealed` enforces and `may_speak_for` has
//! authorised, and `peer` is `path[len - 2]`, which the receive check enforced.
//!
//! Filed as a proposed amendment to §4.4 in this item's PR rather than taken silently.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use mw_component::{PeerEntry, Reject, StationRegistry};
use mw_types::{Envelope, OriginId, Seq};

/// How far ahead of this node's clock an `observed_at` may be (§4.6).
///
/// Five minutes. A satellite with a wrong clock writes rows into the future, which breaks
/// `/latest` (newest-first) and every window computation on the collector. Nothing checks
/// this today.
pub const MAX_FUTURE_SKEW_SECONDS: i64 = 5 * 60;

/// How far behind it may be (§4.6).
///
/// Thirty days, chosen **above** both the seven-day `max_lag` and the ~12-day worst-case
/// ceiling, so that a legitimate replay after a long outage is never rejected. A satellite
/// that has been offline for a fortnight must be able to drain.
pub const MAX_PAST_SKEW_SECONDS: i64 = 30 * 24 * 60 * 60;

/// [`MAX_FUTURE_SKEW_SECONDS`] as a [`Duration`].
///
/// A function rather than a `const`, because `chrono::TimeDelta`'s constructors are not
/// `const fn` and a `OnceLock` for two multiplications would be worse than a call.
pub fn max_future_skew() -> Duration {
    Duration::seconds(MAX_FUTURE_SKEW_SECONDS)
}

/// [`MAX_PAST_SKEW_SECONDS`] as a [`Duration`].
pub fn max_past_skew() -> Duration {
    Duration::seconds(MAX_PAST_SKEW_SECONDS)
}

/// What layer 1 established about a connection, before any method body runs.
///
/// Three shapes because there are three `auth_kind`s, and the enum is what makes "this
/// connection presented nothing" a case the type system forces a caller to handle rather
/// than an `Option` somebody unwraps.
#[derive(Debug, Clone)]
pub enum Presented {
    /// A chain the peer CA validated, carrying a SAN URI that names a node.
    CaValidated {
        /// The node the certificate names.
        node: OriginId,
    },
    /// A certificate whose SPKI digest matched a `peers` row at the handshake.
    Pinned {
        /// The node that row names.
        node: OriginId,
    },
    /// A pre-shared secret, presented in gRPC metadata or `Authorization: Bearer`.
    ///
    /// Carries the *claimed* node, because a PSK says nothing about identity on its own:
    /// the claim comes from the request and the secret is what makes it true.
    Psk {
        /// The node the request claims to be.
        node: OriginId,
        /// The secret, as presented.
        secret: secrecy::SecretString,
    },
}

impl Presented {
    /// The node this connection claims, or has been proved, to be.
    pub fn node(&self) -> OriginId {
        match self {
            Presented::CaValidated { node }
            | Presented::Pinned { node }
            | Presented::Psk { node, .. } => *node,
        }
    }

    /// How a log line should describe it.
    pub const fn kind(&self) -> &'static str {
        match self {
            Presented::CaValidated { .. } => "mtls-ca",
            Presented::Pinned { .. } => "mtls-pinned",
            Presented::Psk { .. } => "psk",
        }
    }
}

/// **Layer 2**: resolve a connection to a peer, before any method body runs.
///
/// An unresolved or revoked peer gets `PERMISSION_DENIED` and **no stream is created**, so
/// no `IngestBatch` is ever read. That is the caller's job; what this returns is the
/// refusal's reason, counted by [`crate::metrics::rejected`] once a peer name exists and
/// by the caller's own log line when one does not.
///
/// **The credential is re-checked against the row even for the two mTLS kinds**, and that is
/// not belt-and-braces: layer 1 refuses an unknown *key*, and this refuses a key that is
/// known but is not the key that row configured. Without it, a peer enrolled as `psk` could
/// present any certificate the peer CA had ever signed and be admitted as itself.
pub fn resolve(
    peers: &dyn mw_component::PeerRegistry,
    presented: &Presented,
) -> Result<PeerEntry, Reject> {
    let peer = peers
        .peer(presented.node())
        .ok_or(Reject::UnauthorizedPeer)?;
    if peer.is_revoked() {
        return Err(Reject::Revoked);
    }
    match (&peer.auth, presented) {
        (mw_component::PeerAuth::MtlsCa, Presented::CaValidated { .. }) => Ok(peer),
        (mw_component::PeerAuth::MtlsPinned { .. }, Presented::Pinned { .. }) => Ok(peer),
        (mw_component::PeerAuth::Psk { phc }, Presented::Psk { secret, .. }) => {
            if verify_psk(phc, secret) {
                Ok(peer)
            } else {
                Err(Reject::BadSecret)
            }
        }
        // The row and the connection disagree about how this peer authenticates. Refused as
        // `UnauthorizedPeer` rather than `BadSecret`, because no secret was wrong: what is
        // wrong is that the peer arrived by a door its row does not open.
        _ => Err(Reject::UnauthorizedPeer),
    }
}

/// Argon2id over the presented secret, compared against `peers.auth_material`.
///
/// Constant-time inside `verify_password` (`password-hash`'s own comparison, **D-33**). It
/// is deliberately **not** cached the way `mw_store::registry` caches a station's
/// `?PASSWORD=`: that cache exists because an Ambient console presents the same secret
/// every minute on `admit`'s hot path, and a PSK is presented **once per connection**, so
/// the verification is amortised over a stream that lasts as long as the link does.
fn verify_psk(phc: &str, secret: &secrecy::SecretString) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    use secrecy::ExposeSecret;
    let Ok(parsed) = PasswordHash::new(phc) else {
        tracing::warn!(
            "a `peers` row's PSK hash is not a PHC string this build can parse, so that \
             peer cannot authenticate"
        );
        return false;
    };
    Argon2::default()
        .verify_password(secret.expose_secret().as_bytes(), &parsed)
        .is_ok()
}

/// **Layer 3** and §4.6: the per-envelope checks that precede the spool.
///
/// Holds the station registry and the node's own identity, both resolved once. It holds no
/// clock: `now` is an argument, because `max_past_skew` is thirty days and a test that
/// cannot move the clock cannot assert either edge of the window.
#[derive(Clone)]
pub struct Gate {
    registry: Arc<dyn StationRegistry>,
    me: OriginId,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").field("me", &self.me).finish()
    }
}

impl Gate {
    /// A gate over one station registry, for one node.
    pub fn new(registry: Arc<dyn StationRegistry>, me: OriginId) -> Self {
        Self { registry, me }
    }

    /// This node's own `NodeId`.
    pub fn node(&self) -> OriginId {
        self.me
    }

    /// Which node owns a station, for `HelloAck.accepted_stations`.
    ///
    /// The same read [`Gate::check`] makes, exposed so that "which of your stations did I
    /// register" is answered from the registry rather than from what the registration path
    /// was asked to write — a peer naming a key it does not own gets a `Hello` that says so
    /// instead of a refusal per envelope.
    pub fn station_owner(&self, station: &mw_types::StationKey) -> Option<OriginId> {
        self.registry.station_owner(station)
    }

    /// Steps 3, 4 and 5 for one envelope, plus §4.6's other two. `Ok(())` means it may be
    /// handed to `accept`.
    ///
    /// Ordered cheapest-first among the ones that cost anything, and with the two that
    /// cannot fail together adjacent: the origin grant is a set membership, the skew window
    /// is two comparisons, and the station lookup is a map read over a published snapshot.
    /// None of them does I/O, which is what lets this run inside the same `await`-free
    /// stretch `accept` does.
    ///
    /// # The two §4.6 clauses that are **not** step 3, 4 or 5
    ///
    /// §4.6's table has four rows; two of them are the skew window and the station lookup
    /// below, and the other two are here because nothing else on the `accept` path performs
    /// them. `admit` has both already (`mw_pipeline::ingress`), and `accept` deliberately
    /// has neither, because `admit`'s versions are about a reading this node measured:
    ///
    /// - **`seq != 0`.** `mw_types::Seq::FIRST` is 1, so a zero is an encoder that omitted
    ///   the field. It is refused here because the dedup gate answers `Duplicate` for it
    ///   (`mw_pipeline::spool::dedup`'s `accept`) — which would be reported to the sender as
    ///   `ACK_OK` with a duplicate count, and a peer whose encoder has that bug would
    ///   advance its cursor over everything it ever sent while both ends logged success.
    /// - **at least one populated metric.** §4.6 calls an all-`None` reading "a row the
    ///   aggregates must still process", which understates it: W3's writer upserts
    ///   `col = EXCLUDED.col` (`mw_store::writer`, asserted there), **not** `coalesce`, so
    ///   an empty reading at a `(station_id, time)` that already holds a real observation
    ///   **erases its values**. That makes this clause a destructive-write guard rather
    ///   than a tidiness one, which is why it is a refusal here and not a filter later.
    pub fn check(
        &self,
        envelope: &Envelope,
        peer: &PeerEntry,
        now: DateTime<Utc>,
    ) -> Result<(), Reject> {
        let origin = envelope.provenance.origin();
        // 3. `origin ∈ peer.origins` (§5.4 layer 3). `relay = false` is the default, so the
        //    ordinary answer for a leaf is "the origin is the peer".
        if !peer.may_speak_for(origin) {
            return Err(Reject::UnauthorizedOrigin);
        }
        if envelope.provenance.seq() == Seq::new(0) {
            return Err(Reject::BadSequence);
        }
        if !carries_a_metric(&envelope.reading) {
            return Err(Reject::EmptyReading);
        }
        // 4. The skew window (§4.6). `observed_at`, not `received_at`: the second is the
        //    origin's own admission instant and is bounded by the first in every honest
        //    encoder, while the first is what every window computation and `/latest` reads.
        // **Two edges, two verdicts**, because their permanence is opposite: a collector
        // whose own clock is fast clears by being corrected, and an envelope that is
        // already thirty days old only ever gets older (`Reject::is_transient`).
        if envelope.observed_at > now + max_future_skew() {
            return Err(Reject::ClockSkew);
        }
        if envelope.observed_at < now - max_past_skew() {
            return Err(Reject::TooOld);
        }
        // 5. The station resolves, **and its owner is on the inbound path** (see this
        //    module's documentation for why the second clause is here).
        let Some(owner) = self.registry.station_owner(&envelope.station) else {
            return Err(Reject::UnknownStation);
        };
        // **The owner must be the origin or the sending peer, and nothing else.**
        //
        // The first version accepted any node on the inbound path, and that was a bypass: a
        // path's *interior* hops are chosen by whoever built it. Nothing constrains them
        // beyond distinctness, the length bound, `me` not being among them,
        // `path[0] == origin` and `path[len - 2] == peer` -- so a relay-granted peer could
        // pad the path with the owner of any station it wanted and walk straight through.
        // `[2, 3, 7, 9]` from relay 7, granted origins `{7, 2}`, writes into node 3's
        // station.
        //
        // Two nodes, because §8's topologies need exactly two -- and both are already
        // proved rather than claimed: `origin` is `path[0]`, which `Provenance::sealed`
        // enforces at the decode and `may_speak_for` has just authorised, and `peer` is
        // `path[len - 2]`, which the receive check enforced. An interior hop is neither.
        if owner != origin && owner != peer.node {
            return Err(Reject::UnauthorizedOrigin);
        }
        Ok(())
    }
}

/// Whether a reading carries anything at all (§4.6's last row).
///
/// The same function `mw_pipeline::ingress` uses on the `admit` path, and written the same
/// way for the same reason: comparing against a `Reading` with the same two dimensions and
/// no metrics makes it **registry-proof**. A metric added to `metrics/registry.toml` is
/// covered without an edit here, and there is no list to fall out of date.
///
/// The obvious implementation — walk `METRICS` asking `Reading::get` — is wrong and subtly
/// so: `get` answers `None` for the fifteen non-numeric metrics, so an envelope whose only
/// content is one of them would read as empty and be refused.
///
/// Not shared with `mw-pipeline` by calling it: that crate's copy is private to its own
/// module, and exporting it to be called from one place here would make a hot-path function
/// part of its public surface. The generated type derives `PartialEq`, so each copy is
/// three lines and neither can drift from the registry.
fn carries_a_metric(reading: &mw_types::Reading) -> bool {
    let empty = mw_types::Reading {
        time: reading.time,
        station_id: reading.station_id.clone(),
        ..mw_types::Reading::default()
    };
    *reading != empty
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{envelope, station_registry, FakePeers};
    use mw_component::{PeerAuth, PeerName};

    fn node(b: u8) -> OriginId {
        OriginId::new([b; 16])
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_762_732_800_000).expect("an instant")
    }

    fn leaf_peer() -> PeerEntry {
        PeerEntry {
            node: node(1),
            name: PeerName::new("satellite-a"),
            auth: PeerAuth::MtlsCa,
            relay: false,
            origins: [node(1)].into_iter().collect(),
            revoked_at: None,
            software: None,
        }
    }

    /// `me` is node 9 throughout: the collector.
    fn gate(stations: &[(&str, u8)]) -> Gate {
        Gate::new(station_registry(stations), node(9))
    }

    #[tokio::test]
    async fn a_leaf_s_own_envelope_passes_every_check() {
        let gate = gate(&[("satellite-a:backyard", 1)]);
        let env = envelope("satellite-a:backyard", node(1), 1, &[1, 9], now());
        assert_eq!(gate.check(&env, &leaf_peer(), now()), Ok(()));
    }

    /// **D-36**, as a test rather than as a claim: a leaf speaking honestly for itself
    /// cannot write into the collector's own station.
    #[tokio::test]
    async fn a_leaf_cannot_inject_history_into_a_station_the_collector_owns() {
        let gate = gate(&[("suncrest:roof", 9)]);
        let env = envelope("suncrest:roof", node(1), 1, &[1, 9], now());
        assert_eq!(
            gate.check(&env, &leaf_peer(), now()),
            Err(Reject::UnauthorizedOrigin),
            "the origin grant passes and the station's owner is this node, which is never \
             an inbound hop"
        );
    }

    /// …nor into a third node's station, even one it is a legitimate peer of.
    #[tokio::test]
    async fn a_leaf_cannot_write_into_another_peer_s_station() {
        let gate = gate(&[("satellite-b:barn", 2)]);
        let env = envelope("satellite-b:barn", node(1), 1, &[1, 9], now());
        assert_eq!(
            gate.check(&env, &leaf_peer(), now()),
            Err(Reject::UnauthorizedOrigin)
        );
    }

    /// **The padding attack the first version of the ownership clause admitted.**
    ///
    /// A relay chooses its own path's interior hops, so "the owner is somewhere on the
    /// inbound path" was satisfiable by inventing a hop. Here relay 7 is granted origins
    /// `{7, 2}` and tries to forward a station owned by node 3 -- which it has no grant for
    /// -- by putting 3 on the path.
    #[tokio::test]
    async fn a_relay_cannot_reach_a_third_partys_station_by_padding_the_path() {
        let gate = gate(&[("satellite-c:shed", 3)]);
        let relay = PeerEntry {
            node: node(7),
            name: PeerName::new("relay-west"),
            auth: PeerAuth::MtlsCa,
            relay: true,
            origins: [node(7), node(2)].into_iter().collect(),
            revoked_at: None,
            software: None,
        };
        // `path[0] == origin == 2` (granted), `path[len - 2] == 7` (the sending peer), and
        // node 3 sits between them -- so every other clause passes.
        let env = envelope("satellite-c:shed", node(2), 4711, &[2, 3, 7, 9], now());
        assert_eq!(
            mw_pipeline::receive_check(env.provenance.path(), node(9), node(7)),
            Ok(()),
            "the receive check has nothing to say about an interior hop, which is why the \
             owner clause cannot lean on one"
        );
        assert_eq!(
            gate.check(&env, &relay, now()),
            Err(Reject::UnauthorizedOrigin)
        );
    }

    /// A relay's forward is the case the ownership clause must not break: the station is
    /// owned by the relay (which registered it as inventory) and the origin is two hops
    /// back.
    #[tokio::test]
    async fn a_relay_may_forward_a_station_it_registered_for_a_node_behind_it() {
        let gate = gate(&[("satellite-b:barn", 7)]);
        let relay = PeerEntry {
            node: node(7),
            name: PeerName::new("relay-west"),
            auth: PeerAuth::MtlsCa,
            relay: true,
            origins: [node(7), node(2)].into_iter().collect(),
            revoked_at: None,
            software: None,
        };
        let env = envelope("satellite-b:barn", node(2), 4711, &[2, 7, 9], now());
        assert_eq!(gate.check(&env, &relay, now()), Ok(()));
    }

    #[tokio::test]
    async fn an_ungranted_origin_is_refused_before_the_station_is_looked_up() {
        // No station rows at all, so an envelope that got as far as step 5 would be
        // `UnknownStation`. It must be `UnauthorizedOrigin`, which is what says step 3 ran
        // first.
        let gate = gate(&[]);
        let env = envelope("satellite-b:barn", node(2), 1, &[2, 1, 9], now());
        assert_eq!(
            gate.check(&env, &leaf_peer(), now()),
            Err(Reject::UnauthorizedOrigin)
        );
    }

    #[tokio::test]
    async fn an_unknown_station_is_refused() {
        let gate = gate(&[]);
        let env = envelope("satellite-a:nowhere", node(1), 1, &[1, 9], now());
        assert_eq!(
            gate.check(&env, &leaf_peer(), now()),
            Err(Reject::UnknownStation)
        );
    }

    #[tokio::test]
    async fn the_skew_window_is_five_minutes_ahead_and_thirty_days_behind() {
        let gate = gate(&[("satellite-a:backyard", 1)]);
        let at = |offset: Duration| {
            envelope("satellite-a:backyard", node(1), 1, &[1, 9], now() + offset)
        };
        // Inside, both edges.
        assert_eq!(
            gate.check(&at(Duration::minutes(4)), &leaf_peer(), now()),
            Ok(())
        );
        assert_eq!(
            gate.check(&at(-Duration::days(29)), &leaf_peer(), now()),
            Ok(()),
            "a satellite draining a month-long outage must not be refused"
        );
        // Outside, both edges.
        assert_eq!(
            gate.check(&at(Duration::minutes(6)), &leaf_peer(), now()),
            Err(Reject::ClockSkew),
            "a wrong clock writes rows into the future, which breaks every window"
        );
        assert_eq!(
            gate.check(&at(-Duration::days(31)), &leaf_peer(), now()),
            Err(Reject::TooOld),
            "the past edge is its own verdict, because time never moves back towards it"
        );
        assert!(
            Reject::ClockSkew.is_transient(),
            "a fast clock can be corrected"
        );
        assert!(
            !Reject::TooOld.is_transient(),
            "an envelope already too old will never be young enough, so stalling on it \
             would stall for ever"
        );
    }

    /// The past window has to be wider than the longest outage the spool can hold, or a
    /// node that was configured correctly loses the data it was configured to keep.
    #[test]
    fn the_past_window_is_wider_than_max_lag_and_the_worst_case_ceiling() {
        assert!(
            max_past_skew()
                > Duration::seconds(
                    i64::try_from(mw_pipeline::spool::MAX_LAG.as_secs()).expect("seven days fits")
                )
        );
        // The ~12-day worst case of §3.5: `CEILING_BYTES` at `WORST_CASE_BYTES_PER_SECOND`.
        #[allow(clippy::cast_precision_loss)]
        let worst_case_seconds = mw_pipeline::spool::CEILING_BYTES as f64
            / mw_pipeline::spool::WORST_CASE_BYTES_PER_SECOND;
        #[allow(clippy::cast_possible_truncation)]
        let worst_case = Duration::seconds(worst_case_seconds as i64);
        assert!(max_past_skew() > worst_case, "{worst_case}");
    }

    /// §4.6, and a **destructive-write** guard rather than a tidiness one: W3's writer
    /// assigns `col = EXCLUDED.col`, so an all-`None` reading at an instant that already
    /// holds a real observation would erase its values.
    #[tokio::test]
    async fn an_empty_reading_is_refused_rather_than_erasing_a_row() {
        let gate = gate(&[("satellite-a:backyard", 1)]);
        let mut env = envelope("satellite-a:backyard", node(1), 1, &[1, 9], now());
        env.reading = std::sync::Arc::new(mw_types::Reading {
            time: env.observed_at,
            station_id: "satellite-a:backyard".to_string(),
            ..mw_types::Reading::default()
        });
        assert_eq!(
            gate.check(&env, &leaf_peer(), now()),
            Err(Reject::EmptyReading)
        );
        // One metric is enough, and it may be a non-numeric one -- which is the case a
        // `METRICS`-walking implementation gets wrong.
        let mut text_only = env.clone();
        text_only.reading = std::sync::Arc::new(mw_types::Reading {
            time: env.observed_at,
            station_id: "satellite-a:backyard".to_string(),
            extra_text1: Some("hail".to_string()),
            ..mw_types::Reading::default()
        });
        assert_eq!(gate.check(&text_only, &leaf_peer(), now()), Ok(()));
    }

    /// A `seq` of zero is an encoder that omitted the field. Refused here, because the
    /// dedup gate answers `Duplicate` for it -- which the sender would read as `ACK_OK`
    /// and advance over everything it had ever sent.
    #[tokio::test]
    async fn a_zero_sequence_number_is_refused_rather_than_deduplicated() {
        let gate = gate(&[("satellite-a:backyard", 1)]);
        let env = envelope("satellite-a:backyard", node(1), 0, &[1, 9], now());
        assert_eq!(
            gate.check(&env, &leaf_peer(), now()),
            Err(Reject::BadSequence)
        );
        assert!(
            !Reject::BadSequence.is_transient(),
            "a sender whose encoder omits `seq` must be allowed to drain rather than \
             stall for ever"
        );
    }

    #[test]
    fn an_unknown_node_is_not_a_peer() {
        let peers = FakePeers::ca("satellite-a", node(1));
        assert_eq!(
            resolve(peers.as_ref(), &Presented::CaValidated { node: node(2) }),
            Err(Reject::UnauthorizedPeer)
        );
    }

    #[test]
    fn a_revoked_peer_is_told_apart_from_an_unknown_one() {
        let peers = FakePeers::ca("satellite-a", node(1)).revoked();
        assert_eq!(
            resolve(peers.as_ref(), &Presented::CaValidated { node: node(1) }),
            Err(Reject::Revoked),
            "an operator who revoked a peer needs a different answer from one who has a \
             stranger pointing a node at their port"
        );
    }

    /// A peer arriving by a door its row does not open is refused: without this, a `psk`
    /// peer could present any certificate the peer CA had signed.
    #[test]
    fn the_credential_has_to_be_the_one_the_row_configured() {
        let peers = FakePeers::psk("satellite-a", node(1), "c0ffee");
        assert_eq!(
            resolve(peers.as_ref(), &Presented::CaValidated { node: node(1) }),
            Err(Reject::UnauthorizedPeer)
        );
        assert_eq!(
            resolve(peers.as_ref(), &Presented::Pinned { node: node(1) }),
            Err(Reject::UnauthorizedPeer)
        );
        assert!(resolve(
            peers.as_ref(),
            &Presented::Psk {
                node: node(1),
                secret: "c0ffee".to_string().into(),
            }
        )
        .is_ok());
    }

    #[test]
    fn a_wrong_psk_is_a_bad_secret_and_not_an_unknown_peer() {
        let peers = FakePeers::psk("satellite-a", node(1), "c0ffee");
        assert_eq!(
            resolve(
                peers.as_ref(),
                &Presented::Psk {
                    node: node(1),
                    secret: "decaf".to_string().into(),
                }
            ),
            Err(Reject::BadSecret)
        );
    }

    #[test]
    fn the_presented_kinds_name_the_column() {
        assert_eq!(
            Presented::CaValidated { node: node(1) }.kind(),
            PeerAuth::MtlsCa.kind()
        );
        assert_eq!(
            Presented::Pinned { node: node(1) }.kind(),
            PeerAuth::MtlsPinned {
                spki_sha256: [0; 32]
            }
            .kind()
        );
        assert_eq!(
            Presented::Psk {
                node: node(1),
                secret: String::new().into()
            }
            .kind(),
            PeerAuth::Psk { phc: "".into() }.kind()
        );
    }
}
