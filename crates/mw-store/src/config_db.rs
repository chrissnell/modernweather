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

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn migrations_root() -> PathBuf {
        repo_root().join("migrations")
    }

    /// Every `.sql` under `root`, recursively, as (path, comment-stripped upper-cased body).
    ///
    /// The extension match is case-insensitive. There is no `.SQL` in the tree today, so
    /// that is not a fix for anything -- but a guard whose scope depends on the case of an
    /// extension is a guard with a one-keystroke bypass, and every caller below is a guard.
    /// [`the_sun_times_guard_bites`] pins it with an upper-cased file, so reverting the
    /// match to `x == "sql"` turns that control red rather than quietly narrowing the scope.
    ///
    /// Takes a root rather than closing over `migrations/` so that
    /// [`the_sun_times_guard_bites`] can run **this** function over a synthetic tree. A
    /// negative control that reimplements the pipeline it is controlling for proves only
    /// that it agrees with itself.
    fn sql_under(root: &std::path::Path) -> Vec<(PathBuf, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let entries = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("{} is readable: {e}", dir.display()));
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("sql"))
                {
                    out.push(path);
                }
            }
        }
        let mut paths = Vec::new();
        walk(root, &mut paths);
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

    /// The repository's **tracked** `.sql` under `root`.
    ///
    /// [`sql_under`] walks the filesystem, and the filesystem is not the repository.
    /// `ci/db/migrate-from-go/` is the migration lab's working area: a Go config dump
    /// staged there by hand is not part of this tree, and a guard that failed on it would
    /// be advising whoever is running the lab to delete the guard. `git ls-files` is the
    /// only available definition of "in the tree".
    ///
    /// When `git` cannot answer -- no `git` on `PATH`, a source tarball, a non-repository
    /// checkout -- the walk stands unfiltered rather than silently scanning nothing. Both
    /// callers assert non-emptiness, so the failure mode of this fallback is reading too
    /// much, which is the direction a guard should fail in.
    fn tracked_sql_under(root: &std::path::Path) -> Vec<(PathBuf, String)> {
        let files = sql_under(root);
        let Some(tracked) = tracked_sql_paths() else {
            return files;
        };
        files
            .into_iter()
            .filter(|(path, _)| {
                path.canonicalize()
                    .is_ok_and(|resolved| tracked.contains(&resolved))
            })
            .collect()
    }

    /// Every tracked `.sql` in the repository, canonicalised, or `None` if `git` cannot say.
    fn tracked_sql_paths() -> Option<std::collections::HashSet<PathBuf>> {
        let root = repo_root();
        let top = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let top = PathBuf::from(String::from_utf8(top.stdout).ok()?.trim());

        let listed = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["ls-files", "-z", "--full-name", "--", "*.sql", "*.SQL"])
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let paths: std::collections::HashSet<PathBuf> = String::from_utf8(listed.stdout)
            .ok()?
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| top.join(entry).canonicalize().ok())
            .collect();
        (!paths.is_empty()).then_some(paths)
    }

    /// Every tracked `.sql` under `migrations/`, recursively.
    fn all_migration_sql() -> Vec<(PathBuf, String)> {
        tracked_sql_under(&migrations_root())
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
            // Two phrasings, because SQLite has two for the same fact and the property
            // being asserted is "a second application is an error", not which sentence it
            // is. `CREATE TABLE x` says "table x already exists"; `ALTER TABLE x ADD
            // COLUMN y` says "duplicate column name: y" -- which is what
            // `20261104120000_station_presented_key_and_weatherlink_options.sql` produces,
            // and it is the same refusal.
            assert!(
                message.contains("already exists") || message.contains("duplicate column name"),
                "{}: expected a second application to be refused, got: {message}",
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

    /// W2a Decision 36: the Go `sun_times` table is **not** carried forward, and sunrise
    /// and sunset are evaluated per request from the station's coordinates instead
    /// (`mw_api_legacy::sun`). Both schemas were authored without the table rather than
    /// authored with it and then dropped, so no `DROP TABLE sun_times` exists anywhere and
    /// "the table is gone" is a property of the *absence* of a statement. Nothing would
    /// notice that being undone, which is why the deletion gets a test and not a migration.
    ///
    /// **Two roots, and the second is the one that matters.** `migrations/` covers both
    /// schemas, the weather one included -- that is the wrong home for the table twice
    /// over, and therefore the likelier hand-edit. But `migrations/` is **not** the only
    /// SQL this system executes: `ci/db/migrate-from-go/` creates `observations_1d_legacy`
    /// and the whole `w9` staging schema outside it, and it is the one place in this tree
    /// that carries Go data forward -- so it is where 366 rows per station would land if
    /// anyone decided they were worth keeping. D-8 forbids DDL at *startup*; it does not
    /// make `migrations/` the only creator of objects, and a guard that assumed otherwise
    /// would be scoped to the less likely vector alone.
    ///
    /// Text over the SQL sources rather than a query against an applied schema, because the
    /// two roots target different engines and one of them is never applied in a unit test.
    ///
    /// Only `--` comments are stripped ([`strip_sql_comments`]), so prose about the absence
    /// must use that form; a `/* ... */` block naming the table would fail this test.
    ///
    /// **This guards the table, not the capability**, and the capability needs no guard of
    /// its own: `Capability` is a closed enum and `mw-component/src/capability.rs` already
    /// asserts `Capability::ALL.len() == 12`, so a `SunTimes` variant turns that test red.
    /// The table has no such single home -- it can arrive in any `.sql` under either root.
    ///
    /// The *values* are guarded elsewhere and by measurement:
    /// `mw-api-legacy/tests/sun_times.rs`'s `the_served_strings_are_the_stored_go_tables`
    /// shows the per-request evaluation reproduces the six captured `/latest` values the Go
    /// daemon served from its table. Six captures sharing one day-of-year key, not the
    /// 366-key table -- that test states its own reach and this comment does not widen it.
    ///
    /// **If the fallback is ever wanted**, W2a §4.3 specifies its shape -- a
    /// `Calendar { yearly, Jan 1, Station }` component with a reactive rebuild on a
    /// coordinate change -- and reinstating it starts by deleting this test. That is the
    /// intended way to undo it: one deliberate edit rather than a silent one.
    #[test]
    fn the_sun_times_table_is_not_carried_forward() {
        for (root, also) in [
            (migrations_root(), Some(migrations_root().join("config"))),
            (repo_root().join("ci/db"), None),
        ] {
            let files = tracked_sql_under(&root);
            assert!(
                !files.is_empty(),
                "{} holds no tracked .sql",
                root.display()
            );
            if let Some(nested) = also {
                assert!(
                    files.iter().any(|(p, _)| p.starts_with(&nested)),
                    "{} holds no .sql, so this guard is scanning one schema alone",
                    nested.display()
                );
            }
            for (path, sql) in files {
                assert!(
                    !sql.contains(SUN_TIMES),
                    "{}: `sun_times` is back. W2a D-36 deletes it in favour of a \
                     per-request evaluation, and the fallback shape is W2a §4.3 -- \
                     adopting it means deleting \
                     `the_sun_times_table_is_not_carried_forward`, not slipping a table \
                     past it.",
                    path.display()
                );
            }
        }
    }

    /// The identifier the guard above looks for, as [`sql_under`] would have upper-cased it.
    const SUN_TIMES: &str = "SUN_TIMES";

    /// And the converse, in the same spirit as `re_running_a_migration_body_fails`: a guard
    /// over a name that is absent from the whole tree passes whether or not it works.
    ///
    /// So this runs the **real** [`sql_under`] over a synthetic tree and asserts the guard's
    /// own predicate fires on it. That is the point of the shape: every step `sql_under`
    /// performs is load-bearing here, so dropping its `to_uppercase`, its recursion, its
    /// extension filter or the case-insensitivity of that filter turns this test red rather
    /// than leaving the guard quietly narrower than it reads.
    ///
    /// It runs `sql_under` and not [`tracked_sql_under`], because the synthetic tree is not
    /// in the repository and `git ls-files` would correctly report none of it -- a control
    /// that filtered by tracked-ness would assert against an empty set.
    #[test]
    fn the_sun_times_guard_bites() {
        let scratch = Scratch::new("suntimes");
        let nested = scratch.0.join("config");
        std::fs::create_dir_all(&nested).expect("a nested directory");

        // Mixed case, a nested directory and a trailing `--` comment, because that is what
        // a hand-edit reinstating the Go table looks like.
        std::fs::write(
            nested.join("20991231000000_reinstate.sql"),
            "create table Sun_Times (\n               station_name TEXT NOT NULL,\n               day_of_year INTEGER NOT NULL\n             ) STRICT; -- W2a 4.3\n",
        )
        .expect("writable");
        // And prose about the absence, which is what every `--` reference to the table in
        // this repository now is. It must NOT trip the guard, or the guard would forbid
        // explaining itself.
        std::fs::write(
            scratch.0.join("20991231000001_prose.sql"),
            "-- W2a D-36: there is no sun_times table here, deliberately.\nSELECT 1;\n",
        )
        .expect("writable");
        // An upper-cased extension, which is the one-keystroke bypass `sql_under`'s
        // `eq_ignore_ascii_case` exists to close. Nothing in the repository is named this
        // way, so without this file reverting that match to `x == "sql"` leaves every test
        // here green and the guard case-sensitive.
        std::fs::write(
            scratch.0.join("20991231000002_reinstate.SQL"),
            "CREATE TABLE sun_times (station_name TEXT NOT NULL) STRICT;\n",
        )
        .expect("writable");
        // Not `.sql`, and so outside the guard's declared scope in either case.
        std::fs::write(scratch.0.join("notes.md"), "sun_times\n").expect("writable");

        let files = sql_under(&scratch.0);
        let mut tripped: Vec<String> = files
            .iter()
            .filter(|(_, sql)| sql.contains(SUN_TIMES))
            .map(|(p, _)| {
                p.file_name()
                    .expect("a file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        tripped.sort();
        assert_eq!(
            tripped,
            vec![
                "20991231000000_reinstate.sql".to_owned(),
                "20991231000002_reinstate.SQL".to_owned(),
            ],
            "the guard fired on {tripped:?}: it must catch the reinstatement in a nested \
             directory, in mixed case and behind an upper-cased extension, and must not \
             fire on a `--` comment about the absence. Files read: {:?}",
            files.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
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
            // The shape `ci/db/config-seed.sql` seeds, and the one that is load-bearing for
            // every FUTURE column added to this table: an inbound endpoint with a secret
            // and NO claim. A migration that added a constraint requiring a claim could not
            // apply to a database holding this row, because `ADD COLUMN` validates a new
            // CHECK against the rows already there -- and the Rust suite would stay green
            // while `cargo xtask gate db-migrate-twice` went red, because this was the one
            // identity strategy no test here used.
            (
                "§4a.4's from_query endpoint: a secret, and no claim configured yet",
                "INSERT INTO stations (station_key, owning_node_id, name, type,
                                       identity_kind, ingest_secret_hash, transport_kind,
                                       transport_mount)
                 VALUES ('ambient', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'ambient',
                         'ambient-customized', 'from_query', 'sha256:0f1e2d3c', 'http_in',
                         '/ingest/ambient')",
            ),
        ] {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the schema rejected {what}: {e}"));
        }
        pool.close().await;
    }

    /// The five columns the snapshot's `(website, station)` key is built from, and the
    /// ordering its `ConfigDigest` depends on.
    ///
    /// This is the only new query the daemon runs on **every** boot, so a typo in a column
    /// name here is a `ConfigDbError::Query` that fails `Config::load` and therefore the
    /// start — and nothing else in the suite inserts a `website` row.
    #[tokio::test]
    async fn a_website_row_round_trips_and_the_order_is_by_id() {
        let scratch = Scratch::new("websites");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        // `from_registration` with no transport, which is the schema's shape for a station
        // a peer forwards readings for -- and the cheapest row that satisfies
        // `(transport_kind IS NULL) = (identity_kind = 'from_registration')`. What this test
        // needs from them is only that the website's foreign keys resolve.
        //
        // Inserted out of order, so `ORDER BY id` is asserted rather than coincidental:
        // `mw_pipeline::snapshot`'s `ConfigDigest` is built by walking this list, and an
        // unordered `SELECT` would make the plan diff depend on SQLite's row order.
        sqlx::query(
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind)
             VALUES ('backyard', x'07070707070707070707070707070707', 'Backyard',
                     'weatherlink-live', 'from_registration'),
                    ('ridge', x'07070707070707070707070707070707', 'Ridge',
                     'campbellscientific', 'from_registration')",
        )
        .execute(&pool)
        .await
        .expect("two stations");
        sqlx::query(
            "INSERT INTO website (id, name, hostname, page_title, about_station_html,
                                  primary_station_key, snow_enabled, snow_station_key,
                                  air_quality_enabled, air_quality_station_key, is_portal,
                                  radar_enabled, radar_token, radar_registered_at)
             VALUES (7, 'portal', 'portal.example.com', 'Portal', '',
                     'ridge', 1, 'ridge', 1, 'backyard', 1, 0, '', NULL),
                    (2, 'suncrest', 'suncrest.example.com', 'Suncrest Live Weather',
                     '<p>On a ridge.</p>', 'backyard', 0, NULL, 0, NULL, 0, 1,
                     'contract-radar-token', 1762000000)",
        )
        .execute(&pool)
        .await
        .expect("two websites");

        let rows = websites(&pool).await.expect("the query runs");
        assert_eq!(
            rows.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![2, 7],
            "ordered by id, not by insertion"
        );
        assert_eq!(
            rows[0],
            WebsiteRow {
                id: 2,
                name: "suncrest".into(),
                primary_station_key: Some("backyard".into()),
                snow_enabled: false,
                // Nullable and `ON DELETE SET NULL`, so `None` has to decode as `None`
                // rather than as an empty string -- which is what `snow_enabled = 0`
                // means and what the table's own CHECK permits only in that pairing.
                snow_station_key: None,
                hostname: "suncrest.example.com".into(),
                page_title: "Suncrest Live Weather".into(),
                about_station_html: "<p>On a ridge.</p>".into(),
                air_quality_enabled: false,
                air_quality_station_key: None,
                is_portal: false,
                radar_enabled: true,
                radar_token: "contract-radar-token".into(),
                apple_app_id: String::new(),
                tls_cert_path: None,
                tls_key_path: None,
                is_default_tls: false,
            }
        );
        assert_eq!(
            rows[1],
            WebsiteRow {
                id: 7,
                name: "portal".into(),
                primary_station_key: Some("ridge".into()),
                snow_enabled: true,
                snow_station_key: Some("ridge".into()),
                hostname: "portal.example.com".into(),
                page_title: "Portal".into(),
                about_station_html: String::new(),
                air_quality_enabled: true,
                air_quality_station_key: Some("backyard".into()),
                is_portal: true,
                radar_enabled: false,
                radar_token: String::new(),
                apple_app_id: String::new(),
                tls_cert_path: None,
                tls_key_path: None,
                is_default_tls: false,
            }
        );

        // A website with no primary device is legal -- the column is nullable, and
        // `ON DELETE SET NULL` can produce it at any time -- so it must not be an error.
        sqlx::query("INSERT INTO website (id, name, hostname) VALUES (9, 'bare', 'bare.test')")
            .execute(&pool)
            .await
            .expect("a bare website");
        let bare = websites(&pool).await.expect("still reads");
        assert_eq!(bare.len(), 3);
        assert_eq!(bare[2].primary_station_key, None);
        assert!(!bare[2].snow_enabled);
    }

    /// **`aerisWeatherEnabled` is a row, and nothing tested the query that reads it.**
    ///
    /// Found by M4-3's code review. The pairing drives the rendered page's whole forecast
    /// band, no fixture asserts that band, and `crates/mw-web/tests/byte_diff.rs` builds its
    /// own `SiteView` and never reads a database -- so a wrong `service` literal or a
    /// renamed column would silently drop the band from every page with no gate noticing.
    ///
    /// Both directions: the `aeris` pairing comes back, and a pairing for a different
    /// service does not.
    #[tokio::test]
    async fn the_aeris_pairing_round_trips_and_excludes_other_services() {
        let scratch = Scratch::new("aeris");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");
        assert!(
            aeris_stations(&pool).await.expect("reads").is_empty(),
            "the migration seeds no pairing"
        );

        for sql in [
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind)
             VALUES ('davis-full', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'davis-full',
                     'davis', 'from_registration')",
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind)
             VALUES ('csi-minimal', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'csi-minimal',
                     'campbellscientific', 'from_registration')",
            // The Aeris pairing carries no `uploader_id` -- an Aeris account is not an
            // uploader, which the table's own CHECK requires.
            "INSERT INTO station_service (station_key, service) VALUES ('davis-full', 'aeris')",
            // A different service on the OTHER station, so that a query missing its
            // `WHERE` would return both and this test would say so.
            "INSERT INTO uploader (id, kind, name, enabled)
             VALUES (1, 'wunderground', 'backyard', 1)",
            "INSERT INTO station_service (station_key, service, uploader_id)
             VALUES ('csi-minimal', 'wunderground', 1)",
        ] {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the schema rejected a row: {e}\n{sql}"));
        }

        assert_eq!(
            aeris_stations(&pool).await.expect("reads"),
            vec!["davis-full".to_string()],
            "only the `aeris` pairing, and by station key"
        );
        pool.close().await;
    }

    /// The `uploader` join, which is the one query in this file that reads three tables.
    ///
    /// Four properties, each of which a wrong literal would break silently: a disabled
    /// uploader is absent, an enabled one with no pairing still arrives (so the registrar
    /// can refuse it by name rather than the row being invisible), one uploader with two
    /// pairings produces two rows (which is what makes the arity refusal reachable), and
    /// the station columns the two `OutputConfig` predicates read come back.
    ///
    /// The join's `ss.service = u.kind` condition is **not** asserted and cannot be from
    /// here: the composite foreign key `(uploader_id, service) -> uploader (id, kind)`
    /// makes a mismatched pairing unrepresentable unless the test switches `foreign_keys`
    /// off. The condition is in the join anyway, because it is only the foreign key that
    /// makes it redundant and a pragma is not a schema.
    #[tokio::test]
    async fn the_uploader_join_reads_the_account_the_pairing_and_the_station() {
        let scratch = Scratch::new("uploaders");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");
        assert!(
            uploaders(&pool).await.expect("reads").is_empty(),
            "the migration seeds no uploader"
        );

        for sql in [
            // `transport_kind` and a `fixed` identity travel together, which the table's
            // own CHECK requires: a station this node collects from has a transport.
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_dev, transport_baud,
                                   latitude, longitude, aprs_callsign)
             VALUES ('backyard', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'backyard',
                     'davis', 'fixed', 'serial', '/dev/ttyUSB0', 19200,
                     45.1, -122.05, 'N0CALL')",
            "INSERT INTO uploader (id, kind, name, enabled, api_key, interval_seconds)
             VALUES (1, 'wunderground', 'wu-backyard', 1, 'hunter2', 300)",
            "INSERT INTO station_service (station_key, service, uploader_id,
                                          remote_station_id)
             VALUES ('backyard', 'wunderground', 1, 'KWABACKYARD')",
            // Enabled, and paired with nothing: it must still be returned.
            "INSERT INTO uploader (id, kind, name, enabled, api_key)
             VALUES (2, 'pwsweather', 'pws-unpaired', 1, 'hunter3')",
            // Disabled: it must not be.
            "INSERT INTO uploader (id, kind, name, enabled, api_key)
             VALUES (3, 'wunderground', 'wu-off', 0, 'hunter4')",
            // An APRS account, with the columns its kind owns.
            "INSERT INTO uploader (id, kind, name, enabled, interval_seconds,
                                   aprs_passcode, aprs_transport, aprs_kiss_tcp_address,
                                   aprs_kiss_path, aprs_comment)
             VALUES (4, 'aprs', 'aprs-backyard', 1, 900, '12345', 'kiss-tcp',
                     'tnc.lan:8001', 'WIDE1-1', 'Suncrest WA')",
            "INSERT INTO station_service (station_key, service, uploader_id)
             VALUES ('backyard', 'aprs', 4)",
            // A SECOND station paired with the **same** Weather Underground account, which
            // `station_service`'s primary key `(station_key, service)` permits. It is what
            // makes this query one row per PAIRING rather than per uploader, and it is the
            // input `mw_output::plan` refuses as W7 D-38's arity -- so it has to be
            // representable here or that refusal is unreachable.
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_dev, transport_baud)
             VALUES ('roof', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'roof',
                     'davis', 'fixed', 'serial', '/dev/ttyUSB1', 19200)",
            "INSERT INTO station_service (station_key, service, uploader_id,
                                          remote_station_id)
             VALUES ('roof', 'wunderground', 1, 'KWAROOF')",
        ] {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the schema rejected a row: {e}\n{sql}"));
        }

        let rows = uploaders(&pool).await.expect("reads");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "aprs-backyard",
                "pws-unpaired",
                "wu-backyard",
                "wu-backyard"
            ],
            "ordered by kind then name then station, and the disabled row is absent"
        );
        // **One row per pairing**, which is the documented shape of this query and the
        // input `mw_output::plan`'s arity refusal reads. Ordered by station key, so the
        // refusal's `first`/`second` are deterministic.
        let paired: Vec<Option<&str>> = rows
            .iter()
            .filter(|r| r.name == "wu-backyard")
            .map(|r| r.station_key.as_deref())
            .collect();
        assert_eq!(paired, vec![Some("backyard"), Some("roof")]);

        let aprs = &rows[0];
        assert_eq!(aprs.kind, "aprs");
        assert_eq!(aprs.station_key.as_deref(), Some("backyard"));
        assert_eq!(aprs.aprs_callsign.as_deref(), Some("N0CALL"));
        assert_eq!(aprs.latitude, Some(45.1));
        assert_eq!(aprs.longitude, Some(-122.05));
        assert_eq!(aprs.interval_seconds, Some(900));
        assert_eq!(aprs.aprs_transport.as_deref(), Some("kiss-tcp"));
        assert_eq!(aprs.aprs_kiss_tcp_address.as_deref(), Some("tnc.lan:8001"));
        assert_eq!(aprs.aprs_comment.as_deref(), Some("Suncrest WA"));
        assert_eq!(aprs.aprs_passcode.as_deref(), Some("12345"));
        // An APRS account carries no `api_key`, which the table's own CHECK requires.
        assert!(aprs.api_key.is_none());

        let pws = &rows[1];
        assert!(
            pws.station_key.is_none(),
            "an enabled uploader with no pairing is returned, so it can be refused by name"
        );
        assert!(pws.remote_station_id.is_none());

        let wu = &rows[2];
        assert_eq!(wu.station_key.as_deref(), Some("backyard"));
        assert_eq!(wu.remote_station_id.as_deref(), Some("KWABACKYARD"));
        // The two columns `mw_output::OutputConfig`'s predicates read, which tell "this
        // station is switched off" from "this node does not collect for it".
        assert_eq!(wu.station_enabled, Some(true));
        assert_eq!(wu.station_transport_kind.as_deref(), Some("serial"));
        // And an unpaired uploader has neither, which is distinguishable from a station
        // that exists and is disabled.
        assert_eq!(pws.station_enabled, None);
        assert_eq!(pws.station_transport_kind, None);
        assert_eq!(
            wu.api_key
                .as_ref()
                .map(secrecy::ExposeSecret::expose_secret),
            Some("hunter2")
        );
        // The station columns are the APRS ones and are read for every kind, because the
        // join does not know which kind asked.
        assert_eq!(wu.latitude, Some(45.1));

        pool.close().await;
    }

    /// The three columns M3-1 added, and the round trip the station registrar depends on.
    #[tokio::test]
    async fn a_weatherlink_live_row_round_trips_through_the_station_reader() {
        let scratch = Scratch::new("stations");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");

        // A broadcasting unit this node also arms, and a polled one. Both are the shapes
        // `mw_station::plan` turns into components.
        for sql in [
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   presented_key, transport_kind, transport_port,
                                   transport_host, wll_sensor_mapping)
             VALUES ('suncrest:roof', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'roof',
                     'weatherlink-live', 'from_payload', '001D0A700002', 'udp_in', 22222,
                     '192.168.1.50', 'th:1, wind:1, baro')",
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_url,
                                   wll_sensor_mapping, wll_poll_interval_seconds)
             VALUES ('suncrest:field', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'field',
                     'weatherlink-live', 'fixed', 'http_out', 'http://192.168.1.51',
                     'th:1, wind:1', 45)",
        ] {
            sqlx::query(sql).execute(&pool).await.expect("a valid row");
        }

        let rows = stations(&pool).await.expect("the station rows read");
        assert_eq!(rows.len(), 2);
        // Ordered by key, which is what keeps a listener's ConfigDigest stable.
        assert_eq!(rows[0].station_key, "suncrest:field");
        assert_eq!(rows[1].station_key, "suncrest:roof");

        let field = &rows[0];
        assert_eq!(field.identity_kind, "fixed");
        assert_eq!(field.presented_key, None);
        assert_eq!(field.transport_kind.as_deref(), Some("http_out"));
        assert_eq!(field.wll_poll_interval_seconds, Some(45));
        assert!(field.enabled, "the column defaults to collecting");

        let roof = &rows[1];
        assert_eq!(roof.presented_key.as_deref(), Some("001D0A700002"));
        assert_eq!(roof.transport_port, Some(22222));
        assert_eq!(roof.transport_host.as_deref(), Some("192.168.1.50"));
        assert_eq!(
            roof.wll_sensor_mapping.as_deref(),
            Some("th:1, wind:1, baro")
        );
        assert_eq!(roof.wll_poll_interval_seconds, None);
        assert!(roof.transport_auth.is_none());
        pool.close().await;
    }

    /// The three CHECKs M3-1's migration adds, as the illegal states they name.
    #[tokio::test]
    async fn the_identity_claim_is_required_exactly_where_there_is_one() {
        let scratch = Scratch::new("claims");
        let pool = open_and_migrate(&scratch.db()).await.expect("migrate");
        let node = "x'0193f3c28b207c71ae3f10cc92d4b7aa'";

        for (what, sql) in [
            (
                "a `fixed` station presenting one anyway",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_url,
                                           presented_key)
                     VALUES ('b', {node}, 'b', 'weatherlink-live', 'fixed', 'http_out',
                             'http://h', 'x')"
                ),
            ),
            (
                "a poll interval below W2a 4a.5's floor",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_url,
                                           wll_poll_interval_seconds)
                     VALUES ('c', {node}, 'c', 'weatherlink-live', 'fixed', 'http_out',
                             'http://h', 5)"
                ),
            ),
            (
                "an empty sensor mapping, which is not a mapping",
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_url,
                                           wll_sensor_mapping)
                     VALUES ('d', {node}, 'd', 'weatherlink-live', 'fixed', 'http_out',
                             'http://h', '')"
                ),
            ),
        ] {
            let e = sqlx::query(&sql)
                .execute(&pool)
                .await
                .expect_err(&format!("the schema accepted {what}"));
            assert!(
                e.to_string().contains("CHECK constraint failed"),
                "{what}: {e}"
            );
        }

        // A `from_payload` station with no claim is NOT rejected here, and that is the
        // decision the migration's own comment carries: `ADD COLUMN` validates a new CHECK
        // against existing rows, so an arm requiring the column to be present cannot be
        // added to a table that already holds an Ambient station. `mw_station::plan`
        // enforces it instead, with a message that names the station and the column.
        sqlx::query(&format!(
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_port)
             VALUES ('a', {node}, 'a', 'weatherlink-live', 'from_payload', 'udp_in', 22222)"
        ))
        .execute(&pool)
        .await
        .expect("the schema leaves the required half to plan time");

        // Two stations cannot present one claim: resolution would not be a function. The
        // index is over `lower(presented_key)`, because that is the comparison
        // `mw_store::registry` makes -- so the case-variant pair below has to be refused
        // too. A byte-exact index would admit it and the registry would then collapse the
        // two, storing one station's readings under the other's key.
        sqlx::query(&format!(
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_port, presented_key)
             VALUES ('e', {node}, 'e', 'weatherlink-live', 'from_payload', 'udp_in', 22222,
                     '001D0A700002')"
        ))
        .execute(&pool)
        .await
        .expect("the first station with that serial");
        for (what, key, port) in [
            ("the same serial, byte for byte", "001D0A700002", 22222),
            // Two broadcast stations on DIFFERENT ports are two listener groups of one unit
            // each, so `StationPlanError::DuplicateClaim` -- which is checked per group --
            // does not see this pair. The index is the only thing that can.
            ("the same serial in the other case", "001d0a700002", 22223),
        ] {
            let e = sqlx::query(&format!(
                "INSERT INTO stations (station_key, owning_node_id, name, type,
                                       identity_kind, transport_kind, transport_port,
                                       presented_key)
                 VALUES ('f{port}', {node}, 'f', 'weatherlink-live', 'from_payload',
                         'udp_in', {port}, '{key}')"
            ))
            .execute(&pool)
            .await
            .expect_err(&format!("the schema accepted {what}"));
            assert!(
                e.to_string().contains("UNIQUE constraint failed"),
                "{what}: {e}"
            );
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

/// One `config.stations` row, as the columns hold it.
///
/// Deliberately close to the SQL and deliberately **not** `mw_station::StationRow`. Both
/// crates are in the `service` layer, so `ci/layers.toml` forbids either naming the other;
/// the binary copies column into field, and what that buys is that this crate cannot grow a
/// `Transport` and that crate cannot grow a query (`mw_station::config`'s module
/// documentation has the other half of this note).
///
/// No `PartialEq`: `transport_auth` is a `SecretString`, which deliberately implements
/// neither comparison nor `Hash` (**D-33**) so that a credential cannot be compared by
/// accident in non-constant time. What needs comparing is the *plan* this row produces,
/// and `mw_component::ConfigDigest` is what compares that.
#[derive(Debug, Clone)]
pub struct StationRow {
    /// `id` -- the surrogate integer the **frozen contract** returns.
    ///
    /// `/stationinfo` emits `stations[].id`, `/api/stations` emits `id`, and both accept
    /// `?station_id=`; W3 §7's config payload emits `stationID` and `airQualityDeviceID`.
    /// `station_key` is the domain identity and is unique, but it is text, so it cannot be
    /// the source of those fields -- which is why the Go `devices.id` surrogate is carried
    /// forward (the column's own comment in `migrations/config/`).
    pub id: i64,
    /// `station_key` -- the domain identity.
    pub station_key: String,
    /// `owning_node_id` -- the node that **measures** this station, 16 bytes.
    ///
    /// Equal to `node.node_id` for a local station and a peer's `node_id` for one that
    /// arrived by registration. Read because W7 §4.4's step 5 is checked against it: an
    /// envelope may only name a station some node on its inbound path owns, which is what
    /// makes **D-36**'s "a compromised leaf cannot inject history for another station"
    /// true of station keys and not only of origins (`mw_component::StationRegistry::
    /// station_owner`).
    pub owning_node_id: Vec<u8>,
    /// `name`.
    pub name: String,
    /// `type` -- the configured device type, in the frozen contract's spelling.
    pub kind: String,
    /// `enabled`.
    pub enabled: bool,
    /// `identity_kind`.
    pub identity_kind: String,
    /// `latitude`, degrees north.
    pub latitude: Option<f64>,
    /// `longitude`, degrees east.
    pub longitude: Option<f64>,
    /// `altitude_m`, **metres**. The weather schema's projection column is `altitude_ft`;
    /// both units are in both column names so the conversion cannot be done twice.
    pub altitude_m: Option<f64>,
    /// `timezone`, an IANA name. `None` is a station whose rollovers happen in UTC.
    pub timezone: Option<String>,
    /// `snow_base_distance_mm`: the distance the gauge reads with no snow under it.
    pub snow_base_distance_mm: Option<f64>,
    /// `wind_dir_correction_degrees`: the vane's offset from north.
    ///
    /// Three drivers apply it -- Davis, Campbell and Ambient, which is the set that applies
    /// it in the Go tree too -- and `mw_codec::WindDirCorrection` is the arithmetic. `0` is
    /// the column's own default and means a vane that is aligned.
    pub wind_dir_correction_degrees: i64,
    /// `presented_key` -- the identifier a non-`fixed` station presents.
    pub presented_key: Option<String>,
    /// `ingest_secret_hash` -- the protocol secret's hash, where the protocol has one.
    pub ingest_secret_hash: Option<String>,
    /// `transport_kind`. `None` for a station received from a peer.
    pub transport_kind: Option<String>,
    /// `transport_dev`.
    pub transport_dev: Option<String>,
    /// `transport_baud`.
    pub transport_baud: Option<i64>,
    /// `transport_host`.
    pub transport_host: Option<String>,
    /// `transport_port`.
    pub transport_port: Option<i64>,
    /// `transport_url`.
    pub transport_url: Option<String>,
    /// `transport_method`.
    pub transport_method: Option<String>,
    /// `transport_auth` -- a credential, so a secret from the moment it is read (**D-33**).
    pub transport_auth: Option<SecretString>,
    /// `transport_streaming`.
    pub transport_streaming: Option<bool>,
    /// `transport_mount`.
    pub transport_mount: Option<String>,
    /// `transport_group`.
    pub transport_group: Option<String>,
    /// `transport_topic`.
    pub transport_topic: Option<String>,
    /// `transport_tls_ca_path`.
    pub transport_tls_ca_path: Option<String>,
    /// `transport_tls_cert_path`.
    pub transport_tls_cert_path: Option<String>,
    /// `transport_tls_key_path`.
    pub transport_tls_key_path: Option<String>,
    /// `wll_sensor_mapping`.
    pub wll_sensor_mapping: Option<String>,
    /// `wll_poll_interval_seconds`.
    pub wll_poll_interval_seconds: Option<i64>,
}

/// Read every `config.stations` row, ordered by key.
///
/// Every row, including the disabled ones and the ones a peer owns: which of them produce
/// components is `mw_station`'s to decide, and a `WHERE enabled = 1` here would put that
/// decision in two places.
///
/// Ordered, because the order reaches the plan: a listener's `ConfigDigest` is over the
/// group of rows it serves, and an unordered `SELECT` would make the digest -- and
/// therefore the plan diff -- depend on SQLite's row order.
///
/// **By `name`, because the frozen `stations` array is.** `pkg/config/provider_sqlite.go`
/// orders the station directory `ORDER BY name`, and `/stationinfo` and `/api/stations`
/// serialise it in that order; `name` and `station_key` are independent `TEXT` columns with
/// nothing tying them together, so a station renamed in the management UI without its key
/// changing reordered a frozen array. `station_key` stays as the tiebreak, because `name`
/// has no uniqueness constraint and the digest has to be stable.
pub async fn stations(pool: &SqlitePool) -> Result<Vec<StationRow>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT id, station_key, owning_node_id, name, type, enabled, identity_kind, \
                presented_key, \
                ingest_secret_hash, latitude, longitude, altitude_m, timezone, \
                snow_base_distance_mm, wind_dir_correction_degrees, \
                transport_kind, transport_dev, transport_baud, \
                transport_host, transport_port, transport_url, transport_method, \
                transport_auth, transport_streaming, transport_mount, transport_group, \
                transport_topic, transport_tls_ca_path, transport_tls_cert_path, \
                transport_tls_key_path, wll_sensor_mapping, wll_poll_interval_seconds \
         FROM stations ORDER BY name, station_key",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the station rows",
        source,
    })?;
    rows.into_iter()
        .map(|r| {
            Ok(StationRow {
                id: r.get("id"),
                station_key: r.get("station_key"),
                owning_node_id: r.get("owning_node_id"),
                name: r.get("name"),
                kind: r.get("type"),
                enabled: flag(&r, "enabled")?,
                identity_kind: r.get("identity_kind"),
                presented_key: r.get("presented_key"),
                ingest_secret_hash: r.get("ingest_secret_hash"),
                latitude: r.get("latitude"),
                longitude: r.get("longitude"),
                altitude_m: r.get("altitude_m"),
                timezone: r.get("timezone"),
                snow_base_distance_mm: r.get("snow_base_distance_mm"),
                wind_dir_correction_degrees: r.get("wind_dir_correction_degrees"),
                transport_kind: r.get("transport_kind"),
                transport_dev: r.get("transport_dev"),
                transport_baud: r.get("transport_baud"),
                transport_host: r.get("transport_host"),
                transport_port: r.get("transport_port"),
                transport_url: r.get("transport_url"),
                transport_method: r.get("transport_method"),
                transport_auth: r
                    .get::<Option<String>, _>("transport_auth")
                    .map(SecretString::from),
                transport_streaming: optional_flag(&r, "transport_streaming")?,
                transport_mount: r.get("transport_mount"),
                transport_group: r.get("transport_group"),
                transport_topic: r.get("transport_topic"),
                transport_tls_ca_path: r.get("transport_tls_ca_path"),
                transport_tls_cert_path: r.get("transport_tls_cert_path"),
                transport_tls_key_path: r.get("transport_tls_key_path"),
                wll_sensor_mapping: r.get("wll_sensor_mapping"),
                wll_poll_interval_seconds: r.get("wll_poll_interval_seconds"),
            })
        })
        .collect()
}

/// One `config.website` row, as the columns hold it.
///
/// Two consumers, and between them they want most of the table. The snapshot key needs the
/// four **D-31** names plus the identity — the primary-device selection, the snow device
/// and the website's own name are per website, which is why the key is `(website, station)`.
/// The frozen REST surface adds the rest: `hostname` is what a request resolves a website
/// by, `about_station_html` is `/stationinfo`'s `about_text`, `is_portal` is what makes
/// `/api/stations` a `403` or a `200`, and the air-quality binding is where `/almanac`
/// reads its particulate extremes from.
///
/// The TLS paths, `is_default_tls` and `apple_app_id` arrived with **M4-3**, which is the
/// item that built the SNI resolver and the rendered page -- the two consumers the earlier
/// note said they were waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebsiteRow {
    /// `id` -- the surrogate key the snapshot table is keyed by.
    pub id: i64,
    /// `name`.
    pub name: String,
    /// `primary_station_key`. Nullable, and `ON DELETE SET NULL`.
    pub primary_station_key: Option<String>,
    /// `snow_enabled`.
    pub snow_enabled: bool,
    /// `snow_station_key`. The table's own `CHECK` makes this non-NULL whenever
    /// `snow_enabled` is set.
    pub snow_station_key: Option<String>,
    /// `hostname`, lower-case and port-free. `""` is the **default** website, which an
    /// unmatched `Host` resolves to -- the schema's three `CHECK`s are what make a stored
    /// value that can never match unrepresentable.
    pub hostname: String,
    /// `page_title`. Reaches the wire through `/api/stations`' nested `website` block.
    pub page_title: String,
    /// `about_station_html`. `/stationinfo`'s `about_text`, HTML and not plain text.
    pub about_station_html: String,
    /// `air_quality_enabled`.
    pub air_quality_enabled: bool,
    /// `air_quality_station_key`. Non-NULL whenever `air_quality_enabled` is set.
    pub air_quality_station_key: Option<String>,
    /// `is_portal`. The only website `/api/stations` answers for.
    pub is_portal: bool,
    /// `radar_enabled`.
    pub radar_enabled: bool,
    /// `radar_token`. **Public** in the rendered markup and on `/stationinfo`, which is
    /// what makes it a plain `String` here rather than a `SecretString`: the schema's
    /// "write-only" note is about the management API, not about secrecy.
    pub radar_token: String,
    /// `apple_app_id`. Reaches the rendered page's `<meta name="apple-itunes-app">` tag and
    /// nothing else; an empty value removes the tag. Added by M4-3, which is the item that
    /// renders a page.
    pub apple_app_id: String,
    /// `tls_cert_path`. `None` for a website with no certificate of its own.
    pub tls_cert_path: Option<String>,
    /// `tls_key_path`. The schema's `CHECK` makes this `Some` exactly when
    /// [`WebsiteRow::tls_cert_path`] is.
    pub tls_key_path: Option<String>,
    /// `is_default_tls` -- W3 §6.4's SNI fallback, tried before the default website's own
    /// certificate. At most one row may set it, and only a row with a certificate may.
    pub is_default_tls: bool,
}

/// Read every `config.website` row, ordered by id.
///
/// Ordered for [`stations`]'s reason: the order reaches the plan through the snapshot
/// components' `ConfigDigest`, and an unordered `SELECT` would make the plan diff depend on
/// SQLite's row order.
///
/// **Every row, including a portal.** Which of them produce components is the consumer's to
/// decide; a `WHERE` here would put that decision in two places.
pub async fn websites(pool: &SqlitePool) -> Result<Vec<WebsiteRow>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT id, name, primary_station_key, snow_enabled, snow_station_key, hostname, \
                page_title, about_station_html, air_quality_enabled, \
                air_quality_station_key, is_portal, radar_enabled, radar_token, \
                apple_app_id, tls_cert_path, tls_key_path, is_default_tls \
         FROM website ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the website rows",
        source,
    })?;
    rows.into_iter()
        .map(|r| {
            Ok(WebsiteRow {
                id: r.get("id"),
                name: r.get("name"),
                primary_station_key: r.get("primary_station_key"),
                snow_enabled: flag(&r, "snow_enabled")?,
                snow_station_key: r.get("snow_station_key"),
                hostname: r.get("hostname"),
                page_title: r.get("page_title"),
                about_station_html: r.get("about_station_html"),
                air_quality_enabled: flag(&r, "air_quality_enabled")?,
                air_quality_station_key: r.get("air_quality_station_key"),
                is_portal: flag(&r, "is_portal")?,
                radar_enabled: flag(&r, "radar_enabled")?,
                radar_token: r.get("radar_token"),
                apple_app_id: r.get("apple_app_id"),
                tls_cert_path: r.get("tls_cert_path"),
                tls_key_path: r.get("tls_key_path"),
                is_default_tls: flag(&r, "is_default_tls")?,
            })
        })
        .collect()
}

/// Every station paired with the Aeris service, in key order.
///
/// **This is W3 §7's `aerisWeatherEnabled`, and it is a row rather than a column.** The Go
/// `devices` table carries `aeris_enabled` per device; the new schema makes "which stations
/// get forecasts and alerts" a `station_service` pairing like any other, which is what the
/// `website` table's own comment means by "`aerisWeatherEnabled` is a `station_service` row
/// with `service = 'aeris'`". So the rendered page's forecast band is gated on membership of
/// this set, and `mw-web` is handed the answer rather than a join.
///
/// Ordered for [`stations`]'s reason: the order reaches the plan through the website
/// components' `ConfigDigest`.
pub async fn aeris_stations(pool: &SqlitePool) -> Result<Vec<String>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT station_key FROM station_service WHERE service = 'aeris' ORDER BY station_key",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the Aeris station pairings",
        source,
    })?;
    Ok(rows.into_iter().map(|r| r.get("station_key")).collect())
}

/// A `0`/`1` column as a `bool`, refusing anything else.
///
/// The column's own `CHECK` permits exactly those two, so a third value means this build
/// and its schema disagree -- which is what `ConfigDbError::Column` says.
fn flag(row: &sqlx::sqlite::SqliteRow, column: &'static str) -> Result<bool, ConfigDbError> {
    match row.get::<i64, _>(column) {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(ConfigDbError::Column {
            column,
            value: other.to_string(),
        }),
    }
}

fn optional_flag(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
) -> Result<Option<bool>, ConfigDbError> {
    match row.get::<Option<i64>, _>(column) {
        None => Ok(None),
        Some(0) => Ok(Some(false)),
        Some(1) => Ok(Some(true)),
        Some(other) => Err(ConfigDbError::Column {
            column,
            value: other.to_string(),
        }),
    }
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

/// One `config.ingest` row.
///
/// W7 §8.3's `[[ingest]]`, and "the only thing that makes this node a collector -- no flag,
/// no mode" (**D-44**) in the table's own words. A node with no row is a satellite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestRow {
    /// `listen` -- `grpc` or `rest`.
    pub listen: String,
    /// `peer_auth` -- `mtls-ca`, `mtls-pinned` or `psk`.
    pub peer_auth: String,
    /// `ca_path`, which even an `mtls-*` row may omit: docs/collector.md §8.3 sets the CA on
    /// the gRPC listener and omits it on the REST one with the same `peer_auth`, so it is a
    /// node-level trust store a listener may name rather than a per-listener setting.
    pub ca_path: Option<String>,
    /// `allow_insecure` -- §5.4's explicit opt-out, which M6-4 is what refuses the absence
    /// of.
    pub allow_insecure: bool,
}

/// Every `config.ingest` row, in `listen` order.
///
/// Ordered for the digest's sake rather than for a reader's: `listen` is the primary key, so
/// the order is total and stable, and a `ConfigDigest` over the rows is then a function of
/// the table's contents rather than of the order SQLite happened to return them in.
pub async fn ingest(pool: &SqlitePool) -> Result<Vec<IngestRow>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT listen, peer_auth, ca_path, allow_insecure FROM ingest ORDER BY listen",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the ingest rows",
        source,
    })?;
    rows.into_iter()
        .map(|r| {
            Ok(IngestRow {
                listen: r.get("listen"),
                peer_auth: r.get("peer_auth"),
                ca_path: r.get("ca_path"),
                allow_insecure: flag(&r, "allow_insecure")?,
            })
        })
        .collect()
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

// ---------------------------------------------------------------------------------------
// Third-party egress: the `uploader` rows, joined to their `station_service` pairings
//
// **D-66.** The Go schema configures the same service in two places -- the `devices` row
// carries that station's PWS Weather, Weather Underground and APRS settings while the
// global credentials live on `controller_configs` -- and this schema splits it by what the
// thing is a property OF: `uploader` is the remote ACCOUNT and the component, and
// `station_service` is the PAIRING. So reading an uploader is a join, and this is the one
// place that join is written.
//
// One row per **pairing**, not per uploader. An uploader with two pairings produces two
// rows with the same `name`, which `mw_output::plan` refuses as W7 D-38's arity rather than
// silently planning for one of them -- the behaviour the `station_service` table's own
// comment predicts as `PlanError::UploaderArity`. A `LEFT JOIN`, so an enabled uploader with
// no pairing still arrives: an uploader configured to share nothing is a configuration
// error that names itself, where an `INNER JOIN` would make it invisible.

/// One `uploader` row joined to one `station_service` pairing and that station's columns.
///
/// The field names are the columns'. `mw-output` owns what an uploader *is* and may not name
/// this crate; this crate owns the query and may not name that one; the binary is where the
/// two meet (§1.3 rule 1), which is why `crate::config` copies this into
/// `mw_output::UploaderRow` field by field.
///
/// No `PartialEq`: `api_key` is a `SecretString`, which deliberately does not implement it
/// (the same reason [`StationRow`] has none).
#[derive(Debug, Clone)]
pub struct UploaderRow {
    /// `uploader.kind` -- `wunderground`, `pwsweather` or `aprs`.
    pub kind: String,
    /// `uploader.name`, the component id's stable suffix.
    pub name: String,
    /// `uploader.interval_seconds`. `None` is the kind's default.
    pub interval_seconds: Option<i64>,
    /// `uploader.api_endpoint`. `None` is the kind's default.
    pub api_endpoint: Option<String>,
    /// `uploader.api_key` -- a credential, so a secret from the moment it is read
    /// (**D-33**). No endpoint returns it (**D-67**).
    pub api_key: Option<SecretString>,

    /// `station_service.station_key` -- the paired station. `None` is no pairing.
    pub station_key: Option<String>,
    /// `station_service.remote_station_id` -- this station's identifier at the far end.
    pub remote_station_id: Option<String>,
    /// `stations.latitude`.
    pub latitude: Option<f64>,
    /// `stations.longitude`.
    pub longitude: Option<f64>,
    /// `stations.aprs_callsign` -- public identity, printed on every packet, and therefore
    /// on the station row rather than beside the passcode.
    pub aprs_callsign: Option<String>,
    /// `stations.enabled`, for the paired station. `None` is no pairing at all.
    ///
    /// Selected because the registrar has to tell two different causes apart: a station
    /// whose collection is switched off (reversible, so skip and warn) from one this node
    /// does not collect for at all (a pairing that cannot work, so refuse).
    /// `mw_output::OutputConfig`'s two predicates are what read it.
    pub station_enabled: Option<bool>,
    /// `stations.transport_kind`, for the paired station.
    ///
    /// `None` on a station row means a station a **peer** forwards readings for -- inventory
    /// on this node rather than a component -- which `window:rollup` maintains no windows
    /// for. Distinguishable from "no pairing" by [`UploaderRow::station_key`].
    pub station_transport_kind: Option<String>,

    /// `uploader.aprs_passcode`.
    ///
    /// A plain `String` rather than a `SecretString`, and `mw_aprs::Passcode` is what wraps
    /// it: the parse is what decides whether the configured value or the CWOP default is
    /// used, and that type's `Debug` redacts. Keeping it a `String` here means the
    /// redaction happens once, where the value is interpreted, rather than being unwrapped
    /// again at the parse.
    pub aprs_passcode: Option<String>,
    /// `uploader.aprs_symbol_table`.
    pub aprs_symbol_table: Option<String>,
    /// `uploader.aprs_symbol_code`.
    pub aprs_symbol_code: Option<String>,
    /// `uploader.aprs_comment`.
    pub aprs_comment: Option<String>,
    /// `uploader.aprs_server`.
    pub aprs_server: Option<String>,
    /// `uploader.aprs_transport`.
    pub aprs_transport: Option<String>,
    /// `uploader.aprs_kiss_serial_device`.
    pub aprs_kiss_serial_device: Option<String>,
    /// `uploader.aprs_kiss_serial_baud`.
    pub aprs_kiss_serial_baud: Option<i64>,
    /// `uploader.aprs_kiss_tcp_address`.
    pub aprs_kiss_tcp_address: Option<String>,
    /// `uploader.aprs_kiss_path`.
    pub aprs_kiss_path: Option<String>,
    /// `uploader.aprs_kiss_destination`.
    pub aprs_kiss_destination: Option<String>,
}

/// Every **enabled** uploader, with its pairing and that station's columns.
///
/// `enabled = 0` rows are not returned at all, which is what makes W5 §1.4's "Stop sharing
/// with Weather Underground" a plan that no longer contains the component (W2a §2.5) rather
/// than `RemoveController` deleting a map entry and logging "will stop on next app restart".
pub async fn uploaders(pool: &SqlitePool) -> Result<Vec<UploaderRow>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT u.kind, u.name, u.interval_seconds, u.api_endpoint, u.api_key, \
                u.aprs_passcode, u.aprs_symbol_table, u.aprs_symbol_code, u.aprs_comment, \
                u.aprs_server, u.aprs_transport, u.aprs_kiss_serial_device, \
                u.aprs_kiss_serial_baud, u.aprs_kiss_tcp_address, u.aprs_kiss_path, \
                u.aprs_kiss_destination, \
                ss.station_key AS station_key, \
                ss.remote_station_id AS remote_station_id, \
                s.latitude AS latitude, s.longitude AS longitude, \
                s.aprs_callsign AS aprs_callsign, \
                s.enabled AS station_enabled, \
                s.transport_kind AS station_transport_kind \
         FROM uploader u \
         LEFT JOIN station_service ss \
                ON ss.uploader_id = u.id AND ss.service = u.kind \
         LEFT JOIN stations s ON s.station_key = ss.station_key \
         WHERE u.enabled = 1 \
         ORDER BY u.kind, u.name, ss.station_key",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the uploader rows",
        source,
    })?;
    rows.into_iter()
        .map(|r| {
            Ok(UploaderRow {
                kind: r.get("kind"),
                name: r.get("name"),
                interval_seconds: r.get("interval_seconds"),
                api_endpoint: r.get("api_endpoint"),
                api_key: r
                    .get::<Option<String>, _>("api_key")
                    .map(SecretString::from),
                station_key: r.get("station_key"),
                remote_station_id: r.get("remote_station_id"),
                latitude: r.get("latitude"),
                longitude: r.get("longitude"),
                aprs_callsign: r.get("aprs_callsign"),
                // `Option<i64>` into `Option<bool>` by hand: `flag` refuses a NULL, and a NULL
                // here is the `LEFT JOIN` finding no station rather than a bad column value.
                station_enabled: match r.get::<Option<i64>, _>("station_enabled") {
                    None => None,
                    Some(0) => Some(false),
                    Some(1) => Some(true),
                    Some(other) => {
                        return Err(ConfigDbError::Column {
                            column: "station_enabled",
                            value: other.to_string(),
                        })
                    }
                },
                station_transport_kind: r.get("station_transport_kind"),
                aprs_passcode: r.get("aprs_passcode"),
                aprs_symbol_table: r.get("aprs_symbol_table"),
                aprs_symbol_code: r.get("aprs_symbol_code"),
                aprs_comment: r.get("aprs_comment"),
                aprs_server: r.get("aprs_server"),
                aprs_transport: r.get("aprs_transport"),
                aprs_kiss_serial_device: r.get("aprs_kiss_serial_device"),
                aprs_kiss_serial_baud: r.get("aprs_kiss_serial_baud"),
                aprs_kiss_tcp_address: r.get("aprs_kiss_tcp_address"),
                aprs_kiss_path: r.get("aprs_kiss_path"),
                aprs_kiss_destination: r.get("aprs_kiss_destination"),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The peer registry, the uplinks, and the node row this node allocates for itself
//
// W7 §5.1 and §8.2. These are implementation-plan.md M6-4's reads and the one write in this
// module: `allocate_node` is "allocated once, by the node itself, at first start"
// (W7 **D-5**), which the `node` loader above has always deferred to this item by name.

/// One `peers` row.
///
/// W7 §5.1. `auth_material` is an SPKI fingerprint or a PSK hash by `auth_kind`, and `NULL`
/// under a shared CA -- which is the common case rather than an exception, because the
/// certificate chain does the authenticating and the row only says who is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRow {
    /// `node_id`, 16 bytes.
    pub node_id: Vec<u8>,
    /// `name` -- operator-facing, unique, and the `peer` metric label.
    pub name: String,
    /// `auth_kind` -- `mtls-ca`, `mtls-pinned` or `psk`.
    pub auth_kind: String,
    /// `auth_material` -- an SPKI digest or an Argon2id PHC string.
    pub auth_material: Option<String>,
    /// `relay`. **False by default** (W7 **D-36**).
    pub relay: bool,
    /// `software` -- `NodeIdentity.software` as last seen, which is what W7 OQ-W7-6 gates
    /// removal of the Go compat shim on.
    pub software: Option<String>,
    /// `last_seen`, unix seconds.
    pub last_seen: Option<i64>,
    /// `revoked_at`, unix seconds. Revocation is a timestamp rather than a deletion.
    pub revoked_at: Option<i64>,
    /// `peer_origins.origin_node_id` for this peer, each 16 bytes.
    pub origins: Vec<Vec<u8>>,
}

/// Every `peers` row with its `peer_origins`, in name order.
///
/// Two queries rather than a join: a join over a one-to-many produces one row per origin
/// and the peer's own columns repeated, which then has to be folded back -- and the fold is
/// the part that gets a peer with no origins wrong. Ordered by name so that a digest over
/// the result is a function of the table's contents.
pub async fn peers(pool: &SqlitePool) -> Result<Vec<PeerRow>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT node_id, name, auth_kind, auth_material, relay, software, last_seen, \
                revoked_at \
         FROM peers ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the peer rows",
        source,
    })?;
    let origins = sqlx::query(
        "SELECT node_id, origin_node_id FROM peer_origins ORDER BY node_id, origin_node_id",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the peer origin grants",
        source,
    })?;
    let mut by_node: std::collections::BTreeMap<Vec<u8>, Vec<Vec<u8>>> =
        std::collections::BTreeMap::new();
    for row in origins {
        by_node
            .entry(row.get("node_id"))
            .or_default()
            .push(row.get("origin_node_id"));
    }
    rows.into_iter()
        .map(|r| {
            let node_id: Vec<u8> = r.get("node_id");
            Ok(PeerRow {
                origins: by_node.get(&node_id).cloned().unwrap_or_else(Vec::new),
                node_id,
                name: r.get("name"),
                auth_kind: r.get("auth_kind"),
                auth_material: r.get("auth_material"),
                relay: flag(&r, "relay")?,
                software: r.get("software"),
                last_seen: r.get("last_seen"),
                revoked_at: r.get("revoked_at"),
            })
        })
        .collect()
}

/// One `uplink` row -- W7 §8.2's `[[uplink]]`, the other half of **D-44**.
///
/// A node with no row sends to nobody. `psk` is a [`SecretString`] from the moment it is
/// read (**D-33**), like `weather_database.url`.
#[derive(Debug)]
pub struct UplinkRow {
    /// `peer_name` -- which peer this uplink sends to, and the `peer` metric label.
    pub peer_name: String,
    /// `endpoint` -- e.g. `https://suncrest.example.net:50051`.
    pub endpoint: String,
    /// `auth_kind` -- `mtls` or `psk`.
    pub auth_kind: String,
    /// The client certificate, for `mtls`.
    pub tls_cert_path: Option<String>,
    /// Its key.
    pub tls_key_path: Option<String>,
    /// The CA the collector's certificate is verified against.
    pub tls_ca_path: Option<String>,
    /// The pre-shared secret, for `psk`.
    pub psk: Option<SecretString>,
}

/// Every `uplink` row, in peer-name order.
pub async fn uplinks(pool: &SqlitePool) -> Result<Vec<UplinkRow>, ConfigDbError> {
    let rows = sqlx::query(
        "SELECT peer_name, endpoint, auth_kind, tls_cert_path, tls_key_path, tls_ca_path, \
                psk \
         FROM uplink ORDER BY peer_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "the uplink rows",
        source,
    })?;
    Ok(rows
        .into_iter()
        .map(|r| UplinkRow {
            peer_name: r.get("peer_name"),
            endpoint: r.get("endpoint"),
            auth_kind: r.get("auth_kind"),
            tls_cert_path: r.get("tls_cert_path"),
            tls_key_path: r.get("tls_key_path"),
            tls_ca_path: r.get("tls_ca_path"),
            psk: r.get::<Option<String>, _>("psk").map(SecretString::from),
        })
        .collect())
}

/// Allocate this node's identity, once, at first start (W7 **D-5**).
///
/// Returns the existing row unchanged when there is one, so this is safe to call on every
/// start and the "allocated once" property is the `CHECK (id = 1)` primary key rather than
/// a decision made here. `INSERT ... ON CONFLICT DO NOTHING` followed by a read, because
/// two processes racing on one configuration database must agree about the identity and the
/// loser must adopt the winner's rather than overwrite it -- a `node_id` that changed under
/// a spool whose `MANIFEST` already holds the old one is a spool this build refuses to
/// open.
///
/// `spool_path` is the caller's: W7 **D-10** puts the spool at a **sibling of `config.db`**,
/// and the binary is what knows where `config.db` is. The column has no `DEFAULT` for the
/// same reason.
pub async fn allocate_node(
    pool: &SqlitePool,
    node_id: &[u8; 16],
    node_name: &str,
    spool_path: &str,
) -> Result<Node, ConfigDbError> {
    sqlx::query(
        "INSERT INTO node (id, node_id, node_name, spool_path) VALUES (1, ?, ?, ?) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(node_id.as_slice())
    .bind(node_name)
    .bind(spool_path)
    .execute(pool)
    .await
    .map_err(|source| ConfigDbError::Query {
        what: "allocating this node's identity",
        source,
    })?;
    node(pool).await?.ok_or(ConfigDbError::Query {
        what: "re-reading the node row this start allocated",
        source: sqlx::Error::RowNotFound,
    })
}
