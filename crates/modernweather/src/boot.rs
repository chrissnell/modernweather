//! The boot sequence of W3 §3.1, in order.
//!
//! ```text
//! 1. parse args (--config <path>)                 W8 assumption 2's spelling
//! 2. install the tracing subscriber, capture the reload handle          §5
//! 3. open the SQLite pool and run its migrations  <- the root resource (D-41)
//! 4. cfg = Config::load(&pool)
//! 4a. assert the database version                 <- D-62, after the pool, before plan()
//! 5. specs = plan(&cfg, registrars)               pure; every structural error caught here
//! 6. supervisor = Supervisor::new(pool.schedule_store(), log_control, ...)
//! 7. supervisor.run(specs).await                  never returns until shutdown completes
//! ```
//!
//! Step 3 before step 5 is forced and worth justifying: `plan()` is a pure function *of
//! configuration*, and configuration lives in SQLite, so something has to read SQLite
//! before any component exists. The supervisor also needs `Misfire::RunLatest` to survive
//! a restart. Routing either need through `ConfigStore` would mean the supervisor
//! depending on a component it supervises, which is the `AppReloader` cycle in a new coat
//! -- so **the SQLite pool is a root resource owned by `main`** (**D-41**), which hands a
//! [`SqliteScheduleStore`] over `component_schedule` alone to the supervisor and will hand
//! the full handle to `config:sqlite`.
//!
//! Step 4a is **D-62**, which discharges the half of **D-42** that had no owner.

use std::sync::Arc;

use anyhow::Context;
use mw_component::Generation;
use mw_store::config_db;
use mw_store::version::{self, Observed, VersionError};
use mw_store::SqliteScheduleStore;
use mw_supervisor::plan::{plan, PlanContext, Registrar};
use mw_supervisor::{Roots, RunReport, Supervisor};
use mw_types::Classify;
use secrecy::ExposeSecret;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::args::Args;
use crate::config::Config;
use crate::logctl::ObsLogControl;
use crate::{notify, signals};

/// Which component crates contribute specs.
///
/// Empty, and that is the whole of this milestone: M2-3 built `plan()` and the supervisor,
/// and the first registrar arrives with the first component crate
/// (implementation-plan.md M3-1's station driver). The list being a value here rather than
/// a `match` inside the supervisor is §1.3 rule 2 -- adding a component crate adds a line
/// to this slice and edits nothing else, which is the shape `managers/controller.go` could
/// not take.
const REGISTRARS: &[&dyn Registrar<Config>] = &[];

/// Run the daemon. Returns when shutdown has completed.
pub async fn run(args: &Args) -> anyhow::Result<RunReport> {
    // Step 2 first, so that every line below is logged rather than printed.
    let obs = mw_obs::install(&mw_obs::Options {
        directive: args.directive.clone(),
        ring_directive: None,
        format: args.format,
    })
    .context("installing the logging subscriber and the metrics recorder")?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %args.config.display(),
        format = obs.shape().as_str(),
        directive = obs.directive(),
        "modernweather starting"
    );

    // One series, one label, from the build rather than from a request -- which is
    // §5.10's cardinality rule satisfied by construction. It is also what makes a scrape
    // of a daemon that has done nothing yet distinguishable from a broken route.
    metrics::describe_gauge!(
        "mw_build_info",
        "1, labelled with the version of the running binary"
    );
    metrics::gauge!("mw_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    // Step 3: the root resource.
    let pool = config_db::open_and_migrate(&args.config)
        .await
        .with_context(|| format!("opening {}", args.config.display()))?;

    // Step 4.
    let cfg = Config::load(&pool)
        .await
        .context("reading the configuration")?;
    match &cfg.node {
        Some(node) => info!(node = %node.node_name, timezone = %node.timezone, "node identity"),
        // The migration deliberately seeds no `node` row: `node_id` is a UUIDv7 the node
        // mints once and never edits, so a migration that invented one would make every
        // fresh install claim the same identity. An empty table means "not yet started",
        // and the boot sequence handles it by starting anyway -- nothing here needs an
        // identity to converge. Minting it is W7's enrolment (M6-4), which is also the
        // only thing that knows what `spool_path` should be: the column has no default in
        // the schema and no value in any design document, so writing the row here would
        // mean this work item deciding where the spool lives.
        None => info!("no node identity yet; this is a first start"),
    }

    // Step 4a -- D-62, and the one place its reading is not literal.
    //
    // A server that is too old, or that has no `timescaledb` extension, is a refusal to
    // start: no retry makes it newer and **D-62** wants that answered here rather than at
    // the first `ADD COLUMN`. A server that is merely *unreachable* is not. Refusing to
    // boot because TimescaleDB is down is precisely the failure **D-27** rejects -- the
    // daemon serves its frozen routes from the snapshot with the database gone (§2a.6),
    // and `db:postgres` moves to `Waiting` rather than taking the process with it. The
    // classification beside the error already says which is which (§4.2), so this branch
    // reads it rather than deciding again.
    //
    // The consequence, stated because it belongs to whoever writes `db:postgres`
    // (implementation-plan.md M3-3): a server that was unreachable at boot has not been
    // checked, so that component must assert the version when it first connects. This
    // assertion covers the server that is there when we start.
    match assert_database_version(&cfg, |url| async move { version::probe(&url).await }).await {
        Ok(_) => {}
        Err(e) if matches!(e.class(), mw_types::Class::Retryable { .. }) => warn!(
            err = %mw_types::DisplayChain(&e),
            class = mw_types::Class::RETRYABLE.as_str(),
            "the database version could not be asserted; starting anyway (D-27)"
        ),
        Err(e) => return Err(e).context("asserting the database version (D-62)"),
    }

    // Step 5. Pure: every structural error in W2a Decision 2's list is caught here,
    // before anything is constructed.
    let shutdown = CancellationToken::new();
    let log: Arc<dyn mw_component::LogControl> = Arc::new(ObsLogControl::new(obs.log()));
    let roots = Roots {
        schedule: Arc::new(SqliteScheduleStore::new(pool.clone())),
        // No reading fan-out in this process yet: the spool and `Ingress` are M3-2, and
        // `None` is a legitimate configuration -- a node that serves but does not ingest.
        // `plan()` rejects an `OnReading` component against this root set rather than
        // letting one start and never be delivered to.
        stream: None,
        log,
    };
    let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
    let plan = plan(&cfg, REGISTRARS, &ctx).context("planning the configuration")?;
    info!(
        components = plan.len(),
        generation = %cfg.generation,
        "plan validated"
    );

    // Step 6.
    let (supervisor, _control, _plans) = Supervisor::new(roots, shutdown.clone());
    // `_control` is the `ComponentControl` handle, which `api:management` consumes (M7-1),
    // and `_plans` is how a re-plan reaches a running supervisor once something can write
    // configuration. Both are held by the supervisor as well; nothing in this milestone
    // has a second consumer for either.
    let readiness = supervisor.readiness();

    // `/metrics` on the one listener (§5.10). Bound before the supervisor runs, so that a
    // port already in use is a startup failure rather than a daemon that reports itself
    // ready and serves nothing.
    let metrics = obs.metrics();
    let bound = mw_http::bind(
        cfg.listen_addr()?,
        mw_http::MetricsRoute::new(Arc::new(move || metrics.render()), mw_obs::CONTENT_TYPE),
    )
    .await
    .context("binding the listener")?;
    info!(addr = %bound.local_addr(), "serving /metrics");

    // Step 7. Four concurrent jobs, one task: the supervisor's own tasks are spawned by
    // `mw_supervisor::task`, which is the only module in the workspace allowed to spawn
    // (§3.2), so this is a `join!` rather than four handles to leak.
    let served = {
        let shutdown = shutdown.clone();
        async move {
            let outcome = bound.serve(shutdown.clone().cancelled_owned()).await;
            if let Err(e) = &outcome {
                // A daemon that is not serving is not working, so the accept loop dying
                // takes the process with it rather than leaving half of one running.
                // `Restart=always` in W8's unit is what makes that the right answer: the
                // supervisor still stops every component in order on the way out.
                error!(
                    err = %mw_types::DisplayChain(e),
                    "the listener stopped; shutting down"
                );
                shutdown.cancel();
            }
            outcome
        }
    };
    let (report, serving, (), ()) = tokio::join!(
        supervisor.run(plan),
        served,
        notify::run(readiness, &obs, &shutdown),
        signals::wait(&shutdown),
    );
    info!(
        stopped = report.stopped,
        aborted = report.aborted,
        clock_steps = report.clock_steps,
        "shutdown complete"
    );
    // Reported after the shutdown rather than instead of it: every component is stopped
    // by here, and what is left to decide is the exit code.
    serving.context("serving the listener")?;
    Ok(report)
}

/// **D-62**: assert the database version, after the pool opens and before `plan()` runs.
///
/// The comparison is `mw_store::version`'s, which is the same one
/// `ci/sql/assert-timescaledb-version.sql` makes -- a test in that module reads the
/// required versions out of the SQL so the two halves of **D-42** cannot drift apart.
///
/// Generic over the probe so that "what does this do against PostgreSQL 15" is a test
/// rather than a container: the one thing a startup assertion has to get right is the
/// message, and arranging a deliberately-wrong server is the one thing a unit test cannot
/// do. `None` means there was nothing to assert.
pub async fn assert_database_version<F, Fut>(
    cfg: &Config,
    probe: F,
) -> Result<Option<Observed>, VersionError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Observed, VersionError>>,
{
    let Some(db) = &cfg.weather_database else {
        // A node with no weather database row has no `db:postgres` and therefore no
        // `writer:timescale` (**D-44**, W7 §8.2). That is the satellite of W7 §8.2 and it
        // is also what makes `--config` against an empty configuration a legitimate
        // start: there is no server, so D-62 has nothing to compare.
        info!("no weather database is configured; there is no server version to assert");
        return Ok(None);
    };
    let observed = probe(db.url.expose_secret().to_string()).await?;
    let toolkit = version::check(&observed)?;
    if !toolkit {
        // The SQL raises a warning rather than an error here, and so does this: W4 owns
        // the sum-and-count versus `stats_agg` decision (OQ-3) and both are correct. This
        // becomes fatal when a migration adds the extension, at which point the migration
        // is the assertion.
        warn!("timescaledb_toolkit is not installed; stats_agg and rollup are unavailable");
    }
    info!(server = %observed, "database version asserted");
    Ok(Some(observed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_store::config_db::{Listener, WeatherDatabase};
    use mw_store::version::Version;
    use secrecy::SecretString;

    fn config(with_database: bool) -> Config {
        Config {
            listener: Listener {
                listen_addr: "127.0.0.1".into(),
                http_port: 8080,
                https_port: None,
                grpc_port: 50051,
                management_listen_addr: "127.0.0.1".into(),
                management_port: 8081,
            },
            weather_database: with_database.then(|| WeatherDatabase {
                url: SecretString::from("postgres://mw:hunter2@db.internal/weather".to_string()),
            }),
            node: None,
            generation: Generation::ZERO,
        }
    }

    fn observed(pg: u32, ts: &str) -> Observed {
        Observed {
            pg_major: pg,
            timescale: Version::parse(ts),
            toolkit: true,
        }
    }

    #[tokio::test]
    async fn a_deliberately_wrong_postgres_version_fails_the_start_and_names_both() {
        let error =
            assert_database_version(&config(true), |_| async { Ok(observed(15, "2.31.0")) })
                .await
                .expect_err("a server older than the required major must not be accepted");
        let message = error.to_string();
        assert!(
            message.contains("15"),
            "the version found is named: {message}"
        );
        assert!(
            message.contains("17"),
            "the version required is named: {message}"
        );
    }

    #[tokio::test]
    async fn a_deliberately_wrong_timescale_version_fails_the_start_and_names_both() {
        // The production pair as of 2026-10-31 is 16.6 / 2.17.2, which is what an operator
        // pointing the new daemon at the old server would actually hit.
        let error =
            assert_database_version(&config(true), |_| async { Ok(observed(17, "2.17.2")) })
                .await
                .expect_err("2.17.2 is older than the required 2.31.0");
        let message = error.to_string();
        assert!(message.contains("2.17.2"), "{message}");
        assert!(message.contains("2.31.0"), "{message}");
    }

    #[tokio::test]
    async fn the_probe_is_handed_the_configured_url() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert_database_version(&config(true), |url| {
            tx.send(url).expect("the receiver is alive");
            async { Ok(observed(17, "2.31.0")) }
        })
        .await
        .expect("the shipped pair is accepted");
        assert_eq!(
            rx.recv().expect("the probe was called"),
            "postgres://mw:hunter2@db.internal/weather",
            "the probe gets the secret, which is why nothing else does"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_database_is_not_a_failure_to_start() {
        let asserted = assert_database_version(&config(false), |_| async {
            panic!("there is nothing to probe");
        })
        .await
        .expect("a node with no weather database is a legitimate node (D-44)");
        assert_eq!(asserted, None);
    }
}
