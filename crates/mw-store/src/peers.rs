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

/// How many `config.stations` rows one **peer** node may own.
///
/// The bound exists because `NodeIdentity.stations` is a repeated field and a `Hello` can
/// be re-sent: `mw_collector::service::MAX_PEER_STATIONS` bounds one call, and this bounds
/// what a peer accumulates across all of them. The owner's fleet is four nodes with a
/// handful of stations each, so 256 is two orders of magnitude above anything real.
///
/// **Peers only.** A local station is an operator's own row and is not counted here;
/// `with_peer_stations`' predicate is the same distinction.
pub const MAX_OWNED_STATIONS: i64 = 256;

/// One peer, as the fleet page shows it (**N-11**).
///
/// Display data, deliberately separate from [`PeerEntry`], which is the authorisation
/// answer. See [`PeerStore::fleet`] for why the two are not one type.
#[derive(Debug, Clone)]
pub struct FleetPeer {
    /// `peers.node_id`, 16 bytes.
    pub node_id: Vec<u8>,
    /// `peers.name`, as it enrolled.
    pub name: String,
    /// When it was last heard from.
    pub last_seen: Option<DateTime<Utc>>,
    /// When it was revoked, which is a timestamp rather than a deletion so that "trusted
    /// until Tuesday" is readable off the page.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Which stations it owns, by key.
    pub stations: Vec<String>,
}

/// One `peer_enrolment_token` row (**N-11**). Never the token itself.
#[derive(Debug, Clone)]
pub struct EnrolmentTokenRow {
    /// `id`, 16 bytes.
    pub id: Vec<u8>,
    /// Which peer it was minted for.
    pub peer_name: String,
    /// When it was issued.
    pub issued_at: DateTime<Utc>,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
    /// When it was consumed, if it was.
    pub consumed_at: Option<DateTime<Utc>>,
    /// Which node consumed it, 16 bytes.
    pub consumed_by: Option<Vec<u8>>,
}

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

    /// The same snapshot, carrying every station a **peer** owns, read from the database.
    ///
    /// # Why this is a read and not an accumulation
    ///
    /// The first version took the stations straight off the `Hello` that had just been
    /// recorded and inserted them unconditionally, carrying the previous snapshot's entries
    /// forward underneath. That was an **ownership hijack**: `upsert_station` guards the
    /// database with `WHERE stations.owning_node_id = excluded.owning_node_id`, so a peer
    /// naming another node's station key writes nothing and is told nothing — but the
    /// snapshot was written from the *claim* rather than from the row, so the second peer to
    /// declare a key became its owner in the only copy `StationRegistry::station_owner`
    /// consults. One `Hello` from any enrolled peer was enough to take over another node's
    /// station and write into its history, and `HelloAck.accepted_stations` — which filters
    /// on that same snapshot — reported the stolen key as accepted.
    ///
    /// Deriving the map from `stations` instead makes ownership in the snapshot *be* the
    /// guarded fact rather than a copy of it, and it removes the accumulator with no
    /// eviction path that a peer could grow by inventing keys.
    ///
    /// `owning_node_id <> this node's id` is the predicate: a **local** station belongs to
    /// the configured station snapshot, which `plan()` was derived from and which this must
    /// never shadow (`crate::registry::ConfigRegistry::peers`).
    pub async fn with_peer_stations(
        mut self,
        pool: &SqlitePool,
        me: OriginId,
    ) -> Result<Self, ConfigDbError> {
        let rows = sqlx::query(
            "SELECT station_key, owning_node_id, name, type, latitude, longitude, \
                    altitude_m, timezone \
             FROM stations WHERE owning_node_id <> ? ORDER BY station_key",
        )
        .bind(me.as_bytes().as_slice())
        .fetch_all(pool)
        .await
        .map_err(|source| ConfigDbError::Query {
            what: "the stations a peer owns",
            source,
        })?;
        for row in rows {
            let key: String = row.get("station_key");
            let Some(owner) = node_id(&row.get::<Vec<u8>, _>("owning_node_id")) else {
                // A column its own `CHECK` would have refused. Dropped rather than
                // defaulted: an unowned station reads as unknown, which fails closed at
                // W7 §4.4's step 5.
                warn!(
                    station = %key,
                    "a `config.stations` row's owning_node_id is not 16 bytes, so that \
                     station cannot be resolved for a peer and is ignored"
                );
                continue;
            };
            let latitude: Option<f64> = row.get("latitude");
            let longitude: Option<f64> = row.get("longitude");
            let altitude_m: Option<f64> = row.get("altitude_m");
            let timezone: Option<String> = row.get("timezone");
            self.stations.insert(
                key.clone(),
                RegisteredStation {
                    owner,
                    meta: StationMeta {
                        latitude,
                        longitude,
                        altitude_m,
                        // A peer does not report a snow base distance: it is a *calibration*
                        // of the collector's own gauge, and §5.2's `StationMeta` has no
                        // field for it.
                        snow_base_distance_mm: None,
                    },
                    projection: StationProjection {
                        name: row.get("name"),
                        kind: row.get("type"),
                        timezone: timezone.filter(|z| !z.trim().is_empty()),
                        latitude,
                        longitude,
                        altitude_m,
                        snow_base_distance_mm: None,
                    },
                },
            );
        }
        Ok(self)
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
    /// This node's own `NodeId`, which is what separates a **local** station from one a
    /// peer owns (`PeerSnapshot::with_peer_stations`).
    me: OriginId,
    /// Serialises [`PeerStore::republish`]'s read-build-publish.
    ///
    /// **Without it the published snapshot can be older than the database.** `republish` is
    /// two awaits between reading the rows and storing the result, and nothing ordered two
    /// concurrent callers — so the later reader could be overtaken by the earlier one's
    /// store. Reproduced: two satellites reconnecting at once, and the published snapshot
    /// is missing the station one of them had just committed. The consequences all come
    /// from the same window: `station_owner` answers `None`, so that peer's envelopes are
    /// `Reject::UnknownStation` — transient, so its ack never advances and its whole uplink
    /// stalls until the next `Hello` — and `HelloAck.accepted_stations` tells it its own
    /// station was refused.
    ///
    /// A `tokio::sync::Mutex` because it is held across awaits. The reads do not need to be
    /// inside the write transaction: re-reading is monotonic, and all that has to be
    /// ordered is the publish against the read that produced it.
    publishing: tokio::sync::Mutex<()>,
}

impl PeerStore {
    /// A store over one configuration database and the registry it publishes into.
    pub fn new(
        pool: SqlitePool,
        registry: Arc<crate::registry::ConfigRegistry>,
        me: OriginId,
    ) -> Self {
        Self {
            pool,
            registry,
            me,
            publishing: tokio::sync::Mutex::new(()),
        }
    }

    /// Read `peers` and the stations they own, and publish the peer half.
    ///
    /// Called once at boot and after every write. Both halves come from the database every
    /// time: a snapshot assembled from anything else is a second answer to "who owns this
    /// station", and that question is an authorisation one
    /// (`PeerSnapshot::with_peer_stations`).
    pub async fn republish(&self) -> Result<Arc<PeerSnapshot>, ConfigDbError> {
        // Held across the read, the build and the store, so a snapshot can never be
        // published over a newer one (see the field's own documentation).
        let _ordered = self.publishing.lock().await;
        // A connection off the pool, because `config_db::peers` runs two queries and takes
        // one it can reborrow (the management API's preview reads inside a transaction).
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|source| ConfigDbError::Query {
                what: "a connection to read the peer rows",
                source,
            })?;
        let rows = config_db::peers(&mut conn).await?;
        let snapshot = PeerSnapshot::new(&rows)
            .with_peer_stations(&self.pool, self.me)
            .await?;
        self.registry.publish_peers(snapshot);
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

    /// **N-11**: what `/data-storage/receive` has to show, in one read.
    ///
    /// W7 D-33 requires the management UI to surface outstanding enrolment tokens, because
    /// **leaving enrolment open is the mistake and it should be visible** -- `Enroll` is
    /// served only while an unexpired, unconsumed token exists, so an outstanding row is a
    /// door that is open right now.
    ///
    /// A purpose-built read rather than a projection of [`PeerSnapshot`], and the reason is
    /// what each one is for: the snapshot is the **authorisation** answer to "may this peer
    /// speak for that origin", so it deliberately carries no `last_seen` and no station
    /// list, and widening it to carry them would put display data on the path that decides
    /// whether an envelope is admitted. One query per question instead.
    pub async fn fleet(&self) -> Result<Vec<FleetPeer>, ConfigDbError> {
        let rows = sqlx::query(
            "SELECT p.node_id, p.name, p.last_seen, p.revoked_at, \
                    (SELECT group_concat(s.station_key) FROM stations s \
                      WHERE s.owning_node_id = p.node_id) AS stations \
             FROM peers p ORDER BY p.name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| ConfigDbError::Query {
            what: "the peer rows for the fleet view",
            source,
        })?;
        Ok(rows
            .iter()
            .map(|row| FleetPeer {
                node_id: row.get::<Vec<u8>, _>("node_id"),
                name: row.get::<String, _>("name"),
                last_seen: row
                    .get::<Option<i64>, _>("last_seen")
                    .and_then(|s| DateTime::from_timestamp(s, 0)),
                revoked_at: row
                    .get::<Option<i64>, _>("revoked_at")
                    .and_then(|s| DateTime::from_timestamp(s, 0)),
                // An explicit `match` rather than a silent default, which
                // `ci/layers.toml` forbids this crate by name: a defaulted value from a
                // database read is how a peer that owns stations comes to be displayed as
                // owning none. (The forbidden name is deliberately not written here --
                // the gate matches source text, so even a comment about it is a hit.)
                stations: match row.get::<Option<String>, _>("stations") {
                    Some(list) => list.split(',').map(str::to_string).collect(),
                    None => Vec::new(),
                },
            })
            .collect())
    }

    /// Every enrolment token this node has minted, newest first.
    ///
    /// The outstanding set and the enrolment log come from these same rows rather than
    /// from a separate audit table. That is the decision: a history assembled from a
    /// different source than the live state is a history that can disagree with it, and
    /// `consumed_at`/`consumed_by` already record exactly what happened to each token.
    ///
    /// The token itself is **not** here and cannot be: the row holds an Argon2id hash and
    /// the plaintext was printed once (W7 §5.3).
    pub async fn enrolment_tokens(&self) -> Result<Vec<EnrolmentTokenRow>, ConfigDbError> {
        let rows = sqlx::query(
            "SELECT id, peer_name, issued_at, expires_at, consumed_at, consumed_by \
             FROM peer_enrolment_token ORDER BY issued_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| ConfigDbError::Query {
            what: "the enrolment tokens",
            source,
        })?;
        Ok(rows
            .iter()
            .map(|row| EnrolmentTokenRow {
                id: row.get::<Vec<u8>, _>("id"),
                peer_name: row.get::<String, _>("peer_name"),
                issued_at: DateTime::from_timestamp(row.get::<i64, _>("issued_at"), 0)
                    .unwrap_or_else(Utc::now),
                expires_at: DateTime::from_timestamp(row.get::<i64, _>("expires_at"), 0)
                    .unwrap_or_else(Utc::now),
                consumed_at: row
                    .get::<Option<i64>, _>("consumed_at")
                    .and_then(|s| DateTime::from_timestamp(s, 0)),
                consumed_by: row.get::<Option<Vec<u8>>, _>("consumed_by"),
            })
            .collect())
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
        // **Which owner a peer may declare**, which is §5.4's layer 3 applied to a
        // registration rather than to an envelope: itself always, and a node in its
        // `peer_origins` grant when `relay` is set. A peer that names an owner it has no
        // authority over has its declaration attributed to itself instead -- not refused,
        // because the station is still inventory it is entitled to tell this node about,
        // and the ownership is the only part it may not choose.
        let peer = self.registry.peer_snapshot().peer(node);
        for station in stations {
            let owner = match station.owner {
                Some(declared) if declared != node => {
                    let granted = peer
                        .as_ref()
                        .is_some_and(|entry| entry.may_speak_for(declared));
                    if granted {
                        declared
                    } else {
                        warn!(
                            node = %node,
                            declared = %declared,
                            station = %station.station,
                            "a peer declared a station as measured by a node it has no \
                             grant for, so it is attributed to the peer itself"
                        );
                        node
                    }
                }
                _ => node,
            };
            upsert_station(&mut tx, owner, station).await?;
        }
        // **The total a node may own, counted after the writes and inside the
        // transaction.**
        //
        // The caller bounds one `Hello`; this bounds the **accumulation** across
        // reconnections, which is the growth a peer inventing a fresh station key per
        // `Hello` would otherwise have no limit on at all — one `config.stations` row and
        // one resident snapshot entry per call, for ever.
        //
        // Counted after the upserts rather than before, and the transaction is what makes
        // that the simple version: if the answer is over the limit the error returns, `tx`
        // drops un-committed, and every row it wrote goes with it. Counting first would
        // mean deciding how many of the declared keys were new, which is the same question
        // asked less directly.
        // Counted over every owner this `Hello` wrote, not only the declaring node: a relay
        // declares its whole inventory, so the rows it creates are spread across the nodes
        // behind it and a per-node count would miss the growth entirely.
        let mut owners: std::collections::BTreeSet<OriginId> =
            stations.iter().filter_map(|s| s.owner).collect();
        owners.insert(node);
        let mut owned = 0i64;
        for owner in owners {
            owned = owned.max(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM stations WHERE owning_node_id = ?",
                )
                .bind(owner.as_bytes().as_slice())
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| query("counting the stations a peer owns", e))?,
            );
        }
        if owned > MAX_OWNED_STATIONS {
            warn!(
                node = %node,
                owned,
                limit = MAX_OWNED_STATIONS,
                "this registration would take a node past the number of stations one may \
                 own, so none of it was written"
            );
            return Err(PeerWriteError::TooManyStations {
                owned,
                limit: MAX_OWNED_STATIONS,
            });
        }
        tx.commit()
            .await
            .map_err(|e| query("committing the registration transaction", e))?;
        // Published after the commit, never before: a gate that could resolve a station
        // whose row had not committed would admit an envelope the next restart cannot
        // explain. And read back from the database rather than assembled from the claim --
        // `with_peer_stations` has why that distinction is an authorisation one.
        self.republish()
            .await
            .map_err(|e| PeerWriteError::Backend {
                reason: DisplayChain(&e).to_string(),
                class: Class::RETRYABLE,
            })?;
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
        let Some((token_id, token_name)) = verify_token(request.token.expose_secret(), &open)
        else {
            return Err(PeerWriteError::BadToken);
        };
        // **The token is scoped to the name the operator minted it for**, and it is checked
        // here because `peer_enrolment_token.peer_name` exists for exactly this and was
        // read by nobody. Without it a token for a new `satellite-d` could be presented
        // with any `node_id`, and a `node_id` that already has a row is a takeover: the
        // `DO UPDATE` below replaces the credential, and `relay` and `peer_origins` are not
        // the credential -- so an attacker inherited whatever the displaced peer had been
        // granted. §5.3 says enrolment "inserts `peers` + `peer_origins` + `stations`
        // rows"; insert, not adopt.
        if request.name != token_name {
            warn!(
                presented = %request.name,
                "an enrolment token was presented with a node name it was not minted for"
            );
            return Err(PeerWriteError::BadToken);
        }
        if request.node == self.me {
            // The collector publishes its own `NodeId` in `HelloAck` and `EnrolGrant`, so
            // a token holder knows it. Enrolling as it would let that peer mint
            // `config.stations` rows attributed to **this node** -- invisible to the peer
            // snapshot, which excludes them, and then indistinguishable from local stations
            // in `/stationinfo`, in the website and in what this node announces upstream.
            // There is no ingest bypass behind it: an envelope would need `path.last()` and
            // `path[len - 2]` both equal to this node, which is a repeated hop no `PathSet`
            // can hold.
            warn!("an enrolment attempted to claim this node's own identity");
            return Err(PeerWriteError::BadToken);
        }
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
        // **Every refusal below rolls this burn back**, because `tx` drops un-committed --
        // so a token survives a failed attempt. That is deliberate rather than overlooked:
        // the attempts that can fail here are an operator typing the wrong name or the
        // wrong node id, and burning their one token for a typo is worse than the
        // alternative. The alternative is a token that can be probed, and probing needs a
        // 128-bit `node_id` guess from someone who already holds an operator-minted,
        // name-scoped, expiring token -- so what it buys an attacker is nothing.
        //
        // What it does cost is visibility: a probed token stays open, so `Enroll` stays
        // open and W5 keeps reporting enrolment as available. The INFO line on every
        // attempt (§5.3) is what an operator reads instead.
        // **`peers.name` is UNIQUE, so the name-taken case is refused by name.**
        //
        // The ordinary reinstall path reaches it: a satellite whose disk was replaced mints
        // a brand-new `NodeId` at first start, so an operator who re-enrols it under the
        // same name presents a new `node_id` for a name the collector still holds. Without
        // this the INSERT below violates `stations_name`'s sibling index and the peer gets
        // an opaque SQLite string classified `Retryable` -- an uplink retrying for ever
        // rather than a message. Adopting the name silently would be a takeover, so the
        // answer is a named refusal with the operator's next action in it.
        let taken: Option<Vec<u8>> = sqlx::query_scalar("SELECT node_id FROM peers WHERE name = ?")
            .bind(&request.name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| query("checking whether that peer name is enrolled", e))?;
        if let Some(existing) = taken {
            if existing != request.node.as_bytes().as_slice() {
                warn!(
                    peer = %request.name,
                    node = %request.node,
                    "that peer name is already enrolled under a different node id"
                );
                return Err(PeerWriteError::NameTaken {
                    name: request.name.clone(),
                });
            }
        }
        // **Three clauses on the `DO UPDATE`, and each one closes a takeover.**
        //
        // A re-enrolment of the same `(node_id, name)` is legitimate: a satellite whose
        // keypair was rotated or whose disk was replaced presents a new SPKI under the same
        // identity. Anything else is not, and the predicate says which is which:
        //
        //   * `peers.name = excluded.name` -- the row this `node_id` already has must be
        //     the peer the token names. Without it, a token for `satellite-d` presented
        //     with `relay-west`'s `node_id` would replace `relay-west`'s credential while
        //     leaving its name on the row, so nothing an operator reads would change.
        //   * `peers.revoked_at IS NULL` -- re-trusting a revoked peer is an operator's
        //     decision, not a decision a peer makes by enrolling again. The previous
        //     version set `revoked_at = NULL` unconditionally, which made a revocation
        //     undoable by whoever held a token.
        //   * `relay = 0` -- a changed credential drops the relay grant. Fail-safe and
        //     deliberate: if the key holder has changed, the authority to speak for other
        //     nodes has to be re-granted by the operator rather than inherited. The grants
        //     themselves are deleted below.
        let upserted = sqlx::query(
            "INSERT INTO peers (node_id, name, auth_kind, auth_material, software) \
             VALUES (?, ?, 'mtls-pinned', ?, ?) \
             ON CONFLICT (node_id) DO UPDATE SET \
                 auth_kind = excluded.auth_kind, \
                 auth_material = excluded.auth_material, \
                 software = excluded.software, \
                 relay = 0 \
             WHERE peers.name = excluded.name AND peers.revoked_at IS NULL",
        )
        .bind(request.node.as_bytes().as_slice())
        .bind(&request.name)
        .bind(&material)
        .bind(&request.software)
        .execute(&mut *tx)
        .await
        .map_err(|e| query("inserting the peer row", e))?;
        if upserted.rows_affected() == 0 {
            // The `node_id` has a row belonging to a different name, or to a peer an
            // operator revoked. The token is burned either way -- the transaction below
            // would roll that back, so the burn is what this returns on -- because a token
            // that can be probed against node ids until one is accepted is a token that
            // enumerates the fleet.
            warn!(
                peer = %request.name,
                node = %request.node,
                "an enrolment named a node id that belongs to a different peer, or to one \
                 whose enrolment was revoked, so nothing was changed"
            );
            return Err(PeerWriteError::BadToken);
        }
        // **Every grant but the peer's own is removed**, for the `relay = 0` reason: a
        // changed credential must not inherit the authority the previous key holder had.
        sqlx::query("DELETE FROM peer_origins WHERE node_id = ? AND origin_node_id <> ?")
            .bind(request.node.as_bytes().as_slice())
            .bind(request.node.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(|e| query("clearing the peer's inherited origin grants", e))?;
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
        let snapshot = self
            .republish()
            .await
            .map_err(|e| PeerWriteError::Backend {
                reason: DisplayChain(&e).to_string(),
                class: Class::RETRYABLE,
            })?;
        let peer = snapshot
            .peer(request.node)
            .ok_or_else(|| PeerWriteError::Backend {
                reason: "the peer row this enrolment inserted does not read back as a peer"
                    .to_string(),
                class: Class::Fatal,
            })?;
        Ok(Enrolled {
            peer,
            pinned_spki_sha256: Some(digest),
        })
    }

    /// Every unexpired, unconsumed token.
    async fn open_tokens(&self) -> Result<Vec<OpenToken>, PeerWriteError> {
        let rows = sqlx::query(
            "SELECT id, peer_name, token_hash FROM peer_enrolment_token \
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
                Some(OpenToken {
                    id: <[u8; 16]>::try_from(id.as_slice()).ok()?,
                    peer_name: r.get("peer_name"),
                    phc: r.get("token_hash"),
                })
            })
            .collect())
    }
}

/// Upsert one registered station, without letting a peer touch a row it does not own.
///
/// # `presented_key` is **not** written, and the schema is why
///
/// `config.stations` carries
/// `CHECK (CASE identity_kind WHEN 'from_registration' THEN presented_key IS NULL ... END)`:
/// a station that arrived by registration is identified by the key it was issued, not by a
/// claim a frame presents. The first version of this function set the column to the station
/// key, which meant **every** `Hello` failed on that constraint — a complete break of the
/// registration path, found by `crates/mw-store/tests/peers.rs` and by nothing else, because
/// every other test that touches a `from_registration` row builds a `StationRow` value
/// rather than inserting one.
///
/// Nothing on the collector's path wants the column: `mw_collector::admit::Gate` authorises
/// an envelope through `StationRegistry::station_owner`, which reads `owning_node_id`.
/// What a NULL `presented_key` does cost is the **Go compat shim**: `station:grpcrx`
/// resolves a frame through `StationRegistry::resolve`, which indexes on `presented_key` —
/// so a station registered here is not reachable through `WeatherV1.SendWeatherReadings`
/// until an operator sets that column by hand, which is what
/// `mw_grpc::grpcrx::route_table`'s `Unimplemented` message already tells them to do. That
/// is a property of the schema's own choice rather than of this item, and it is recorded in
/// this item's PR.
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
                               latitude, longitude, altitude_m, timezone, \
                               aprs_callsign, last_seen) \
         VALUES (?, ?, ?, ?, 'from_registration', ?, ?, ?, ?, ?, unixepoch()) \
         ON CONFLICT (station_key) DO UPDATE SET \
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
    // **The station KEY, not the name the peer declared.** `config.stations` carries
    // `CREATE UNIQUE INDEX stations_name ON stations (name)` -- the frozen contract looks a
    // station up by name, so the column is one global namespace and the schema's own comment
    // says a collision has to be a loud failure. A peer does not get to choose a value in
    // that namespace: two satellites both calling a station `backyard` is the ordinary case
    // (the key is namespaced, the name is not), and writing the declared name made the
    // second satellite's EVERY `Hello` fail on that index -- an opaque SQLite string,
    // classified `Retryable`, so the symptom was an uplink retrying for ever rather than a
    // message.
    //
    // The key is unique by its own index, so this cannot collide. It is also not pretty,
    // which is the right trade: an operator can rename the row afterwards and the `DO
    // UPDATE` below deliberately does **not** touch `name`, so a peer cannot rename it
    // back. `StationMeta.name` reaches this node and is logged at registration; it is not
    // the row's name, and that is recorded as a proposed amendment to §5.2 in this item's
    // PR.
    .bind(station.station.as_str())
    .bind(&station.kind)
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
fn verify_token(secret: &str, open: &[OpenToken]) -> Option<([u8; 16], String)> {
    let mut found = None;
    for token in open {
        let Ok(parsed) = PasswordHash::new(&token.phc) else {
            warn!("an enrolment token row holds a hash this build cannot parse; ignoring it");
            continue;
        };
        if Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok()
            && found.is_none()
        {
            found = Some((token.id, token.peer_name.clone()));
        }
    }
    found
}

/// One unexpired, unconsumed enrolment token.
///
/// `peer_name` is on the row because the operator minted the token **for a peer**, and
/// `enrol_inner` refuses a request that presents it for a different one — which is what
/// stops a token being probed against node ids until one is accepted.
#[derive(Debug)]
struct OpenToken {
    id: [u8; 16],
    peer_name: String,
    phc: String,
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

    /// A snapshot with no station rows resolves nothing, which is what fails closed at
    /// W7 §4.4's step 5.
    ///
    /// The populated case is asserted against a real database in
    /// `crates/mw-store/tests/db.rs`, because the map is a **read** of `config.stations`
    /// rather than an accumulation of what a `Hello` claimed — see
    /// [`PeerSnapshot::with_peer_stations`] for why that distinction is an authorisation
    /// one and what it cost before.
    #[test]
    fn a_snapshot_with_no_station_rows_owns_nothing() {
        let snapshot = PeerSnapshot::new(&[row("satellite-a", "mtls-ca", None)]);
        let key = StationKey::new("satellite-a:backyard");
        assert_eq!(snapshot.owner(&key), None);
        assert_eq!(snapshot.metadata(&key), None);
        assert!(snapshot.projection(&key).is_none());
    }

    #[test]
    fn a_token_verifies_against_its_own_hash_and_not_another() {
        let open = vec![
            OpenToken {
                id: [1; 16],
                peer_name: "satellite-b".to_string(),
                phc: hash_secret("decaf").expect("hashable"),
            },
            OpenToken {
                id: [2; 16],
                peer_name: "satellite-a".to_string(),
                phc: hash_secret("c0ffee").expect("hashable"),
            },
        ];
        assert_eq!(
            verify_token("c0ffee", &open),
            Some(([2; 16], "satellite-a".to_string())),
            "the token resolves to its own row, and carries the name it was minted for -- \
             which is what `enrol_inner` refuses a mismatch against"
        );
        assert_eq!(verify_token("tea", &open), None);
        assert_eq!(verify_token("c0ffee", &[]), None);
    }

    #[test]
    fn a_minted_token_hash_is_the_phc_form_the_schema_checks_for() {
        let phc = hash_secret("c0ffee").expect("hashable");
        assert!(phc.starts_with("$argon2id$"), "{phc}");
    }
}
