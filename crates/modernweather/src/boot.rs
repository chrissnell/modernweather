//! The boot sequence of W3 §3.1, in order.
//!
//! ```text
//! 1. parse args (--config <path>)                 W8 assumption 2's spelling
//! 2. install the tracing subscriber, capture the reload handle          §5
//! 3. open the SQLite pool and run its migrations  <- the root resource (D-41)
//! 4. cfg = Config::load(&pool)
//! 4a. assert the database version                 <- D-62, after the pool, before plan()
//! 4b. open the spool                              <- the fan-out, the second root (D-30a)
//! 4c. registry + rollup + snapshot table           <- `config:sqlite` has no work item
//! 4d. ingress                                     <- needs 4b's spool
//! 5. specs = plan(&cfg, registrars)               pure; every structural error caught here
//! 6. supervisor = Supervisor::new(pool.schedule_store(), log_control, ...)
//! 6a. bind the listener                           <- NOT in §3.1; see below
//! 7. supervisor.run(specs).await                  never returns until shutdown completes
//! ```
//!
//! **Step 6a is a deviation from two sections and it is temporary.** §3.1 says `main`
//! touches exactly three resources outside a component, and §6.1 gives the socket to an
//! `http:listener` *component*. This binary binds one directly, because M2-4's
//! done-criteria require `/metrics` to be served, §5.10 puts it on an additive route on
//! the one listener, and §1.4 allows only `mw-http` to bind -- so the socket exists before
//! the component that should own it. Two consequences, neither of which bites yet: `main`
//! owns a fourth root resource, and the listener's teardown runs concurrently with the
//! supervisor's reverse-topological component stops rather than being ranked by them, so
//! §3.6's "a consumer must never observe a revoked capability" does not cover it. It is
//! safe today because the only route closes over a renderer rather than over a capability
//! handle. implementation-plan.md M4-3 builds `http:listener` and removes this step along
//! with `mw_http::bind`, the `served` future and the `join!` arm.
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
use std::time::Duration;

use anyhow::Context;
use mw_component::Generation;
use mw_store::config_db;
use mw_store::version::{self, Observed, VersionError};
use mw_store::SqliteScheduleStore;
use mw_supervisor::plan::{plan, PlanContext, RegisterError, Registrar};
use mw_supervisor::{Roots, RunReport, Supervisor};
use mw_types::Classify;
use secrecy::ExposeSecret;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::args::Args;
use crate::config::Config;
use crate::logctl::ObsLogControl;
use crate::notify;
use crate::signals::Signals;
use crate::spool;

/// Which component crates contribute specs.
///
/// One line per component crate, which is §1.3 rule 2: adding a component crate adds a line
/// here and edits nothing else -- the shape `managers/controller.go` could not take, down to
/// its `if con.Type == "management" { continue }`.
const REGISTRARS: &[&dyn Registrar<Config>] = &[&Stations, &Database, &Rollup, &Snapshot];

/// `mw-station`'s registrar.
///
/// The adapter exists because `Registrar` is `mw-supervisor`'s and both crates are in the
/// `service` layer, so `ci/layers.toml` forbids `mw-station` from naming it -- the
/// supervisor must not know that a station exists. The binary is the one place a consumer
/// and a provider meet (§1.3 rule 1), so the trait is implemented here over
/// `mw_station::register`, which is the pure function that does the work.
#[derive(Debug)]
struct Stations;

impl Registrar<Config> for Stations {
    fn name(&self) -> &'static str {
        "mw-station"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        // The chain, flattened once, at the boundary where `RegisterError` is a string:
        // `PlanError::Registrar` carries a `reason: String`, so a `#[source]` chain that
        // was not walked here would lose every level but the first.
        mw_station::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `db:postgres` and `writer:timescale`, which are the two halves of the write path's
/// far end.
///
/// An adapter here for `Stations`' reason and one more: the two specs come from two
/// different crates — the pool from `mw-store`, the cursor from `mw-pipeline` — and those
/// two crates are in the same layer and may not name each other (`mw_pipeline::writer`'s
/// module documentation has the whole of that note). The binary is where they meet.
///
/// **Both are conditional, and on different things.**
///
/// - No `weather_database` row means neither: a node with no server has no `db:postgres`
///   and therefore no `writer:timescale` (**D-44**, W7 §8.2's satellite). Emitting the
///   writer alone would be a hard need with no provider, which `plan()` refuses — so the
///   satellite shape would be a daemon that will not start.
/// - No spool means no writer. `Trigger::OnReading` is validated against the root
///   `ReadingStream`, which exists only for a node that has a `node` row and therefore a
///   spool; without one `plan()` refuses the component, and refusing the *whole
///   configuration* for a node that is serving but not ingesting is wrong. The pool still
///   gets planned, because the read path (M3-4, M4-1) needs it with or without ingest.
#[derive(Debug)]
struct Database;

impl Registrar<Config> for Database {
    fn name(&self) -> &'static str {
        "mw-store/mw-pipeline (the write path's far end)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        let Some(db) = &cfg.weather_database else {
            return Ok(());
        };
        // The same snapshot the ingress resolves against. `attach_registry` runs before
        // `plan()` in every path that reaches here, and the `expect` would be a bug in the
        // boot sequence rather than a configuration error -- so it is reported as a
        // registrar error naming what is missing instead of a panic.
        let registry = cfg.registry.as_ref().ok_or_else(|| {
            RegisterError::new(
                "the station registry has not been published, so `db:postgres` has \
                 nothing to copy the read-side projection from",
            )
        })?;
        out.push(
            mw_store::pg::spec(db.url.clone(), Arc::clone(registry))
                .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))?,
        );
        if cfg.ingress.is_some() {
            out.push(
                mw_pipeline::writer::spec()
                    .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))?,
            );
        } else {
            info!(
                "a weather database is configured and this node has no spool, so the \
                 pool is planned and `writer:timescale` is not -- there is no reading \
                 stream for it to consume (W7 §8.2)"
            );
        }
        Ok(())
    }
}

/// `window:rollup` and one `rain:rollover:<station>` per collecting station.
///
/// An adapter here for `Stations`' reason: `Registrar` is `mw-supervisor`'s and
/// `mw-pipeline` is in the same layer, so the trait is implemented in the binary over
/// `mw_pipeline::rollup::register`, which is the pure function that does the work.
///
/// Conditional on the same thing `writer:timescale` is — a `weather_database` row, because
/// without one there is no `db:postgres` to provide `ReadingStore` (**D-44**) — and
/// additionally on this node collecting for at least one station. `mw_pipeline::rollup`
/// owns both choices and this adapter makes neither.
#[derive(Debug)]
struct Rollup;

impl Registrar<Config> for Rollup {
    fn name(&self) -> &'static str {
        "mw-pipeline (window:rollup and the rainfall rollovers)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_pipeline::rollup::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `snapshot:latest` and `snapshot:warm` — the hot path's memory and its cold start.
///
/// An adapter here for `Rollup`'s reason. The four conditions under which one or both are
/// not planned belong to `mw_pipeline::snapshot::register`, which makes all of them and
/// logs each; this adapter makes none.
///
/// **Neither is planned in this build**, because the payload renderer is the frozen 133-key
/// DTO's serialiser and `mw-api-legacy` exclusively owns it (implementation-plan.md M4-2).
/// `Config::snapshot_render` answers `None` and the registrar says so at `info`. The
/// registrar is wired now rather than with M4-2 so that landing the renderer is one line
/// in `crate::config` and nothing here.
#[derive(Debug)]
struct Snapshot;

impl Registrar<Config> for Snapshot {
    fn name(&self) -> &'static str {
        "mw-pipeline (the latest-conditions snapshot)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_pipeline::snapshot::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// Build the station registry, the window rollup and the snapshot table, and hand all
/// three to the configuration.
///
/// **Two things that are not in W3 §3.1's list, and the reason is the same for both: the
/// component that should own them does not exist.** W2a §4.1b gives `StationRegistry` to a
/// `config:sqlite` component and no work item in `docs/implementation-plan.md` creates one,
/// so the snapshot is built here, once, from the rows step 4 already read -- the same way
/// the SQLite pool and the spool are root resources `main` owns (**D-41**, **D-30a**). What
/// that costs is stated at `mw_store::registry`: a station added while the daemon runs is
/// not resolvable until it restarts, which is true of every configuration change until
/// `ConfigAdmin` exists (M7-1).
///
/// `SpoolIngress` is the other half. It holds the spool and the registry and nothing else,
/// which is what makes **D-87** -- `admit` does no I/O but the spool append -- structurally
/// true rather than a rule to remember: there is no database handle in it to reach for.
///
/// A function rather than inline code, so that the tests below drive the *same* assembly
/// the daemon does. A second copy in a test would be a test of the copy.
fn attach_registry(cfg: &mut Config) -> anyhow::Result<()> {
    // The receivers a `Fixed` frame resolves through, minted by the registrar that minted
    // the component ids -- so the registry and the plan cannot disagree about what a
    // component is called.
    // A `StationPlanError`, so this reads as the configuration rejection it is rather than
    // as a failure to assemble a pipeline: `fixed_receivers` can refuse a station key that
    // cannot name a component, which is a structural error step 5 would otherwise report.
    let receivers = mw_station::fixed_receivers(cfg)
        .map_err(|e| anyhow::anyhow!("{}", mw_types::DisplayChain(&e)))
        .context("the station configuration is not usable")?;
    let snapshot = mw_store::RegistrySnapshot::new(
        &cfg.stations,
        receivers.iter().map(|r| (&r.component, &r.station)),
    );
    info!(
        stations = cfg.stations.len(),
        claims = snapshot.claims(),
        receivers = snapshot.receivers(),
        "station registry published"
    );
    // A station carrying `ingest_secret_hash` resolves to `Reject::BadSecret` on every
    // frame until M6-1 brings the driver that verifies one (`mw_store::registry`). That is
    // the correct answer and an invisible one: the counter only moves when the station
    // sends something, so a hash pasted onto the wrong row is a station that reports
    // nothing with nothing anywhere naming it. Said once, at startup, where an operator
    // reading the boot log can act on it.
    for station in snapshot.unverifiable_secrets() {
        warn!(
            %station,
            "this station has an ingest secret configured and no driver in this build can \
             verify one, so every reading from it will be refused. \
             implementation-plan.md M6-1 adds the driver that presents one; until then, \
             clear `ingest_secret_hash` for a station that does not need it."
        );
    }
    // **The validation the W4 migration's comment claimed the writer did.** The writer
    // copies `config.stations.timezone` and substitutes `'UTC'` for a NULL or blank one
    // (`mw_store::writer`), so a typo becomes a `stations.timezone` of `Amercia/Denver` and
    // a rollover that silently happens in UTC. A SQL `CHECK` cannot catch it either --
    // `now() AT TIME ZONE x` is `STABLE`, so it is not an expression a constraint may
    // contain -- and `RegistrySnapshot::new` is where the column is already read and
    // already normalised, so it is where the parse goes.
    //
    // **A refusal rather than a warning**, and the difference matters: a station whose zone
    // does not resolve has no boundary for its rainfall day to accumulate from, so carrying
    // on means serving a `dayrain` that resets at the database server's midnight -- which
    // is exactly the defect implementation-plan.md M3-4 exists to close. This is a
    // configuration error, not an unavailable dependency, so it is not the D-27 carve-out
    // that lets a down database be started past.
    if !snapshot.zone_problems().is_empty() {
        let mut chain = String::new();
        for problem in snapshot.zone_problems() {
            if !chain.is_empty() {
                chain.push_str("; ");
            }
            chain.push_str(&mw_types::DisplayChain(problem).to_string());
        }
        anyhow::bail!("{chain}");
    }
    let zones: Vec<(mw_types::StationKey, Option<mw_component::Zone>)> = snapshot
        .zones()
        .map(|(key, zone)| (key.clone(), zone))
        .collect();
    cfg.set_registry(
        Arc::new(mw_store::ConfigRegistry::new(snapshot)) as Arc<dyn mw_component::StationRegistry>,
        zones,
    );
    // The maintained window values, over the same station set the registrar plans from --
    // read through the trait rather than rebuilt here, so the state's keys and the
    // components' ids cannot disagree about which stations exist. A root resource `main`
    // owns, for the reason `crate::config`'s `rollup` field records: two components write
    // it.
    let stations = mw_pipeline::RollupConfig::rollup_stations(&*cfg);
    cfg.set_rollup(Arc::new(mw_pipeline::RollupState::new(stations)));
    // The snapshot table, over the same two sets the key is a product of (**D-31**).
    // Another root resource for the rollup state's reason -- two components write it -- and
    // built here rather than in the registrar so that both of them are handed the same
    // table.
    //
    // **Every station, not only the collecting ones.** `window:rollup` maintains windows
    // for the stations this node collects for, because a window is accumulated from frames
    // this node received. A snapshot is the other way round: `/latest?station=` may name
    // **any** configured device (W3 §2a.4), and a station a peer forwards readings for
    // arrives through `Ingress::accept` and has conditions to serve. Filtering here would
    // make those readings fold into no key at all.
    //
    // **Including the disabled ones**, which is the axis `rollup_stations` does filter on. A
    // station with `enabled = 0` has collection switched off and can be switched back on
    // without a restart of anything but its own component; its key answering 404 in the
    // meantime is the same answer a station that has not reported gets. The alternative --
    // no key at all -- would mean a re-enabled station serves nothing until the daemon
    // restarts, because the key set is fixed at construction.
    let websites: Vec<mw_component::WebsiteView> = cfg
        .websites
        .iter()
        .map(|w| mw_component::WebsiteView {
            id: mw_types::WebsiteId::new(w.id),
            name: w.name.clone(),
            primary_station: w
                .primary_station_key
                .as_deref()
                .map(mw_types::StationKey::new),
            snow_enabled: w.snow_enabled,
            snow_station: w.snow_station_key.as_deref().map(mw_types::StationKey::new),
        })
        .collect();
    let snapshot_stations: Vec<mw_types::StationKey> = cfg
        .stations
        .iter()
        .map(|r| mw_types::StationKey::new(r.station_key.as_str()))
        .collect();
    debug!(
        websites = websites.len(),
        stations = snapshot_stations.len(),
        keys = websites.len() * snapshot_stations.len(),
        "snapshot table allocated"
    );
    cfg.set_snapshot(Arc::new(mw_pipeline::SnapshotState::new(
        websites,
        snapshot_stations,
    )));
    Ok(())
}

/// Hand the configuration the write end of the pipeline, once the spool is open.
///
/// Separate from [`attach_registry`] because the two have different preconditions: the
/// registry is built from rows step 4 already read and exists for every node, while the
/// ingress needs the spool and therefore exists only for a node with an identity (W7
/// §8.2). Splitting them is also what lets `db:postgres` be planned for a node that serves
/// and does not ingest.
///
/// `SpoolIngress` holds the spool and the registry and nothing else, which is what makes
/// **D-87** -- `admit` does no I/O but the spool append -- structurally true rather than a
/// rule to remember: there is no database handle in it to reach for.
fn attach_ingress(cfg: &mut Config, fanout: &spool::Fanout) -> anyhow::Result<()> {
    let registry = cfg
        .registry
        .as_ref()
        .context("the station registry has not been published")?;
    cfg.set_ingress(Arc::new(mw_pipeline::SpoolIngress::new(
        Arc::clone(&fanout.spool),
        Arc::clone(registry),
    )) as Arc<dyn mw_component::Ingress>);
    Ok(())
}

/// How long the **D-62** probe may take before the boot sequence gives up on it.
///
/// Chosen against `TimeoutStartSec=60` in W8's unit rather than against a round number:
/// the probe is one of several startup steps and none of them may spend the start budget.
/// Giving up is not the same as passing -- the version is then unasserted, which is the
/// `Retryable` outcome, and `db:postgres` is what re-asserts it (see the call site).
const PROBE_DEADLINE: Duration = Duration::from_secs(5);

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

    // Before anything that can block, so that a `systemctl stop` during startup is an
    // ordered exit rather than a default-disposition kill. Until this exists SIGTERM has
    // no handler, and the window is every step below: the migrations, the configuration
    // read, the D-62 probe and the bind.
    let shutdown = CancellationToken::new();
    let mut signals = Signals::install();
    // The first thing the service manager hears, and the first point at which SIGTERM is
    // handled rather than fatal. Ordered this way deliberately: everything below can
    // block, and an operator who runs `systemctl stop` during a slow start should get an
    // ordered exit rather than "killed by signal TERM".
    notify::starting("opening the configuration database");

    // Step 3: the root resource.
    let Some(pool) = signals
        .race(&shutdown, config_db::open_and_migrate(&args.config))
        .await
        .transpose()
        .with_context(|| format!("opening {}", args.config.display()))?
    else {
        notify::stopping();
        return Ok(RunReport::default());
    };

    // Step 4.
    let mut cfg = Config::load(&pool)
        .await
        .context("reading the configuration")?;
    match &cfg.node {
        Some(node) => info!(
            node = %node.node_name,
            timezone = %node.timezone,
            spool = %node.spool_path,
            "node identity"
        ),
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

    notify::starting("asserting the database version");

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
    // The consequence belongs to whoever writes `db:postgres`, so it is recorded in that
    // item's own text rather than only here: implementation-plan.md M3-3 now carries
    // "assert the database version when it first connects" as a constraint from this
    // item. A comment in a source file is not a work item (docs/reconciliation.md G-5).
    // This assertion covers the server that is there when we start.
    let asserting = assert_database_version(&cfg, |url| async move { version::probe(&url).await });
    let probed = signals
        .race(&shutdown, tokio::time::timeout(PROBE_DEADLINE, asserting))
        .await;
    let Some(probed) = probed else {
        pool.close().await;
        notify::stopping();
        return Ok(RunReport::default());
    };
    match probed {
        Ok(Ok(_)) => {}
        Ok(Err(e)) if matches!(e.class(), mw_types::Class::Retryable { .. }) => warn!(
            err = %mw_types::DisplayChain(&e),
            class = mw_types::Class::RETRYABLE.as_str(),
            "the database version could not be asserted; starting anyway (D-27)"
        ),
        Ok(Err(e)) => return Err(e).context("asserting the database version (D-62)"),
        // The deadline is the same decision as the `Retryable` branch, reached a different
        // way: a refused connection answers in microseconds, but a blackholed address --
        // a firewall, a dead host on a routed subnet, a partition -- answers only when the
        // kernel gives up, which is minutes. Without this the daemon never binds, never
        // notifies, and systemd kills the start at `TimeoutStartSec=60` and restarts it
        // forever: "refusing to boot because TimescaleDB is down", which is exactly what
        // the branch above exists to avoid.
        Err(_) => warn!(
            deadline_s = PROBE_DEADLINE.as_secs(),
            "the weather database did not answer inside the deadline; starting anyway (D-27)"
        ),
    }

    // Step 4b: the spool, which is the reading fan-out and therefore the second root
    // resource `main` owns (W3 §3.1, **D-30a**).
    //
    // Opened before `plan()`, because `Roots::capabilities()` is what `plan()` validates an
    // `OnReading` component against -- so whether this process has a `ReadingStream` has to
    // be known before the plan is built, not after.
    //
    // `None` when there is no `node` row, which is a legitimate configuration and not a
    // degraded one: `node.spool_path` has no default in the schema, a node that has never
    // enrolled has no identity to put in `MANIFEST`, and a spool whose `node_id` was
    // invented here would be adopted by the real identity later and refused. A node that
    // serves but does not ingest is W7 §8.2's shape.
    mw_pipeline::metrics::register();
    notify::starting("opening the spool");
    let fanout = match &cfg.node {
        Some(node) => Some(
            spool::open(node)
                .with_context(|| format!("opening the spool at {}", node.spool_path))?,
        ),
        None => {
            info!("no node identity, so no spool and no reading fan-out yet");
            None
        }
    };

    // Step 4c: identity resolution and the write end of the pipeline.
    mw_station::metrics::register();
    attach_registry(&mut cfg).context("publishing the station registry")?;
    if let Some(fanout) = &fanout {
        attach_ingress(&mut cfg, fanout).context("assembling the reading pipeline")?;
    }

    // Step 5. Pure: every structural error in W2a Decision 2's list is caught here,
    // before anything is constructed.
    let log: Arc<dyn mw_component::LogControl> = Arc::new(ObsLogControl::new(obs.log()));
    let roots = Roots {
        schedule: Arc::new(SqliteScheduleStore::new(pool.clone())),
        // `plan()` rejects an `OnReading` component against a root set without this rather
        // than letting one start and never be delivered to.
        stream: fanout
            .as_ref()
            .map(|f| Arc::clone(&f.stream) as Arc<dyn mw_component::ReadingStream>),
        log,
    };
    notify::starting("planning the configuration");
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
            if outcome.is_err() {
                // A daemon that is not serving is not working, so the accept loop dying
                // takes the process with it rather than leaving half of one running.
                // `Restart=always` in W8's unit is what makes that the right answer: the
                // supervisor still stops every component in order on the way out.
                //
                // Not logged here. The error is returned, and `main` logs the chain once
                // for whatever ended the process -- two lines for one event would undo
                // the care `main` takes to use whichever channel exists and not both.
                shutdown.cancel();
            }
            outcome
        }
    };
    let (report, serving, (), ()) = tokio::join!(
        supervisor.run(plan),
        served,
        notify::run(readiness, &obs, &shutdown),
        signals.wait(&shutdown),
    );
    // The spool is `main`'s too, and closing it is just dropping it: every cursor is
    // released with the component that held it, and every append was `fsync`ed before its
    // caller was told it committed -- so there is no flush to forget.
    //
    // What is worth saying out loud on the way out is how much it still holds. A spool
    // that is deep at shutdown is a spool whose consumers were behind, and the next start
    // will replay from exactly here; the appends-per-commit ratio is the group-commit
    // coalescing (W7 D-13), which is the one number that says whether a replay burst cost
    // one group commit or hundreds.
    if let Some(fanout) = fanout {
        let stats = fanout.spool.stats();
        info!(
            depth_bytes = fanout.spool.depth_bytes(),
            tail_seq = fanout.spool.tail_seq().get(),
            appends = stats.appends,
            commits = stats.commits,
            "spool closed"
        );
    }
    // The pool is `main`'s to close (D-41) and nothing can hold it by here: every
    // component is stopped. WAL mode means the final checkpoint happens on the last
    // connection closing, so skipping this leaves `-wal`/`-shm` beside the database and a
    // `config.db` copied for backup missing its most recent commits.
    pool.close().await;
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
    use secrecy::SecretString;

    fn config(with_database: bool) -> Config {
        Config {
            stations: Vec::new(),
            declared_stations: Vec::new(),
            websites: Vec::new(),
            ingress: None,
            registry: None,
            rollup: None,
            snapshot: None,
            snapshot_windows: None,
            station_zones: Vec::new(),
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
        Observed::new(pg, Some(ts), true)
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

    // -----------------------------------------------------------------------------------
    // The done-criterion, end to end: a WeatherLink Live station configured in the
    // configuration database produces readings.
    //
    // Here rather than in `tests/station_ingest.rs` because `modernweather` is a `[[bin]]`
    // and an integration test cannot import from a binary target -- so a test over there
    // would have had to restate `declare` and `attach_ingress`, and a test of a copy is not
    // a test. These run the real conversion, the real registry, the real registrar, the
    // real `plan()` and the real supervisor, and read the envelope back out of the spool's
    // files:
    //
    //   config.db -> Config::load -> declare -> register -> plan -> Supervisor
    //        |                                                         | Builder::serve
    //        +-- RegistrySnapshot -> ConfigRegistry --+                 v
    //                                                 \-> SpoolIngress <- BroadcastListener
    //                                                          |              ^
    //                                                          v              | a datagram
    //                                                        Spool -> Cursor  | on loopback

    /// A scratch directory per test, removed on drop. Twenty lines rather than a
    /// dev-dependency, which is the call `mw-store`'s and `mw-pipeline`'s tests make.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mw-ingest-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
            Self(dir)
        }

        fn db(&self) -> std::path::PathBuf {
            self.0.join("config.db")
        }

        fn spool(&self) -> std::path::PathBuf {
            self.0.join("spool")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The serial a WeatherLink Live puts in every datagram, in the case it reports it.
    const SERIAL: &str = "001D0A700002";

    /// The sensor mapping these tests configure, which is `vp2_basic` minus the indices.
    const MAPPING: &str = "th:1, rain:1, wind:1, th_indoor, baro";

    /// A free UDP port, for a station row that must not collide with anything on the host.
    ///
    /// 22222 is the port a real WeatherLink Live broadcasts to and is deliberately **not**
    /// used: a developer running the daemon while the tests run would make this flaky, and
    /// the port is configuration (`transport_port`) precisely so that it can be.
    async fn free_port() -> u16 {
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port is available");
        probe.local_addr().expect("a bound socket").port()
    }

    /// A configuration database with a node identity, a spool, and the given station rows.
    async fn configure(scratch: &Scratch, stations: &[String]) -> sqlx::SqlitePool {
        let pool = config_db::open_and_migrate(&scratch.db())
            .await
            .expect("the configuration schema applies");
        // The `node` row the spool needs. The migration deliberately seeds none, because
        // `node_id` is minted once by the node; a test is the one place it is a literal.
        sqlx::query(
            "INSERT INTO node (id, node_id, node_name, spool_path)
             VALUES (1, x'07070707070707070707070707070707', 'test', ?)",
        )
        .bind(scratch.spool().to_string_lossy().to_string())
        .execute(&pool)
        .await
        .expect("the node row");
        // **A `website` row, and the test below depends on it being here.** The snapshot's
        // key is `(website, station)`, so with no website the key set is empty and
        // `mw_pipeline::snapshot::register` short-circuits on *that* rather than on the
        // missing renderer. The assertion in
        // `a_weather_database_and_a_spool_plan_the_pool_and_the_writer` would then be green
        // for a reason that has nothing to do with M4-2, and would stay green when M4-2
        // lands — an inert tripwire, which is worse than none.
        //
        // `primary_station_key` is deliberately NULL: the column is nullable and
        // `ON DELETE SET NULL`, the station rows are added by the caller afterwards, and a
        // website with no primary device is a configuration the daemon must accept.
        sqlx::query("INSERT INTO website (id, name, hostname) VALUES (1, 'test', '')")
            .execute(&pool)
            .await
            .expect("the website row");
        for sql in stations {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the schema rejected a station row: {e}"));
        }
        pool
    }

    /// A broadcasting WeatherLink Live, as an operator would configure one.
    fn broadcast_station(key: &str, serial: &str, port: u16) -> String {
        format!(
            // `timezone` is set, and it has to be: every collecting station gets a
            // `rain:rollover:<station>` whose `Calendar` schedules in the station's own
            // zone, and `plan()` refuses one with no valid zone rather than rolling over in
            // the server's (`mw_pipeline::rollup::register`). A fixture without it would be
            // a configuration the daemon correctly declines to start on.
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   presented_key, transport_kind, transport_port,
                                   transport_host, wll_sensor_mapping, timezone)
             VALUES ('{key}', x'07070707070707070707070707070707', '{key}',
                     'weatherlink-live', 'from_payload', '{serial}', 'udp_in', {port},
                     '192.168.1.50', '{MAPPING}', 'America/Denver')"
        )
    }

    /// Unix seconds as the instant a transport observed a frame at.
    fn instant(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).expect("a literal instant")
    }

    /// A datagram in the shape a WeatherLink Live broadcasts.
    fn datagram(serial: &str, ts: i64, temp: f64) -> Vec<u8> {
        format!(
            r#"{{"did":"{serial}","ts":{ts},"conditions":[{{"lsid":48308,
               "data_structure_type":1,"txid":1,"temp":{temp},"hum":52.0,
               "wind_speed_last":4.0,"wind_dir_last":280,"rain_size":1,
               "rain_rate_last":0,"rainfall_daily":12}}]}}"#
        )
        .into_bytes()
    }

    /// Steps 3, 4, 4b and 4c, over a scratch database: the daemon's own code.
    async fn boot_to_pipeline(
        scratch: &Scratch,
        stations: &[String],
    ) -> (sqlx::SqlitePool, Config, spool::Fanout) {
        let pool = configure(scratch, stations).await;
        let mut cfg = Config::load(&pool).await.expect("the configuration reads");
        let node = cfg.node.clone().expect("the node row was inserted");
        let fanout = spool::open(&node).expect("the spool opens");
        attach_registry(&mut cfg).expect("the registry publishes");
        attach_ingress(&mut cfg, &fanout).expect("the pipeline assembles");
        (pool, cfg, fanout)
    }

    /// **The headline.** The real plan, the real supervisor, a datagram on loopback, and a
    /// durable envelope carrying the station the operator configured.
    ///
    /// `current_thread`, like every other test here, and that is **required** rather than
    /// incidental. `.github/workflows/pr.yml`'s `race` job documents a standing
    /// ThreadSanitizer false positive in `tokio`'s I/O driver: a `ScheduledIo` is published
    /// to the driver as an `epoll_ctl` token and read back from `epoll_wait`, TSan does not
    /// model those syscalls as synchronisation, so it reports the *allocating* write
    /// against the driver thread's atomic load. It needs the registration and the driver to
    /// be two threads, which is exactly what a multi-threaded runtime makes them.
    ///
    /// This test was written `multi_thread` and was the first target in the workspace to
    /// combine a multi-threaded runtime with a socket registration — `mw-pipeline`'s
    /// four-worker fan-out test does file I/O and never calls `Handle::add_source` — so it
    /// turned the `race` job red on `tokio::runtime::io::driver::Handle::add_source::<mio::
    /// net::udp::UdpSocket>`, frame for frame as that comment predicts. Nothing in this
    /// crate was racing; the runtime flavour was a free choice and this is the one that
    /// keeps the job able to speak. A `Serve` body and a test body interleave at every
    /// await on one thread, which is all this test needs.
    #[tokio::test(flavor = "current_thread")]
    async fn a_configured_weatherlink_live_station_produces_a_durable_reading() {
        let scratch = Scratch::new("produces");
        let port = free_port().await;
        let (pool, cfg, fanout) = boot_to_pipeline(
            &scratch,
            &[broadcast_station("suncrest:roof", SERIAL, port)],
        )
        .await;

        // Step 5, with the real registrar list.
        let shutdown = CancellationToken::new();
        let roots = Roots {
            schedule: Arc::new(SqliteScheduleStore::new(pool.clone())),
            stream: Some(Arc::clone(&fanout.stream) as Arc<dyn mw_component::ReadingStream>),
            // The real one is `ObsLogControl` over the subscriber's reload handle, and
            // installing a subscriber is a process-global act that only one test per
            // binary could perform. What the supervisor needs from this root is a handle
            // that answers, which is what the double is.
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx).expect("the configuration plans");
        let mut ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "station:wll:suncrest:roof:rearm",
                &format!("station:wll:udp:{port}")[..],
            ],
            "one listener for the endpoint, one re-arm for the unit (W2a D-44)"
        );

        // Step 6: the real supervisor, which is what runs `Builder::serve` and therefore
        // what binds the socket. Nothing here constructs a component by hand.
        let (supervisor, _control, _plans) = Supervisor::new(roots, shutdown.clone());
        // `spawn` is forbidden in the daemon (`clippy.toml`, §3.2) and this test is `main`:
        // `Supervisor::run` returns only on shutdown, so driving it and sending to it at
        // the same time is the only way to exercise anything it supervises.
        #[allow(clippy::disallowed_methods)]
        let running = tokio::task::spawn(supervisor.run(planned));

        // Send the datagram once the listener has had a chance to bind. Retried rather
        // than slept on: a fixed sleep is a flake, and a lost UDP datagram before the bind
        // is indistinguishable from a broken driver.
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a sending socket");
        let target = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        // A `Durable` subscription through the root capability, which is what
        // `writer:timescale` will be (M3-3) and the only way to read the spool back as a
        // consumer rather than as a file.
        let mut cursor = mw_component::ReadingStream::subscribe(
            fanout.stream.as_ref(),
            &mw_component::ComponentId::new("writer:timescale").expect("a literal id"),
            &mw_component::ReadingFilter::All,
            mw_component::ReadingPolicy::Durable,
        )
        .expect("a durable subscription");
        let batch = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                sender
                    .send_to(&datagram(SERIAL, 1_762_200_000, 62.7), target)
                    .await
                    .expect("the datagram is sent");
                if fanout.spool.tail_seq() > mw_types::Seq::new(0) {
                    break cursor.next_batch().await.expect("a batch");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the listener bound and admitted a datagram");

        let envelope = &batch.envelopes()[0];
        // 1. The station the operator configured, resolved from the serial the device sent.
        //    This is the whole of identity resolution, and the assertion no driver-level
        //    test can make: a recording ingress resolves nothing.
        assert_eq!(envelope.station.as_str(), "suncrest:roof");
        assert_eq!(envelope.reading.station_id, "suncrest:roof");
        // 2. The payload's own instant, not the arrival time.
        assert_eq!(envelope.observed_at.timestamp(), 1_762_200_000);
        // 3. The metrics the configured mapping names, in the registry's units.
        assert_eq!(envelope.reading.out_temp, Some(62.7), "degF, not Celsius");
        assert_eq!(envelope.reading.out_humidity, Some(52.0));
        assert_eq!(envelope.reading.wind_speed, Some(4.0));
        assert_eq!(envelope.reading.wind_dir, Some(280.0));
        let day_rain = envelope.reading.day_rain.expect("rainfall is populated");
        assert!(
            (day_rain - 0.12).abs() < 1e-5,
            "12 tips of 0.01 in: {day_rain}"
        );
        // 4. And nothing invented: a broadcast datagram carries no barometer.
        assert_eq!(
            envelope.reading.barometer, None,
            "D-26: absent stays absent"
        );
        assert_eq!(envelope.reading.snow_depth, None);
        // 5. Sealed by this node, at the first sequence number.
        assert_eq!(
            envelope.provenance.origin(),
            mw_types::OriginId::new([7; 16])
        );
        assert_eq!(envelope.provenance.seq(), mw_types::Seq::FIRST);

        shutdown.cancel();
        let report = tokio::time::timeout(Duration::from_secs(20), running)
            .await
            .expect("the supervisor stops")
            .expect("the task did not panic");
        assert_eq!(report.aborted, 0, "the listener stopped inside its grace");
        pool.close().await;
    }

    /// A neighbour's unit on the same LAN reaches the spool as nothing at all.
    ///
    /// The listener is configured for a serial the registry does not know, which is the
    /// case that separates the two lookups: the driver decodes the datagram and the gate
    /// refuses it, so nothing is appended.
    #[tokio::test(flavor = "current_thread")]
    async fn a_datagram_from_an_unconfigured_unit_is_not_stored() {
        let scratch = Scratch::new("unknown");
        let port = free_port().await;
        let (pool, cfg, fanout) = boot_to_pipeline(
            &scratch,
            &[broadcast_station("suncrest:roof", SERIAL, port)],
        )
        .await;
        let ingress = mw_station::StationConfig::ingress(&cfg)
            .expect("the pipeline is attached")
            .clone();

        let listener = mw_station::wll::BroadcastListener::new(
            port,
            None,
            [mw_station::wll::BroadcastUnit {
                serial: "001D0A7FFFFF".to_string(),
                station: mw_types::StationKey::new("somebody:else"),
                codec: mw_codec::WllConditions::new(
                    mw_codec::SensorMapping::parse(MAPPING).expect("a literal mapping"),
                ),
            }],
            ingress,
        );
        listener
            .on_datagram(
                &mw_component::ComponentId::new("station:wll:udp:1").expect("a literal id"),
                &datagram("001D0A7FFFFF", 1_762_200_001, 44.0),
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 99)),
                instant(1_762_200_001),
            )
            .await;
        assert_eq!(
            fanout.spool.tail_seq(),
            mw_types::Seq::new(0),
            "nothing was appended, so there is no reading to store"
        );
        pool.close().await;
    }

    /// **The production shape: a weather database *and* a spool.**
    ///
    /// This is the only configuration that emits `writer:timescale`, and it is the one
    /// every real deployment uses — so a mistake in that branch of the `Database`
    /// registrar (the wrong `Need`, a provider that is not in the plan) is a daemon that
    /// refuses to start. The other two shapes were covered and this one was not.
    #[tokio::test(flavor = "current_thread")]
    async fn a_weather_database_and_a_spool_plan_the_pool_and_the_writer() {
        let scratch = Scratch::new("writer");
        let port = free_port().await;
        let (pool, mut cfg, fanout) = boot_to_pipeline(
            &scratch,
            &[broadcast_station("suncrest:roof", SERIAL, port)],
        )
        .await;
        // The `weather_database` row the registrar reads. Set on the loaded configuration
        // rather than inserted, because `Config::load` has already run.
        cfg.weather_database = Some(config_db::WeatherDatabase {
            url: secrecy::SecretString::from(
                "postgresql://mw@127.0.0.1:5432/modernweather".to_string(),
            ),
        });

        let roots = Roots {
            schedule: Arc::new(SqliteScheduleStore::new(pool.clone())),
            stream: Some(Arc::clone(&fanout.stream) as Arc<dyn mw_component::ReadingStream>),
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx).expect("the production shape plans");
        let mut ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        ids.sort_unstable();
        // The listener's id carries the ephemeral port, so the assertion is on the two
        // components this registrar owns plus the shape of the third.
        assert!(
            ids.contains(&"db:postgres"),
            "the pool must be planned: got {ids:?}"
        );
        assert!(
            ids.contains(&"writer:timescale"),
            "and so must the writer, which is the branch nothing covered: got {ids:?}"
        );
        // The read path's two, which arrive with the pool rather than with the spool:
        // `window:rollup` maintains the windows R2 keeps out of SQL, and one
        // `rain:rollover:<station>` per collecting station moves its local day boundary.
        assert!(ids.contains(&"window:rollup"), "got {ids:?}");
        assert!(
            ids.contains(&"rain:rollover:suncrest:roof"),
            "one rollover per collecting station: got {ids:?}"
        );
        // **The snapshot components are not planned, and the renderer is the only reason.**
        // `snapshot:latest` and `snapshot:warm` are implementation-plan.md M4-1's and are
        // complete; what they need and do not have is the payload renderer, which is the
        // frozen 133-key DTO's serialiser and belongs exclusively to `mw-api-legacy`
        // (M4-2). `Config::snapshot_render` answers `None`, so
        // `mw_pipeline::snapshot::register` plans neither and says so at `info`.
        //
        // The fixture has a `website` row and a station row, so the key set is **not**
        // empty and `register`'s other three conditions are all satisfied -- which is what
        // makes this a tripwire rather than a tautology. Setting `snapshot_render` to
        // `Some` is sufficient to fail this assertion and the count below.
        assert!(
            !cfg.websites.is_empty() && !cfg.stations.is_empty(),
            "the key set must be non-empty, or this asserts the wrong thing: {} websites, \
             {} stations",
            cfg.websites.len(),
            cfg.stations.len()
        );
        assert!(
            !ids.contains(&"snapshot:latest") && !ids.contains(&"snapshot:warm"),
            "the snapshot components need M4-2's renderer; when it lands, this assertion \
             inverts and the count below becomes 8: got {ids:?}"
        );
        // The listener, its re-arm (the fixture's station has a `transport_host`), the
        // pool, the writer, the rollup and the rollover.
        assert_eq!(ids.len(), 6, "got {ids:?}");
        assert!(
            ids.iter().any(|id| id.starts_with("station:wll:udp:")),
            "got {ids:?}"
        );
        pool.close().await;
    }

    /// The satellite shape (W7 §8.2): a weather database and **no** spool.
    ///
    /// The pool is still planned, because the read path needs it with or without ingest;
    /// `writer:timescale` is not, because `Trigger::OnReading` is validated against the
    /// root `ReadingStream` and refusing the whole configuration for a node that serves
    /// without ingesting would be wrong.
    #[tokio::test(flavor = "current_thread")]
    async fn a_weather_database_without_a_spool_plans_the_pool_and_not_the_writer() {
        let mut cfg = config(true);
        cfg.registry = Some(Arc::new(mw_store::ConfigRegistry::new(
            mw_store::RegistrySnapshot::default(),
        )));
        // No `ingress`, which is what "no spool" means to the registrar.
        assert!(cfg.ingress.is_none());

        let roots = Roots {
            schedule: Arc::new(mw_supervisor::testing::MemoryScheduleStore::new()),
            stream: None,
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx).expect("a node that serves but does not ingest");
        let ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        assert_eq!(ids, vec!["db:postgres"]);
    }

    /// No weather database: neither component, and that is not an error (**D-44**).
    #[test]
    fn no_weather_database_plans_neither_the_pool_nor_the_writer() {
        let mut cfg = config(false);
        cfg.registry = Some(Arc::new(mw_store::ConfigRegistry::new(
            mw_store::RegistrySnapshot::default(),
        )));
        let mut out = Vec::new();
        Database
            .register(&cfg, &mut out)
            .expect("no database is a legitimate configuration");
        assert!(out.is_empty());
    }

    /// The registrar refuses rather than panicking when the boot sequence has not
    /// published the registry — which would be a bug in `run()`'s ordering, not a
    /// configuration error, and is reported as such.
    #[test]
    fn the_registrar_names_what_is_missing_rather_than_panicking() {
        let cfg = config(true);
        assert!(cfg.registry.is_none());
        let mut out = Vec::new();
        let e = Database
            .register(&cfg, &mut out)
            .expect_err("the registry has not been published");
        assert!(e.to_string().contains("station registry"), "{e}");
    }

    /// Two units on one endpoint, a polled third, and the component set that follows.
    #[tokio::test(flavor = "current_thread")]
    async fn the_three_shapes_of_a_weatherlink_live_station_plan_together() {
        let scratch = Scratch::new("shapes");
        let port = free_port().await;
        let (pool, cfg, fanout) = boot_to_pipeline(
            &scratch,
            &[
                broadcast_station("suncrest:roof", SERIAL, port),
                broadcast_station("suncrest:barn", "001D0A700003", port),
                format!(
                    "INSERT INTO stations (station_key, owning_node_id, name, type,
                                           identity_kind, transport_kind, transport_url,
                                           wll_sensor_mapping, wll_poll_interval_seconds,
                                           timezone)
                     VALUES ('suncrest:field', x'07070707070707070707070707070707', 'field',
                             'weatherlink-live', 'fixed', 'http_out', 'http://192.168.1.51',
                             '{MAPPING}', 30, 'America/Denver')"
                ),
            ],
        )
        .await;
        let roots = Roots {
            schedule: Arc::new(SqliteScheduleStore::new(pool.clone())),
            stream: Some(Arc::clone(&fanout.stream) as Arc<dyn mw_component::ReadingStream>),
            // The real one is `ObsLogControl` over the subscriber's reload handle, and
            // installing a subscriber is a process-global act that only one test per
            // binary could perform. What the supervisor needs from this root is a handle
            // that answers, which is what the double is.
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx).expect("the configuration plans");
        let mut ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "station:wll:suncrest:barn:rearm",
                "station:wll:suncrest:field",
                "station:wll:suncrest:roof:rearm",
                &format!("station:wll:udp:{port}")[..],
            ],
            "a second unit on one port is a row, not a second socket (W2a §4a.4)"
        );
        // A polled station resolves by its receiver, so the registry has to have an entry
        // for exactly that component -- which is `fixed_receivers`' whole job.
        let receivers = mw_station::fixed_receivers(&cfg).expect("the receivers derive");
        assert_eq!(receivers.len(), 1);
        assert_eq!(
            receivers[0].component.as_str(),
            "station:wll:suncrest:field"
        );
        assert_eq!(receivers[0].station.as_str(), "suncrest:field");
        pool.close().await;
    }

    /// A node with stations and no spool is refused at plan time, not started.
    #[tokio::test(flavor = "current_thread")]
    async fn a_station_with_no_reading_stream_refuses_to_plan() {
        let scratch = Scratch::new("nostream");
        let pool = config_db::open_and_migrate(&scratch.db())
            .await
            .expect("migrate");
        // No `node` row, so no spool and no fan-out -- W7 §8.2's "serves but does not
        // ingest" node, with a station configured on it by mistake.
        sqlx::query(&broadcast_station("suncrest:roof", SERIAL, 22222))
            .execute(&pool)
            .await
            .expect("the row is valid");
        let cfg = Config::load(&pool).await.expect("the configuration reads");
        assert!(cfg.node.is_none(), "no identity, so step 4b opens no spool");
        let ctx =
            PlanContext::new(Generation::ZERO).with_roots(mw_supervisor::plan::RootSet::empty());
        let e = plan(&cfg, REGISTRARS, &ctx).expect_err("nowhere to admit readings to");
        let text = mw_types::DisplayChain(&e).to_string();
        assert!(
            text.contains("no reading stream"),
            "the refusal says what is missing: {text}"
        );
        pool.close().await;
    }

    /// W5 §1.2's toggle, through the daemon's own conversion.
    #[tokio::test(flavor = "current_thread")]
    async fn switching_a_station_off_removes_its_components() {
        let scratch = Scratch::new("disabled");
        let port = free_port().await;
        let (pool, cfg, _fanout) = boot_to_pipeline(
            &scratch,
            &[broadcast_station("suncrest:roof", SERIAL, port)],
        )
        .await;
        sqlx::query("UPDATE stations SET enabled = 0")
            .execute(&pool)
            .await
            .expect("the toggle is a column");
        let reloaded = Config::load(&pool)
            .await
            .expect("the configuration re-reads");
        let mut specs = Vec::new();
        mw_station::register(&reloaded, &mut specs).expect("a disabled station is not an error");
        assert!(specs.is_empty(), "the toggle is a plan diff, not an error");
        // ... and the same rows enabled do produce components, so the assertion above is
        // about the toggle and not about the fixture being wrong.
        let mut specs = Vec::new();
        mw_station::register(&cfg, &mut specs).expect("the enabled rows plan");
        assert_eq!(specs.len(), 2);
        pool.close().await;
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
