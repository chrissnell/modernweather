//! `migrate-config`: the configuration migration (**M7-4**), as a command.
//!
//! `ci/db/migrate-from-go/` moves the *weather* database; this moves the *configuration*
//! one. The translation is `mw_store::config_go` -- it is a set of statements against two
//! SQLite databases, which is the storage layer's job -- and what is here is the four things
//! the translation cannot decide for itself:
//!
//!   1. **Where the spool goes.** `node.spool_path` has no default in the schema and no
//!      value in any design document, and this is the one place that knows where the state
//!      directory is: it is the directory `--config` is in, which is
//!      `/var/lib/modernweather` under W8's unit and a scratch directory under a test.
//!   2. **What the node is called.** The host's name, which is what an operator would type.
//!   3. **Whether the `weatherlink-live` sensor mappings parse.** `ci/layers.toml` puts
//!      `mw-codec` below `mw-store` and forbids `mw-store` from naming `mw-station`, so the
//!      one crate that may name both the translation and the parser is this one (§1.3
//!      rule 1). A mapping that does not parse is a station that does not plan and therefore
//!      a daemon that does not start (docs/reconciliation.md **DEV-9**), so the migration
//!      switches that station off and names the column rather than handing the daemon a
//!      configuration it will refuse.
//!   4. **Whether to commit.** `--dry-run` rolls the transaction back, and
//!      that is the only difference between the two paths -- the same relationship
//!      `POST /api/v1/config/preview` has to a save (W5 §2.4).
//!
//! # Why this is a subcommand of the daemon and not a script
//!
//! The destination's schema arrives through `sqlx::migrate` and its ledger, so a tool that
//! composed the `CREATE TABLE`s itself would leave a database the daemon then tries to
//! migrate again -- and `migrations/config/` contains no `IF NOT EXISTS`, deliberately
//! (**D-8**, A-W6-2), so the second application is an error rather than a tolerated no-op.
//! The binary that applies those migrations is this one. It is also the artefact that is
//! already on the machine holding the Go configuration at cutover, which a repository
//! checkout and a Rust toolchain are not.

use std::path::Path;

use anyhow::Context;
use mw_store::config_db;
use mw_store::config_go::{self, NoteKind, Options};
use tracing::{info, warn};

use crate::args::MigrateArgs;

/// Translate the Go configuration database into the one `--config` names.
///
/// Returns the number of lines in the report, which is what the integration test asserts on
/// and what a caller would otherwise have to re-derive.
pub async fn run(m: &MigrateArgs) -> anyhow::Result<usize> {
    // Step 2 of the boot sequence, for the boot sequence's reason: every line below is
    // logged rather than printed, so the report lands in the journal in the operator's
    // configured format alongside everything else this binary ever says.
    let obs = mw_obs::install(&mw_obs::Options {
        directive: m.args.directive.clone(),
        ring_directive: None,
        format: m.args.format,
    })
    .context("installing the logging subscriber")?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        format = obs.shape().as_str(),
        "migrating a RemoteWeather configuration"
    );
    migrate(m).await.map(|migrated| migrated.report.notes.len())
}

/// The migration itself, without installing a subscriber.
///
/// Split from [`run`] for one reason: installing a subscriber is a process-global act that
/// only one test per binary could perform, and the test that answers this work item's "done
/// when" has to migrate a configuration and then plan it. So the subscriber is [`run`]'s and
/// the work is here, which is the same split `mw_supervisor::testing::MemoryLogControl`
/// exists for one crate down.
pub(crate) async fn migrate(m: &MigrateArgs) -> anyhow::Result<config_go::Migrated> {
    info!(
        from = %m.from.display(),
        to = %m.args.config.display(),
        dry_run = m.dry_run,
        "reading the Go configuration"
    );

    let mut source = config_go::open_source(&m.from)
        .await
        .with_context(|| format!("opening {} read-only", m.from.display()))?;
    let go = config_go::read(&mut source)
        .await
        .with_context(|| format!("reading {}", m.from.display()))?;
    // Closed before the destination is opened: the source has nothing left to say, and a
    // held SQLite handle on a database another process is writing is a lock to no purpose.
    use sqlx::Connection as _;
    source
        .close()
        .await
        .context("closing the Go configuration database")?;
    info!(
        devices = go.devices.len(),
        websites = go.websites.len(),
        registered_stations = go.remote_stations.len(),
        weather_database = go.timescale.is_some(),
        "read the Go configuration"
    );

    let options = Options {
        node_name: node_name(),
        spool_path: spool_path(&m.args.config),
    };
    let mut migrated = config_go::translate(&go, &options, &mut config_go::mint_node_id)
        .context("translating the Go configuration")?;

    // (3) above. `mw_codec::SensorMapping::parse` is the parser `plan()` uses, so what is
    // checked here is exactly what would refuse at startup.
    for (key, mapping) in migrated
        .stations_of_type("weatherlink-live")
        .into_iter()
        .map(|(k, m)| (k.to_string(), m.map(str::to_string)))
        .collect::<Vec<_>>()
    {
        let Some(text) = mapping else { continue };
        if let Err(e) = mw_codec::SensorMapping::parse(&text) {
            migrated.disable_station(
                &key,
                &format!(
                    "`wll_sensor_mapping` is `{text}`, which this daemon cannot parse \
                     ({e}). The Go parser accepted it and its transformer then had no arm \
                     for part of it, so the station reported less than it should"
                ),
            );
        }
    }

    let pool = config_db::open_and_migrate(&m.args.config)
        .await
        .with_context(|| format!("opening {}", m.args.config.display()))?;
    let mut tx = pool
        .begin()
        .await
        .context("opening the migration's transaction")?;
    config_go::write(&mut tx, &migrated)
        .await
        .context("writing the migrated configuration")?;
    if m.dry_run {
        // Rolled back explicitly rather than by dropping the transaction, so that a failure
        // to roll back is reported instead of swallowed in a destructor.
        tx.rollback()
            .await
            .context("rolling back the dry run's transaction")?;
    } else {
        tx.commit().await.context("committing the migration")?;
    }
    report(&migrated, m.dry_run);
    pool.close().await;
    Ok(migrated)
}

/// `node.spool_path`: the `spool` directory beside the configuration database.
///
/// Beside it rather than a flag, because the two are one deployment decision: W8's unit
/// passes `--config /var/lib/modernweather/config.db` and `ReadWritePaths` is that
/// directory, so the spool belongs in it -- and a test that puts the configuration in a
/// scratch directory gets a spool in the same scratch directory without being told.
fn spool_path(config: &Path) -> String {
    config
        .parent()
        .unwrap_or(Path::new("."))
        .join("spool")
        .to_string_lossy()
        .into_owned()
}

/// `node.node_name`: the host's name, or `modernweather`.
///
/// `/proc/sys/kernel/hostname` is where [`crate::hostinfo`] reads the same fact from, and
/// the column is operator-facing and mutable -- renaming a node is not an identity change
/// (W7 §5.2) -- so a wrong guess here is one field to edit rather than anything to redo.
fn node_name() -> String {
    match std::fs::read_to_string("/proc/sys/kernel/hostname") {
        Ok(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => "modernweather".to_string(),
    }
}

/// Log the report, one note per line, with the owner's to-do list last.
///
/// Ordered by kind rather than by occurrence: an operator reads the end of a migration's
/// output, so the lines that need them to do something are the ones that should be there.
/// Through `tracing` rather than the terminal, which `ci/layers.toml`'s `no-stdout-logging`
/// rule reserves for the argument-parsing path -- and which is also right here, because this
/// is the one record that the deployment was migrated.
///
/// # `target: AUDIT_TARGET`, and it is load-bearing rather than decorative
///
/// `mw_obs`'s dedupe layer is keyed on the **callsite**, not the message (§5.6), and the
/// whole report comes out of the two `info!`/`warn!` calls below. With `burst: 3` that meant
/// the first three lines of a thirty-line report were emitted and the rest were suppressed
/// as if they were a flapping serial port -- measured on the owner's own configuration,
/// where it swallowed the listener's new ports, two time-zone fallbacks and the O-8 raise.
///
/// `AUDIT_TARGET` is the exemption that exists for exactly this: "configuration mutations,
/// authentication outcomes, certificate reloads -- an audit log with holes is not an audit
/// log". A configuration migration is the largest configuration mutation this program
/// performs, and it happens once per deployment, so there is nothing here for repetition
/// control to protect anybody from.
fn report(migrated: &config_go::Migrated, dry_run: bool) {
    for kind in [NoteKind::Derived, NoteKind::NotCarried, NoteKind::Raised] {
        for note in migrated.report.of(kind) {
            info!(
                target: mw_obs::AUDIT_TARGET,
                kind = kind.as_str(),
                subject = %note.subject,
                "{}",
                note.message
            );
        }
    }
    let actions: Vec<&config_go::Note> = migrated.report.of(NoteKind::Action).collect();
    for note in &actions {
        warn!(
            target: mw_obs::AUDIT_TARGET,
            kind = NoteKind::Action.as_str(),
            subject = %note.subject,
            "{}",
            note.message
        );
    }
    let keys = migrated.station_keys();
    info!(
        target: mw_obs::AUDIT_TARGET,
        stations = keys.len(),
        station_keys = %keys.join(", "),
        peers = migrated.peers.len(),
        actions = actions.len(),
        "the migrated station keys. These are the ids the weather history is stored under: \
         if one of them is not in `observations.station_id`, the charts will draw a live \
         station beside history nothing can see."
    );
    if dry_run {
        warn!(
            target: mw_obs::AUDIT_TARGET,
            "this was a dry run: the transaction was rolled back and nothing was kept. \
             Run it again without --dry-run to write it."
        );
    } else {
        info!(
            target: mw_obs::AUDIT_TARGET,
            config = "migrated",
            "the configuration is written. Start the daemon against it; the one-time \
             administrator setup token is logged at its first start."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use mw_store::config_go::testing as go;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mw-migrate-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn args(scratch: &Scratch, dry_run: bool) -> MigrateArgs {
        MigrateArgs {
            args: Args {
                config: scratch.0.join("config.db"),
                format: None,
                directive: None,
                setup_listen: None,
            },
            from: scratch.0.join("go-config.db"),
            dry_run,
        }
    }

    /// The spool goes beside the configuration database, which is the one deployment fact
    /// this file knows and `mw_store::config_go` does not.
    #[test]
    fn the_spool_is_beside_the_configuration_database() {
        assert_eq!(
            spool_path(Path::new("/var/lib/modernweather/config.db")),
            "/var/lib/modernweather/spool"
        );
        // A bare filename has no parent, and a migration run from a working directory is
        // still a migration.
        assert_eq!(spool_path(Path::new("config.db")), "spool");
    }

    /// The node name is the host's, and a host that cannot be read is not a failure: the
    /// column is operator-facing and mutable, so a wrong guess is one field to edit.
    #[test]
    fn the_node_is_named_after_the_host() {
        let name = node_name();
        assert!(!name.is_empty());
        assert_eq!(name.trim(), name);
    }

    /// **DEV-9's half of this work item.** A `wll_sensor_mapping` the Go parser accepted and
    /// this one cannot is a station that does not plan, and a station that does not plan is
    /// a daemon that does not start -- so it is migrated off with the column named.
    ///
    /// `wnid:1` is one of the classes `docs/reconciliation.md` DEV-9 lists: the Go parser
    /// accepts an unknown role and its transformer then has no arm for it, so the station
    /// silently reported less than it should.
    #[tokio::test]
    async fn a_sensor_mapping_this_daemon_cannot_parse_switches_its_station_off() {
        let scratch = Scratch::new("mapping");
        go::go_db(
            &scratch.0.join("go-config.db"),
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, wll_host,
                                    wll_broadcast, wll_sensor_mapping, timezone)
               VALUES (1, 1, 'roof', 'weatherlink-live', 1, '192.168.1.50', 0,
                       'wnid:1', 'America/Denver');",
            ],
        )
        .await;
        let migrated = migrate(&args(&scratch, false))
            .await
            .expect("the migration completes rather than refusing");
        let note = migrated
            .report
            .of(mw_store::config_go::NoteKind::Action)
            .find(|n| n.subject == "roof")
            .expect("the station is named");
        assert!(
            note.message.contains("wll_sensor_mapping"),
            "{}",
            note.message
        );
        assert!(note.message.contains("wnid:1"), "{}", note.message);

        let pool = config_db::open_and_migrate(&scratch.0.join("config.db"))
            .await
            .expect("the destination opens");
        let enabled: i64 = sqlx::query_scalar("SELECT enabled FROM stations")
            .fetch_one(&pool)
            .await
            .expect("the station");
        assert_eq!(enabled, 0);
        pool.close().await;
    }

    /// A mapping this daemon **can** parse is carried and the station stays on, which is the
    /// other half of the branch above.
    #[tokio::test]
    async fn a_sensor_mapping_that_parses_is_carried_unchanged() {
        let scratch = Scratch::new("mapping-ok");
        go::go_db(
            &scratch.0.join("go-config.db"),
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, wll_host,
                                    wll_broadcast, wll_sensor_mapping, timezone)
               VALUES (1, 1, 'roof', 'weatherlink-live', 1, '192.168.1.50', 0,
                       'th:1, rain:1, wind:1, th_indoor, baro', 'America/Denver');",
            ],
        )
        .await;
        let migrated = migrate(&args(&scratch, false))
            .await
            .expect("the migration");
        assert!(
            !migrated
                .report
                .of(mw_store::config_go::NoteKind::Action)
                .any(|n| n.subject == "roof"),
            "nothing to do: {:#?}",
            migrated.report.notes
        );
        let pool = config_db::open_and_migrate(&scratch.0.join("config.db"))
            .await
            .expect("the destination opens");
        let row: (i64, Option<String>) =
            sqlx::query_as("SELECT enabled, wll_sensor_mapping FROM stations")
                .fetch_one(&pool)
                .await
                .expect("the station");
        assert_eq!(row.0, 1);
        assert_eq!(
            row.1.as_deref(),
            Some("th:1, rain:1, wind:1, th_indoor, baro")
        );
        pool.close().await;
    }

    /// `migrate-config --dry-run` produces the report and keeps nothing.
    #[tokio::test]
    async fn a_dry_run_reports_and_writes_nothing() {
        let scratch = Scratch::new("dry");
        go::go_db(
            &scratch.0.join("go-config.db"),
            &[go::OWNER_DEVICES, go::OWNER_WEBSITE, go::OWNER_STORAGE],
        )
        .await;
        let migrated = migrate(&args(&scratch, true)).await.expect("the dry run");
        assert_eq!(
            migrated.station_keys(),
            vec!["csi", "suncrest-air-quality", "snow"]
        );
        assert!(!migrated.report.notes.is_empty());
        let pool = config_db::open_and_migrate(&scratch.0.join("config.db"))
            .await
            .expect("the destination opens");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM stations")
            .fetch_one(&pool)
            .await
            .expect("a count");
        assert_eq!(n, 0, "the transaction was rolled back");
        pool.close().await;
    }

    /// A source that is not a Go configuration database fails with a sentence that names the
    /// flag, because that is the mistake somebody at cutover actually makes.
    #[tokio::test]
    async fn pointing_the_flag_at_the_wrong_file_says_so() {
        let scratch = Scratch::new("wrongfile");
        std::fs::write(scratch.0.join("go-config.db"), b"not a database").expect("a file");
        let e = migrate(&args(&scratch, false))
            .await
            .expect_err("not a Go configuration database");
        let chain = mw_types::DisplayChain(e.as_ref()).to_string();
        assert!(chain.contains("go-config.db"), "{chain}");
    }

    /// **Every line of the report carries the audit target, and a source assertion is the
    /// only kind that can say so.**
    ///
    /// `mw_obs`'s dedupe layer is keyed on the callsite and the whole report comes out of
    /// two of them, so with `burst: 3` the first three lines were emitted and the rest were
    /// suppressed as a flapping callsite would be. It was found by running the migration
    /// against a real configuration and noticing that the report stopped after three lines,
    /// which is the one way a reader would ever notice.
    ///
    /// A behavioural test cannot reach it: `mw_obs::install` is process-global, so only one
    /// test per binary could install a subscriber, and this binary's one is the boot
    /// sequence's. So the property is asserted over the source of this file -- which is
    /// exactly what `ci/layers.toml`'s own source rules do one level up, and for the same
    /// reason: the alternative is a convention a reviewer has to remember.
    #[test]
    fn the_report_is_exempt_from_repetition_control() {
        let src = include_str!("migrate.rs");
        let body = src.split_once("fn report(").expect("the report function").1;
        let emits = body.matches("info!(").count() + body.matches("warn!(").count();
        let targeted = body.matches("target: mw_obs::AUDIT_TARGET").count();
        assert!(emits > 0, "the report emits nothing at all");
        assert_eq!(
            emits, targeted,
            "{} of {emits} report lines carry `target: mw_obs::AUDIT_TARGET`. Without it \
             the dedupe layer keeps the first three and suppresses the rest, because it is \
             keyed on the callsite and this report has two.",
            targeted
        );
    }
}
