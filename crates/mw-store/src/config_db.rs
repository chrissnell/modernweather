//! The configuration store: SQLite, migrated by name from `migrations/config/`.
//!
//! DESIGN.md D-8 as amended by A-W6-2 governs **every** schema this system owns, including
//! this one: schema changes happen solely through migrations, there is no DDL at startup,
//! and the `IF NOT EXISTS` idiom does not appear. The Go configuration database is the
//! actual instance of the defect D-8 was written about — `initializeSchema` hands a
//! `CREATE TABLE IF NOT EXISTS` script to one `Exec` on every start, so the schema has been
//! frozen at first-run shape ever since — which is why reinventing startup DDL here would
//! have reproduced the original defect in the only place it ever happened
//! (docs/reconciliation.md G-2).
//!
//! Two things this module does **not** do, both on purpose:
//!
//! - It does not own the pool. W3 D-41 makes the SQLite pool a root resource owned by
//!   `main`, which hands a `ScheduleStore` over `component_schedule` alone to the supervisor
//!   and the full handle to `config:sqlite`. [`open`] therefore returns the pool rather than
//!   storing it; who may see which table is M2-3's and M2-4's to arrange.
//! - It does not create a table. The only writes [`apply_migrations`] performs are the
//!   migration files' own, and the only file it creates is the database file itself, which
//!   has no schema in it.

use secrecy::SecretString;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::Row;
use std::path::Path;
use std::time::Duration;

/// The configuration migrations, embedded at compile time and applied by name.
///
/// Embedded rather than read from disk at run time because the schema a binary applies has
/// to be the schema that binary was built against: a daemon that read `migrations/config/`
/// from the filesystem would apply whatever happened to be installed there, which is the
/// same class of problem as the Go schema drifting 72 columns from its own source
/// definition.
///
/// The Arch package also installs the files under `/usr/share/modernweather/migrations/` so
/// an operator can read what will run. The `.deb` and `.rpm` asset lists in
/// `crates/modernweather/Cargo.toml` do not, so that is an Arch-only convenience rather
/// than a property of every artefact — W8's packaging to settle, not this module's.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations/config");

/// Errors from opening or migrating the configuration store.
///
/// Typed per module and preserving its cause chain (D-34). The path is carried in the
/// message because "unable to open database file" without one is the least useful error
/// SQLite produces.
#[derive(Debug, thiserror::Error)]
pub enum ConfigDbError {
    #[error("configuration database {path}: cannot open")]
    Open {
        path: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("configuration database {path}: migrations failed to apply")]
    Migrate {
        path: String,
        #[source]
        source: sqlx::migrate::MigrateError,
    },

    /// A read the boot sequence depends on failed.
    #[error("the configuration database could not be read: {what}")]
    Query {
        /// Which read, in words an operator can act on.
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// A column held something its own `CHECK` should have rejected, which means this
    /// build and its schema disagree rather than that the operator did anything.
    #[error("the configuration database column `{column}` holds `{value}`, which is out of range")]
    Column {
        /// Which column.
        column: &'static str,
        /// What it held.
        value: String,
    },
}

/// Connection settings for the configuration store.
///
/// `create_if_missing` creates an empty **file**, which carries no schema — the tables still
/// arrive only through [`apply_migrations`]. Without it a first start would have to be told
/// to initialise, and "initialise" is the word that grows a DDL path.
fn connect_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // Stated rather than inherited. SQLite's default is OFF, per connection, so every
        // foreign key in migrations/config/ is decorative on a connection that forgets —
        // and the ones that matter here are the website's station references and
        // `station_service`, where a dangling row means an uploader component planned for a
        // station that does not exist.
        .foreign_keys(true)
        // One writer and a handful of readers on a Raspberry-Pi-class machine with an SD
        // card: WAL so a management API read is not blocked by a config write, NORMAL
        // because the configuration is written by hand a few times a year and a lost
        // last-commit on power failure is recoverable by retyping one form.
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
}

/// Open the configuration store. Applies nothing; call [`apply_migrations`] next.
pub async fn open(path: &Path) -> Result<SqlitePool, ConfigDbError> {
    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options(path))
        .await
        .map_err(|source| ConfigDbError::Open {
            path: path.display().to_string(),
            source,
        })
}

/// Apply every migration in `migrations/config/` that has not been applied, by name.
///
/// A second call applies nothing, because the ledger says so — not because the statements
/// tolerate being run twice. That distinction is the whole of A-W6-2, and
/// `cargo xtask gate db-migrate-twice` is where it is checked.
pub async fn apply_migrations(pool: &SqlitePool, path: &Path) -> Result<(), ConfigDbError> {
    MIGRATOR
        .run(pool)
        .await
        .map_err(|source| ConfigDbError::Migrate {
            path: path.display().to_string(),
            source,
        })
}

/// Open and migrate in one step — the boot sequence's step 3 (W3 §3.1).
pub async fn open_and_migrate(path: &Path) -> Result<SqlitePool, ConfigDbError> {
    let pool = open(path).await?;
    apply_migrations(&pool, path).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory per test, removed on drop. `tempfile` would be a dev-dependency
    /// for twenty lines; A-4 permits a test harness its own crates, but not needing one is
    /// better than being permitted one.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mw-config-db-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
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

    /// Every object the schema defines, as the SQL that defines it, with the migration
    /// ledger and whitespace differences removed. Comparing this rather than a `.schema`
    /// dump is what makes "the two paths agree" a statement about the schema rather than
    /// about the order sqlite happened to print it in.
    async fn schema(pool: &SqlitePool) -> Vec<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT type, name, COALESCE(replace(replace(sql, char(10), ' '), char(9), ' '), '')
               FROM sqlite_master
              WHERE name NOT LIKE 'sqlite_%'
                AND name NOT LIKE '\\_sqlx\\_migrations' ESCAPE '\\'
              ORDER BY type, name",
        )
        .fetch_all(pool)
        .await
        .expect("sqlite_master is readable")
    }

    async fn applied(pool: &SqlitePool) -> Vec<i64> {
        sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .expect("the ledger is readable")
    }

    fn migrations_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../migrations")
    }

    /// Every `.sql` under `migrations/`, recursively, as (path, comment-stripped body).
    fn all_migration_sql() -> Vec<(PathBuf, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let entries = std::fs::read_dir(dir).expect("migrations/ is readable");
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|x| x == "sql") {
                    out.push(path);
                }
            }
        }
        let mut paths = Vec::new();
        walk(&migrations_root(), &mut paths);
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let body = std::fs::read_to_string(&path).expect("readable");
                let stripped = strip_sql_comments(&body).to_uppercase();
                (path, stripped)
            })
            .collect()
    }

    /// `--` line comments removed, so a comment *about* the idiom is not a use of it. The
    /// existing weather migrations and the config schema both say "no `IF NOT EXISTS`" in
    /// prose, and a grep that could not tell those apart would have to be either wrong or
    /// ignored.
    fn strip_sql_comments(sql: &str) -> String {
        sql.lines()
            .map(|line| match line.find("--") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn migrations_apply_from_empty() {
        let scratch = Scratch::new("empty");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        let objects = schema(&pool).await;
        assert!(
            objects
                .iter()
                .any(|(t, n, _)| t == "table" && n == "stations"),
            "config.stations is missing from {objects:#?}"
        );
        // Every table the work item names, by the name the document that specified it uses.
        for required in [
            "component_schedule",
            "admin_credential",
            "admin_session",
            "peers",
            "peer_origins",
            "stations",
            "station_service",
            "uploader",
            "website",
        ] {
            assert!(
                objects
                    .iter()
                    .any(|(t, n, _)| t == "table" && n == required),
                "{required} is missing"
            );
        }
        assert_eq!(applied(&pool).await.len(), MIGRATOR.iter().count());
        pool.close().await;
    }

    /// The headline done-criterion. The second application must apply nothing and must
    /// leave the schema byte-identical — and it must do so because of the ledger, which is
    /// the next test.
    #[tokio::test]
    async fn second_application_is_a_no_op() {
        let scratch = Scratch::new("twice");
        let path = scratch.db();

        let pool = open_and_migrate(&path).await.expect("first apply");
        let before = schema(&pool).await;
        let versions = applied(&pool).await;
        pool.close().await;

        // A second process over the same file, which is what a restart actually is.
        let pool = open_and_migrate(&path).await.expect("second apply");
        assert_eq!(
            schema(&pool).await,
            before,
            "the schema changed on re-apply"
        );
        assert_eq!(applied(&pool).await, versions, "a migration ran twice");
        pool.close().await;
    }

    /// And the converse, which is what makes the test above worth having: the statements do
    /// **not** tolerate a second application. If this test ever fails, an `IF NOT EXISTS`
    /// has appeared and `second_application_is_a_no_op` has become green for the wrong
    /// reason — the exact failure mode D-8 exists to prevent.
    #[tokio::test]
    async fn re_running_a_migration_body_fails() {
        let scratch = Scratch::new("body");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        let mut attempted = 0;
        for migration in MIGRATOR.iter() {
            attempted += 1;
            let err = sqlx::raw_sql(&migration.sql)
                .execute(&pool)
                .await
                .expect_err("a migration body applied twice without error");
            let message = err.to_string();
            assert!(
                message.contains("already exists"),
                "{}: expected an 'already exists' error, got: {message}",
                migration.description
            );
        }
        assert!(attempted > 0, "there are no migrations to re-run");
        pool.close().await;
    }

    /// Two scopes, because the two schemas are not in the same position, and
    /// `ci/db/migrate-twice.sh` enforces exactly this pair so the shell half and the Rust
    /// half of one rule agree.
    ///
    /// Everywhere under `migrations/`: `IF NOT EXISTS`, which is the work item's
    /// done-criterion verbatim and which both schemas satisfy. Under `migrations/config/`
    /// also `OR REPLACE` and `IF EXISTS`, because this schema is written from scratch and
    /// has no legitimate use for either — while the weather schema does and always will,
    /// since `cargo xtask codegen` emits `CREATE OR REPLACE FUNCTION mw_mean` and
    /// `cargo xtask migrate` emits `DROP MATERIALIZED VIEW IF EXISTS ... CASCADE`. A rule
    /// the repository's own generator cannot satisfy is not a rule.
    #[tokio::test]
    async fn no_conditional_ddl_under_migrations() {
        let config_dir = migrations_root().join("config");
        let files = all_migration_sql();
        assert!(!files.is_empty(), "migrations/ holds no .sql");
        assert!(
            files.iter().any(|(p, _)| p.starts_with(&config_dir)),
            "migrations/config/ holds no .sql"
        );

        for (path, sql) in files {
            let mut idioms: Vec<&str> = vec!["IF NOT EXISTS", "IF_NOT_EXISTS"];
            if path.starts_with(&config_dir) {
                idioms.extend(["OR REPLACE", "IF EXISTS"]);
            }
            for idiom in idioms {
                assert!(
                    !sql.contains(idiom),
                    "{}: `{idiom}` in a statement. D-8/A-W6-2: a migration that tolerates \
                     being applied twice is how the Go schema froze at first-run shape.",
                    path.display()
                );
            }
        }
    }

    /// Foreign keys are per-connection and OFF by default in SQLite, so the pragma in
    /// `connect_options` is load-bearing rather than tidy. A pool that forgot it would make
    /// every reference in the schema decorative, and the symptom would be an uploader
    /// component planned for a station nobody can find.
    #[tokio::test]
    async fn foreign_keys_are_enforced_on_a_pooled_connection() {
        let scratch = Scratch::new("fk");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
                .fetch_one(&pool)
                .await
                .expect("pragma"),
            1
        );

        let err = sqlx::query(
            "INSERT INTO station_service (station_key, service, uploader_id)
             VALUES ('nobody:nowhere', 'aprs', 1)",
        )
        .execute(&pool)
        .await
        .expect_err("a dangling station_service row was accepted");
        assert!(
            err.to_string().to_uppercase().contains("FOREIGN KEY"),
            "expected a foreign-key failure, got: {err}"
        );
        pool.close().await;
    }

    /// The pairing's service and the account's kind are written on two rows, and this is
    /// the one join where they could disagree. A composite foreign key is what stops it;
    /// `uploader_id` alone would not, and the same-service-in-two-places defect that
    /// `station_service` exists to remove would come back wearing a join.
    #[tokio::test]
    async fn a_pairing_cannot_name_an_account_of_another_service() {
        let scratch = Scratch::new("pairing");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        for setup in [
            "INSERT INTO uploader (kind, name) VALUES ('wunderground', 'wu')",
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_dev, transport_baud)
             VALUES ('s', x'0193f3c28b207c71ae3f10cc92d4b7aa', 's', 'davis', 'fixed',
                     'serial', '/dev/ttyUSB0', 19200)",
        ] {
            sqlx::query(setup).execute(&pool).await.expect("setup");
        }

        let err = sqlx::query(
            "INSERT INTO station_service (station_key, service, uploader_id)
             VALUES ('s', 'aprs', (SELECT id FROM uploader WHERE name = 'wu'))",
        )
        .execute(&pool)
        .await
        .expect_err("an aprs pairing pointing at a wunderground account was accepted");
        assert!(
            err.to_string().to_uppercase().contains("FOREIGN KEY"),
            "expected a foreign-key failure, got: {err}"
        );

        // And the matching pairing goes in, so the constraint is not simply rejecting
        // everything.
        sqlx::query(
            "INSERT INTO station_service (station_key, service, uploader_id)
             VALUES ('s', 'wunderground', (SELECT id FROM uploader WHERE name = 'wu'))",
        )
        .execute(&pool)
        .await
        .expect("the matching pairing was rejected");
        pool.close().await;
    }

    /// The rows the design documents DO contain, which earlier constraints rejected. Each
    /// one is a `docs/collector.md` §8.3 block or a Go `devices` column M7-4 has to move,
    /// and §8 says those blocks are the shape of the `ci/examples/*.db` fixtures `plan()`
    /// is checked against — so a constraint that rejects one makes a documented fixture
    /// unrepresentable.
    #[tokio::test]
    async fn the_documented_configurations_are_representable() {
        let scratch = Scratch::new("documented");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        for (what, sql) in [
            (
                "§8.3's [[peer]] under a shared CA, which carries no auth material",
                "INSERT INTO peers (node_id, name, auth_kind)
                 VALUES (x'0193f3c27a107b649c2e4f1b8a6d0e11', 'satellite-a', 'mtls-ca')",
            ),
            (
                "§8.3's second [[ingest]], which omits the CA the first one names",
                "INSERT INTO ingest (listen, peer_auth) VALUES ('rest', 'mtls-ca')",
            ),
            (
                "the configured-device type vocabulary /stationinfo freezes (§3.1)",
                "INSERT INTO stations (station_key, owning_node_id, name, type,
                                       identity_kind, transport_kind, transport_host,
                                       transport_port)
                 VALUES ('c', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'c',
                         'campbellscientific', 'fixed', 'tcp_out', '10.0.0.9', 6785)",
            ),
            (
                "an Aeris pairing, which has no uploader because Aeris is not an egress",
                "INSERT INTO station_service (station_key, service) VALUES ('c', 'aeris')",
            ),
        ] {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the schema rejected {what}: {e}"));
        }
        pool.close().await;
    }

    /// The converse of the last case in the test below, and the reason that case is scoped
    /// to local stations. A station received by registration carries the SATELLITE's
    /// `station_type` (W7 §5.2), so a newer peer running a driver this collector has never
    /// heard of must still enrol. A schema that rejected it would make version skew between
    /// nodes an enrolment failure, which is the opposite of why `peers.software` exists.
    #[tokio::test]
    async fn a_received_station_may_name_an_unknown_driver() {
        let scratch = Scratch::new("skew");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        sqlx::query(
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind)
             VALUES ('satellite-b:new', x'0193f3c27a107b649c2e4f1b8a6d0e11', 'new-sensor',
                     'some-future-driver', 'from_registration')",
        )
        .execute(&pool)
        .await
        .expect("a received station with an unknown driver type was rejected");
        pool.close().await;
    }

    /// The constraints the design documents name an illegal state for, exercised as a group
    /// so that one of them being accidentally always-true is visible. W2a §4a.4's Ambient
    /// bug is the one that matters most: identity-from-the-port is what it was.
    #[tokio::test]
    async fn the_named_illegal_states_are_unrepresentable() {
        let scratch = Scratch::new("checks");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        let node = "x'0193f3c28b207c71ae3f10cc92d4b7aa'";
        let phc = "'$argon2id$v=19$m=65536,t=3,p=4$c2FsdHNhbHQ$aGFzaGhhc2g'";
        let cases: [(&str, String); 19] = [
            (
                "an inbound transport with a fixed identity (W2a §4a.4)",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_mount)
                     VALUES ('a', {node}, 'a', 'ambient-customized', 'fixed', 'http_in',
                             '/i')"
                ),
            ),
            (
                "a serial station with no baud rate",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_dev)
                     VALUES ('b', {node}, 'b', 'davis', 'fixed', 'serial', '/dev/ttyUSB0')"
                ),
            ),
            (
                "a registered station carrying a local transport",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_dev,
                                           transport_baud)
                     VALUES ('c', {node}, 'c', 'davis', 'from_registration', 'serial',
                             '/dev/ttyUSB0', 19200)"
                ),
            ),
            (
                "a second administrator (D-35)",
                format!(
                    "INSERT INTO admin_credential (id, username, password_phc)
                     VALUES (2, 'second', {phc})"
                ),
            ),
            (
                "a bare digest where a PHC string belongs (W5 §3.2)",
                "INSERT INTO admin_credential (id, username, password_phc)
                 VALUES (1, 'only', 'deadbeef')"
                    .to_string(),
            ),
            (
                "a Weather Underground uploader carrying an APRS passcode (D-66)",
                "INSERT INTO uploader (kind, name, aprs_passcode)
                 VALUES ('wunderground', 'wrong', '12345')"
                    .to_string(),
            ),
            (
                "the gRPC port colliding with the HTTP port (W3 D-92)",
                "UPDATE listener SET grpc_port = http_port WHERE id = 1".to_string(),
            ),
            (
                "an upper-case hostname (W3 §6.3)",
                "INSERT INTO website (name, hostname) VALUES ('w', 'EXAMPLE.COM')".to_string(),
            ),
            (
                "a bare wildcard CORS origin (A-W5-3)",
                "INSERT INTO management_cors_origin (origin) VALUES ('*')".to_string(),
            ),
            (
                // The case that matters, and the one the bare '*' above does NOT cover:
                // '*' fails on the scheme CHECK and never reaches the wildcard CHECK, so
                // without this row the wildcard rule is untested.
                "a subdomain-wildcard CORS origin (A-W5-3)",
                "INSERT INTO management_cors_origin (origin)
                 VALUES ('https://*.example.com')"
                    .to_string(),
            ),
            (
                // SQLite's LIKE is ASCII-case-insensitive, so the GLOB form is load-bearing:
                // an upper-case origin can never match a browser Origin header, and an
                // allowlist entry that silently does nothing is worse than a rejected one.
                "an upper-case CORS origin (A-W5-3)",
                "INSERT INTO management_cors_origin (origin) VALUES ('HTTPS://EVIL.COM')"
                    .to_string(),
            ),
            (
                "a Weather Underground uploader carrying APRS KISS settings (D-66)",
                "INSERT INTO uploader (kind, name, aprs_kiss_path)
                 VALUES ('wunderground', 'leak', 'WIDE1-1')"
                    .to_string(),
            ),
            (
                "an APRS-IS uploader carrying a KISS TNC address (D-66)",
                "INSERT INTO uploader (kind, name, aprs_transport, aprs_kiss_tcp_address)
                 VALUES ('aprs', 'leak2', 'aprs-is', '127.0.0.1:8001')"
                    .to_string(),
            ),
            (
                "an mtls-ca peer carrying auth material the CA makes meaningless (W7 §8.3)",
                "INSERT INTO peers (node_id, name, auth_kind, auth_material)
                 VALUES (x'0193f3c27a107b649c2e4f1b8a6d0e11', 'p', 'mtls-ca', 'x')"
                    .to_string(),
            ),
            (
                "a grpc_out transport with no streaming flag for plan() to read (W2a §4a.3)",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_url)
                     VALUES ('g', {node}, 'g', 'snowgauge', 'fixed', 'grpc_out',
                             'https://h:1')"
                ),
            ),
            (
                // A CHECK that evaluates to NULL passes in SQLite, and `date('2026-99-99')`
                // is NULL — so this case and the two below it are the ones that caught
                // three accidentally-always-true constraints.
                "a Calendar occurrence on a date that does not exist (W2a D-13)",
                "INSERT INTO component_schedule
                 VALUES ('c', '2026-99-99', 0, 1, 'ok')"
                    .to_string(),
            ),
            (
                "an APRS uploader with KISS settings and no KISS transport (D-66)",
                "INSERT INTO uploader (kind, name, aprs_kiss_path)
                 VALUES ('aprs', 'nulltrap', 'WIDE1-1')"
                    .to_string(),
            ),
            (
                "a received station carrying a transport field it has no transport for",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_streaming)
                     VALUES ('n', {node}, 'n', 'davis', 'from_registration', 1)"
                ),
            ),
            (
                "a local station naming a driver that does not exist (W2a §4a.5)",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_dev,
                                           transport_baud)
                     VALUES ('d', {node}, 'd', 'some-future-driver', 'fixed', 'serial',
                             '/dev/ttyUSB0', 19200)"
                ),
            ),
        ];

        for (what, sql) in cases {
            let err = match sqlx::query(&sql).execute(&pool).await {
                Ok(_) => panic!("the schema accepted {what}"),
                Err(e) => e.to_string(),
            };
            // Which failure matters. `is_err()` alone lets a case pass for the wrong
            // reason — a typo in a column name is also an error — and the point of this
            // test is that a constraint which is accidentally always-true becomes visible.
            let upper = err.to_uppercase();
            assert!(
                upper.contains("CONSTRAINT FAILED") || upper.contains("UNIQUE CONSTRAINT"),
                "{what} failed, but not on a constraint: {err}"
            );
        }
        pool.close().await;
    }
}

// ---------------------------------------------------------------------------------------
// What the boot sequence reads
//
// Three tables, because the boot sequence needs three facts before any component exists
// (W3 §3.1): where to bind, whether there is a weather database to assert the version of
// (**D-62**), and whether this node has started before. The rest of the schema is read by
// the components that consume it, through `ConfigStore` -- a reader here for a table no
// consumer exists for yet would be a guess at the query that consumer wants.

/// The listener row (W3 §6.1).
///
/// Seeded by the migration, so this row always exists: there is nothing here that has to
/// be allocated, and a daemon that had to upsert its own listener row before it could bind
/// would be a startup DDL path wearing different clothes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// The address the main socket binds.
    pub listen_addr: String,
    /// The main port: HTTP, and gRPC over ALPN.
    pub http_port: u16,
    /// The TLS port, when one is configured.
    pub https_port: Option<u16>,
    /// The legacy gRPC port, which keeps its own socket (W3 D-92) so that no peer's
    /// endpoint changes at upgrade.
    pub grpc_port: u16,
    /// The management API's address. Loopback by default (A-W5-3).
    pub management_listen_addr: String,
    /// The management API's port.
    pub management_port: u16,
}

/// Read the listener row.
pub async fn listener(pool: &SqlitePool) -> Result<Listener, ConfigDbError> {
    let row = sqlx::query(
        "SELECT listen_addr, http_port, https_port, grpc_port, \
                management_listen_addr, management_port \
         FROM listener WHERE id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the listener row",
        source,
    })?;
    Ok(Listener {
        listen_addr: row.get("listen_addr"),
        http_port: port(&row, "http_port")?,
        https_port: optional_port(&row, "https_port")?,
        grpc_port: port(&row, "grpc_port")?,
        management_listen_addr: row.get("management_listen_addr"),
        management_port: port(&row, "management_port")?,
    })
}

/// The weather database's connection URL (**D-33**: a secret from the moment it is read).
#[derive(Debug, Clone)]
pub struct WeatherDatabase {
    /// The PostgreSQL URL. `GET /api/v1/config` omits it rather than redacting it.
    pub url: SecretString,
}

/// Read the weather database row, if there is one.
///
/// **Its absence is meaningful**: a node with no row here has no `db:postgres` and
/// therefore no `writer:timescale` (**D-44**, W7 §8.2). It is also what makes
/// `modernweather --config <path>` on an empty configuration a legitimate start rather
/// than a misconfiguration -- there is no database to assert a version against, and
/// **D-62** has nothing to do.
pub async fn weather_database(pool: &SqlitePool) -> Result<Option<WeatherDatabase>, ConfigDbError> {
    let row = sqlx::query("SELECT url FROM weather_database WHERE id = 1")
        .fetch_optional(pool)
        .await
        .map_err(|source| ConfigDbError::Query {
            what: "the weather database row",
            source,
        })?;
    Ok(row.map(|r| WeatherDatabase {
        url: SecretString::from(r.get::<String, _>("url")),
    }))
}

/// The node's own identity and spool settings (W7 §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// 16 bytes, UUIDv7, allocated by the node itself.
    pub node_id: Vec<u8>,
    /// Operator-facing and mutable: renaming a node is not an identity change.
    pub node_name: String,
    /// `TzSource::Deployment` for components not scoped to a station.
    pub timezone: String,
    /// Where the spool lives.
    pub spool_path: String,
    /// `[spool].ceiling` in bytes — W3 §2.4's 2 GiB default, as the schema's `DEFAULT`.
    ///
    /// Read here rather than defaulted in `mw-pipeline` because the column exists and a
    /// second copy of the default is a second place for it to drift (**D-8**: the schema is
    /// the schema).
    pub spool_ceiling_bytes: i64,
    /// `[spool].max_lag` in seconds — W3 §2.7's seven days.
    ///
    /// W7 **D-16** requires this to fire before the ceiling does at the node's observed
    /// rate. That is not a `CHECK` and not a `plan()` check either, because it depends on
    /// the admit rate; it is the `mw_spool_time_remaining_seconds` gauge and a startup
    /// warning the spool itself emits.
    pub spool_max_lag_seconds: i64,
}

/// Read the node row, if this node has one.
///
/// **An empty table means "not yet started"**, which the migration says in the comment
/// above the table and leaves to the boot sequence to handle. It is handled by being a
/// legitimate state: nothing in the boot sequence needs a node identity to converge, so a
/// first start with no configuration starts, serves `/metrics` and stops cleanly. What
/// *allocates* the identity is W7's enrolment (`node_id` is a UUIDv7 the node mints) and
/// the row it writes also needs a `spool_path`, which has no default in the schema and no
/// value in any design document -- so inventing one here would be this work item deciding
/// something implementation-plan.md M6-4 owns.
pub async fn node(pool: &SqlitePool) -> Result<Option<Node>, ConfigDbError> {
    let row = sqlx::query(
        "SELECT node_id, node_name, timezone, spool_path, \
                spool_ceiling_bytes, spool_max_lag_seconds \
         FROM node WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the node row",
        source,
    })?;
    Ok(row.map(|r| Node {
        node_id: r.get("node_id"),
        node_name: r.get("node_name"),
        timezone: r.get("timezone"),
        spool_path: r.get("spool_path"),
        spool_ceiling_bytes: r.get("spool_ceiling_bytes"),
        spool_max_lag_seconds: r.get("spool_max_lag_seconds"),
    }))
}

/// A port column, which SQLite hands back as an `i64` and whose range the schema already
/// constrains to 1-65535.
fn port(row: &sqlx::sqlite::SqliteRow, column: &'static str) -> Result<u16, ConfigDbError> {
    let value: i64 = row.get(column);
    u16::try_from(value).map_err(|_| ConfigDbError::Column {
        column,
        value: value.to_string(),
    })
}

fn optional_port(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
) -> Result<Option<u16>, ConfigDbError> {
    let Some(value) = row.get::<Option<i64>, _>(column) else {
        return Ok(None);
    };
    u16::try_from(value)
        .map(Some)
        .map_err(|_| ConfigDbError::Column {
            column,
            value: value.to_string(),
        })
}
