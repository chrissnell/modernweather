//! The tests that need a real TimescaleDB — and **fail rather than skip** when they do not
//! have one (W3 Decision 84).
//!
//! # Why this file's existence is itself a deliverable
//!
//! `ci/gates.toml` arms the `db-tests` gate on this path: the gate was declared by W6 with
//! `arms_when = "crates/mw-store/tests/db.rs"` and reported `DEFERRED` until the file
//! appeared. It is one of G-4's four undeclared gates and the one whose entire purpose is
//! to make *"green because it did not run"* unreachable — the failure mode
//! `docs/reconciliation.md` G-1 documents, where the `conformance` gate reported green by
//! deferral for two milestones while the suite it guarded had never run.
//!
//! So this file gets the failure mode right in **both** directions, and neither half is
//! enough on its own:
//!
//! 1. **A missing database is a panic, not a skip.** [`url`] has no fallback. Contrast
//!    `contract/conformance/tests/conformance.rs`, which self-skips when
//!    `CONFORMANCE_BASE_URL` is unset — correct for a crate that must stay green in a
//!    checkout with no server, and the reason the conformance gates drive the *binary*
//!    rather than relying on `cargo test` to notice.
//! 2. **Every test here is `#[ignore]`d, and `cargo xtask db-tests` asserts that exactly
//!    the declared ones ran and passed.** `#[ignore]` is what keeps the `test` job — which
//!    has no service container — green and honest; the declared list is what stops the
//!    gate passing because a filter matched nothing, a test was renamed, or a body was
//!    emptied. That is `xtask/src/emulators.rs`'s lesson applied here: a count catches a
//!    deleted test and does not catch a rename, so the names bind identity.
//!
//! Run them with `cargo xtask db-tests`, which creates the database, applies `migrations/`
//! and sets `MW_DB_TESTS_URL`. There is no second way in, by design.
//!
//! # Isolation
//!
//! One database, shared by every test, and **isolation by station key** rather than by
//! database: each test owns a key nobody else uses and deletes its own rows first. The
//! alternative — a database per test — means dropping and recreating the whole aggregate
//! hierarchy per test, which is seconds of DDL each for a property none of them is about.
//! `observations`' unique index is `(station_id, time)`, so a key is a partition.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use mw_component::{ReadingSink, StationRegistry};
use mw_store::config_db::StationRow;
use mw_store::{version, ConfigRegistry, RegistrySnapshot, TimescaleWriter};
use mw_types::{Envelope, OriginId, Provenance, Reading, Seq, StationKey, DIMENSIONS, METRICS};
use sqlx::{PgPool, Row};

/// The connection string, or a failure that names the gate.
///
/// **No fallback, no skip.** W3 Decision 84 is that these tests fail CI if they skip, and
/// an `Option` here with a `return` at every call site is how that decision would quietly
/// stop holding — so the absence is a panic and the message says what to run.
fn url() -> String {
    match std::env::var("MW_DB_TESTS_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => panic!(
            "MW_DB_TESTS_URL is not set.\n\
             These tests need a real TimescaleDB and W3 Decision 84 makes skipping them a \
             CI failure rather than a pass, so this is a panic and not a `return`.\n\
             Run them with `cargo xtask db-tests`, which creates the database, applies \
             migrations/ and sets this variable. CI runs them in the `db-tests` job \
             against timescale/timescaledb-ha:pg17 (DESIGN.md D-2)."
        ),
    }
}

/// A pool against a migrated database, with `station`'s rows removed.
///
/// The migrator is `sqlx`'s own and is idempotent, so every test may call this: the first
/// one applies `migrations/`, the rest find `_sqlx_migrations` already at HEAD. It is also
/// the **only** place in this repository where the real `sqlx` migrator runs over the
/// weather schema — `ci/db/migrate-twice.sh` and `cargo xtask conformance` both shell out
/// to `psql` — so a migration that `psql -1 -f` accepts and `sqlx::migrate` does not is a
/// difference this notices.
async fn prepare(stations: &[&str]) -> PgPool {
    // A short acquire timeout rather than `sqlx`'s 30 s default: the common failure here is
    // "there is no server", and the gate's whole job is to say so. Eight tests waiting 30 s
    // each turns a four-second verdict into a four-minute one, which is long enough that
    // somebody reads it as a hung job rather than as the answer.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url())
        .await
        .expect("MW_DB_TESTS_URL names a reachable server");

    // The daemon never issues DDL at startup (**D-8**), and the migrations assume
    // `timescaledb` is already installed — exactly as `migrate-twice.sh` and the
    // conformance harness assume it. A test harness is where that assumption is satisfied.
    sqlx::query("CREATE EXTENSION IF NOT EXISTS timescaledb")
        .execute(&pool)
        .await
        .expect("the server has the timescaledb extension available (D-2)");
    // Best effort: the plain `timescale/timescaledb` images have no toolkit, and
    // `mw_store::version::check` treats its absence as a warning rather than an error
    // because W4 owns the `stats_agg` question (OQ-3).
    let _ = sqlx::query("CREATE EXTENSION IF NOT EXISTS timescaledb_toolkit")
        .execute(&pool)
        .await;

    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("migrations/ applies through the sqlx migrator");

    for station in stations {
        // `stations` cascades to `station_metrics` and the two caches; `observations` has
        // no foreign key to it (W4 §2.1 records why), so it is deleted separately.
        sqlx::query("DELETE FROM observations WHERE station_id = $1")
            .bind(station)
            .execute(&pool)
            .await
            .expect("the test's own rows are removable");
        sqlx::query("DELETE FROM stations WHERE station_id = $1")
            .bind(station)
            .execute(&pool)
            .await
            .expect("the test's own projection row is removable");
    }
    pool
}

/// A `config.stations` row with coordinates, for the projection.
fn row(key: &str) -> StationRow {
    StationRow {
        station_key: key.into(),
        name: format!("{key} (configured)"),
        kind: "weatherlink-live".into(),
        enabled: true,
        identity_kind: "from_payload".into(),
        presented_key: Some(key.to_ascii_uppercase()),
        ingest_secret_hash: None,
        latitude: Some(39.7392),
        longitude: Some(-104.9903),
        altitude_m: Some(1609.0),
        timezone: Some("America/Denver".into()),
        snow_base_distance_mm: Some(1500.0),
        transport_kind: Some("udp_in".into()),
        transport_dev: None,
        transport_baud: None,
        transport_host: None,
        transport_port: Some(22222),
        transport_url: None,
        transport_method: None,
        transport_auth: None,
        transport_streaming: None,
        transport_mount: None,
        transport_group: None,
        transport_topic: None,
        transport_tls_ca_path: None,
        transport_tls_cert_path: None,
        transport_tls_key_path: None,
        wll_sensor_mapping: Some("th:1,wind:1,rain:1,baro".into()),
        wll_poll_interval_seconds: None,
    }
}

/// The writer, over a registry that knows `station`.
fn writer(pool: &PgPool, station: &str) -> TimescaleWriter {
    let registry: Arc<dyn StationRegistry> = Arc::new(ConfigRegistry::new(RegistrySnapshot::new(
        &[row(station)],
        std::iter::empty(),
    )));
    TimescaleWriter::new(pool.clone(), registry)
        .expect("the writer places every stored dimension metrics/registry.toml declares")
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().expect("in range")
}

/// One envelope, with the reading the caller built.
fn envelope(station: &str, observed_at: DateTime<Utc>, seq: u64, reading: Reading) -> Envelope {
    Envelope {
        station: StationKey::new(station),
        observed_at,
        received_at: observed_at,
        provenance: Provenance::admitted(OriginId::new([9; 16]), Seq::new(seq)),
        reading: Arc::new(Reading {
            time: observed_at,
            station_id: station.to_string(),
            ..reading
        }),
    }
}

// ---------------------------------------------------------------------------------------
// The constraint this item was handed from M2-4
// ---------------------------------------------------------------------------------------

/// **`mw_store::version::probe` against a real server.** Its SQL, its column aliases and
/// its decoded types have no other test, because no gate ran PostgreSQL for `mw-store`
/// until this file armed one.
///
/// The *comparison* [`version::check`] makes is a pure function and is tested against
/// versions no container here runs — that is the whole reason it is a pure function. What
/// a unit test cannot cover is the half this asserts: that the query parses, that
/// `pg_major`, `ts` and `toolkit` are the aliases the row is read back under, and that
/// `int4`, `Option<String>` and `bool` are what they decode as. A typo in any of those is
/// a startup assertion that fails on every server, including a correct one — and **D-62**
/// is allowed to be *skipped* for a server that did not answer in time (**D-27**), so that
/// failure would read as "the database was slow" rather than as a defect in this build.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn the_version_probe_reads_the_server_it_is_pointed_at() {
    let observed = version::probe(&url())
        .await
        .expect("the probe's SQL runs and its columns decode");
    assert!(
        observed.pg_major >= version::REQUIRED_PG_MAJOR,
        "the container is PostgreSQL {} and this build requires {}",
        observed.pg_major,
        version::REQUIRED_PG_MAJOR
    );
    let reported = observed
        .timescale
        .as_deref()
        .expect("`extversion` decoded as a Some for an installed extension");
    assert!(
        version::Version::parse(reported).is_some(),
        "`{reported}` is not a dotted version, so `check` would answer Unreadable"
    );
    // The same call the boot sequence and `db:postgres` both make. `Ok(false)` is the
    // toolkit warning and is a pass.
    version::check(&observed).expect("the container satisfies D-42's required versions");

    // `probe` closes its connection on both paths, which is what keeps the server from
    // logging an unexpected EOF. Asserted by running it again: a leaked connection would
    // accumulate, and the pool's own limit is what would eventually say so.
    for _ in 0..3 {
        version::probe(&url()).await.expect("repeatable");
    }
}

// ---------------------------------------------------------------------------------------
// "Readings land in `observations`"
// ---------------------------------------------------------------------------------------

/// The headline: a batch goes in, and comes back as the values that went in.
///
/// Also **D-26** at the only layer where it is observable: a metric the station did not
/// report is `NULL` in the column, not `0.0`. Asserted with `IS NULL` rather than by
/// reading a value back, because a `0.0` read into an `Option<f32>` is a `Some(0.0)` and
/// the question is whether the *column* is null.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn readings_land_in_observations_and_an_unreported_metric_is_null() {
    let station = "db-tests:lands";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    let batch = vec![
        envelope(
            station,
            at(1_767_225_600),
            1,
            Reading {
                out_temp: Some(41.5),
                out_humidity: Some(62.0),
                wind_speed: Some(7.25),
                station_name: Some("the back yard".into()),
                station_type: Some("weatherlink-live".into()),
                ..Reading::default()
            },
        ),
        envelope(
            station,
            at(1_767_225_602),
            2,
            Reading {
                out_temp: Some(41.0),
                ..Reading::default()
            },
        ),
    ];
    let stored = w.store(&batch).await.expect("the batch stores");
    assert_eq!(stored.stored, 2);
    assert_eq!(stored.rows, 2, "two distinct instants, two rows");

    let rows = sqlx::query(
        "SELECT time, outtemp, outhumidity, windspeed, stationname, stationtype, \
                (barometer IS NULL) AS barometer_is_null, \
                (rainrate IS NULL) AS rainrate_is_null, \
                (extratext1 IS NULL) AS extratext1_is_null, \
                (stormstart IS NULL) AS stormstart_is_null, \
                (lightning_day IS NULL) AS lightning_day_is_null, \
                (dateutc IS NULL) AS dateutc_is_null \
           FROM observations WHERE station_id = $1 ORDER BY time",
    )
    .bind(station)
    .fetch_all(&pool)
    .await
    .expect("the rows read back");
    assert_eq!(rows.len(), 2);

    assert_eq!(rows[0].get::<DateTime<Utc>, _>("time"), at(1_767_225_600));
    assert_eq!(rows[0].get::<Option<f32>, _>("outtemp"), Some(41.5));
    assert_eq!(rows[0].get::<Option<f32>, _>("outhumidity"), Some(62.0));
    assert_eq!(rows[0].get::<Option<f32>, _>("windspeed"), Some(7.25));
    assert_eq!(
        rows[0].get::<Option<String>, _>("stationname").as_deref(),
        Some("the back yard"),
        "the two attribute dimensions are columns too"
    );
    assert_eq!(
        rows[0].get::<Option<String>, _>("stationtype").as_deref(),
        Some("weatherlink-live")
    );

    // One column of each of the five `sql_type`s the registry declares, all unreported.
    for row in &rows {
        for column in [
            "barometer_is_null",
            "rainrate_is_null",
            "extratext1_is_null",
            "stormstart_is_null",
            "lightning_day_is_null",
            "dateutc_is_null",
        ] {
            assert!(
                row.get::<bool, _>(column),
                "{column}: an unreported metric is NULL and never 0.0 (D-26)"
            );
        }
    }
    // The second reading reported only a temperature, so every other column of it is null
    // — including the two dimensions, which the first reading did fill.
    assert_eq!(rows[1].get::<Option<f32>, _>("outtemp"), Some(41.0));
    assert_eq!(rows[1].get::<Option<f32>, _>("outhumidity"), None);
    assert_eq!(rows[1].get::<Option<String>, _>("stationname"), None);
}

/// **Every column the registry declares exists in `observations`, with the declared type,
/// and nothing in `observations` is unaccounted for.**
///
/// This is what makes the writer's claim — "the column list is the registry's, not a
/// literal" — checkable rather than asserted, and it is checked against the catalogue
/// rather than by writing a value into all 212: the statement the other tests run already
/// *names* every column, so a missing one fails them, and what a catalogue comparison adds
/// is the two things an `INSERT` cannot see. A column the registry declares and the
/// migration never created, and a column the migration created that the registry has
/// forgotten — which is the direction `cargo xtask migrate` cannot catch either, because
/// it emits from the registry and has nothing to compare the database against.
///
/// `extra` is the one expected extra: W4 §4's escape hatch for unmodelled vendor fields,
/// deliberately outside `metrics/registry.toml` because the registry is the source of
/// truth for *modelled* metrics (OQ-W4-3). It is named here rather than filtered silently,
/// so a *second* unregistered column is a failure.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn the_observations_columns_are_exactly_what_the_registry_declares() {
    let pool = prepare(&[]).await;

    let actual: BTreeSet<(String, String)> = sqlx::query(
        "SELECT column_name, udt_name FROM information_schema.columns \
           WHERE table_name = 'observations' AND table_schema = current_schema()",
    )
    .fetch_all(&pool)
    .await
    .expect("the catalogue reads")
    .into_iter()
    .map(|r| {
        (
            r.get::<String, _>("column_name"),
            r.get::<String, _>("udt_name"),
        )
    })
    .collect();
    assert!(
        !actual.is_empty(),
        "`observations` has no columns, so migrations/ did not apply"
    );

    let mut declared: BTreeSet<(String, String)> = DIMENSIONS
        .iter()
        .filter(|d| d.stored)
        .map(|d| (d.column.to_string(), udt(d.sql_type)))
        .chain(
            METRICS
                .iter()
                .map(|m| (m.column.to_string(), udt(m.sql_type))),
        )
        .collect();
    declared.insert(("extra".to_string(), "jsonb".to_string()));

    let only_declared: Vec<_> = declared.difference(&actual).collect();
    let only_actual: Vec<_> = actual.difference(&declared).collect();
    assert!(
        only_declared.is_empty(),
        "metrics/registry.toml declares {} column(s) `observations` does not have, so the \
         writer binds a column nothing created: {only_declared:?}. Run `cargo xtask \
         migrate` and apply the migration it emits.",
        only_declared.len()
    );
    assert!(
        only_actual.is_empty(),
        "`observations` has {} column(s) the registry does not declare, so the writer \
         never fills them: {only_actual:?}. Either add the [[metric]] entry or drop the \
         column -- `extra` is the one sanctioned exception (W4 §4).",
        only_actual.len()
    );
}

/// One metric of each `sql_type` the registry declares, written and read back.
///
/// The catalogue test above compares *names and types*; this is the half it cannot see —
/// that the writer's `$n::<type>[]` cast and `sqlx`'s binding of the Rust type agree with
/// the column. A `text` metric bound as a float, or a `timestamptz` bound as a string, is
/// a statement PostgreSQL rejects or a value it silently coerces, and five columns is the
/// whole of the type surface.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn one_metric_of_each_declared_type_round_trips() {
    let station = "db-tests:types";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    let observed_at = at(1_767_312_000);
    let reported = at(1_767_311_000);
    w.store(&[envelope(
        station,
        observed_at,
        1,
        Reading {
            // float4
            barometer: Some(29.9375),
            // int4
            lightning_day: Some(17),
            // int8 -- the registry's only one, and deliberately past 2^32 so a column
            // narrowed to int4 fails rather than truncating quietly.
            date_utc: Some(4_294_967_296),
            // text
            extra_text1: Some("a note from the station".into()),
            // timestamptz
            storm_start: Some(reported),
            ..Reading::default()
        },
    )])
    .await
    .expect("stores");

    let row = sqlx::query(
        "SELECT barometer, lightning_day, dateutc, extratext1, stormstart \
           FROM observations WHERE station_id = $1 AND time = $2",
    )
    .bind(station)
    .bind(observed_at)
    .fetch_one(&pool)
    .await
    .expect("one row");
    assert_eq!(row.get::<Option<f32>, _>("barometer"), Some(29.9375));
    assert_eq!(row.get::<Option<i32>, _>("lightning_day"), Some(17));
    assert_eq!(row.get::<Option<i64>, _>("dateutc"), Some(4_294_967_296));
    assert_eq!(
        row.get::<Option<String>, _>("extratext1").as_deref(),
        Some("a note from the station")
    );
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("stormstart"),
        Some(reported)
    );

    // A text metric is inventory too, which `Reading::get`'s `f64` would have missed.
    let keys: BTreeSet<String> =
        sqlx::query_scalar("SELECT metric_key FROM station_metrics WHERE station_id = $1")
            .bind(station)
            .fetch_all(&pool)
            .await
            .expect("read")
            .into_iter()
            .collect();
    assert!(keys.contains("extra_text1"), "got {keys:?}");
    assert!(keys.contains("storm_start"), "got {keys:?}");
}

/// The PostgreSQL type name for a declared `sql_type`.
///
/// `information_schema.columns.udt_name` is the underlying type's name, which for
/// `float4`/`int4`/`int8` is the alias itself and for `timestamptz` is `timestamptz`.
fn udt(sql_type: mw_types::SqlType) -> String {
    sql_type.ddl().to_string()
}

// ---------------------------------------------------------------------------------------
// "A replayed prefix produces no duplicate rows"
// ---------------------------------------------------------------------------------------

/// `ON CONFLICT (station_id, time) DO UPDATE`, which is what makes a durable sink's
/// cancelled delivery safe to replay (W3 §2.3).
///
/// Three deliveries of overlapping prefixes of the same 40 envelopes, as a cancelled
/// occurrence and its retry would produce them. The row count must be 40 throughout, and
/// the stored values must be the batch's — not the first delivery's, which a `DO NOTHING`
/// would have left behind and which is the version of this that passes the count and
/// loses a correction.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_replayed_prefix_produces_no_duplicate_rows() {
    let station = "db-tests:replay";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    let batch: Vec<Envelope> = (0..40u8)
        .map(|i| {
            envelope(
                station,
                at(1_767_398_400 + i64::from(i) * 2),
                u64::from(i) + 1,
                Reading {
                    out_temp: Some(40.0 + f32::from(i)),
                    ..Reading::default()
                },
            )
        })
        .collect();

    // The whole batch, then a prefix, then the whole batch again.
    for slice in [&batch[..], &batch[..17], &batch[..]] {
        w.store(slice).await.expect("every delivery stores");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM observations WHERE station_id = $1")
        .bind(station)
        .fetch_one(&pool)
        .await
        .expect("counted");
    assert_eq!(count, 40, "a replayed prefix produced no duplicate rows");

    // And every value is the one the envelope carried, which `DO NOTHING` would also
    // satisfy here -- so the next assertion is the one that distinguishes them.
    let temps: Vec<Option<f32>> =
        sqlx::query_scalar("SELECT outtemp FROM observations WHERE station_id = $1 ORDER BY time")
            .bind(station)
            .fetch_all(&pool)
            .await
            .expect("read back");
    assert_eq!(
        temps,
        (0..40u8)
            .map(|i| Some(40.0 + f32::from(i)))
            .collect::<Vec<_>>()
    );

    // A corrected reading for an instant already stored must replace it. `DO UPDATE` is
    // what W3 §2.3 specifies and `DO NOTHING` is the plausible alternative that would pass
    // the count above and silently keep the old value.
    let corrected = envelope(
        station,
        batch[5].observed_at,
        6,
        Reading {
            out_temp: Some(-11.5),
            barometer: Some(30.01),
            ..Reading::default()
        },
    );
    w.store(&[corrected]).await.expect("the correction stores");
    let row = sqlx::query(
        "SELECT outtemp, barometer FROM observations WHERE station_id = $1 AND time = $2",
    )
    .bind(station)
    .bind(batch[5].observed_at)
    .fetch_one(&pool)
    .await
    .expect("still exactly one row for that instant");
    assert_eq!(row.get::<Option<f32>, _>("outtemp"), Some(-11.5));
    assert_eq!(row.get::<Option<f32>, _>("barometer"), Some(30.01));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM observations WHERE station_id = $1")
        .bind(station)
        .fetch_one(&pool)
        .await
        .expect("counted");
    assert_eq!(
        count, 40,
        "the correction replaced a row rather than adding one"
    );
}

/// Two envelopes in **one batch** sharing a `(station_id, time)`.
///
/// Not a corner case: `Ingress::admit` substitutes the frame's arrival instant for every
/// `PartialReading` the frame did not timestamp, so a Davis `LOOP 4` response is four
/// readings at one instant. PostgreSQL refuses `ON CONFLICT DO UPDATE` twice against one
/// row in one statement — *"cannot affect row a second time"* — so without the writer's
/// collapse the **whole batch** fails, every time, for that station. This is the test that
/// would have found it.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn envelopes_sharing_an_instant_in_one_batch_collapse_instead_of_failing() {
    let station = "db-tests:collapse";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    let observed_at = at(1_767_484_800);
    let batch: Vec<Envelope> = (0..4u8)
        .map(|i| {
            envelope(
                station,
                observed_at,
                u64::from(i) + 1,
                Reading {
                    out_temp: Some(30.0 + f32::from(i)),
                    ..Reading::default()
                },
            )
        })
        .collect();

    let stored = w
        .store(&batch)
        .await
        .expect("a batch with a repeated instant is stored, not refused");
    assert_eq!(
        stored.stored, 4,
        "all four are durably handled, so the cursor advances past all four"
    );
    assert_eq!(stored.rows, 1, "and they are one row");

    let temp: Option<f32> =
        sqlx::query_scalar("SELECT outtemp FROM observations WHERE station_id = $1 AND time = $2")
            .bind(station)
            .bind(observed_at)
            .fetch_one(&pool)
            .await
            .expect("one row");
    assert_eq!(
        temp,
        Some(33.0),
        "the last envelope wins, which is what a sequence of upserts would have left"
    );
}

// ---------------------------------------------------------------------------------------
// "`station_metrics` reflects what has been written", and the `stations` projection
// ---------------------------------------------------------------------------------------

/// **D-56**, both halves, in one transaction with the rows they describe.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn the_projections_reflect_what_has_been_written() {
    let station = "db-tests:projections";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    let first = at(1_767_571_200);
    let middle = at(1_767_571_230);
    let second = at(1_767_571_260);
    // **Two readings in the first batch, at different instants.** A batch of one cannot
    // tell `first_seen = min` from `first_seen = max`, and binding one instant to both
    // columns is the obvious implementation: measured, it stored a station's `first_seen`
    // thirty seconds after the earliest reading it had ever been sent.
    w.store(&[
        envelope(
            station,
            first,
            1,
            Reading {
                out_temp: Some(44.0),
                barometer: Some(29.92),
                ..Reading::default()
            },
        ),
        envelope(
            station,
            middle,
            2,
            Reading {
                out_temp: Some(44.5),
                ..Reading::default()
            },
        ),
    ])
    .await
    .expect("stores");

    // `stations`: the configured name, type, zone and coordinates, with the altitude
    // converted from the configuration's metres to the projection's feet.
    let row = sqlx::query(
        "SELECT name, type, timezone, latitude, longitude, altitude_ft, \
                snow_base_distance_mm, first_seen, last_seen \
           FROM stations WHERE station_id = $1",
    )
    .bind(station)
    .fetch_one(&pool)
    .await
    .expect("the writer created the projection row");
    assert_eq!(
        row.get::<String, _>("name"),
        format!("{station} (configured)"),
        "the configuration is authoritative for the name, not the reading"
    );
    assert_eq!(row.get::<String, _>("type"), "weatherlink-live");
    assert_eq!(row.get::<String, _>("timezone"), "America/Denver");
    assert_eq!(row.get::<Option<f64>, _>("latitude"), Some(39.7392));
    let feet = row
        .get::<Option<f64>, _>("altitude_ft")
        .expect("the altitude is projected");
    assert!(
        (feet - 1609.0 / 0.3048).abs() < 1e-6,
        "1609 m is {feet} ft, which should be ~5279.5 -- the column names carry the units \
         so the conversion cannot be skipped"
    );
    assert_eq!(
        row.get::<Option<f64>, _>("snow_base_distance_mm"),
        Some(1500.0)
    );
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("last_seen"),
        Some(middle),
        "last_seen is the newest observation instant in the batch, not `now()`"
    );
    assert_eq!(
        row.get::<DateTime<Utc>, _>("first_seen"),
        first,
        "and first_seen is the oldest, not the same instant as last_seen"
    );
    let first_seen = row.get::<DateTime<Utc>, _>("first_seen");

    // `station_metrics`: exactly the metrics the batch carried, keyed by the registry's
    // stable key rather than by the column name.
    let keys: BTreeSet<String> =
        sqlx::query_scalar("SELECT metric_key FROM station_metrics WHERE station_id = $1")
            .bind(station)
            .fetch_all(&pool)
            .await
            .expect("read")
            .into_iter()
            .collect();
    assert_eq!(
        keys,
        ["barometer", "out_temp"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
        "`out_temp` is the registry key; `outtemp` is the column, and the column's own \
         COMMENT says which one belongs here"
    );
    // The same min/max property per metric. `out_temp` was in both readings and
    // `barometer` only in the first, so their spans differ -- which a single bound instant
    // would have collapsed.
    let span = sqlx::query(
        "SELECT metric_key, first_seen, last_seen FROM station_metrics \
           WHERE station_id = $1 ORDER BY metric_key",
    )
    .bind(station)
    .fetch_all(&pool)
    .await
    .expect("read");
    assert_eq!(span[0].get::<String, _>("metric_key"), "barometer");
    assert_eq!(span[0].get::<DateTime<Utc>, _>("first_seen"), first);
    assert_eq!(span[0].get::<DateTime<Utc>, _>("last_seen"), first);
    assert_eq!(span[1].get::<String, _>("metric_key"), "out_temp");
    assert_eq!(span[1].get::<DateTime<Utc>, _>("first_seen"), first);
    assert_eq!(span[1].get::<DateTime<Utc>, _>("last_seen"), middle);

    // A second batch adding a metric: the inventory grows, `last_seen` advances, and
    // `first_seen` does not move.
    w.store(&[envelope(
        station,
        second,
        2,
        Reading {
            out_temp: Some(45.0),
            wind_speed: Some(3.5),
            ..Reading::default()
        },
    )])
    .await
    .expect("stores");

    let keys: BTreeSet<String> =
        sqlx::query_scalar("SELECT metric_key FROM station_metrics WHERE station_id = $1")
            .bind(station)
            .fetch_all(&pool)
            .await
            .expect("read")
            .into_iter()
            .collect();
    assert!(
        keys.contains("wind_speed") && keys.contains("barometer"),
        "a metric that stopped being reported stays in the inventory: the table answers \
         'what has this station ever reported', which is what the Go TODO at \
         grpcreceiver/station.go:64 wanted. Got {keys:?}"
    );
    let row = sqlx::query("SELECT first_seen, last_seen FROM stations WHERE station_id = $1")
        .bind(station)
        .fetch_one(&pool)
        .await
        .expect("read");
    assert_eq!(row.get::<DateTime<Utc>, _>("first_seen"), first_seen);
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("last_seen"),
        Some(second)
    );

    // An out-of-order replay must not move `last_seen` backwards. `greatest(...)` is what
    // does it, and a plain assignment is the plausible alternative that passes every
    // assertion above.
    w.store(&[envelope(
        station,
        first,
        1,
        Reading {
            out_temp: Some(44.0),
            ..Reading::default()
        },
    )])
    .await
    .expect("stores");
    let last: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_seen FROM stations WHERE station_id = $1")
            .bind(station)
            .fetch_one(&pool)
            .await
            .expect("read");
    assert_eq!(last, Some(second), "a replay does not rewind last_seen");
}

/// A station the registry has never heard of — a peer's received station, until M6-4
/// resolves its key — is **stored**, with the projection filled from the reading.
///
/// The alternative is refusing a reading because a non-authoritative projection row cannot
/// be built, and W4 §2.1 is explicit that readings are the thing that must not be lost
/// (**D-30**): "bookkeeping for a non-authoritative projection is not worth putting in
/// front of them".
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_station_the_registry_does_not_know_is_still_stored() {
    let station = "db-tests:unknown";
    let pool = prepare(&[station]).await;
    // A registry that knows a *different* station, so the lookup genuinely misses.
    let registry: Arc<dyn StationRegistry> = Arc::new(ConfigRegistry::new(RegistrySnapshot::new(
        &[row("db-tests:somebody-else")],
        std::iter::empty(),
    )));
    let w = TimescaleWriter::new(pool.clone(), registry).expect("builds");

    let observed_at = at(1_767_657_600);
    w.store(&[envelope(
        station,
        observed_at,
        1,
        Reading {
            out_temp: Some(33.0),
            station_name: Some("a satellite's station".into()),
            station_type: Some("davis".into()),
            ..Reading::default()
        },
    )])
    .await
    .expect("an unknown station's reading is stored");

    let row =
        sqlx::query("SELECT name, type, timezone, latitude FROM stations WHERE station_id = $1")
            .bind(station)
            .fetch_one(&pool)
            .await
            .expect("the projection row exists");
    assert_eq!(
        row.get::<String, _>("name"),
        "a satellite's station",
        "the reading's own name is the fallback"
    );
    assert_eq!(row.get::<String, _>("type"), "davis");
    assert_eq!(
        row.get::<String, _>("timezone"),
        "UTC",
        "the schema's own default, repeated because the column is NOT NULL"
    );
    assert_eq!(
        row.get::<Option<f64>, _>("latitude"),
        None,
        "a guessed coordinate would be worse than a NULL one"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM observations WHERE station_id = $1")
        .bind(station)
        .fetch_one(&pool)
        .await
        .expect("counted");
    assert_eq!(count, 1);
}
