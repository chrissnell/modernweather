//! The two halves of the boot sequence that live in this crate, against a real SQLite
//! database: the one-table schedule store (**D-41**) and what the boot sequence reads
//! before `plan()` runs.
//!
//! Not named `db.rs`: `ci/gates.toml` arms the `db-tests` gate on
//! `crates/mw-store/tests/db.rs`, which is implementation-plan.md M3-3's deliverable and
//! needs a real TimescaleDB container. Everything here runs against a file.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, TimeZone, Utc};
use mw_component::{ComponentId, OccurrenceKey, OccurrenceRecord, Outcome, ScheduleStore};
use mw_store::config_db;
use mw_store::SqliteScheduleStore;

/// A scratch directory per test, removed on drop -- the same shape `config_db`'s own
/// tests use, and for the same reason: `tempfile` would be a dev-dependency for twenty
/// lines.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mw-schedule-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
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

async fn migrated(path: &Path) -> sqlx::SqlitePool {
    config_db::open_and_migrate(path)
        .await
        .expect("the configuration schema applies from empty")
}

fn record(id: &str, day: u32, slot: u32, outcome: Outcome) -> OccurrenceRecord {
    OccurrenceRecord {
        key: OccurrenceKey::new(
            ComponentId::new(id).expect("a literal id"),
            NaiveDate::from_ymd_opt(2026, 11, day).expect("a real date"),
            slot,
        ),
        completed_at: Utc
            .with_ymd_and_hms(2026, 11, day, 7, 30, 0)
            .single()
            .expect("an unambiguous instant"),
        outcome,
    }
}

#[tokio::test]
async fn an_occurrence_round_trips_through_the_one_table() {
    let scratch = Scratch::new("round-trip");
    let pool = migrated(&scratch.db()).await;
    let store = SqliteScheduleStore::new(pool);
    let id = ComponentId::new("aprs:backyard").expect("a literal id");

    assert_eq!(
        store.latest(&id).await.expect("an empty table reads"),
        None,
        "a first start has no recorded occurrence"
    );

    let first = record("aprs:backyard", 2, 0, Outcome::Ok);
    store.record(&first).await.expect("the row is written");
    assert_eq!(store.latest(&id).await.expect("read back"), Some(first));
}

#[tokio::test]
async fn a_second_occurrence_replaces_the_first_rather_than_appending() {
    let scratch = Scratch::new("replace");
    let pool = migrated(&scratch.db()).await;
    let store = SqliteScheduleStore::new(pool);

    store
        .record(&record("rollup:daily", 2, 0, Outcome::Ok))
        .await
        .expect("written");
    let second = record("rollup:daily", 3, 1, Outcome::Failed);
    store.record(&second).await.expect("written");

    let all = store.all().await.expect("read");
    assert_eq!(
        all.len(),
        1,
        "one row per component, not a history: {all:#?}"
    );
    assert_eq!(all[0], second);
}

#[tokio::test]
async fn every_outcome_survives_the_column_it_is_stored_in() {
    let scratch = Scratch::new("outcomes");
    let pool = migrated(&scratch.db()).await;
    let store = SqliteScheduleStore::new(pool);

    // The migration's `CHECK (outcome IN (...))` and `Outcome::as_str` have to agree, and
    // a disagreement would be a write that fails at run time rather than at build time.
    for (i, outcome) in Outcome::ALL.into_iter().enumerate() {
        store
            .record(&record(&format!("tick:slot{i}"), 2, 0, outcome))
            .await
            .unwrap_or_else(|e| panic!("{outcome} is not storable: {e}"));
    }
    let mut stored: Vec<_> = store
        .all()
        .await
        .expect("read")
        .into_iter()
        .map(|r| r.outcome)
        .collect();
    stored.sort_by_key(|o| o.as_str());
    let mut expected = Outcome::ALL.to_vec();
    expected.sort_by_key(|o| o.as_str());
    assert_eq!(stored, expected);
}

#[tokio::test]
async fn all_is_one_query_for_every_component() {
    let scratch = Scratch::new("all");
    let pool = migrated(&scratch.db()).await;
    let store = SqliteScheduleStore::new(pool);
    for i in 0..5 {
        store
            .record(&record(&format!("uploader:wu{i}"), 2, 0, Outcome::Ok))
            .await
            .expect("written");
    }
    // The supervisor loads these once at startup so that misfire handling does not need a
    // query per component.
    assert_eq!(store.all().await.expect("read").len(), 5);
}

#[tokio::test]
async fn the_boot_sequence_reads_the_seeded_listener_and_an_absent_everything_else() {
    let scratch = Scratch::new("boot-reads");
    let pool = migrated(&scratch.db()).await;

    let listener = config_db::listener(&pool).await.expect("the row is seeded");
    assert_eq!(listener.http_port, 8080);
    assert_eq!(
        listener.grpc_port, 50051,
        "W3 D-92: gRPC keeps its own socket"
    );
    assert_eq!(listener.https_port, None);
    assert_eq!(
        listener.management_listen_addr, "127.0.0.1",
        "A-W5-3: the management bind defaults to loopback"
    );

    assert!(
        config_db::weather_database(&pool)
            .await
            .expect("the query runs")
            .is_none(),
        "no row means no db:postgres, which is a legitimate node (D-44)"
    );
    assert!(
        config_db::node(&pool)
            .await
            .expect("the query runs")
            .is_none(),
        "an empty node table means `not yet started`, which the boot sequence tolerates"
    );
}
