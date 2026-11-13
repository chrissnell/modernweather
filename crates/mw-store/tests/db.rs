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
//! Run them with `cargo xtask db-tests`. It checks that something is listening, sets
//! `MW_DB_TESTS_URL` from the standard libpq variables, and asserts afterwards that every
//! declared test ran and reached the database. It does **not** create a database and does
//! not apply the migrations: `PGDATABASE` defaults to `postgres`, which is the database the
//! `timescale/timescaledb-ha:pg17` service container already has, and [`prepare`] applies
//! `migrations/` from inside the test process through `sqlx`'s own migrator.
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
use mw_component::{
    BucketLevel, BucketQuery, Piece, ReadingSink, ReadingStore, SeriesQuery, StationRegistry,
    TotalsQuery,
};
use mw_store::config_db::StationRow;
use mw_store::{version, ConfigRegistry, RegistrySnapshot, TimescaleReader, TimescaleWriter};
use mw_types::{
    Agg, Envelope, MetricKey, OriginId, Provenance, Reading, Seq, StationKey, DIMENSIONS, METRICS,
};
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
             Run them with `cargo xtask db-tests`, which sets this variable from \
             PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE and refuses to run if nothing is \
             listening. It does not create a database: PGDATABASE defaults to `postgres`. \
             CI runs them in the `db-tests` job against timescale/timescaledb-ha:pg17 \
             (DESIGN.md D-2)."
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

/// **Evidence that this test reached the database**, which `cargo xtask db-tests` requires
/// of every declared test before it reports the gate green.
///
/// The declared names stop a filter that matched nothing and stop a rename. They do not
/// stop a **gutted body**: a test emptied to `{}` still lists, still prints `... ok`, and
/// still counts. This is what stops that, and the mechanism is the point — the marker is
/// printed *after* a query, so it cannot be produced by a test that never connected.
/// Printing it from a helper the test must call is weaker than inspecting the rows from
/// outside, and it is what is available to a gate that deliberately carries no database
/// client of its own.
async fn touched(pool: &PgPool, test: &str) {
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM observations")
        .fetch_one(pool)
        .await
        .expect("the test reached the database");

    // Appended to a file the gate names, rather than printed. Two reasons, and the second
    // is the one that decided it:
    //
    // - `--nocapture` was the obvious route and does not work. libtest interleaves a
    //   passing test's stdout onto the same line as its own `test <name> ... ` prefix, so
    //   the verdict stops being parseable and `ok` lands on a line of its own.
    // - `ci/layers.toml`'s `no-stdout-logging` rule forbids this crate stdout and stderr
    //   macros outright (§5.1: those belong on the binary's argument-parsing path and
    //   nowhere else). A test convenience is not a reason to widen a deliberate rule --
    //   and that rule's scan is a substring match over the whole file, so this comment
    //   may not name the macro either.
    //
    // A file is unambiguous either way, needs no libtest flag, and is appended to in order
    // because the gate runs single-threaded. When the variable is unset -- a developer
    // running `cargo test` directly -- the query above still runs, which is the half that
    // belongs to the test; only the recording is skipped.
    if let Ok(path) = std::env::var("MW_DB_TESTS_EVIDENCE") {
        if !path.is_empty() {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .unwrap_or_else(|e| panic!("opening the evidence file {path}: {e}"));
            writeln!(f, "{test} observations={rows}").expect("writing the evidence file");
        }
    }
}

/// A `config.stations` row with coordinates, for the projection.
fn row(key: &str) -> StationRow {
    StationRow {
        id: 1,
        station_key: key.into(),
        name: format!("{key} (configured)"),
        kind: "weatherlink-live".into(),
        enabled: true,
        identity_kind: "from_payload".into(),
        aprs_callsign: None,
        presented_key: Some(key.to_ascii_uppercase()),
        ingest_secret_hash: None,
        latitude: Some(39.7392),
        longitude: Some(-104.9903),
        altitude_m: Some(1609.0),
        timezone: Some("America/Denver".into()),
        snow_base_distance_mm: Some(1500.0),
        wind_dir_correction_degrees: 0,
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
    // Also the migrated schema, because the marker query at the end reads `observations`.
    let pool = prepare(&[]).await;
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

    // Repeatable, which is the half of `probe`'s contract a test can see: it opens a bare
    // `PgConnection` rather than a pool, so three extra iterations would not approach the
    // server's `max_connections` and this does **not** assert that the connection is
    // closed. Closure is why `probe` calls `conn.close()` on both paths -- without it the
    // server logs an unexpected EOF -- and asserting it would mean reading
    // `pg_stat_activity`, which is a test about PostgreSQL's bookkeeping rather than about
    // this function.
    for _ in 0..3 {
        version::probe(&url()).await.expect("repeatable");
    }
    touched(&pool, "the_version_probe_reads_the_server_it_is_pointed_at").await;
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
    // **M5-5: the two retired columns are never written, and the reading carried both.**
    // `station_name` and `station_type` are `role = "metadata"` now, so they reach the
    // daemon on the wire, feed the `stations` projection below, and are not bound by the
    // `INSERT`. The columns themselves are still there because the migration emits no
    // `DROP COLUMN` (H-4, D-19), which is exactly why this is worth asserting: a writer
    // that still bound them would pass every other test in this file.
    assert_eq!(
        rows[0].get::<Option<String>, _>("stationname"),
        None,
        "the reading reported a station name and the raw table is not where it goes"
    );
    assert_eq!(rows[0].get::<Option<String>, _>("stationtype"), None);

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
    // The second reading reported only a temperature, so every other column of it is null.
    assert_eq!(rows[1].get::<Option<f32>, _>("outtemp"), Some(41.0));
    assert_eq!(rows[1].get::<Option<f32>, _>("outhumidity"), None);
    // The wire metadata still reached the projection, which is the half of M5-5 that is a
    // move rather than a removal: the display name is read from `config.stations` first
    // and from the reading only as a fallback, and `row()` configures this station, so the
    // configured name is what is there.
    let name: String = sqlx::query_scalar("SELECT name FROM stations WHERE station_id = $1")
        .bind(station)
        .fetch_one(&pool)
        .await
        .expect("the projection row exists");
    assert_eq!(name, format!("{station} (configured)"));
    touched(
        &pool,
        "readings_land_in_observations_and_an_unreported_metric_is_null",
    )
    .await;
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
/// Three columns are expected extras, each for a stated reason, and they are named here
/// rather than filtered silently so that a *fourth* unregistered column is a failure.
///
/// `extra` is W4 §4's escape hatch for unmodelled vendor fields, deliberately outside
/// `metrics/registry.toml` because the registry is the source of truth for *modelled*
/// metrics (OQ-W4-3).
///
/// `stationname` and `stationtype` are **M5-5's retired dimensions**, and their presence
/// here is the migration behaving as designed rather than a gap in it: a `metadata`
/// dimension leaves the generated DDL and no `DROP COLUMN` is emitted, because a dropped
/// attribute number is never released and is charged twice against the 1600-column ceiling
/// once the hypertable is compressed (H-4, D-19). So the columns stay where they are,
/// unwritten, and `metrics/snapshot.toml` keeps both identifiers allocated. The assertion
/// this test makes about them is therefore the opposite of the one it makes about the
/// metrics: these two must be in the catalogue and must NOT be in the registry's stored
/// set, and `readings_land_in_observations_and_an_unreported_metric_is_null` is what
/// asserts nothing writes them.
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
    // See the three-extras note above. Asserted as a *difference* between the catalogue and
    // the registry rather than inserted blindly: if a later migration does drop them, this
    // fails and names the line to delete, instead of quietly sanctioning a column that is
    // no longer there.
    for retired in ["stationname", "stationtype"] {
        assert!(
            actual.contains(&(retired.to_string(), "text".to_string())),
            "`{retired}` is M5-5's retired dimension and the migration emits no DROP \
             COLUMN, so it is still a column of `observations`. If it has been dropped by \
             hand, remove it from this list."
        );
        assert!(
            !DIMENSIONS.iter().any(|d| d.stored && d.column == retired),
            "`{retired}` is back in the registry's stored set, which is a column the \
             writer binds -- not a retired one"
        );
        declared.insert((retired.to_string(), "text".to_string()));
    }

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
    touched(
        &pool,
        "the_observations_columns_are_exactly_what_the_registry_declares",
    )
    .await;
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
    touched(&pool, "one_metric_of_each_declared_type_round_trips").await;
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
    touched(&pool, "a_replayed_prefix_produces_no_duplicate_rows").await;
}

/// Several envelopes in **one batch** sharing a `(station_id, time)`.
///
/// Not a corner case: `Ingress::admit` substitutes the frame's arrival instant for every
/// `PartialReading` the frame did not timestamp, so a Davis `LOOP 4` response is four
/// readings at one instant. Two things have to hold, and the second is the one that is
/// about data rather than about SQL.
///
/// 1. PostgreSQL refuses `ON CONFLICT DO UPDATE` twice against one row in one statement —
///    *"cannot affect row a second time"* — so without the merge the **whole batch** fails,
///    every time, for that station.
/// 2. Those four observations do not measure the same things. Keeping the last envelope
///    wholesale stores one of them and writes the other three as `NULL`, which is
///    indistinguishable from "the station did not report it" (**W3 D-26**) and is a
///    measurement nothing can recover. Each column takes the last non-`NULL` value instead.
///
/// The envelopes here carry **disjoint** metrics, which is the shape the Go frame actually
/// has and the shape a homogeneous fixture cannot distinguish.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn envelopes_sharing_an_instant_in_one_batch_are_merged_instead_of_failing() {
    let station = "db-tests:collapse";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    let observed_at = at(1_767_484_800);
    let batch: Vec<Envelope> = [
        Reading {
            out_temp: Some(30.0),
            ..Reading::default()
        },
        Reading {
            barometer: Some(29.92),
            ..Reading::default()
        },
        Reading {
            wind_speed: Some(7.25),
            ..Reading::default()
        },
        Reading {
            // A second value for a metric the first envelope also carried: last wins per
            // column, which is what a sequence of upserts would have left.
            out_temp: Some(33.0),
            out_humidity: Some(61.0),
            ..Reading::default()
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(i, r)| envelope(station, observed_at, i as u64 + 1, r))
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

    let row = sqlx::query(
        "SELECT outtemp, outhumidity, barometer, windspeed \
           FROM observations WHERE station_id = $1 AND time = $2",
    )
    .bind(station)
    .bind(observed_at)
    .fetch_one(&pool)
    .await
    .expect("exactly one row");
    assert_eq!(
        row.get::<Option<f32>, _>("outtemp"),
        Some(33.0),
        "the later value for a metric two observations carried"
    );
    assert_eq!(row.get::<Option<f32>, _>("outhumidity"), Some(61.0));
    assert_eq!(
        row.get::<Option<f32>, _>("barometer"),
        Some(29.92),
        "the second observation's barometer survived the merge rather than being nulled"
    );
    assert_eq!(
        row.get::<Option<f32>, _>("windspeed"),
        Some(7.25),
        "and so did the third observation's wind speed"
    );

    // Every observation contributed, so every metric the frame carried is inventory.
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
        ["barometer", "out_humidity", "out_temp", "wind_speed"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
        "got {keys:?}"
    );
    touched(
        &pool,
        "envelopes_sharing_an_instant_in_one_batch_are_merged_instead_of_failing",
    )
    .await;
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
                // The DEVICE's claim about itself, which `row()` configures as
                // `weatherlink-live` -- two different strings on purpose, so the two
                // columns below cannot both pass by coincidence.
                station_type: Some("davis-vantage-pro".into()),
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
        "SELECT name, type, reported_type, timezone, latitude, longitude, altitude_ft, \
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
    // **M5-5: the two station types are two columns and neither is the other's fallback.**
    // `type` is `config.stations.type`, the vocabulary `/stationinfo` returns; `reported_type`
    // is the driver's self-reported name, which is what `/latest`'s `stationtype` must be
    // (docs/legacy-api-contract.md §3.1). The configuration wins the first and does not
    // touch the second, which is the assertion -- a writer that coalesced them would pass
    // every other check here.
    assert_eq!(
        row.get::<Option<String>, _>("reported_type").as_deref(),
        Some("davis-vantage-pro"),
        "the device's own claim, not the configured type"
    );
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
    touched(&pool, "the_projections_reflect_what_has_been_written").await;
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

    let row = sqlx::query(
        "SELECT name, type, reported_type, timezone, latitude FROM stations \
           WHERE station_id = $1",
    )
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
        row.get::<Option<String>, _>("reported_type").as_deref(),
        Some("davis"),
        "with no configuration row the two type columns agree, because there is only one \
         answer -- which is why the test above uses two different strings"
    );
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
    touched(
        &pool,
        "a_station_the_registry_does_not_know_is_still_stored",
    )
    .await;
}

/// **A station whose configuration row goes away does not lose its projection.**
///
/// The `stations` row is a projection of `config.stations` (W4 §2.1), and the writer is
/// what copies it. An operator who deletes or re-keys a configuration row while that
/// station still has unacked envelopes in the spool would otherwise have the replay rewrite
/// the projection from the reading: coordinates to `NULL`, `timezone` to `'UTC'`, and the
/// display name to whatever the driver calls itself — or to the bare station key. `/span`
/// renders that name, and M3-4's `Calendar { TzSource::Station }` reads that zone.
///
/// So the `DO UPDATE` is non-destructive: `coalesce` for the nullable columns, and the
/// three `NOT NULL` ones are only overwritten when the configuration actually had an answer.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_vanished_configuration_row_does_not_erase_the_projection() {
    let station = "db-tests:vanished";
    let pool = prepare(&[station]).await;

    // First pass: the station is configured, so the projection is filled from it.
    let configured = writer(&pool, station);
    let first = at(1_767_744_000);
    configured
        .store(&[envelope(
            station,
            first,
            1,
            Reading {
                out_temp: Some(44.0),
                station_name: Some("what the driver calls itself".into()),
                ..Reading::default()
            },
        )])
        .await
        .expect("stores");
    let projection =
        sqlx::query("SELECT name, timezone, latitude FROM stations WHERE station_id = $1")
            .bind(station)
            .fetch_one(&pool)
            .await
            .expect("the projection row exists");
    assert_eq!(
        projection.get::<String, _>("name"),
        format!("{station} (configured)")
    );
    assert_eq!(projection.get::<String, _>("timezone"), "America/Denver");
    assert_eq!(projection.get::<Option<f64>, _>("latitude"), Some(39.7392));

    // Second pass: the configuration row is gone and a spooled envelope replays.
    let orphaned: Arc<dyn StationRegistry> = Arc::new(ConfigRegistry::new(RegistrySnapshot::new(
        &[row("db-tests:somebody-else")],
        std::iter::empty(),
    )));
    let w = TimescaleWriter::new(pool.clone(), orphaned).expect("builds");
    w.store(&[envelope(
        station,
        at(1_767_744_060),
        2,
        Reading {
            out_temp: Some(45.0),
            station_name: Some("what the driver calls itself".into()),
            ..Reading::default()
        },
    )])
    .await
    .expect("an orphaned station's reading is still stored");

    let projection = sqlx::query(
        "SELECT name, type, timezone, latitude, longitude, altitude_ft, \
                snow_base_distance_mm, last_seen \
           FROM stations WHERE station_id = $1",
    )
    .bind(station)
    .fetch_one(&pool)
    .await
    .expect("the projection row is still there");
    assert_eq!(
        projection.get::<String, _>("name"),
        format!("{station} (configured)"),
        "the configured name survived; the reading's own name did not overwrite it"
    );
    assert_eq!(projection.get::<String, _>("type"), "weatherlink-live");
    assert_eq!(
        projection.get::<String, _>("timezone"),
        "America/Denver",
        "a station's rollover zone is not quietly reset to UTC"
    );
    assert_eq!(projection.get::<Option<f64>, _>("latitude"), Some(39.7392));
    assert_eq!(
        projection.get::<Option<f64>, _>("longitude"),
        Some(-104.9903)
    );
    assert!(projection.get::<Option<f64>, _>("altitude_ft").is_some());
    assert_eq!(
        projection.get::<Option<f64>, _>("snow_base_distance_mm"),
        Some(1500.0)
    );
    // `last_seen` still advances: the reading was stored, which is the fact it records.
    assert_eq!(
        projection.get::<Option<DateTime<Utc>>, _>("last_seen"),
        Some(at(1_767_744_060))
    );
    touched(
        &pool,
        "a_vanished_configuration_row_does_not_erase_the_projection",
    )
    .await;
}

/// **One statement at the designed batch size.**
///
/// W3 §2.4 sizes the durable cursor's read-ahead at 256 envelopes against PostgreSQL's
/// 65,535-bind-parameter limit — "at the registry's 216 allocated columns the hard ceiling
/// is 303 rows per multi-row `INSERT`; 256 is the power of two below it" — and the writer
/// uses `unnest`, which binds one array per column and is therefore 216 parameters for any
/// batch size. That turns §2.4's figure from a *limit* into a *latency* figure, and the
/// unit test asserts the parameter count against the statement text.
///
/// What only a real server can answer is whether the statement actually executes at that
/// width: array length is not parameter count, but a 256-element `timestamptz[]` across 216
/// arrays is 55,296 values in one round trip and nothing had ever run it.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_full_batch_of_256_is_one_statement() {
    let station = "db-tests:full-batch";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    // 256 is `mw_pipeline::READ_AHEAD`, which is what the cursor hands a sink.
    let batch: Vec<Envelope> = (0..256u32)
        .map(|i| {
            envelope(
                station,
                at(1_767_830_400 + i64::from(i) * 2),
                u64::from(i) + 1,
                Reading {
                    out_temp: Some(40.0 + i as f32 / 8.0),
                    barometer: Some(29.92),
                    out_humidity: Some(55.0),
                    extra_text1: Some(format!("reading {i}")),
                    lightning_day: Some(i as i32),
                    ..Reading::default()
                },
            )
        })
        .collect();

    let stored = w.store(&batch).await.expect("256 rows in one statement");
    assert_eq!(stored.stored, 256);
    assert_eq!(stored.rows, 256, "256 distinct instants, 256 rows");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM observations WHERE station_id = $1")
        .bind(station)
        .fetch_one(&pool)
        .await
        .expect("counted");
    assert_eq!(count, 256);
    // The ends, so a truncated array would be visible rather than averaged away.
    let first: Option<f32> = sqlx::query_scalar(
        "SELECT outtemp FROM observations WHERE station_id = $1 ORDER BY time LIMIT 1",
    )
    .bind(station)
    .fetch_one(&pool)
    .await
    .expect("read");
    let last: Option<f32> = sqlx::query_scalar(
        "SELECT outtemp FROM observations WHERE station_id = $1 ORDER BY time DESC LIMIT 1",
    )
    .bind(station)
    .fetch_one(&pool)
    .await
    .expect("read");
    assert_eq!(first, Some(40.0));
    assert_eq!(last, Some(40.0 + 255.0 / 8.0));
    touched(&pool, "a_full_batch_of_256_is_one_statement").await;
}

// ---------------------------------------------------------------------------------------
// The read half — implementation-plan.md M3-4
// ---------------------------------------------------------------------------------------

/// Refresh the aggregate hierarchy over `[from, to)`, bottom level first.
///
/// Every level is `materialized_only = true` (the W4 migration says why: TimescaleDB
/// cannot build an aggregate on a real-time aggregate), so an unrefreshed bucket returns
/// **no row** rather than a partial one — and each level reads the level below it
/// (**D-11**), so the order is load-bearing. In the daemon this is four policy jobs on four
/// staggered schedules; in a test it has to be done by hand or the reads below find
/// nothing.
///
/// The window is widened to whole buckets **per level**, because
/// `refresh_continuous_aggregate` refuses a window narrower than one of the view's own
/// buckets — "refresh window too small" — and a ten-minute range is narrower than a day.
/// Widening is also what the real policies do: each one's `start_offset` covers several
/// buckets so that a late or replayed reading in a recent bucket is picked up.
async fn materialise(pool: &PgPool, from: DateTime<Utc>, to: DateTime<Utc>) {
    for (view, bucket) in [
        ("observations_1m", 60i64),
        ("observations_5m", 300),
        ("observations_1h", 3600),
        ("observations_1d", 86_400),
    ] {
        let lo = from.timestamp() - from.timestamp().rem_euclid(bucket);
        let floored = to.timestamp() - to.timestamp().rem_euclid(bucket);
        let hi = if floored == to.timestamp() {
            to.timestamp()
        } else {
            floored + bucket
        };
        sqlx::query(&format!(
            "CALL refresh_continuous_aggregate('{view}', $1, $2)"
        ))
        .bind(at(lo))
        .bind(at(hi.max(lo + bucket)))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("refreshing {view}: {e}"));
    }
}

/// **M3-4's first done-criterion: a ten-minute mean over the rolled-up buckets equals the
/// same mean computed directly from raw.**
///
/// Ten minutes with **uneven** counts, which is the case that distinguishes a correct
/// roll-up from the obvious wrong one: minute *k* carries *k+1* readings, so the mean of
/// the per-minute means is not the mean of the readings. **D-12** measured that
/// `avg(avg(x))` disagreed with the direct average in 658 of 868 real bucket pairs and in
/// **all 56** uneven ones.
///
/// What this covers and what it does not, stated because the two halves are in two crates
/// that may not name each other (`ci/layers.toml` puts `mw-store` and `mw-pipeline` in the
/// same layer):
///
/// - **Here**: that `observations_1m`'s `_sum` and `_count` really are the sum and count of
///   the raw readings, that [`TimescaleReader::buckets`] returns them in the positions
///   `mw_types::LEVELS[0].columns` declares, and that `Σsum / Σcount` over ten of them is
///   `avg()` over the raw rows — the database half, which no unit test can assert.
/// - **`mw_pipeline::rollup::ring`**: that `Ring::resolve` performs exactly that expression
///   for `Agg::Mean`, and the same for the other four strategies. The arithmetic below is
///   the same expression deliberately, so the two are checkable against each other by
///   reading them.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_ten_minute_mean_over_the_rolled_up_buckets_equals_the_mean_from_raw() {
    let station = "db-tests:rollup-mean";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    // 2026-01-05T00:00:00Z, well away from every other test's window so that the
    // continuous-aggregate refresh below is cheap.
    let base = 1_767_571_200i64;
    let mut batch: Vec<Envelope> = Vec::new();
    let mut seq = 0u64;
    for minute in 0..10i64 {
        // Minute k carries k+1 readings, each 1 degree apart from a per-minute base, so
        // the counts are uneven and the per-minute means differ.
        for reading in 0..=minute {
            seq += 1;
            batch.push(envelope(
                station,
                at(base + minute * 60 + reading),
                seq,
                Reading {
                    out_temp: Some(40.0 + (minute * 10 + reading) as f32),
                    wind_speed: Some(2.0 + minute as f32),
                    ..Reading::default()
                },
            ));
        }
    }
    let expected_rows = batch.len();
    w.store(&batch).await.expect("the readings are stored");
    materialise(&pool, at(base), at(base + 600)).await;

    // What the database says, directly from raw. `avg(real)` returns double precision, so
    // this is the same accumulation width the aggregate uses (**D-14**).
    let direct: f64 = sqlx::query_scalar(
        "SELECT avg(outtemp)::float8 FROM observations
         WHERE station_id = $1 AND time >= $2 AND time < $3",
    )
    .bind(station)
    .bind(at(base))
    .bind(at(base + 600))
    .fetch_one(&pool)
    .await
    .expect("the raw average");

    // What the read half says, rolled up. The same expression
    // `mw_pipeline::rollup::ring::Ring::resolve` evaluates for `Agg::Mean`.
    let reader = TimescaleReader::new(pool.clone());
    let buckets = reader
        .buckets(BucketQuery {
            station: StationKey::new(station),
            since: at(base),
            limit: 60,
        })
        .await
        .expect("the 1-minute level is materialised");
    assert_eq!(buckets.len(), 10, "ten minutes, ten buckets");
    assert!(
        buckets.windows(2).all(|w| w[0].bucket < w[1].bucket),
        "oldest first, which is what an incremental refresh resumes from"
    );
    let columns = mw_types::LEVELS
        .iter()
        .find(|l| l.name == "1m")
        .map(|l| l.columns)
        .expect("the 1-minute level");
    for b in &buckets {
        assert_eq!(
            b.values.len(),
            columns.len(),
            "one value per declared column, which is what lets a consumer index"
        );
    }
    let position = |name: &str| {
        columns
            .iter()
            .position(|c| c.column == name)
            .unwrap_or_else(|| panic!("{name} is not a 1-minute column"))
    };
    let (sum_at, count_at) = (position("outtemp_sum"), position("outtemp_count"));
    let mut total = 0.0f64;
    let mut n = 0.0f64;
    for b in &buckets {
        if let (Some(s), Some(c)) = (b.values[sum_at], b.values[count_at]) {
            total += s;
            n += c;
        }
    }
    assert!(n > 0.0, "ten buckets of readings");
    assert_eq!(n as usize, expected_rows, "every reading is counted once");
    let rolled = total / n;

    // The two agree. Asserted as an exact difference rather than an epsilon chosen for
    // comfort: both sides accumulate `outtemp::double precision` over the same values, so
    // the only room for disagreement is summation order, and the figure this prints is
    // what the PR reports.
    assert!(
        (rolled - direct).abs() < 1e-9,
        "rolled up {rolled}, direct {direct}, difference {}",
        rolled - direct
    );

    // **And the wrong answer is a long way away**, which is what makes the assertion above
    // mean something. The mean of the per-minute means is what `avg(avg(x))` would give.
    let mean_of_means: f64 = buckets
        .iter()
        .filter_map(|b| match (b.values[sum_at], b.values[count_at]) {
            (Some(s), Some(c)) if c > 0.0 => Some(s / c),
            _ => None,
        })
        .sum::<f64>()
        / buckets.len() as f64;
    assert!(
        (mean_of_means - rolled).abs() > 1.0,
        "the uneven counts have to make avg(avg(x)) wrong for this test to be testing \
         anything: mean-of-means {mean_of_means}, correct {rolled}"
    );

    // The max follows the same path and is exact for a different reason: the max of the
    // per-bucket maxima **is** the max of the readings, which is why a ten-minute wind gust
    // needs no raw access even though `calculate_wind_gust` reads raw.
    let max_at = position("windspeed_max");
    let gust = buckets
        .iter()
        .filter_map(|b| b.values[max_at])
        .fold(f64::NEG_INFINITY, f64::max);
    let direct_gust: f64 = sqlx::query_scalar(
        "SELECT max(windspeed)::float8 FROM observations
         WHERE station_id = $1 AND time >= $2 AND time < $3",
    )
    .bind(station)
    .bind(at(base))
    .bind(at(base + 600))
    .fetch_one(&pool)
    .await
    .expect("the raw maximum");
    assert!(
        (gust - direct_gust).abs() < 1e-9,
        "{gust} against {direct_gust}"
    );

    // A metric nothing reported is NULL in the bucket state too, so an empty window is
    // absent rather than zero (**D-26**) all the way down.
    let unreported = position("soiltemp4_sum");
    assert!(buckets.iter().all(|b| b.values[unreported].is_none()));

    touched(
        &pool,
        "a_ten_minute_mean_over_the_rolled_up_buckets_equals_the_mean_from_raw",
    )
    .await;
}

/// **The decomposition's exactness claim, against the database.** A range stitched from day
/// buckets with hour and minute edges sums to the same total as the raw rows it covers.
///
/// This is what makes a station-local month affordable: a local month in `America/Denver`
/// begins at `07:00Z`, which is on the 1-hour grid and not the 1-day one, so the shape is
/// *hour edges around day buckets*. **D-13** is why it is exact rather than close — a 1-day
/// bucket's `rainincremental_total` is the sum of its 24 hourly totals, which are the sums
/// of their 1-minute totals, every addition in double precision (**D-14**).
///
/// The Go equivalent stitches the same levels and loses up to five minutes of rain from
/// every figure: `update_rainfall_summary` reads raw rows `>= date_trunc('hour', NOW()) +
/// (minute/5*5) minutes`, so the 5-minute bucket in progress is counted by neither its
/// aggregate term nor its raw term.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_range_stitched_across_three_levels_totals_the_same_as_raw() {
    let station = "db-tests:rollup-stitch";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    // 2026-01-07T00:00:00Z. Three days of rain, one hundredth of an inch every ten
    // minutes, so the total over any range is a count of ten-minute slots.
    let base = 1_767_744_000i64;
    let minutes = 3 * 24 * 60;
    let mut seq = 0u64;
    for chunk in (0..minutes).step_by(256) {
        let batch: Vec<Envelope> = (chunk..(chunk + 256).min(minutes))
            .map(|m| {
                seq += 1;
                envelope(
                    station,
                    at(base + m * 60),
                    seq,
                    Reading {
                        rain_incremental: if m % 10 == 0 { Some(0.01) } else { None },
                        ..Reading::default()
                    },
                )
            })
            .collect();
        w.store(&batch).await.expect("stored");
    }
    materialise(&pool, at(base), at(base + minutes * 60)).await;

    let reader = TimescaleReader::new(pool.clone());
    // A range whose ends are on neither the day nor the hour grid, which is the shape a
    // station-local month in a 45-minute zone has: 2026-01-07T07:15Z to 2026-01-09T18:42Z,
    // floored to the minute.
    let from = at(base + 7 * 3600 + 15 * 60);
    let until = at(base + 2 * 86_400 + 18 * 3600 + 42 * 60);
    // `now` far enough past the range that every level's refresh margin is satisfied, so
    // the decomposition really does reach for day buckets.
    let now = until + chrono::Duration::days(2);

    // The pieces a caller builds. Re-derived here rather than imported: `mw-pipeline` owns
    // `decompose` and `ci/layers.toml` forbids this crate from naming it, so what this
    // test asserts is the **store's** half — that one sum per piece, across three levels,
    // adds up to the raw total. `mw_pipeline::rollup::calendar`'s own tests assert that the
    // pieces tile the range.
    let hour = chrono::Duration::hours(1);
    let first_hour = from + (hour - chrono::Duration::minutes(15));
    let first_day = at(base + 86_400);
    let last_day = at(base + 2 * 86_400);
    let last_hour = until - chrono::Duration::minutes(42);
    let pieces = vec![
        Piece {
            level: BucketLevel::Minute,
            from,
            until: first_hour,
        },
        Piece {
            level: BucketLevel::Hour,
            from: first_hour,
            until: first_day,
        },
        Piece {
            level: BucketLevel::Day,
            from: first_day,
            until: last_day,
        },
        Piece {
            level: BucketLevel::Hour,
            from: last_day,
            until: last_hour,
        },
        Piece {
            level: BucketLevel::Minute,
            from: last_hour,
            until,
        },
    ];
    // The pieces really do tile the range, asserted here so that a mistake in the fixture
    // above is a failure about the fixture rather than about the store.
    assert_eq!(pieces[0].from, from);
    assert_eq!(pieces.last().expect("non-empty").until, until);
    for pair in pieces.windows(2) {
        assert_eq!(pair[0].until, pair[1].from, "{:?} {:?}", pair[0], pair[1]);
    }
    assert!(
        pieces.iter().any(|p| p.level == BucketLevel::Day),
        "the point of the stitch is that the middle is day buckets"
    );
    // The margin `mw_pipeline::rollup::calendar` applies: `now` is two days past the
    // range's end, so every level's refresh interval has certainly passed and reaching for
    // day buckets is legitimate rather than a read of something unmaterialised.
    assert!(now - until > chrono::Duration::hours(1));

    let sums = reader
        .totals(TotalsQuery {
            station: StationKey::new(station),
            metric: mw_types::MetricKey::RainIncremental,
            pieces: pieces.clone(),
        })
        .await
        .expect("every level is materialised");
    assert_eq!(sums.len(), pieces.len(), "one answer per piece, in order");
    let stitched: f64 = sums.iter().flatten().sum();

    // **`sum(rainincremental::double precision)`, not `sum(rainincremental)::float8`**, and
    // the difference is the whole of **D-14**: `sum(real)` accumulates in `real` and
    // returns `real`, so casting its answer afterwards compares a float4 accumulation
    // against a float8 one. Measured here, that disagrees in the sixth significant figure
    // — 3.5699973106384277 against 3.569999920204282 — which is the aggregate hierarchy
    // being *right* and the convenient query being wrong. The generated views widen every
    // sum for exactly this reason ("sums accumulate in double precision, never in the
    // column's own type", which the metric registry's generator says in the header of every
    // SQL file it emits), and this is the comparison that makes them comparable.
    let direct: f64 = sqlx::query_scalar(
        "SELECT sum(rainincremental::double precision) FROM observations
         WHERE station_id = $1 AND time >= $2 AND time < $3",
    )
    .bind(station)
    .bind(from)
    .bind(until)
    .fetch_one(&pool)
    .await
    .expect("the raw total");
    assert!(
        (stitched - direct).abs() < 1e-12,
        "stitched {stitched}, direct {direct}, difference {}",
        stitched - direct
    );
    assert!(
        direct > 0.0,
        "there has to be rain for this to mean anything"
    );

    // A piece no bucket falls in is `None`, not `0.0` — which is what makes an offline
    // gauge distinguishable from a dry day (**D-26**). The Go equivalents all write
    // `COALESCE(SUM(period_rain), 0)`.
    let empty = reader
        .totals(TotalsQuery {
            station: StationKey::new(station),
            metric: mw_types::MetricKey::RainIncremental,
            pieces: vec![Piece {
                level: BucketLevel::Minute,
                from: at(base - 86_400),
                until: at(base - 86_000),
            }],
        })
        .await
        .expect("an empty range is a legitimate question");
    assert_eq!(empty, vec![None], "no buckets is absent, not zero");

    touched(
        &pool,
        "a_range_stitched_across_three_levels_totals_the_same_as_raw",
    )
    .await;
}

/// The hourly series the storm walk reads: in bucket order, with a dry hour as `None`
/// rather than as a gap in the sequence.
///
/// The **shape** is the answer here rather than the sum, which is why it is a third method
/// on the read half: a rain event ends after 24 hours with no rain, and a dry spell is not
/// expressible as a total. The Go implementation has no concept of an event at all —
/// `calculate_storm_rainfall` carries `-- Simple approach: storm is any rainfall in the
/// last 24 hours`.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn an_hourly_rain_series_is_read_in_order_with_dry_hours_absent() {
    let station = "db-tests:rollup-series";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);

    // 2026-01-11T00:00:00Z, six hours, raining in the first and the last only.
    let base = 1_768_089_600i64;
    let batch: Vec<Envelope> = (0..6i64)
        .map(|h| {
            envelope(
                station,
                at(base + h * 3600 + 90),
                u64::try_from(h + 1).expect("in range"),
                Reading {
                    rain_incremental: if h == 0 || h == 5 { Some(0.2) } else { None },
                    out_temp: Some(35.0),
                    ..Reading::default()
                },
            )
        })
        .collect();
    w.store(&batch).await.expect("stored");
    materialise(&pool, at(base), at(base + 6 * 3600)).await;

    let reader = TimescaleReader::new(pool.clone());
    let series = reader
        .series(SeriesQuery {
            station: StationKey::new(station),
            metric: mw_types::MetricKey::RainIncremental,
            level: BucketLevel::Hour,
            since: at(base),
            limit: 720,
        })
        .await
        .expect("the hourly level is materialised");

    assert_eq!(series.len(), 6, "six hours with readings in them");
    assert!(
        series.windows(2).all(|w| w[0].bucket < w[1].bucket),
        "oldest first, which the dry-gap walk depends on"
    );
    let wet: Vec<usize> = series
        .iter()
        .enumerate()
        .filter(|(_, b)| b.total.is_some_and(|t| t > 0.0))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(wet, vec![0, 5], "the first and the last hour, by position");
    assert!(
        series[1..5].iter().all(|b| b.total.is_none()),
        "a dry hour is NULL and not 0.0, because the bucket's sum is over no rain rows: \
         {series:?}"
    );
    // Within float4's precision of 0.2, not within a double's: `rainincremental` is a
    // `float4` column, so 0.2 is stored as 0.20000000298023224 and the bucket's
    // double-precision sum of it is exactly that. The widening (**D-14**) protects the
    // *accumulation* order, not the value a 4-byte column can hold.
    let first = series[0].total.expect("the first hour had rain");
    assert!((first - 0.2).abs() < 1e-6, "{first}");

    // A metric with no declared total is a named refusal rather than an empty series: W2
    // D-11's vocabulary doing its job.
    let e = reader
        .series(SeriesQuery {
            station: StationKey::new(station),
            metric: mw_types::MetricKey::OutTemp,
            level: BucketLevel::Hour,
            since: at(base),
            limit: 720,
        })
        .await
        .expect_err("out_temp declares mean, min and max and no total");
    let chain = mw_types::DisplayChain(&e).to_string();
    assert!(chain.contains("OutTemp"), "{chain}");
    assert!(chain.contains("does not declare a `total`"), "{chain}");

    touched(
        &pool,
        "an_hourly_rain_series_is_read_in_order_with_dry_hours_absent",
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// `ReadingStore::latest` — the newest raw reading, read back whole
// ---------------------------------------------------------------------------------------

/// **The one half `crates/mw-pipeline/src/snapshot/tests.rs` cannot assert.** Those tests
/// drive `snapshot:warm` over a fake store, so they check what the component does with a
/// `Reading`; this one checks that a `Reading` comes back out of 214 real columns in the
/// types the columns hold.
///
/// Three properties, and each has its own way of failing silently:
///
/// 1. **Every declared type survives the round trip.** The decode is one arm per
///    `sql_type` and the placement is the generated `Reading::set`, so a column whose
///    SQL type and whose generated field disagree would read back as `None` — a sensor
///    that looks like it stopped reporting. `float4`, `int4`, `int8`, `text` and
///    `timestamptz` are all asserted, because `buckets`' float8 cast — which is correct
///    there — would turn the last two into NULL here.
/// 2. **An unreported metric comes back `None`, not `0.0`** (**D-26**). The write path's
///    own test asserts the column is NULL; this asserts the read path does not fill it in.
/// 3. **The `since` bound is W1 Decision 15's ten minutes and it is in the statement.** A
///    row outside the window is not returned at all, which is what makes `/latest`'s
///    documented 404 a property of the query rather than of a filter a caller might
///    forget.
/// 4. **`station_type` comes back and `station_name` does not** — M5-5. Neither is a column
///    of `observations` any more, and they take different routes on purpose:
///    `/latest`'s `stationtype` is the driver's self-reported name and is read from
///    `stations.reported_type`, while `stationname` is configuration and is resolved by
///    `mw_api_legacy::Renderer` rather than here. This is the only test that can see
///    either, because the statement is what both facts are properties of.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn the_newest_reading_in_the_window_is_read_back_whole() {
    let station = "db-tests:latest";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);
    let reader = TimescaleReader::new(pool.clone());

    // Three rows: one well outside the recency window, and two inside it.
    let stale = at(1_767_300_000);
    let older = at(1_767_311_940);
    let newest = at(1_767_312_000);
    let reported = at(1_767_311_000);
    w.store(&[
        envelope(
            station,
            stale,
            1,
            Reading {
                barometer: Some(28.00),
                ..Reading::default()
            },
        ),
        envelope(
            station,
            older,
            2,
            Reading {
                barometer: Some(29.00),
                out_temp: Some(40.0),
                ..Reading::default()
            },
        ),
        envelope(
            station,
            newest,
            3,
            Reading {
                // float4
                barometer: Some(29.9375),
                // int4
                lightning_day: Some(17),
                // int8, past 2^32 so a column narrowed to int4 fails rather than
                // truncating quietly.
                date_utc: Some(4_294_967_296),
                // text
                extra_text1: Some("a note from the station".into()),
                // timestamptz
                storm_start: Some(reported),
                // Wire metadata, and the driver's own vocabulary rather than the
                // configured one (`row()` configures `weatherlink-live`).
                station_type: Some("campbell".into()),
                station_name: Some("what the device calls itself".into()),
                ..Reading::default()
            },
        ),
    ])
    .await
    .expect("stores");

    // 1 + 2. The newest row inside the window, in the types the columns hold.
    let got = reader
        .latest(mw_component::LatestQuery {
            station: StationKey::new(station),
            since: newest - chrono::Duration::seconds(600),
        })
        .await
        .expect("the query runs")
        .expect("one row is inside the ten-minute window");
    assert_eq!(got.time, newest, "the newest, not the first or the oldest");
    assert_eq!(got.station_id, station);
    assert_eq!(got.barometer, Some(29.9375), "float4");
    assert_eq!(got.lightning_day, Some(17), "int4");
    assert_eq!(got.date_utc, Some(4_294_967_296), "int8");
    assert_eq!(
        got.extra_text1.as_deref(),
        Some("a note from the station"),
        "text -- which a float8 cast would have turned into NULL"
    );
    assert_eq!(got.storm_start, Some(reported), "timestamptz, likewise");
    // 4. The device's claim survives a restart. The reading carried it on the wire, no
    // column of `observations` holds it, and `snapshot:warm` reads it back through this
    // query -- which is the whole reason `stations.reported_type` exists.
    assert_eq!(
        got.station_type.as_deref(),
        Some("campbell"),
        "`/latest`'s `stationtype` is the driver's self-reported name, read back from the \
         projection now that no raw column carries it"
    );
    assert_eq!(
        got.station_name, None,
        "and `stationname` is NOT reconstructed here: it is configuration, and \
         `mw_api_legacy::Renderer` reads it from `config.stations` (M5-5 criterion 2)"
    );
    assert_eq!(
        got.out_temp, None,
        "the previous row reported an outdoor temperature and this one did not, so the \
         field is None rather than 0.0 (D-26) -- and rather than the earlier row's value, \
         which is what a read that merged rows would produce"
    );
    // Every other metric is absent, which is the whole point of storing NULL: a snapshot
    // rendered from this reading emits nothing for them rather than a zero.
    let reported_metrics = METRICS
        .iter()
        .filter(|m| got.value(m.key).is_some())
        .count();
    assert_eq!(
        reported_metrics, 5,
        "exactly the five metrics this row carried"
    );

    // 3. The recency bound is in the statement.
    let outside = reader
        .latest(mw_component::LatestQuery {
            station: StationKey::new(station),
            since: newest + chrono::Duration::seconds(1),
        })
        .await
        .expect("the query runs");
    assert!(
        outside.is_none(),
        "a station whose newest row is outside the window has no reading to seed, which \
         is the 404 `contract/fixtures/latest-no-recent-data` pins -- got {outside:?}"
    );

    // A station that has never reported is the same answer, by a different route.
    let unknown = reader
        .latest(mw_component::LatestQuery {
            station: StationKey::new("db-tests:latest-nobody"),
            since: newest - chrono::Duration::seconds(600),
        })
        .await
        .expect("the query runs");
    assert!(unknown.is_none());

    touched(&pool, "the_newest_reading_in_the_window_is_read_back_whole").await;
}

/// **W1-BUG-1 stays dead, end to end**: `/span` for a station that has never reported a
/// wind direction returns a bucket with `winddir = 0`, not an error.
///
/// `ci/db/schema-assertions.sql` A-3 asserts the finaliser in isolation --
/// `mw_circular_mean(NULL, NULL, NULL)` is `0` and not a division by zero. This asserts the
/// **query the frozen endpoint actually issues**, over a station whose `winddir` column is
/// NULL in every row, through `ReadingStore::span`. The two are different failures: the Go
/// bug is `SQLSTATE 22012` from `circular_avg_final` dividing by an accumulator that stayed
/// zero, and it is a *query* failure rather than a wrong number -- so a finaliser that was
/// right and a `SELECT` that called it with the wrong arguments would pass A-3 and `500`
/// here.
///
/// It is latent in the Go tree only because the Go writer inserts its whole reading struct,
/// zeros included. **This** writer stores `NULL` for an unreported metric (**D-26**), which
/// makes the defect live -- so the regression test belongs to the item that first issues
/// the query, which is M4-2.
///
/// `windd` is `"swift_optional": false` in `contract/legacy-v1.json`, so returning NULL
/// instead of `0` would fail the iOS decode of the whole `/span` response rather than one
/// field. W4 **D-8** chose zero for that reason.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_span_over_a_station_with_no_wind_direction_returns_zero_rather_than_failing() {
    let station = "db-tests:w1-bug-1";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);
    let reader = TimescaleReader::new(pool.clone());

    // 2026-01-12T00:00:00Z, away from every other test's window.
    let base = 1_768_176_000i64;
    let mut batch: Vec<Envelope> = Vec::new();
    for minute in 0..10i64 {
        batch.push(envelope(
            station,
            at(base + minute * 60),
            (minute + 1) as u64,
            Reading {
                out_temp: Some(40.0 + minute as f32),
                wind_speed: Some(3.0),
                // **No `wind_dir`.** Which is the whole test: the column is NULL in every
                // row, so the aggregate's sine sum, cosine sum and direction count are all
                // NULL and the Go finaliser divides by zero.
                ..Reading::default()
            },
        ));
    }
    w.store(&batch).await.expect("the readings are stored");
    materialise(&pool, at(base), at(base + 600)).await;

    let columns = vec![
        (MetricKey::OutTemp, Agg::Mean),
        (MetricKey::WindDir, Agg::CircularMean),
        (MetricKey::WindSpeed, Agg::Mean),
    ];
    let span = reader
        .span(mw_component::SpanQuery {
            station: StationKey::new(station),
            level: mw_component::BucketLevel::Minute,
            since: at(base),
            // The whole window is closed, so the prefix is all ten buckets and the tail is
            // the minute after them -- which has no readings and must come back `None`
            // rather than as a row of NULLs.
            tail_start: at(base + 600),
            columns,
            limit: 64,
        })
        .await
        .expect("the query RUNS, which is the half W1-BUG-1 is about");

    assert_eq!(span.prefix.len(), 10, "ten one-minute buckets");
    assert!(
        span.tail.is_none(),
        "the trailing bucket has no readings in it, so it is absent from the response \
         rather than a row of zeros"
    );
    for (i, bucket) in span.prefix.iter().enumerate() {
        assert_eq!(
            bucket.values[1],
            Some(0.0),
            "bucket {i}: a station that never reported a direction gets 0, not NULL -- \
             `windd` is a non-optional Swift Double and a missing key fails the decode of \
             the whole /span response"
        );
        assert_eq!(
            bucket.values[0],
            Some(f64::from(40.0 + i as f32)),
            "bucket {i}: the metrics it did report are unaffected"
        );
        assert_eq!(bucket.values[2], Some(3.0), "bucket {i}");
    }

    // And the converse, so that the zero above is not simply what this column always
    // returns: a station that *does* report a direction gets the vector mean of it.
    let reporting = "db-tests:w1-bug-1-reporting";
    let pool = prepare(&[reporting]).await;
    let w = writer(&pool, reporting);
    let reader = TimescaleReader::new(pool.clone());
    w.store(&[
        envelope(
            reporting,
            at(base),
            1,
            Reading {
                wind_dir: Some(80.0),
                ..Reading::default()
            },
        ),
        envelope(
            reporting,
            at(base + 10),
            2,
            Reading {
                wind_dir: Some(100.0),
                ..Reading::default()
            },
        ),
    ])
    .await
    .expect("stores");
    materialise(&pool, at(base), at(base + 60)).await;
    let span = reader
        .span(mw_component::SpanQuery {
            station: StationKey::new(reporting),
            level: mw_component::BucketLevel::Minute,
            since: at(base),
            tail_start: at(base + 60),
            columns: vec![(MetricKey::WindDir, Agg::CircularMean)],
            limit: 8,
        })
        .await
        .expect("the query runs");
    let got = span.prefix[0].values[0].expect("a direction was reported");
    assert!(
        (got - 90.0).abs() < 1e-6,
        "the vector mean of 80 and 100 degrees is 90, got {got}"
    );

    touched(
        &pool,
        "a_span_over_a_station_with_no_wind_direction_returns_zero_rather_than_failing",
    )
    .await;
}

/// **A closed bucket the refresh policy has not reached is still in the response.**
///
/// Every level is `materialized_only = true`, and each policy runs on a schedule -- one
/// minute at `1m` and `5m`, five at `1h`, **thirty** at `1d`. So between a bucket closing
/// and its policy running, that bucket is inside the prefix's range and absent from the
/// view. The tail used to be exactly one bucket wide starting at `floor(now)`, which put
/// that bucket in neither half: `/span/720h` dropped *yesterday* for the first half hour of
/// every UTC day, and the Go server cannot reproduce it because its views are
/// `materialized_only = false`.
///
/// This is the state constructed directly: ten minutes of readings, the aggregate refreshed
/// over the first seven only, and a query whose window covers all ten.
///
/// **No gate in the tree could fail on this before.** `ci/conformance/rust-seed/03-refresh.sql`
/// refreshes every level over `(NULL, NULL)` before the replay and every test above calls
/// `materialise` by hand, so every one of them sees a fully materialised prefix.
#[tokio::test]
#[ignore = "needs a real TimescaleDB; run `cargo xtask db-tests`"]
async fn a_closed_bucket_the_aggregate_has_not_caught_up_to_is_still_returned() {
    let station = "db-tests:refresh-lag";
    let pool = prepare(&[station]).await;
    let w = writer(&pool, station);
    let reader = TimescaleReader::new(pool.clone());

    // 2026-01-19T00:00:00Z, away from every other test's window.
    let base = 1_768_780_800i64;
    let mut batch: Vec<Envelope> = Vec::new();
    for minute in 0..10i64 {
        batch.push(envelope(
            station,
            at(base + minute * 60),
            (minute + 1) as u64,
            Reading {
                out_temp: Some(10.0 + minute as f32),
                ..Reading::default()
            },
        ));
    }
    w.store(&batch).await.expect("the readings are stored");

    // The policy has reached minute 7 and no further, which is the ordinary steady state
    // for the few seconds after a bucket closes -- and for half an hour at the `1d` level.
    materialise(&pool, at(base), at(base + 7 * 60)).await;

    let query = mw_component::SpanQuery {
        station: StationKey::new(station),
        level: mw_component::BucketLevel::Minute,
        since: at(base),
        // Minute 10 is the still-filling bucket; minutes 0..=9 are closed.
        tail_start: at(base + 10 * 60),
        columns: vec![(MetricKey::OutTemp, Agg::Mean)],
        limit: 64,
    };
    let span = reader.span(query.clone()).await.expect("the query runs");

    assert_eq!(
        span.prefix.len(),
        7,
        "the view holds minutes 0..=6, which is what the policy was told to materialise"
    );
    assert_eq!(
        span.backfill.len(),
        3,
        "minutes 7, 8 and 9 are closed and not in the view, and used to be in neither half \
         of the answer: {:?}",
        span.backfill.iter().map(|b| b.bucket).collect::<Vec<_>>()
    );
    assert!(
        span.tail.is_none(),
        "no reading landed in minute 10, so the still-filling bucket is absent rather than \
         a row of zeros"
    );

    // **The response is continuous, and the backfilled buckets carry the right values** --
    // the same expression set the view uses, grouped on the same grid, so a backfilled
    // bucket is the one the policy will later materialise.
    let all: Vec<_> = span.buckets().collect();
    assert_eq!(all.len(), 10, "ten minutes in, ten buckets out");
    for (minute, bucket) in all.iter().enumerate() {
        assert_eq!(
            bucket.bucket,
            at(base + minute as i64 * 60),
            "bucket {minute} is on the grid and in order"
        );
        assert_eq!(
            bucket.values[0],
            Some(f64::from(10.0 + minute as f32)),
            "bucket {minute}'s mean"
        );
    }

    // And the cache-hit shape (**D-54**): `since == tail_start` asks for everything after a
    // prefix the caller already holds, which is the tail alone -- the prefix the handler
    // cached was complete by construction, because it only caches one that is.
    let hit = reader
        .span(mw_component::SpanQuery {
            since: at(base + 10 * 60),
            ..query
        })
        .await
        .expect("the query runs");
    assert!(hit.prefix.is_empty(), "the caller holds the prefix");
    assert!(
        hit.backfill.is_empty(),
        "a hit asks for nothing before the tail, because a cached prefix is a complete one"
    );

    touched(
        &pool,
        "a_closed_bucket_the_aggregate_has_not_caught_up_to_is_still_returned",
    )
    .await;
}
