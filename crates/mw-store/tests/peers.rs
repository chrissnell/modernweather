//! The peer registry's **authorisation** properties, against a real SQLite database.
//!
//! Every test here was written against a defect a code review found, and each one failed
//! before the fix beside it. They are in one file because they are one question asked four
//! ways: *can a peer acquire authority it was not given?*
//!
//! | Test | The defect it pins |
//! |---|---|
//! | [`a_peer_cannot_take_over_a_station_another_peer_owns`] | the published snapshot took ownership from the **claim** rather than from the row the upsert had guarded, so the second peer to name a key became its owner in the only copy the gate reads |
//! | [`a_peer_cannot_take_over_a_local_station`] | the same hole, aimed at the collector's own sensor |
//! | [`an_enrolment_token_is_scoped_to_the_name_it_was_minted_for`] | `peer_enrolment_token.peer_name` was written and never read, so a token for a new node could be presented with an existing node's id |
//! | [`an_enrolment_cannot_clear_a_revocation_or_inherit_relay_grants`] | the upsert set `revoked_at = NULL` unconditionally and left `relay` and `peer_origins` alone, so a token holder inherited whatever the displaced peer had been granted |
//!
//! **Why SQLite and not a double.** The guard that matters is a `WHERE` clause on an
//! `ON CONFLICT DO UPDATE`, and the thing being asserted is that the *database* refused —
//! which is not a fact a mock can have. `crates/mw-store/tests/db.rs` is the PostgreSQL
//! half of the same principle.

use std::path::PathBuf;
use std::sync::Arc;

use mw_component::{PeerRegistry as _, PeerStation, PeerWriteError};
use mw_store::config_db::open_and_migrate;
use mw_store::{ConfigRegistry, PeerStore, RegistrySnapshot};
use mw_types::{OriginId, StationKey};
use sqlx::SqlitePool;

/// This node: the collector.
const ME: u8 = 9;

fn node(b: u8) -> OriginId {
    OriginId::new([b; 16])
}

/// A scratch directory per test, removed on drop.
///
/// `config_db`'s own test module has the same type for the same reason — `tempfile` would
/// be a dev-dependency for eight lines — and it is private to that module.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mw-peers-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("a writable scratch directory");
        Scratch(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("config.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A migrated configuration database and a peer store over it.
async fn store(scratch: &Scratch) -> (SqlitePool, Arc<PeerStore>) {
    let pool = open_and_migrate(&scratch.db())
        .await
        .expect("the configuration schema applies from empty");
    // The node row, because `with_peer_stations` separates local from peer-owned by it.
    sqlx::query(
        "INSERT INTO node (id, node_id, node_name, spool_path) \
         VALUES (1, ?, 'suncrest', '/tmp/spool')",
    )
    .bind(node(ME).as_bytes().as_slice())
    .execute(&pool)
    .await
    .expect("the node row is insertable");
    let registry = Arc::new(ConfigRegistry::new(RegistrySnapshot::default()));
    let store = Arc::new(PeerStore::new(pool.clone(), registry, node(ME)));
    store.republish().await.expect("an empty peer snapshot");
    (pool, store)
}

/// An enrolled, unrevoked `mtls-ca` peer.
async fn peer(pool: &SqlitePool, name: &str, id: u8) {
    sqlx::query("INSERT INTO peers (node_id, name, auth_kind) VALUES (?, ?, 'mtls-ca')")
        .bind(node(id).as_bytes().as_slice())
        .bind(name)
        .execute(pool)
        .await
        .expect("a peer row is insertable");
    sqlx::query("INSERT INTO peer_origins (node_id, origin_node_id) VALUES (?, ?)")
        .bind(node(id).as_bytes().as_slice())
        .bind(node(id).as_bytes().as_slice())
        .execute(pool)
        .await
        .expect("a peer may speak for itself");
}

fn station(key: &str) -> PeerStation {
    PeerStation {
        station: StationKey::new(key),
        name: key.to_string(),
        kind: "davis".to_string(),
        latitude: Some(39.64),
        longitude: Some(-105.2),
        altitude_m: Some(1800.0),
        timezone: Some("America/Denver".to_string()),
        aprs_callsign: None,
    }
}

/// **W7 D-36's whole point, at the registration layer.**
///
/// Peer A registers a station. Peer B — enrolled, unrevoked, entitled to speak for itself
/// and nothing else — declares the same key. B must not become its owner, because
/// `station_owner` is what `mw_collector::admit::Gate` authorises an envelope against: if B
/// owned it, B could write into A's history.
#[tokio::test]
async fn a_peer_cannot_take_over_a_station_another_peer_owns() {
    let scratch = Scratch::new("steal-peer");
    let (pool, store) = store(&scratch).await;
    peer(&pool, "satellite-a", 1).await;
    peer(&pool, "satellite-b", 2).await;

    store
        .record_hello(
            node(1),
            "modernweather 1.0",
            &[station("satellite-a:backyard")],
        )
        .await
        .expect("peer A registers its own station");
    let snapshot = store.republish().await.expect("published");
    assert_eq!(
        snapshot.owner(&StationKey::new("satellite-a:backyard")),
        Some(node(1))
    );

    // Peer B declares A's key. Reported as success -- B's `Hello` is not *wrong*, it simply
    // does not own that key -- and nothing about the key changes.
    store
        .record_hello(
            node(2),
            "modernweather 1.0",
            &[station("satellite-a:backyard")],
        )
        .await
        .expect("a Hello naming another node's station is not an error");
    let snapshot = store.republish().await.expect("published");
    assert_eq!(
        snapshot.owner(&StationKey::new("satellite-a:backyard")),
        Some(node(1)),
        "the second peer to name a key must not become its owner: `station_owner` is what \
         authorises an envelope, so this is the difference between D-36 holding and not"
    );
    // …and the metadata is still A's, not B's.
    let owning: Vec<u8> =
        sqlx::query_scalar("SELECT owning_node_id FROM stations WHERE station_key = ?")
            .bind("satellite-a:backyard")
            .fetch_one(&pool)
            .await
            .expect("the row exists");
    assert_eq!(owning, node(1).as_bytes().to_vec());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM stations")
        .fetch_one(&pool)
        .await
        .expect("countable");
    assert_eq!(rows, 1, "and no second row was created under the same key");
}

/// The same hole aimed at the collector's own sensor, which is the worst version of it: a
/// local station is one an operator configured and `plan()` was derived from.
#[tokio::test]
async fn a_peer_cannot_take_over_a_local_station() {
    let scratch = Scratch::new("steal-local");
    let (pool, store) = store(&scratch).await;
    peer(&pool, "satellite-a", 1).await;
    sqlx::query(
        "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind, \
                               transport_kind, transport_port) \
         VALUES ('suncrest:roof', ?, 'Roof', 'weatherlink-live', 'from_payload', \
                 'udp_in', 22222)",
    )
    .bind(node(ME).as_bytes().as_slice())
    .execute(&pool)
    .await
    .expect("a local station row is insertable");

    store
        .record_hello(node(1), "modernweather 1.0", &[station("suncrest:roof")])
        .await
        .expect("a Hello naming this node's station is not an error");
    let snapshot = store.republish().await.expect("published");
    assert_eq!(
        snapshot.owner(&StationKey::new("suncrest:roof")),
        None,
        "a local station is not in the PEER snapshot at all -- `with_peer_stations` reads \
         `owning_node_id <> this node`, so the configured station snapshot is the only \
         answer and a peer cannot shadow it"
    );
    let owning: Vec<u8> =
        sqlx::query_scalar("SELECT owning_node_id FROM stations WHERE station_key = ?")
            .bind("suncrest:roof")
            .fetch_one(&pool)
            .await
            .expect("the row exists");
    assert_eq!(
        owning,
        node(ME).as_bytes().to_vec(),
        "and the row still belongs to this node"
    );
    let name: String = sqlx::query_scalar("SELECT name FROM stations WHERE station_key = ?")
        .bind("suncrest:roof")
        .fetch_one(&pool)
        .await
        .expect("the row exists");
    assert_eq!(name, "Roof", "its metadata was not rewritten either");
}

/// **A token is scoped to the peer the operator minted it for.**
///
/// `peer_enrolment_token.peer_name` exists for this and was read by nobody. Without the
/// check, a token for a new `satellite-d` could be presented with `relay-west`'s node id —
/// and the `DO UPDATE` would replace `relay-west`'s credential while leaving its name on
/// the row, so nothing an operator reads would have changed.
#[tokio::test]
async fn an_enrolment_token_is_scoped_to_the_name_it_was_minted_for() {
    let scratch = Scratch::new("token-scope");
    let (pool, store) = store(&scratch).await;
    let token = store
        .mint_enrolment_token("satellite-d", std::time::Duration::from_secs(3600))
        .await
        .expect("a token is mintable");
    assert!(store.enrolment_open().await.expect("readable"));

    // The right token, the wrong name.
    let wrong = mw_component::Enrolment {
        token: token.clone().into(),
        node: node(4),
        name: "someone-else".to_string(),
        software: "modernweather 1.0".to_string(),
        spki_sha256: [0xab; 32],
        stations: Vec::new(),
    };
    let e = store
        .enrol(&wrong)
        .await
        .expect_err("a token is not a licence to pick a name");
    assert!(matches!(e, PeerWriteError::BadToken), "{e}");
    let peers: i64 = sqlx::query_scalar("SELECT count(*) FROM peers")
        .fetch_one(&pool)
        .await
        .expect("countable");
    assert_eq!(peers, 0, "nothing was enrolled");
    assert!(
        store.enrolment_open().await.expect("readable"),
        "and the token was not burned, so the operator's own attempt still works"
    );

    // The right token and the right name.
    let right = mw_component::Enrolment {
        token: token.into(),
        name: "satellite-d".to_string(),
        ..wrong
    };
    let enrolled = store.enrol(&right).await.expect("the scoped name enrols");
    assert_eq!(enrolled.peer.name.as_str(), "satellite-d");
    assert_eq!(enrolled.pinned_spki_sha256, Some([0xab; 32]));
    assert!(
        !store.enrolment_open().await.expect("readable"),
        "single-use: the token was burned in the same transaction (§5.3)"
    );
}

/// **An enrolment cannot un-revoke a peer, and cannot inherit its relay grants.**
///
/// Both were true of the first version: the upsert set `revoked_at = NULL` unconditionally,
/// and `relay` and `peer_origins` were not in it at all — so a token holder who presented a
/// revoked relay's node id took over a row that could already speak for three nodes.
#[tokio::test]
async fn an_enrolment_cannot_clear_a_revocation_or_inherit_relay_grants() {
    let scratch = Scratch::new("takeover");
    let (pool, store) = store(&scratch).await;
    // A revoked relay granted three origins.
    sqlx::query(
        "INSERT INTO peers (node_id, name, auth_kind, relay, revoked_at) \
         VALUES (?, 'relay-west', 'mtls-ca', 1, unixepoch())",
    )
    .bind(node(7).as_bytes().as_slice())
    .execute(&pool)
    .await
    .expect("insertable");
    for origin in [7u8, 2, 3] {
        sqlx::query("INSERT INTO peer_origins (node_id, origin_node_id) VALUES (?, ?)")
            .bind(node(7).as_bytes().as_slice())
            .bind(node(origin).as_bytes().as_slice())
            .execute(&pool)
            .await
            .expect("insertable");
    }
    let token = store
        .mint_enrolment_token("relay-west", std::time::Duration::from_secs(3600))
        .await
        .expect("mintable");

    // The token even names the right peer. It still must not resurrect it.
    let e = store
        .enrol(&mw_component::Enrolment {
            token: token.into(),
            node: node(7),
            name: "relay-west".to_string(),
            software: "modernweather 1.0".to_string(),
            spki_sha256: [0xcd; 32],
            stations: Vec::new(),
        })
        .await
        .expect_err("re-trusting a revoked peer is an operator's decision, not a peer's");
    assert!(matches!(e, PeerWriteError::BadToken), "{e}");

    let revoked: Option<i64> = sqlx::query_scalar("SELECT revoked_at FROM peers WHERE node_id = ?")
        .bind(node(7).as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .expect("the row exists");
    assert!(revoked.is_some(), "the revocation stands");
    let material: Option<String> =
        sqlx::query_scalar("SELECT auth_material FROM peers WHERE node_id = ?")
            .bind(node(7).as_bytes().as_slice())
            .fetch_one(&pool)
            .await
            .expect("the row exists");
    assert_eq!(material, None, "and the credential was not replaced");

    // The same attempt against an **unrevoked** peer of a different name is also refused,
    // which is the takeover the name check closes.
    peer(&pool, "satellite-a", 1).await;
    let token = store
        .mint_enrolment_token("satellite-d", std::time::Duration::from_secs(3600))
        .await
        .expect("mintable");
    let e = store
        .enrol(&mw_component::Enrolment {
            token: token.into(),
            node: node(1),
            name: "satellite-d".to_string(),
            software: "modernweather 1.0".to_string(),
            spki_sha256: [0xef; 32],
            stations: Vec::new(),
        })
        .await
        .expect_err("a token may not adopt a node id that belongs to another peer");
    assert!(matches!(e, PeerWriteError::BadToken), "{e}");
    let name: String = sqlx::query_scalar("SELECT name FROM peers WHERE node_id = ?")
        .bind(node(1).as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .expect("the row exists");
    assert_eq!(name, "satellite-a", "`satellite-a` is still itself");
}

/// A legitimate re-enrolment — same node, same name, new key — works, and **drops the
/// relay grant**.
///
/// Fail-safe and deliberate: if the key holder has changed, the authority to speak for
/// other nodes has to be re-granted by an operator rather than inherited.
#[tokio::test]
async fn a_re_enrolment_keeps_the_identity_and_drops_the_grants() {
    let scratch = Scratch::new("re-enrol");
    let (pool, store) = store(&scratch).await;
    sqlx::query(
        "INSERT INTO peers (node_id, name, auth_kind, auth_material, relay) \
         VALUES (?, 'relay-west', 'mtls-pinned', 'aa', 1)",
    )
    .bind(node(7).as_bytes().as_slice())
    .execute(&pool)
    .await
    .expect("insertable");
    for origin in [7u8, 2] {
        sqlx::query("INSERT INTO peer_origins (node_id, origin_node_id) VALUES (?, ?)")
            .bind(node(7).as_bytes().as_slice())
            .bind(node(origin).as_bytes().as_slice())
            .execute(&pool)
            .await
            .expect("insertable");
    }
    let token = store
        .mint_enrolment_token("relay-west", std::time::Duration::from_secs(3600))
        .await
        .expect("mintable");

    let enrolled = store
        .enrol(&mw_component::Enrolment {
            token: token.into(),
            node: node(7),
            name: "relay-west".to_string(),
            software: "modernweather 1.4.2".to_string(),
            spki_sha256: [0x11; 32],
            stations: Vec::new(),
        })
        .await
        .expect("a rotated keypair under the same identity re-enrols");
    assert_eq!(enrolled.peer.name.as_str(), "relay-west");
    assert!(!enrolled.peer.relay, "the relay grant is dropped");
    assert_eq!(
        enrolled.peer.origins,
        [node(7)].into_iter().collect(),
        "and so is every origin but its own"
    );
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM peer_origins WHERE node_id = ?")
        .bind(node(7).as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .expect("countable");
    assert_eq!(grants, 1);
}

/// The total a peer may own is bounded, and the refusal is **whole**: registering some of a
/// satellite's stations and silently refusing envelopes for the rest is the worse failure.
#[tokio::test]
async fn the_stations_one_peer_may_own_are_bounded() {
    let scratch = Scratch::new("station-cap");
    let (pool, store) = store(&scratch).await;
    peer(&pool, "satellite-a", 1).await;

    let limit = usize::try_from(mw_store::peers::MAX_OWNED_STATIONS).expect("a small limit");
    let many: Vec<PeerStation> = (0..=limit)
        .map(|i| station(&format!("satellite-a:s{i}")))
        .collect();
    let e = store
        .record_hello(node(1), "modernweather 1.0", &many)
        .await
        .expect_err("one more than the limit is refused");
    assert!(matches!(e, PeerWriteError::TooManyStations { .. }), "{e}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM stations")
        .fetch_one(&pool)
        .await
        .expect("countable");
    assert_eq!(
        rows, 0,
        "the whole transaction rolled back, so none of them was registered"
    );

    // Exactly the limit is accepted, so the refusal above is the bound and not the write.
    store
        .record_hello(node(1), "modernweather 1.0", &many[..limit])
        .await
        .expect("the limit itself is allowed");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM stations")
        .fetch_one(&pool)
        .await
        .expect("countable");
    assert_eq!(rows, i64::try_from(limit).expect("a small limit"));
}
