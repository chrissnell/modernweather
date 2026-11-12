//! `PeerRegistry` over `config.db`: who may speak to this node, and the registration path.
//!
//! The peer half of [`crate::registry::ConfigRegistry`]'s capability (W7 §5.1). Reads are
//! a pointer load over [`PeerSnapshot`], for the reason every read on the admission path
//! is: W3 **D-87** allows no I/O there but the spool append. Writes are transactions and
//! happen twice in a peer's life — once per connection (`Hello`) and once ever
//! (`Enroll`) — so they are `async` and they republish the snapshot when they commit.
//!
//! # The registration path is why `ingest:*` holds `Write`
//!
//! W2a Decision 45 and W7 §5.1: a peer is a **node**, and a node has stations, so
//! registration writes two kinds of row — `peers` and `config.stations`. Without the
//! second, a satellite that authenticates perfectly stores nothing: every envelope it
//! sends names a station key this node has no row for, and W7 §4.4's step 5 refuses it.
//! That is the whole of why `StationRegistry` is `H W` for `ingest:*` and read-only
//! everywhere else — registration mints station keys and nothing else may.
//!
//! **A peer can neither shadow nor alter a row it does not own.** Every upsert below
//! carries `WHERE stations.owning_node_id = excluded.owning_node_id`, so a peer naming this
//! node's own roof sensor writes nothing and the configured row stands. The *reading* side
//! of the same property is `StationRegistry::station_owner`, which is what stops that peer
//! from writing history into the key anyway.
//!
//! # What enrolment can and cannot do here
//!
//! §5.3 offers two outcomes for an accepted token: sign the peer's CSR with the collector's
//! peer CA, or record its SPKI fingerprint. Only the second is expressible — `DESIGN.md`
//! **D-32** fixes the dependency set and there is no X.509 issuer in it, and "a substitution
//! is an amendment, not a manifest edit". So `mtls-pinned` and `psk` enrol here and
//! `mtls-ca` is refused **by name**, pointing at §5.3's own alternative: under a shared CA
//! the operator copies a certificate and inserts the `peers` row, which that section
//! already describes as the path `Enroll` exists to shorten rather than to replace.

use std::collections::BTreeMap;
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use chrono::{DateTime, Utc};
use mw_component::{
    BoxFuture, Enrolled, Enrolment, PeerAuth, PeerEntry, PeerName, PeerRegistry, PeerStation,
    PeerWriteError, StationMeta, StationProjection,
};
use mw_types::{Class, DisplayChain, OriginId, StationKey};
use secrecy::ExposeSecret;
use sqlx::{Row as _, SqlitePool};
use tracing::{info, warn};

use crate::config_db::{self, ConfigDbError, PeerRow};
use crate::ident;

/// One immutable view of `peers`, `peer_origins` and the stations they registered.
#[derive(Debug, Default)]
pub struct PeerSnapshot {
    by_node: BTreeMap<OriginId, PeerEntry>,
    /// Pinned SPKI digest to node, for layer 1's `mtls-pinned` verifier.
    ///
    /// Keyed by the digest because at the handshake there is no node id yet: the
    /// certificate is all there is, and refusing an unknown key there is what "refused at
    /// the TLS layer, before any frame is decoded" means.
    by_spki: BTreeMap<[u8; 32], OriginId>,
    /// Stations registered by a peer **since this process started**.
    ///
    /// Not every peer-owned station: the ones that existed at boot are in the station
    /// snapshot already, because `config_db::stations` reads local and received rows alike.
    /// This is what makes a station a satellite registered a moment ago usable before the
    /// next restart (see `crate::registry::ConfigRegistry::peers`).
    stations: BTreeMap<String, RegisteredStation>,
}

/// One station a peer registered, in the two shapes the registry answers with.
#[derive(Debug, Clone)]
struct RegisteredStation {
    owner: OriginId,
    meta: StationMeta,
    projection: StationProjection,
}

impl PeerSnapshot {
    /// Build a snapshot from the `peers` rows.
    ///
    /// A row this build cannot read — an `auth_kind` outside the schema's `CHECK`, a
    /// `node_id` that is not 16 bytes, an `mtls-pinned` row whose material is not a digest
    /// — is **dropped with a warning** rather than failing the load. Failing would make one
    /// unreadable row refuse every other peer on the node, and the only way a row gets into
    /// that state is this build disagreeing with its own schema.
    pub fn new(rows: &[PeerRow]) -> Self {
        let mut by_node = BTreeMap::new();
        let mut by_spki = BTreeMap::new();
        for row in rows {
            let Some(node) = node_id(&row.node_id) else {
                warn!(
                    peer = %row.name,
                    bytes = row.node_id.len(),
                    "a `peers` row's node_id is not 16 bytes, so that peer cannot be \
                     resolved and is ignored"
                );
                continue;
            };
            let Some(auth) = auth(row) else {
                warn!(
                    peer = %row.name,
                    auth_kind = %row.auth_kind,
                    "a `peers` row's auth_kind and auth_material do not form credentials \
                     this build can check, so that peer is ignored"
                );
                continue;
            };
            if let PeerAuth::MtlsPinned { spki_sha256 } = &auth {
                by_spki.insert(*spki_sha256, node);
            }
            by_node.insert(
                node,
                PeerEntry {
                    node,
                    name: PeerName::new(row.name.as_str()),
                    auth,
                    relay: row.relay,
                    origins: row.origins.iter().filter_map(|o| node_id(o)).collect(),
                    revoked_at: row.revoked_at.and_then(seconds),
                    software: row.software.clone(),
                },
            );
        }
        Self {
            by_node,
            by_spki,
            stations: BTreeMap::new(),
        }
    }

    /// The same snapshot, carrying the stations a `Hello` registered.
    pub fn with_stations(mut self, owner: OriginId, stations: &[PeerStation]) -> Self {
        for station in stations {
            self.stations.insert(
                station.station.as_str().to_string(),
                RegisteredStation {
                    owner,
                    meta: StationMeta {
                        latitude: station.latitude,
                        longitude: station.longitude,
                        altitude_m: station.altitude_m,
                        // A peer does not report a snow base distance: it is a *calibration*
                        // of the collector's own gauge, and §5.2's `StationMeta` has no
                        // field for it.
                        snow_base_distance_mm: None,
                    },
                    projection: StationProjection {
                        name: station.name.clone(),
                        kind: station.kind.clone(),
                        timezone: station.timezone.clone().filter(|z| !z.trim().is_empty()),
                        latitude: station.latitude,
                        longitude: station.longitude,
                        altitude_m: station.altitude_m,
                        snow_base_distance_mm: None,
                    },
                },
            );
        }
        self
    }

    /// Carry forward the stations another snapshot had registered.
    ///
    /// Called when a write republishes: the rows a *different* peer registered earlier in
    /// this process's life are still only in memory, and dropping them would make one
    /// satellite's reconnection un-resolve another's stations until the next restart.
    pub fn inheriting(mut self, previous: &PeerSnapshot) -> Self {
        for (key, station) in &previous.stations {
            self.stations
                .entry(key.clone())
                .or_insert_with(|| station.clone());
        }
        self
    }

    /// The peer this node id names, revoked or not.
    pub fn peer(&self, node: OriginId) -> Option<PeerEntry> {
        self.by_node.get(&node).cloned()
    }

    /// The peer whose pinned SPKI digest is this.
    pub fn peer_by_spki(&self, spki_sha256: &[u8; 32]) -> Option<PeerEntry> {
        self.by_spki
            .get(spki_sha256)
            .and_then(|node| self.by_node.get(node))
            .cloned()
    }

    /// Every peer, in node-id order.
    pub fn peers(&self) -> Vec<PeerEntry> {
        self.by_node.values().cloned().collect()
    }

    /// How many peers this snapshot holds, for the startup log line.
    pub fn len(&self) -> usize {
        self.by_node.len()
    }

    /// Whether this node trusts nobody yet, which is a collector awaiting enrolment.
    pub fn is_empty(&self) -> bool {
        self.by_node.is_empty()
    }

    /// The owner of a station a peer registered in this process's lifetime.
    pub fn owner(&self, station: &StationKey) -> Option<OriginId> {
        self.stations.get(station.as_str()).map(|s| s.owner)
    }

    /// What a peer said about one of its stations' coordinates.
    pub fn metadata(&self, station: &StationKey) -> Option<StationMeta> {
        self.stations.get(station.as_str()).map(|s| s.meta)
    }

    /// What the weather schema's `stations` projection should say about it.
    pub fn projection(&self, station: &StationKey) -> Option<StationProjection> {
        self.stations
            .get(station.as_str())
            .map(|s| s.projection.clone())
    }
}

fn node_id(bytes: &[u8]) -> Option<OriginId> {
    <[u8; 16]>::try_from(bytes).ok().map(OriginId::new)
}

fn seconds(at: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(at, 0)
}

/// `peers.auth_kind` + `peers.auth_material` as credentials this build can check.
fn auth(row: &PeerRow) -> Option<PeerAuth> {
    match row.auth_kind.as_str() {
        "mtls-ca" => Some(PeerAuth::MtlsCa),
        "mtls-pinned" => {
            let digest = ident::unhex(row.auth_material.as_deref()?)?;
            Some(PeerAuth::MtlsPinned {
                spki_sha256: <[u8; 32]>::try_from(digest.as_slice()).ok()?,
            })
        }
        "psk" => Some(PeerAuth::Psk {
            phc: row.auth_material.as_deref()?.into(),
        }),
        _ => None,
    }
}

/// The `PeerRegistry` the daemon runs on.
///
/// Holds the pool for the writes and the published registry for the reads, which is what
/// makes "a `Hello` registered this station" and "the gate can resolve it" the same fact
/// rather than two.
#[derive(Debug)]
pub struct PeerStore {
    pool: SqlitePool,
    registry: Arc<crate::registry::ConfigRegistry>,
}

impl PeerStore {
    /// A store over one configuration database and the registry it publishes into.
    pub fn new(pool: SqlitePool, registry: Arc<crate::registry::ConfigRegistry>) -> Self {
        Self { pool, registry }
    }

    /// Read `peers` and publish the peer half. Called once at boot and after every write.
    pub async fn republish(&self) -> Result<Arc<PeerSnapshot>, ConfigDbError> {
        let rows = config_db::peers(&self.pool).await?;
        let previous = self.registry.peer_snapshot();
        self.registry
            .publish_peers(PeerSnapshot::new(&rows).inheriting(&previous));
        Ok(self.registry.peer_snapshot())
    }

    /// Mint an enrolment token, store it hashed, and hand back the plaintext **once**.
    ///
    /// Argon2id at W5 **D-35**'s parameters, which is what the schema's `CHECK (token_hash
    /// GLOB '$argon2id$?*')` requires. The returned string is the only copy that will ever
    /// exist: the row holds a hash, and §5.3's "printed once and never logged" is a
    /// property of there being nothing else to print.
    pub async fn mint_enrolment_token(
        &self,
        peer_name: &str,
        ttl: std::time::Duration,
    ) -> Result<String, PeerWriteError> {
        let token = ident::enrolment_token().map_err(|e| backend(&e, Class::Fatal))?;
        let id = ident::node_id(Utc::now()).map_err(|e| backend(&e, Class::Fatal))?;
        let hash = hash_secret(&token)?;
        let expires_at =
            Utc::now().timestamp() + i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX / 2);
        sqlx::query(
            "INSERT INTO peer_enrolment_token (id, peer_name, token_hash, expires_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(id.as_slice())
        .bind(peer_name)
        .bind(&hash)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| query("minting an enrolment token", e))?;
        info!(
            peer = peer_name,
            expires_at, "an enrolment token was minted; enrolment is now open"
        );
        Ok(token)
    }

    async fn record_hello_inner(
        &self,
        node: OriginId,
        software: &str,
        stations: &[PeerStation],
    ) -> Result<(), PeerWriteError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| query("opening the registration transaction", e))?;
        sqlx::query("UPDATE peers SET last_seen = unixepoch(), software = ? WHERE node_id = ?")
            .bind(software)
            .bind(node.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(|e| query("recording a peer's Hello", e))?;
        for station in stations {
            upsert_station(&mut tx, node, station).await?;
        }
        tx.commit()
            .await
            .map_err(|e| query("committing the registration transaction", e))?;
        // Published after the commit, never before: a gate that could resolve a station
        // whose row had not committed would admit an envelope the next restart cannot
        // explain.
        let previous = self.registry.peer_snapshot();
        let rows = config_db::peers(&self.pool)
            .await
            .map_err(|e| PeerWriteError::Backend {
                reason: DisplayChain(&e).to_string(),
                class: Class::RETRYABLE,
            })?;
        self.registry.publish_peers(
            PeerSnapshot::new(&rows)
                .inheriting(&previous)
                .with_stations(node, stations),
        );
        Ok(())
    }

    async fn enrol_inner(&self, request: &Enrolment) -> Result<Enrolled, PeerWriteError> {
        // The token first, and with no reference to the request's claimed identity: a
        // caller that cannot present an open token learns nothing about which names or node
        // ids this collector knows.
        let open = self.open_tokens().await?;
        if open.is_empty() {
            return Err(PeerWriteError::EnrolmentClosed);
        }
        let Some(token_id) = verify_token(request.token.expose_secret(), &open) else {
            return Err(PeerWriteError::BadToken);
        };
        let digest = request.spki_sha256;
        if digest == [0u8; 32] {
            // An all-zero digest is not a SHA-256 of anything a caller could have hashed;
            // it is the shape of a caller that forgot to fill it.
            return Err(PeerWriteError::NoMaterial);
        }
        let material = ident::hex(&digest);

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| query("opening the enrolment transaction", e))?;
        // **Burned in the same transaction that inserts the peer** (§5.3), and burned with
        // `consumed_at IS NULL` in the predicate so that two requests racing on one token
        // cannot both enrol: the second `UPDATE` matches no row.
        let burned = sqlx::query(
            "UPDATE peer_enrolment_token SET consumed_at = unixepoch(), consumed_by = ? \
             WHERE id = ? AND consumed_at IS NULL",
        )
        .bind(request.node.as_bytes().as_slice())
        .bind(token_id.as_slice())
        .execute(&mut *tx)
        .await
        .map_err(|e| query("burning the enrolment token", e))?;
        if burned.rows_affected() == 0 {
            return Err(PeerWriteError::BadToken);
        }
        sqlx::query(
            "INSERT INTO peers (node_id, name, auth_kind, auth_material, software) \
             VALUES (?, ?, 'mtls-pinned', ?, ?) \
             ON CONFLICT (node_id) DO UPDATE SET \
                 auth_kind = excluded.auth_kind, \
                 auth_material = excluded.auth_material, \
                 software = excluded.software, \
                 revoked_at = NULL",
        )
        .bind(request.node.as_bytes().as_slice())
        .bind(&request.name)
        .bind(&material)
        .bind(&request.software)
        .execute(&mut *tx)
        .await
        .map_err(|e| query("inserting the peer row", e))?;
        // The peer itself, so that `peer_origins` is never empty and §8.5's
        // `PeerCannotSpeakForItself` cannot be reached by an enrolment this code performed.
        sqlx::query(
            "INSERT INTO peer_origins (node_id, origin_node_id) VALUES (?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(request.node.as_bytes().as_slice())
        .bind(request.node.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(|e| query("granting the peer its own origin", e))?;
        for station in &request.stations {
            upsert_station(&mut tx, request.node, station).await?;
        }
        tx.commit()
            .await
            .map_err(|e| query("committing the enrolment transaction", e))?;
        info!(
            peer = %request.name,
            node = %request.node,
            stations = request.stations.len(),
            "a peer enrolled and its SPKI was pinned"
        );
        let previous = self.registry.peer_snapshot();
        let rows = config_db::peers(&self.pool)
            .await
            .map_err(|e| PeerWriteError::Backend {
                reason: DisplayChain(&e).to_string(),
                class: Class::RETRYABLE,
            })?;
        let snapshot = PeerSnapshot::new(&rows)
            .inheriting(&previous)
            .with_stations(request.node, &request.stations);
        let peer = snapshot
            .peer(request.node)
            .ok_or_else(|| PeerWriteError::Backend {
                reason: "the peer row this enrolment inserted does not read back as a peer"
                    .to_string(),
                class: Class::Fatal,
            })?;
        self.registry.publish_peers(snapshot);
        Ok(Enrolled {
            peer,
            pinned_spki_sha256: Some(digest),
        })
    }

    /// Every unexpired, unconsumed token, as `(id, phc)`.
    async fn open_tokens(&self) -> Result<Vec<([u8; 16], String)>, PeerWriteError> {
        let rows = sqlx::query(
            "SELECT id, token_hash FROM peer_enrolment_token \
             WHERE consumed_at IS NULL AND expires_at > unixepoch() \
             ORDER BY expires_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| query("reading the open enrolment tokens", e))?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id: Vec<u8> = r.get("id");
                let id = <[u8; 16]>::try_from(id.as_slice()).ok()?;
                Some((id, r.get::<String, _>("token_hash")))
            })
            .collect())
    }
}

/// Upsert one registered station, without letting a peer touch a row it does not own.
///
/// The `WHERE` on the `DO UPDATE` is the whole security property: SQLite applies it to the
/// *existing* row, so a conflict on a key owned by this node or by another peer updates
/// nothing and reports success. Reporting success is right — the peer's `Hello` is not
/// wrong, it simply does not own that key — and the envelope it later sends for it is
/// refused by `station_owner`, which is where a refusal belongs.
async fn upsert_station(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: OriginId,
    station: &PeerStation,
) -> Result<(), PeerWriteError> {
    sqlx::query(
        "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind, \
                               presented_key, latitude, longitude, altitude_m, timezone, \
                               aprs_callsign, last_seen) \
         VALUES (?, ?, ?, ?, 'from_registration', ?, ?, ?, ?, ?, ?, unixepoch()) \
         ON CONFLICT (station_key) DO UPDATE SET \
             name = excluded.name, \
             type = excluded.type, \
             latitude = excluded.latitude, \
             longitude = excluded.longitude, \
             altitude_m = excluded.altitude_m, \
             timezone = excluded.timezone, \
             aprs_callsign = excluded.aprs_callsign, \
             last_seen = unixepoch(), \
             updated_at = unixepoch() \
         WHERE stations.owning_node_id = excluded.owning_node_id",
    )
    .bind(station.station.as_str())
    .bind(owner.as_bytes().as_slice())
    .bind(&station.name)
    .bind(&station.kind)
    // `presented_key` is the station key itself: a `from_registration` station is
    // identified by the key it was issued, and the envelope carries that key rather than a
    // second claim. The Go compat shim's `station:grpcrx` resolves by this column, so a
    // station registered here is also reachable from that method without a second row.
    .bind(station.station.as_str())
    .bind(station.latitude)
    .bind(station.longitude)
    .bind(station.altitude_m)
    .bind(station.timezone.as_deref())
    .bind(station.aprs_callsign.as_deref())
    .execute(&mut **tx)
    .await
    .map_err(|e| query("registering a peer's station", e))?;
    Ok(())
}

impl PeerRegistry for PeerStore {
    fn peer(&self, node: OriginId) -> Option<PeerEntry> {
        self.registry.peer_snapshot().peer(node)
    }

    fn peer_by_spki(&self, spki_sha256: &[u8; 32]) -> Option<PeerEntry> {
        self.registry.peer_snapshot().peer_by_spki(spki_sha256)
    }

    fn peers(&self) -> Vec<PeerEntry> {
        self.registry.peer_snapshot().peers()
    }

    fn record_hello<'a>(
        &'a self,
        node: OriginId,
        software: &'a str,
        stations: &'a [PeerStation],
    ) -> BoxFuture<'a, Result<(), PeerWriteError>> {
        Box::pin(self.record_hello_inner(node, software, stations))
    }

    fn enrolment_open(&self) -> BoxFuture<'_, Result<bool, PeerWriteError>> {
        Box::pin(async move { Ok(!self.open_tokens().await?.is_empty()) })
    }

    fn enrol<'a>(
        &'a self,
        request: &'a Enrolment,
    ) -> BoxFuture<'a, Result<Enrolled, PeerWriteError>> {
        Box::pin(self.enrol_inner(request))
    }
}

/// Argon2id over a token, at the parameters the schema's `CHECK` constrains the format of.
///
/// `SaltString::encode_b64` over kernel randomness rather than `SaltString::generate`,
/// which wants an `RngCore` the workspace has no crate for: `DESIGN.md` **D-32** names no
/// random-number generator, so `crate::ident` reads `/dev/urandom` and this encodes the
/// result. Sixteen bytes is `password-hash`'s own `RECOMMENDED_LENGTH`.
fn hash_secret(secret: &str) -> Result<String, PeerWriteError> {
    let bytes = ident::salt_bytes().map_err(|e| backend(&e, Class::Fatal))?;
    let salt = SaltString::encode_b64(&bytes).map_err(|e| PeerWriteError::Backend {
        reason: format!("a salt could not be encoded: {e}"),
        class: Class::Fatal,
    })?;
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PeerWriteError::Backend {
            reason: format!("the enrolment token could not be hashed: {e}"),
            class: Class::Fatal,
        })
}

/// Which open token this secret is, or `None`.
///
/// **Every open token is tried, and the loop does not stop at the first match**: an early
/// return would make the number of Argon2 verifications a function of which token was
/// presented, which is a timing oracle over the token table's order. There are at most a
/// handful of open tokens at any time, so the cost of being uniform is bounded by the
/// operator's own behaviour. The comparison inside `verify_password` is `password-hash`'s
/// own constant-time one (**D-33**).
fn verify_token(secret: &str, open: &[([u8; 16], String)]) -> Option<[u8; 16]> {
    let mut found = None;
    for (id, phc) in open {
        let Ok(parsed) = PasswordHash::new(phc) else {
            warn!("an enrolment token row holds a hash this build cannot parse; ignoring it");
            continue;
        };
        if Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok()
            && found.is_none()
        {
            found = Some(*id);
        }
    }
    found
}

fn query(what: &'static str, source: sqlx::Error) -> PeerWriteError {
    let e = ConfigDbError::Query { what, source };
    PeerWriteError::Backend {
        reason: DisplayChain(&e).to_string(),
        // A write that failed may succeed on the next connection: SQLite returns `BUSY`
        // under contention and a closed pool is a shutdown in progress. The caller turns
        // this into `UNAVAILABLE`, which is what a satellite's own retry answers.
        class: Class::RETRYABLE,
    }
}

fn backend(e: &dyn std::error::Error, class: Class) -> PeerWriteError {
    PeerWriteError::Backend {
        reason: DisplayChain(e).to_string(),
        class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, material: Option<&str>) -> PeerRow {
        PeerRow {
            node_id: vec![7; 16],
            name: name.into(),
            auth_kind: kind.into(),
            auth_material: material.map(str::to_string),
            relay: false,
            software: None,
            last_seen: None,
            revoked_at: None,
            origins: vec![vec![7; 16]],
        }
    }

    #[test]
    fn a_pinned_peer_is_findable_by_its_digest() {
        let digest = [0xab; 32];
        let rows = [row(
            "satellite-a",
            "mtls-pinned",
            Some(&ident::hex(&digest)),
        )];
        let snapshot = PeerSnapshot::new(&rows);
        let peer = snapshot
            .peer_by_spki(&digest)
            .expect("the pinned digest resolves a peer at the handshake");
        assert_eq!(peer.name.as_str(), "satellite-a");
        assert!(
            snapshot.peer_by_spki(&[0x00; 32]).is_none(),
            "an unknown key is refused rather than matched to the only peer there is"
        );
    }

    /// An unreadable row costs that peer and no other. Failing the load instead would let
    /// one bad row refuse a whole fleet.
    #[test]
    fn a_row_this_build_cannot_read_is_dropped_and_the_rest_survive() {
        let mut bad = row("broken", "mtls-pinned", Some("not-hex"));
        bad.node_id = vec![1, 2, 3];
        let rows = [bad, row("good", "mtls-ca", None)];
        let snapshot = PeerSnapshot::new(&rows);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot
                .peer(OriginId::new([7; 16]))
                .expect("the readable row")
                .name
                .as_str(),
            "good"
        );
    }

    #[test]
    fn an_mtls_ca_row_needs_no_material_and_a_pinned_one_does() {
        assert!(PeerSnapshot::new(&[row("a", "mtls-ca", None)]).len() == 1);
        assert!(
            PeerSnapshot::new(&[row("a", "mtls-pinned", None)]).is_empty(),
            "a pinned row with nothing to pin is not credentials"
        );
        assert!(
            PeerSnapshot::new(&[row("a", "psk", None)]).is_empty(),
            "a psk row with no hash is not credentials"
        );
        assert!(
            PeerSnapshot::new(&[row("a", "something-else", Some("x"))]).is_empty(),
            "an auth_kind outside the schema's CHECK is not credentials"
        );
    }

    #[test]
    fn revocation_round_trips_through_the_snapshot() {
        let mut r = row("a", "mtls-ca", None);
        r.revoked_at = Some(1_762_732_800);
        let snapshot = PeerSnapshot::new(&[r]);
        assert!(snapshot
            .peer(OriginId::new([7; 16]))
            .expect("a revoked peer is still resolvable")
            .is_revoked());
    }

    #[test]
    fn a_registered_station_answers_its_owner_and_its_metadata() {
        let owner = OriginId::new([7; 16]);
        let snapshot = PeerSnapshot::default().with_stations(
            owner,
            &[PeerStation {
                station: StationKey::new("satellite-a:backyard"),
                name: "Back Yard".into(),
                kind: "davis".into(),
                latitude: Some(39.64),
                longitude: Some(-105.2),
                altitude_m: Some(1800.0),
                timezone: Some("America/Denver".into()),
                aprs_callsign: None,
            }],
        );
        let key = StationKey::new("satellite-a:backyard");
        assert_eq!(snapshot.owner(&key), Some(owner));
        assert_eq!(
            snapshot.metadata(&key).expect("registered").latitude,
            Some(39.64)
        );
        assert_eq!(snapshot.projection(&key).expect("registered").kind, "davis");
        assert_eq!(snapshot.owner(&StationKey::new("other")), None);
    }

    /// A second peer's reconnection must not un-resolve the first peer's stations.
    #[test]
    fn republishing_inherits_the_stations_other_peers_registered() {
        let a = OriginId::new([1; 16]);
        let first = PeerSnapshot::default().with_stations(
            a,
            &[PeerStation {
                station: StationKey::new("a:one"),
                name: "One".into(),
                kind: "davis".into(),
                latitude: None,
                longitude: None,
                altitude_m: None,
                timezone: None,
                aprs_callsign: None,
            }],
        );
        let second = PeerSnapshot::default()
            .inheriting(&first)
            .with_stations(OriginId::new([2; 16]), &[]);
        assert_eq!(second.owner(&StationKey::new("a:one")), Some(a));
    }

    #[test]
    fn a_token_verifies_against_its_own_hash_and_not_another() {
        let phc = hash_secret("c0ffee").expect("hashable");
        let other = hash_secret("decaf").expect("hashable");
        let open = vec![([1; 16], other), ([2; 16], phc)];
        assert_eq!(verify_token("c0ffee", &open), Some([2; 16]));
        assert_eq!(verify_token("tea", &open), None);
        assert_eq!(verify_token("c0ffee", &[]), None);
    }

    #[test]
    fn a_minted_token_hash_is_the_phc_form_the_schema_checks_for() {
        let phc = hash_secret("c0ffee").expect("hashable");
        assert!(phc.starts_with("$argon2id$"), "{phc}");
    }
}
