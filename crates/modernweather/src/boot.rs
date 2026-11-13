//! The boot sequence of W3 §3.1, in order.
//!
//! ```text
//! 1. parse args (--config <path>)                 W8 assumption 2's spelling
//! 2. install the tracing subscriber, capture the reload handle          §5
//! 3. open the SQLite pool and run its migrations  <- the root resource (D-41)
//! 4. cfg = Config::load(&mut pool.acquire().await.expect("a connection"))
//! 4a. assert the database version                 <- D-62, after the pool, before plan()
//! 4b. open the spool                              <- the fan-out, the second root (D-30a)
//! 4c. registry + rollup + snapshot table           <- `config:sqlite` has no work item
//! 4d. ingress                                     <- needs 4b's spool
//! 4e. route tables + host table + certs + mount  <- NOT in §3.1; see below
//! 5. specs = plan(&cfg, registrars)               pure; every structural error caught here
//! 6. supervisor = Supervisor::new(pool.schedule_store(), log_control, ...)
//! 7. supervisor.run(specs).await                  never returns until shutdown completes
//! ```
//!
//! **Step 6a is gone, and step 4e replaced it.** M2-4 bound one socket directly here,
//! because `/metrics` had to be served before any component existed, and recorded two
//! consequences: `main` owned a fourth root resource, and the listener's teardown ran
//! concurrently with the supervisor's reverse-topological component stops rather than being
//! ranked by them -- so §3.6's "a consumer must never observe a revoked capability" did not
//! cover it. M4-3 made `http:listener` the component §6.1 describes, which closes the
//! second: the socket now closes *after* the components whose tables it serves have
//! stopped.
//!
//! The first is not closed, and is now explicit rather than temporary. [`attach_listener`]
//! builds the **mounted router** as a root resource, because the route tables come from
//! `mw-api-legacy` and `mw-web` and `mw-http` may name neither (`ci/layers.toml`) -- so the
//! binary is where they meet (§1.3 rule 1), exactly as it is for the spool, the station
//! registry, the window rollup, the snapshot table and the frozen surface's state slot.
//! §3.1 names three root resources and there are seven; what makes that safe is that every
//! one of them is handed to its component through a registrar trait rather than reached for.
//!
//! One behaviour change comes with it, and `mw_http::component`'s module documentation has
//! the whole of it: **a port another process holds is now a `Failed -> Backoff` loop rather
//! than a non-zero exit.**
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
use mw_store::config_db;
use mw_store::version::{self, Observed, VersionError};
use mw_store::SqliteScheduleStore;
use mw_supervisor::plan::{PlanContext, RegisterError, Registrar};
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
/// One line per component **family**, which is §1.3 rule 2's property: adding one adds a
/// line here and edits nothing else -- the shape `managers/controller.go` could not take,
/// down to its `if con.Type == "management" { continue }`.
///
/// Rule 2's own wording is "each component crate contributes one `fn register(cfg, out)`",
/// and three crates here contribute more than one: `mw-pipeline` through `Rollup`, `Snow`
/// and `Snapshot`, `mw-output` through `Outputs` and `Aeris`, and `mw-station` through
/// `Stations` and `SnowGauges`. Each split is one configuration table against another --
/// `uploader` against `aeris_account`, the station rows against the ingest rows -- and the
/// reason is the same every time: one registrar reading two tables is one `RegisterError`
/// for two unrelated refusals, so an operator who mistyped a credential is told about a
/// coordinate. The order is not load-bearing: `Phase::Spread` is derived from a hash of the
/// component id and `plan()` orders by the hard-edge digraph, so position affects only
/// which refusal surfaces first.
const REGISTRARS: &[&dyn Registrar<Config>] = &[
    &Stations,
    &SnowGauges,
    &ReadingReceiver,
    &Database,
    &Rollup,
    &Snow,
    &Snapshot,
    &Legacy,
    &Outputs,
    &Aeris,
    &Web,
    &Listener,
];

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

/// `mw-grpc`'s snow gauge registrar.
///
/// **A second station registrar, and `ci/layers.toml` is why.** `mw-station` may not name
/// `tonic`, so the component that dials a gauge cannot live there — what lives there is
/// everything about the gauge that is not gRPC: the `type` value, the component id, and the
/// plan-time validation of the row (`mw_station::snowgauge`). So `mw_station::register`
/// checks a `snowgauge` row and emits nothing, and this emits the component for the rows it
/// checked.
///
/// The two therefore read the **same** rows and mint the **same** ids, which is the one
/// property that has to hold: the registry resolves a `Fixed` frame *by its receiver*, so
/// an id mismatch would be a gauge whose every reading is `unknown_station` with nothing
/// naming it. `mw_station::fixed_receivers` is the third caller of that same function, and
/// `crates/mw-station/tests/plan.rs` asserts the three agree.
#[derive(Debug)]
struct SnowGauges;

impl Registrar<Config> for SnowGauges {
    fn name(&self) -> &'static str {
        "mw-grpc:snowgauge"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_grpc::snowgauge::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `station:grpcrx` — `mw-grpc`'s reading receiver.
///
/// Conditional on a `config.ingest` row with `listen = 'grpc'`, which is the only thing
/// that makes this node a collector (W7 **D-44**). The state slot it installs into is built
/// beside the listener's route tables in [`attach_listener`], because the route has to exist
/// before `plan()` runs — `mw_api_legacy::RouteState`'s reason, one more time.
#[derive(Debug)]
struct ReadingReceiver;

impl Registrar<Config> for ReadingReceiver {
    fn name(&self) -> &'static str {
        "mw-grpc:grpcrx"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_grpc::grpcrx::register(cfg, cfg.grpcrx_service.as_ref(), out)
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

/// `aprs:*`, `pws:*` and `wu:*` -- the rate-limited outputs.
///
/// An adapter here for `Rollup`'s reason: `Registrar` is `mw-supervisor`'s and `mw-output`
/// is a higher layer, so the trait is implemented in the binary over
/// `mw_output::register`, which is the pure function that does the work.
///
/// Conditional on two things, both of which `mw_output::register` decides: an enabled
/// `uploader` row, and a `WindowedView` to resolve against. The second is the interesting
/// one — an uploader sends **windows**, so a node with no rollup has nothing to upload and
/// the registrar says so at `info` rather than refusing the configuration.
#[derive(Debug)]
struct Outputs;

impl Registrar<Config> for Outputs {
    fn name(&self) -> &'static str {
        "mw-output (the rate-limited outputs)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_output::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `snow:*` -- the four components behind `/snow`'s seven numbers.
///
/// An adapter here for `Rollup`'s reason. Every decision belongs to
/// `mw_pipeline::snow::register`: a node with no weather database plans nothing (**D-44**),
/// a gauge with no `snow_base_distance_mm` is skipped because the schema makes that state
/// representable on purpose, and a gauge with no usable zone is a refusal naming the
/// station -- "today" and "this season" are both station-local (**D-24**).
#[derive(Debug)]
struct Snow;

impl Registrar<Config> for Snow {
    fn name(&self) -> &'static str {
        "mw-pipeline (the snow cache)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_pipeline::snow::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `aeris:*` -- the two forecast spans, the alert fetch and the expired-alert sweep.
///
/// A **second** registrar over `mw-output`, beside `Outputs`, and the reason is that they
/// are two component families with two configuration tables: an `uploader` row is an egress
/// account and the `aeris_account` row is a fetch account (**D-66**'s split, one layer up).
/// One registrar reading both would be one `RegisterError` for two unrelated refusals.
///
/// Every decision belongs to `mw_output::aeris::register`: no account or no pairing plans
/// nothing, and a paired station with no coordinates is a refusal naming it -- the forecast
/// API is queried by `latitude,longitude`, so such a station can never be asked about.
#[derive(Debug)]
struct Aeris;

impl Registrar<Config> for Aeris {
    fn name(&self) -> &'static str {
        "mw-output (the Aeris components)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_output::aeris::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `api:legacy` -- the frozen REST surface.
///
/// An adapter here for `Rollup`'s reason. `mw_api_legacy::register` makes every decision,
/// and there is only one: a node whose boot sequence built the route state slot plans the
/// component, and one that did not is a wiring mistake rather than a configuration.
///
/// **Unconditional on the configuration**, which is deliberate and is the opposite of the
/// three registrars above it. There is no configuration under which the frozen table should
/// be absent: a node with no `website` row serves the contract's `503` from the handlers
/// (§6.3), not a `404` from a route that is not there, and those are different bytes.
#[derive(Debug)]
struct Legacy;

impl Registrar<Config> for Legacy {
    fn name(&self) -> &'static str {
        "mw-api-legacy (the frozen REST surface)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_api_legacy::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `web:static` and one `web:site:<id>` per website -- the public website.
///
/// An adapter here for `Legacy`'s reason. `mw_web::register` makes every decision, and the
/// one it makes is the same one `api:legacy`'s does: a node with no `LatestSnapshot`
/// provider gets no website component, because §2.6 declares that need `Hard` and `plan()`
/// refuses a configuration whose hard need nobody provides. The asset tree, `/privacy` and
/// `/support` are planned unconditionally -- they depend on nothing.
#[derive(Debug)]
struct Web;

impl Registrar<Config> for Web {
    fn name(&self) -> &'static str {
        "mw-web (the public website)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_web::register(cfg, out)
            .map_err(|e| RegisterError::new(mw_types::DisplayChain(&e).to_string()))
    }
}

/// `http:listener` -- the one listener of §6.1.
///
/// **Unconditional**, and the last registrar in the list rather than the first: the order
/// in `REGISTRARS` has no effect on the plan (`plan()` sorts by the hard-edge digraph), but
/// reading the list top to bottom should read as the dependency direction, and the listener
/// is what everything above it is served through.
#[derive(Debug)]
struct Listener;

impl Registrar<Config> for Listener {
    fn name(&self) -> &'static str {
        "mw-http (the one listener)"
    }

    fn register(
        &self,
        cfg: &Config,
        out: &mut Vec<mw_component::ComponentSpec>,
    ) -> Result<(), RegisterError> {
        mw_http::register(cfg, out)
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
fn attach_registry(
    cfg: &mut Config,
    slots: &mut Slots,
) -> anyhow::Result<Option<mw_store::RegistrySnapshot>> {
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
    // A stored ingest secret that will never verify anything. Both kinds are invisible
    // from the counters alone -- `mw_readings_rejected_total{bad_secret}` only moves when
    // the station sends something, so a hash pasted onto the wrong row is a station that
    // reports nothing with nothing anywhere naming it. Said once, at startup, where an
    // operator reading the boot log can act on it
    // (`mw_store::registry::RegistrySnapshot::secret_problems`).
    for problem in snapshot.secret_problems() {
        // One line per problem, each naming the station: a counter does not, and a station
        // whose uploads are all refused is otherwise indistinguishable from one that is
        // switched off. The message is deliberately about the *secret* rather than about
        // the station, because `SecretProblem::Missing` is also reached by a station with
        // no secret at all.
        warn!(problem = %problem, "a configured ingest secret cannot authenticate anything");
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

    // **The reuse decision, and it is made here because here is where every input exists.**
    // `Slots`' documentation has the whole argument; the fingerprint below is the exact set
    // of values the four shared objects are constructed from, written out rather than
    // approximated by "the station rows changed" -- which would rebuild the snapshot table
    // when an operator corrected a baud rate, and with it restart the entire plan.
    let fingerprint = shared_fingerprint(cfg, &zones);

    // The registry is **published into**, never rebuilt, so it is handled before the reuse
    // branch and on both sides of it. The snapshot travels back to the caller rather than
    // being stored here, because a preview must not publish: see [`Mounted::registry`].
    let registry = match &slots.registry {
        Some(live) => {
            cfg.set_registry(
                Arc::clone(live) as Arc<dyn mw_component::StationRegistry>,
                zones.clone(),
            );
            Some(snapshot)
        }
        None => {
            let live = Arc::new(mw_store::ConfigRegistry::new(snapshot));
            cfg.set_registry(
                Arc::clone(&live) as Arc<dyn mw_component::StationRegistry>,
                zones.clone(),
            );
            slots.registry = Some(live);
            None
        }
    };

    if let Some(shared) = &slots.shared {
        if shared.fingerprint == fingerprint {
            debug!(
                epoch = slots.epoch,
                "the station and website sets are unchanged, so the shared root state is \
                 reused and only the components whose own configuration changed restart"
            );
            cfg.set_rollup(Arc::clone(&shared.rollup));
            cfg.set_snapshot(Arc::clone(&shared.snapshot));
            cfg.set_legacy(Arc::clone(&slots.legacy_routes), Arc::clone(&shared.render));
            return Ok(registry);
        }
    }
    // The maintained window values, over the same station set the registrar plans from --
    // read through the trait rather than rebuilt here, so the state's keys and the
    // components' ids cannot disagree about which stations exist. A root resource `main`
    // owns, for the reason `crate::config`'s `rollup` field records: two components write
    // it.
    let stations = mw_pipeline::RollupConfig::rollup_stations(&*cfg);
    let rollup = Arc::new(mw_pipeline::RollupState::new(stations));
    cfg.set_rollup(Arc::clone(&rollup));
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
    let snapshot_state = Arc::new(mw_pipeline::SnapshotState::new(websites, snapshot_stations));
    cfg.set_snapshot(Arc::clone(&snapshot_state));
    // The frozen surface's two root resources: the route table's state slot, which the
    // listener reads and `api:legacy` fills, and the DTO's serialiser, which both snapshot
    // components render through. Built together because the slot without the renderer is a
    // configuration that plans `api:legacy` and no snapshot for it to read, and the
    // renderer without the slot is one that refuses to plan `api:legacy` at all.
    //
    // **The zones come from the station rows rather than from `station_zones`**, which is
    // the registry's resolved list and covers only the stations it resolved a zone for.
    // `mw_api_legacy::register` parses them itself, from the column, and skips one that is
    // not an IANA name with a warning -- see its own note on why a cosmetic field may not
    // take the frozen table down.
    let render_zones: std::collections::BTreeMap<mw_types::StationKey, mw_component::Zone> = cfg
        .station_zones
        .iter()
        .filter_map(|(key, zone)| zone.map(|z| (key.clone(), z)))
        .collect();
    // The coordinates, for the same table: `/latest`'s `moonCrescentAngle` is computed at
    // the **website's primary** station rather than at the station the request named
    // (`handlers.go:387`), so the renderer needs a position the snapshot does not carry.
    let positions: std::collections::BTreeMap<mw_types::StationKey, (f64, f64)> = cfg
        .stations
        .iter()
        .filter_map(|row| {
            let (lat, lon) = (row.latitude?, row.longitude?);
            Some((
                mw_types::StationKey::new(row.station_key.as_str()),
                (lat, lon),
            ))
        })
        .collect();
    let renderer = Arc::new(mw_api_legacy::Renderer::new(render_zones, positions));
    info!(
        zones = renderer.zones(),
        "the frozen DTO's serialiser is built, so the snapshot components are planned"
    );
    cfg.set_legacy(Arc::clone(&slots.legacy_routes), Arc::clone(&renderer));

    // Everything that was just built, and the epoch that makes the whole plan restart.
    // Bumped rather than set from the fingerprint, so that the log line and the digests
    // read as "the Nth shape of this process" rather than as an opaque hash.
    slots.epoch = slots.epoch.saturating_add(1);
    slots.shared = Some(SharedState {
        fingerprint,
        rollup,
        snapshot: snapshot_state,
        render: renderer,
    });
    Ok(registry)
}

/// The exact inputs the four shared root objects are constructed from.
///
/// Written out as a string rather than hashed from the rows, because that is the whole
/// point: the rollup state is built from the **collecting** stations and their zones, the
/// snapshot table from every station and the five website fields its key is a product of,
/// and the renderer from the resolved zones and the coordinates. A station's baud rate is in
/// none of them, so correcting one restarts that station and nothing else.
fn shared_fingerprint(
    cfg: &Config,
    zones: &[(mw_types::StationKey, Option<mw_component::Zone>)],
) -> mw_component::ConfigDigest {
    use std::fmt::Write as _;
    // Every free-text value is length-prefixed. Without that, a website an operator named
    // `x;w9:` could make two different configurations produce one string -- and a collision
    // here means stale shared root state with no epoch bump, which is the one input on this
    // path a person types freely.
    fn text(s: &mut String, value: &str) {
        let _ = write!(s, "{}:{value}", value.len());
    }
    let mut s = String::new();
    for (key, zone) in zones {
        let _ = write!(s, "z");
        text(&mut s, key.as_str());
        text(&mut s, zone.map_or("-", |z| z.name()));
        s.push(';');
    }
    for row in &cfg.stations {
        // `enabled` and "has a transport" are `rollup_stations`' filter; the coordinates
        // are the renderer's; the key is both the snapshot table's and the rollup's.
        //
        // The columns that are **not** here -- the name, the device type, the altitude, the
        // snow base distance, the protocol secret, the presented key -- are the station
        // registry's, and the registry is published into rather than rebuilt
        // ([`Slots::publish_registry`]). Putting them here would restart the whole plan for
        // a rename.
        let _ = write!(s, "s");
        text(&mut s, &row.station_key);
        let _ = write!(
            s,
            ":{}:{}:{:?}:{:?};",
            row.enabled,
            row.transport_kind.is_some(),
            row.latitude,
            row.longitude
        );
    }
    for w in &cfg.websites {
        let _ = write!(s, "w{}", w.id);
        text(&mut s, &w.name);
        text(&mut s, w.primary_station_key.as_deref().unwrap_or("-"));
        text(&mut s, w.snow_station_key.as_deref().unwrap_or("-"));
        let _ = write!(s, ":{};", w.snow_enabled);
    }
    mw_component::ConfigDigest::of(&s)
}

/// The root resources that **outlive one plan**, and the fingerprint that decides it.
///
/// # Why anything is carried across a re-plan at all
///
/// Four of these hold live state that is not in the configuration database: the rollup's
/// window values, the snapshot table's rendered bytes, and the two route-state slots a
/// component installs its handlers into. Rebuilding them on every save would mean a
/// configuration change silently emptied the current conditions and 503-ed every frozen
/// path -- which is the class of defect W5 §2 exists to remove, reached from the other
/// direction.
///
/// # The fingerprint, and the epoch it drives
///
/// They cannot *always* be reused either: the rollup state's and the snapshot table's key
/// sets are fixed at construction, so a station the operator just added has nowhere to fold
/// into. Which gives the rule:
///
/// > A shared root resource is reused while the inputs it was constructed from are
/// > unchanged, and rebuilt when they change. A rebuild bumps [`Slots::epoch`], which
/// > [`stamp`] folds into **every** component's `ConfigDigest` -- so the whole plan restarts
/// > and nothing is left holding the previous object.
///
/// The epoch is what makes that honest rather than hopeful. Without it, a station added to
/// the configuration would rebuild the rollup state, restart `window:rollup` -- whose digest
/// *is* the station set -- and leave every `rain:rollover:<station>` holding the old one,
/// because that component's digest is its own station and its own zone. Two writers of two
/// states, and the symptom is a rain total that stops rolling over at midnight for every
/// station except the new one.
///
/// What it costs is stated rather than implied: **adding or removing a station or a website
/// restarts every component**, which the save preview says in advance because the preview is
/// the same `plan()` and the same diff. Editing a station's settings, a service, the forecast
/// account, the storage connection or the ports restarts only what changed.
#[derive(Clone)]
pub(crate) struct Slots {
    /// The frozen route table's state slot. Never rebuilt: its identity is not a function
    /// of the configuration, and `api:legacy` installs into it while it runs.
    legacy_routes: Arc<mw_api_legacy::RouteState>,
    /// The Ambient ingest endpoint's slot. Never rebuilt, for the same reason.
    ambient: Arc<mw_station::ambient::UploadState>,
    /// The reading receiver's slot. Never rebuilt, for the same reason.
    grpcrx: Arc<mw_grpc::grpcrx::ServiceState>,
    /// One slot per website, **by id** rather than by position.
    web: std::collections::BTreeMap<i64, Arc<mw_web::SiteState>>,
    /// The station registry, which is **published into** rather than rebuilt.
    ///
    /// The one shared root object with a swap of its own: `ConfigRegistry` holds its
    /// snapshot in an `ArcSwap` precisely so that a configuration change can replace it
    /// without restarting the components that hold the handle -- which is the seam
    /// `mw_store::registry::ConfigRegistry::publish` was written for and that nothing called
    /// until this item. Rebuilding it instead would mean a rotated ingest secret, a renamed
    /// station or a corrected altitude did not take effect until the daemon restarted,
    /// because `mw_station::config` already declines to restart a station for a secret
    /// rotation on the grounds that the registry is what compares it.
    ///
    /// `None` until the first build, which is the one call that has a snapshot and no
    /// registry to put it in.
    registry: Option<Arc<mw_store::ConfigRegistry>>,
    /// The three whose key sets are fixed at construction, and the fingerprint of the inputs
    /// they were built from. `None` before the first build.
    shared: Option<SharedState>,
    /// Bumped whenever [`Slots::shared`] is rebuilt. Folded into every digest by [`stamp`].
    epoch: u64,
}

/// The shared root state, and what it was built from.
#[derive(Clone)]
struct SharedState {
    fingerprint: mw_component::ConfigDigest,
    rollup: Arc<mw_pipeline::RollupState>,
    snapshot: Arc<mw_pipeline::SnapshotState>,
    render: Arc<mw_api_legacy::Renderer>,
}

impl Slots {
    /// The slots a process starts with: the four permanent ones, and nothing shared yet.
    pub(crate) fn new() -> Self {
        Self {
            legacy_routes: Arc::new(mw_api_legacy::RouteState::new()),
            ambient: Arc::new(mw_station::ambient::UploadState::new()),
            grpcrx: Arc::new(mw_grpc::grpcrx::ServiceState::new()),
            web: std::collections::BTreeMap::new(),
            registry: None,
            shared: None,
            epoch: 0,
        }
    }

    /// The epoch every spec's digest is stamped with.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Replace the station registry's snapshot. **After a commit only.**
    ///
    /// This is what makes a rotated ingest secret, a renamed station, a corrected altitude
    /// or a changed device type take effect without a restart -- `ConfigRegistry` holds its
    /// snapshot in an `ArcSwap` for exactly this, and `mw_station::config` already declines
    /// to restart a station for a secret rotation on the grounds that the registry is what
    /// compares it. Before this call that reasoning had nothing behind it.
    pub(crate) fn publish_registry(&self, snapshot: mw_store::RegistrySnapshot) {
        if let Some(registry) = &self.registry {
            registry.publish(snapshot);
        }
    }
}

/// Both directions of the pairing between what is mounted and what is planned, over tables
/// that have **not been mounted yet**.
///
/// [`assert_route_tables_match_the_plan`] asks the live `Mount` the same question and is the
/// boot sequence's. A re-plan cannot use it: the live mount still holds the previous
/// configuration's tables at the point the question has to be answered, which is before the
/// commit. Same assertion, same two defects, different subject.
pub(crate) fn assert_mounted_tables_match(
    mounted: &Mounted,
    specs: &[mw_component::ComponentSpec],
) -> anyhow::Result<()> {
    use mw_component::Capability;
    use std::collections::BTreeSet;

    let planned: BTreeSet<String> = specs
        .iter()
        .filter(|spec| {
            spec.provides
                .iter()
                .any(|p| p.capability == Capability::HttpRoutes)
        })
        .map(|spec| spec.id.as_str().to_string())
        .collect();
    // `http:listener` owns `/metrics` and provides nothing (§5.10), so it is mounted and
    // not planned by design.
    let owners: BTreeSet<String> = mounted
        .owners()
        .into_iter()
        .filter(|owner| owner != mw_http::ID)
        .collect();
    anyhow::ensure!(
        planned == owners,
        "the route tables and the planned route providers disagree: \
         mounted-not-planned {:?}, planned-not-mounted {:?}",
        owners.difference(&planned).collect::<Vec<_>>(),
        planned.difference(&owners).collect::<Vec<_>>()
    );
    Ok(())
}

/// Fold the shared root state's epoch into every component's `ConfigDigest`.
///
/// [`Slots`]' documentation has the argument. In one sentence: a component's digest covers
/// the configuration **its own builder** closes over, and the shared root state is
/// configuration it closes over that no registrar can see -- so the one place that can
/// account for it is the one place that owns it.
pub(crate) fn stamp(specs: &mut [mw_component::ComponentSpec], epoch: u64) {
    for spec in specs {
        spec.config_digest = mw_component::ConfigDigest::of(&(spec.config_digest.get(), epoch));
    }
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

/// Step 4e: build every route table, the host table, the certificate store and the one
/// router, and hand them to the configuration.
///
/// **Before `plan()`**, because two registrars plan from them. The ordering is forced for
/// `mw_api_legacy::RouteState`'s reason one level up: the tables come from crates
/// `mw-http` may not name, so the binary assembles them (§1.3 rule 1).
///
/// `render` is the metrics payload renderer -- `mw-obs`'s handle in the daemon, a stub in a
/// plan test. Taken as a closure rather than as `&mw_obs::Obs` because installing a
/// subscriber is a process-global act only one test per binary could perform, and what this
/// function needs from it is one `String`.
///
/// # Which tables are mounted, and the condition each one carries
///
/// | Table | Condition |
/// |---|---|
/// | `/metrics` | none -- §5.10 puts it on the one listener, so every node has it |
/// | the frozen REST surface | the same predicate `mw_api_legacy::register` plans on |
/// | the Ambient ingest endpoint | the same predicate `mw_station::register` plans on |
/// | the reading receiver's two methods | a `config.ingest` row with `listen = 'grpc'` |
/// | the asset tree, `/privacy`, `/support` | none |
/// | one per website | the same predicate `mw_web::register` plans on |
///
/// The two conditional ones ask the **registrar's own** predicate rather than a second
/// reading of the configuration, and [`assert_route_tables_match_the_plan`] checks after
/// `plan()` that the answers agreed. M4-2's code review found the other direction of that
/// mismatch -- a table mounted where no component was planned, answering a permanent
/// body-less `503` on five frozen paths -- so the pairing is asserted rather than arranged.
/// The route tables, the host table and the certificate store, built and not yet published.
///
/// Returned rather than mounted, because **M7-1** needs the two halves apart. A committed
/// configuration change has to swap the *live* `Mount` -- `http:listener` holds that `Arc`
/// and its digest covers only the socket set, so a second `Mount` would never be served --
/// and the swap has to happen **after** the transaction commits, or a `plan()` refusal would
/// leave the router serving tables for a configuration the database rolled back. So the
/// caller builds, validates by planning, commits, and only then swaps.
pub(crate) struct Mounted {
    tables: Vec<mw_http::RouteTable>,
    hosts: mw_http::HostTable,
    /// `None` when `listener.https_port` is not set, which is every node without TLS.
    certs: Option<mw_http::CertStore>,
    /// For the log line: how many hostnames the table resolves.
    hostnames: usize,
    /// The station registry's new snapshot, waiting to be published.
    ///
    /// Carried rather than published in place for the `Mount`'s reason exactly: publishing
    /// is a change to what the **running** daemon resolves, and a preview must not make one.
    /// `None` on the first build, where there was no registry to publish into and the
    /// snapshot went straight into it.
    registry: Option<mw_store::RegistrySnapshot>,
}

/// Build the route tables, the host table and the certificate store.
///
/// `slots` carries the per-surface state slots **across** a re-plan, and that is not an
/// optimisation. Each slot is the seam between a mounted route and the component that
/// answers it -- `api:legacy` installs its handler set into `RouteState` while it is
/// `Running`, which is what makes "the table is mounted" and "the component is running" one
/// fact (**D-48**). A re-plan that minted a fresh slot would point the swapped router at a
/// slot nobody has installed into, and every frozen path would answer `503` until the
/// component happened to restart for an unrelated reason. The website slots are keyed by
/// website id for the same reason, and not by position: `web_states[i]` and
/// `cfg.web_sites[i]` are paired by index, so inserting a website would otherwise hand one
/// site's component another site's slot.
fn build_route_tables(
    cfg: &mut Config,
    render: mw_http::Render,
    slots: &mut Slots,
    registry: Option<mw_store::RegistrySnapshot>,
) -> anyhow::Result<Mounted> {
    use mw_api_legacy::LegacyConfig as _;

    // Parsed here rather than inside the registrar: a bad `listener.listen_addr` is a
    // named startup failure, and `mw_http::ListenerConfig::listen` says so where it
    // `expect`s.
    let listen = cfg
        .listen()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("`listener.listen_addr` is not an address to listen on")?;

    // One slot per website, in id order -- **the same slot as last time for a website that
    // already had one**. `mw_web::register` refuses a count that does not match, which is
    // what stops one website's page being mounted under another's hostname.
    let states: Vec<Arc<mw_web::SiteState>> = cfg
        .web_sites
        .iter()
        .map(|site| {
            Arc::clone(
                slots
                    .web
                    .entry(site.id.get())
                    .or_insert_with(|| Arc::new(mw_web::SiteState::new())),
            )
        })
        .collect();
    // A website that has been deleted leaves a slot nothing mounts, which would otherwise
    // accumulate one entry per delete for the life of the process.
    let live: std::collections::BTreeSet<i64> = cfg.web_sites.iter().map(|s| s.id.get()).collect();
    slots.web.retain(|id, _| live.contains(id));
    cfg.set_web_states(states);

    let snapshot = cfg.snapshot.clone();
    let mut tables = vec![mw_http::metrics_table(mw_http::MetricsRoute::new(
        Arc::new(move || {
            // **M4-1's constraint 2, on the scrape path.** `mw_snapshot_age_seconds` and
            // `mw_snapshot_degraded` are written only when a snapshot is folded, so a
            // station that stops reporting freezes its gauge instead of crossing 600 --
            // the one transition worth alerting on. The sweep's natural home is the scrape
            // path, and this is it: every key's age is recomputed from the published table
            // immediately before the payload is rendered.
            if let Some(state) = &snapshot {
                mw_pipeline::metrics::sweep_snapshot_ages(state, chrono::Utc::now());
            }
            render()
        }),
        mw_obs::CONTENT_TYPE,
    ))];
    if cfg.has_latest_snapshot() {
        if let Some(state) = cfg.legacy_routes.as_ref().map(Arc::clone) {
            tables.push(mw_api_legacy::route_table(state)?);
        }
    }
    // The Ambient ingest endpoint: one route, however many consoles. Built from the
    // registrar's own predicate (`mw_station::ambient_mount`) rather than from a second
    // reading of the configuration, and `assert_route_tables_match_the_plan` checks after
    // `plan()` that the two agreed -- which is the pairing M4-2's review found broken in
    // the other direction on the frozen surface.
    if let Some(mount) = mw_station::ambient_mount(cfg) {
        let state = Arc::clone(&slots.ambient);
        tables.push(mw_http::upload_table(
            mw_component::ComponentId::new(mw_station::ambient::ID)
                .context("`station:ambient` is not a usable component id")?,
            &mount,
            // **The driver's own manifest**, not a second one built to match it. Rebuilding
            // it here meant the mounted table and the component's `Provision` were two
            // declarations of the same route, and `assert_route_tables_match_the_plan`
            // compares owner ids -- so a divergence in scope or methods between them would
            // not have been caught. `grpcrx` already shares one; this makes them agree the
            // same way.
            mw_station::ambient::manifest(&mount),
            state.endpoint(),
        ));
        cfg.set_ambient_upload(state);
    }
    // The reading receiver's two methods, on the same predicate `mw_grpc::grpcrx::register`
    // plans on: a `config.ingest` row with `listen = 'grpc'`. A node with no such row is a
    // satellite and mounts nothing, which is W7 D-44.
    if cfg
        .ingest
        .iter()
        .any(|row| row.listen == mw_grpc::IngestRow::GRPC)
    {
        let state = Arc::clone(&slots.grpcrx);
        tables.push(
            mw_grpc::grpcrx::route_table(Arc::clone(&state))
                .map_err(|e| anyhow::anyhow!("{e}"))
                .context("building the reading receiver's route table")?,
        );
        cfg.set_grpcrx_service(state);
    }
    tables.extend(mw_web::tables(cfg).context("building the website's route tables")?);

    // §6.3's host table, in configuration order -- which is id order, because
    // `config_db::websites` says `ORDER BY id`. The default is `controller.go:186`
    // reproduced: the first row, unless one has an empty hostname.
    let hosts = mw_http::HostTable::new(
        cfg.websites
            .iter()
            .map(|w| (w.hostname.clone(), mw_types::WebsiteId::new(w.id))),
    );
    let hostnames = hosts.len();

    // §6.4's certificate store, loaded only where there is a TLS port to serve. A
    // configured-but-unreadable path is a startup failure naming the path, never a silent
    // substitution of some other site's certificate.
    let paths = cfg.cert_paths();
    let sni = if listen.tls() {
        let fallback = cfg
            .websites
            .iter()
            .find(|w| w.hostname.is_empty())
            .or_else(|| cfg.websites.first())
            .map(|w| w.hostname.clone());
        Some(
            mw_http::CertStore::load(&paths, fallback.as_deref())
                .context("loading the configured TLS certificates")?,
        )
    } else {
        if !paths.is_empty() {
            // A certificate with nowhere to be served from is the kind of half-finished
            // configuration an operator should hear about once, at startup, rather than
            // discover when a browser cannot connect.
            warn!(
                certificates = paths.len(),
                "websites have TLS certificates configured and `listener.https_port` is \
                 not set, so nothing serves them; set the port or clear the paths"
            );
        }
        None
    };

    Ok(Mounted {
        tables,
        hosts,
        certs: sni,
        hostnames,
        registry,
    })
}

impl Mounted {
    /// The three pieces, for the caller that is about to publish them.
    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<mw_http::RouteTable>,
        mw_http::HostTable,
        Option<mw_http::CertStore>,
        Option<mw_store::RegistrySnapshot>,
    ) {
        (self.tables, self.hosts, self.certs, self.registry)
    }

    /// Which components own a mounted table, for the pairing assertion.
    pub(crate) fn owners(&self) -> std::collections::BTreeSet<String> {
        self.tables
            .iter()
            .map(|t| t.owner().as_str().to_string())
            .collect()
    }
}

/// Steps 4c to 4e of the boot sequence, as one function a re-plan can call.
///
/// The same assembly the daemon does at start, which is the point: `ConfigAdmin`'s save
/// path runs this against the configuration it is proposing, so a saved configuration is
/// assembled by the same code as a booted one rather than by a second implementation that
/// has to keep agreeing with it.
///
/// Returns the route tables **unpublished** -- see [`Mounted`] for why the two halves are
/// apart.
pub(crate) fn assemble(
    cfg: &mut Config,
    render: mw_http::Render,
    fanout: Option<&spool::Fanout>,
    slots: &mut Slots,
) -> anyhow::Result<Mounted> {
    let registry = attach_registry(cfg, slots).context("publishing the station registry")?;
    if let Some(fanout) = fanout {
        attach_ingress(cfg, fanout).context("assembling the reading pipeline")?;
    }
    build_route_tables(cfg, render, slots, registry).context("building the route tables")
}

/// `plan()`, with every spec's digest stamped with the shared root state's epoch.
///
/// The registrar loop is `mw_supervisor::plan::plan`'s, repeated here rather than called,
/// because the stamp has to happen **between** registration and validation: `validate`
/// orders the specs and builds the `Plan`, and a `Plan`'s digests are not reachable
/// afterwards. [`Slots`] has the argument for why the stamp exists at all.
pub(crate) fn plan_stamped(
    cfg: &Config,
    ctx: &PlanContext,
    epoch: u64,
) -> Result<mw_supervisor::plan::Plan, mw_component::PlanError> {
    let mut specs = Vec::new();
    for registrar in REGISTRARS {
        registrar
            .register(cfg, &mut specs)
            .map_err(|e| mw_component::PlanError::Registrar {
                registrar: registrar.name(),
                reason: e.to_string(),
            })?;
    }
    stamp(&mut specs, epoch);
    mw_supervisor::plan::validate(specs, ctx)
}

/// Build the route tables and publish them into a **new** `Mount` — the boot sequence's
/// step 4e.
fn attach_listener(
    cfg: &mut Config,
    render: mw_http::Render,
    slots: &mut Slots,
    registry: Option<mw_store::RegistrySnapshot>,
) -> anyhow::Result<()> {
    let listen = cfg
        .listen()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("`listener.listen_addr` is not an address to listen on")?;
    let built = build_route_tables(cfg, render, slots, registry)?;
    let hostnames = built.hostnames;
    let sni = built.certs.map(|store| {
        info!(
            certificates = store.len(),
            fallback = store.has_default(),
            "the SNI resolver is loaded"
        );
        Arc::new(mw_http::SniResolver::new(store))
    });
    let (mount, report) =
        mw_http::Mount::new(built.tables, built.hosts).context("mounting the route tables")?;
    info!(
        shared = report.shared,
        sites = report.sites,
        paths = report.paths,
        catch_all = report.catch_all.as_ref().map(|c| c.as_str()),
        hostnames,
        main = %listen.main(),
        grpc = %listen.grpc(),
        "route tables mounted"
    );
    cfg.set_listener(Arc::new(mount), sni);
    Ok(())
}

/// Both directions of the pairing between what is mounted and what is planned.
///
/// A mounted table whose owner was not planned answers a `503` for ever on paths nobody
/// will ever fill; a planned `HttpRoutes` provider with no mounted table reaches `Running`
/// while its paths 404. Neither is a state worth representing, and neither is visible
/// without this check: the mount happens before `plan()`, so the two are derived from the
/// same configuration by two functions that have to agree.
fn assert_route_tables_match_the_plan(
    cfg: &Config,
    specs: &[mw_component::ComponentSpec],
) -> anyhow::Result<()> {
    use mw_component::Capability;
    use std::collections::BTreeSet;

    let planned: BTreeSet<String> = specs
        .iter()
        .filter(|spec| {
            spec.provides
                .iter()
                .any(|p| p.capability == Capability::HttpRoutes)
        })
        .map(|spec| spec.id.as_str().to_string())
        .collect();
    // `http:listener` owns `/metrics` and provides nothing: §5.10 makes that route the
    // listener's own rather than a component's, so it is mounted and not planned, by
    // design. Every other owner must be in both sets.
    let mounted: BTreeSet<String> = cfg
        .mount
        .as_ref()
        .context("the route tables have not been mounted")?
        .owners()
        .into_iter()
        .filter(|owner| owner != mw_http::ID)
        .collect();
    anyhow::ensure!(
        planned == mounted,
        "the mounted route tables and the planned route providers disagree: \
         mounted-not-planned {:?}, planned-not-mounted {:?}",
        mounted.difference(&planned).collect::<Vec<_>>(),
        planned.difference(&mounted).collect::<Vec<_>>()
    );
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
    let mut conn = pool
        .acquire()
        .await
        .context("taking a connection to read the configuration")?;
    let mut cfg = Config::load(&mut conn)
        .await
        .context("reading the configuration")?;
    // Returned to the pool before anything else runs: every read above is done, and SQLite
    // in WAL mode has a bounded connection budget that a held handle spends for nothing.
    drop(conn);
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
    // `Arc`, because the spool outlives one plan: a re-plan rebuilds the `SpoolIngress`
    // over the **same** spool and the same registry, and a second `Spool::open` on a
    // directory this process already holds would be a second cursor over one log.
    let fanout = match &cfg.node {
        Some(node) => {
            Some(Arc::new(spool::open(node).with_context(|| {
                format!("opening the spool at {}", node.spool_path)
            })?))
        }
        None => {
            info!("no node identity, so no spool and no reading fan-out yet");
            None
        }
    };

    // Step 4c: identity resolution and the write end of the pipeline.
    mw_station::metrics::register();
    // The two series the rate-limited outputs own, described beside the others so that a
    // component that has sent nothing is distinguishable from one whose metrics are broken
    // (§5.10).
    mw_output::metrics::register();
    let mut slots = Slots::new();
    let registry =
        attach_registry(&mut cfg, &mut slots).context("publishing the station registry")?;
    if let Some(fanout) = &fanout {
        attach_ingress(&mut cfg, fanout).context("assembling the reading pipeline")?;
    }

    // Step 4e: the route tables, the host table, the certificate store and the one
    // router. **Before `plan()`**, because `http:listener` and every `web:site:<id>` plan
    // from them -- the same ordering the snapshot table and the frozen route state already
    // have, and for the same reason (§1.3 rule 1: the binary is where a consumer and a
    // provider meet).
    let metrics = obs.metrics();
    let render: mw_http::Render = Arc::new(move || metrics.render());
    attach_listener(&mut cfg, Arc::clone(&render), &mut slots, registry)
        .context("mounting the route tables")?;
    // Read before `slots` moves into the `ConfigAdmin`: the stamp below is the first plan's,
    // and every later one reads it back out of the admin's own copy.
    let epoch = slots.epoch();

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
    let root_set = roots.capabilities();

    // Step 5a, which is new with **M7-1**: the supervisor, before `plan()` rather than
    // after.
    //
    // The ordering is forced by what the management API is. `ComponentControl` and the plan
    // sink are both the supervisor's, and the management router holds the first and the
    // `ConfigAdmin` behind it holds the second -- while `http:listener`'s own
    // `ConfigDigest` covers the management socket, so the router has to be in the
    // configuration **before** `plan()` runs. Constructing the supervisor earlier costs
    // nothing: `Supervisor::new` publishes the two root capabilities and returns, and
    // nothing is started until `run`.
    let (supervisor, control, plans) = Supervisor::new(roots, shutdown.clone());
    let readiness = supervisor.readiness();

    // Step 5b: the management API, and the one thing in this process that can write
    // configuration (W2a §2.5: a write bumps the generation and the supervisor reconciles
    // the plan diff; nothing calls "reload").
    //
    // The first plan is handed to the `ConfigAdmin` below, so it is built here against a
    // configuration that already names the management socket -- and the `Arc` is cloned
    // rather than re-planned, because every diff this process takes is against *the plan the
    // supervisor is running*.
    let mgmt_sni = match cfg.management_cert() {
        Some(paths) => {
            let store = mw_http::CertStore::load(std::slice::from_ref(&paths), Some(""))
                .context("loading the management API's TLS certificate")?;
            Some(Arc::new(mw_http::SniResolver::new(store)))
        }
        None => None,
    };
    let admin = Arc::new(crate::admin::SqliteConfigAdmin::new(
        crate::admin::Shared {
            pool: pool.clone(),
            render: Arc::clone(&render),
            fanout: fanout.clone(),
            roots: root_set.clone(),
            plans,
            mount: Arc::clone(
                cfg.mount
                    .as_ref()
                    .context("the route tables have not been mounted")?,
            ),
            router: std::sync::OnceLock::new(),
        },
        slots,
        cfg.sni.clone(),
        mgmt_sni.clone(),
    ));
    let mgmt_router = mw_api_mgmt::router(Arc::new(mw_api_mgmt::Mgmt::new(
        Arc::clone(&admin) as Arc<dyn mw_api_mgmt::ConfigAdmin>,
        Arc::clone(&control),
        Arc::new(crate::hostinfo::Rings::new(obs.rings())),
        Arc::new(crate::hostinfo::Host::new()),
    )));
    admin.install_router(mgmt_router.clone());
    cfg.set_management(mw_http::Management {
        addr: cfg
            .management_addr()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("`listener.management_listen_addr` is not an address to listen on")?,
        router: mgmt_router,
        tls: mgmt_sni,
    });

    // Step 5. Pure: every structural error in W2a Decision 2's list is caught here,
    // before anything is constructed.
    notify::starting("planning the configuration");
    let ctx = PlanContext::new(cfg.generation).with_roots(root_set);
    let plan = plan_stamped(&cfg, &ctx, epoch).context("planning the configuration")?;
    info!(
        components = plan.len(),
        generation = %cfg.generation,
        management = %cfg
            .mgmt
            .as_ref()
            .map(|m| m.addr.to_string())
            .unwrap_or_else(|| "-".to_string()),
        "plan validated"
    );
    admin.adopt(&plan).await;

    // **Every mounted route table has a planned owner, and every planned route provider
    // has a mounted table.** The mount happens before `plan()` now, so the pairing that
    // M4-2's review found broken in the other direction -- a table mounted on a node where
    // `register` planned no component, which then answered a permanent body-less `503` on
    // five frozen paths -- has to be asserted rather than arranged. Both directions, because
    // each one is a different defect: a table with no owner is that `503`, and an owner with
    // no table is a component that reaches `Running` while its paths 404.
    assert_route_tables_match_the_plan(&cfg, plan.specs())?;

    let (report, (), ()) = tokio::join!(
        supervisor.run(plan),
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
    // `Generation` and `plan` are the tests' alone now: `run` carries the generation on the
    // configuration and plans through `plan_stamped`, so importing them at the module level
    // would be a dead import in the daemon's own build.
    use mw_component::Generation;
    use mw_supervisor::plan::plan;

    use super::*;
    use mw_store::config_db::{Listener, WeatherDatabase};
    use secrecy::SecretString;

    /// A hand-built configuration, with the two root resources `attach_registry` gives a
    /// real one.
    ///
    /// Without them `plan()` refuses: `api:legacy` is planned unconditionally and needs
    /// both the route state slot and the DTO's serialiser, and a configuration that has
    /// neither is a boot sequence that has not reached step 4c rather than a shape worth
    /// planning (`mw_api_legacy::LegacyPlanError`). That refusal has its own test in that
    /// crate; here it would only mean every plan assertion below asserted the refusal.
    fn config(with_database: bool) -> Config {
        let mut cfg = Config {
            declared_uploaders: Vec::new(),
            services: Vec::new(),
            aeris_account: None,
            declared_aeris: Vec::new(),
            stations: Vec::new(),
            declared_stations: Vec::new(),
            websites: Vec::new(),
            legacy_websites: Vec::new(),
            legacy_stations: Vec::new(),
            ingest: Vec::new(),
            declared_ingest: Vec::new(),
            legacy_routes: None,
            ambient_upload: None,
            grpcrx_service: None,
            legacy_render: None,
            snapshot_render: None,
            ingress: None,
            registry: None,
            rollup: None,
            snapshot: None,
            snapshot_windows: None,
            station_zones: Vec::new(),
            web_sites: Vec::new(),
            web_states: None,
            mount: None,
            sni: None,
            mgmt: None,
            listener: Listener {
                listen_addr: "127.0.0.1".into(),
                http_port: 8080,
                https_port: None,
                grpc_port: 50051,
                management_listen_addr: "127.0.0.1".into(),
                management_port: 8081,
                management_tls_cert_path: None,
                management_tls_key_path: None,
            },
            weather_database: with_database.then(|| WeatherDatabase {
                url: SecretString::from("postgres://mw:hunter2@db.internal/weather".to_string()),
            }),
            node: None,
            generation: Generation::ZERO,
        };
        // The three root resources `attach_registry` builds, over this fixture's empty
        // station and website sets. The snapshot's key set is therefore empty, which is one
        // of `mw_pipeline::snapshot::register`'s four reasons to plan neither component --
        // so the plans below are about the pool and the writer, which is what these tests
        // are for.
        cfg.set_rollup(Arc::new(mw_pipeline::RollupState::new(Vec::new())));
        cfg.set_snapshot(Arc::new(mw_pipeline::SnapshotState::new(
            Vec::new(),
            Vec::new(),
        )));
        cfg.set_legacy(
            Arc::new(mw_api_legacy::RouteState::new()),
            Arc::new(mw_api_legacy::Renderer::default()),
        );
        cfg
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
        let mut cfg = Config::load(&mut pool.acquire().await.expect("a connection"))
            .await
            .expect("the configuration reads");
        let node = cfg.node.clone().expect("the node row was inserted");
        let fanout = spool::open(&node).expect("the spool opens");
        let mut slots = Slots::new();
        let _registry = attach_registry(&mut cfg, &mut slots).expect("the registry publishes");
        attach_ingress(&mut cfg, &fanout).expect("the pipeline assembles");
        // Step 4e, which `plan()` needs: `http:listener` and every `web:site:<id>` plan
        // from the mounted router and the per-website slots.
        attach_listener(&mut cfg, Arc::new(String::new), &mut slots, None)
            .expect("the route tables mount");
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
                // The frozen REST surface, which is planned unconditionally: a node with
                // no `website` row serves the contract's `503` from the handlers and not
                // a `404` from a route that is not there (`mw_api_legacy::register`).
                "api:legacy",
                // The one listener, planned unconditionally: §5.10 puts `/metrics` on it
                // and the schema seeds a `listener` row, so every node has one.
                "http:listener",
                // And the snapshot it reads, which needs a reading stream and not a
                // database -- this node has a spool and no `weather_database` row, so
                // `snapshot:warm` is absent and `snapshot:latest` is not.
                "snapshot:latest",
                "station:wll:suncrest:roof:rearm",
                &format!("station:wll:udp:{port}")[..],
                // The fixture's one `website` row, and the asset tree every node serves.
                "web:site:1",
                "web:static",
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
        // **The snapshot components are planned, and the renderer is the only reason they
        // were not.** `snapshot:latest` and `snapshot:warm` are implementation-plan.md
        // M4-1's and were complete; what they needed and did not have was the payload
        // renderer, which is the frozen 133-key DTO's serialiser and belongs exclusively to
        // `mw-api-legacy`. `attach_registry` now builds one, so
        // `Config::snapshot_render` answers `Some` and `mw_pipeline::snapshot::register`
        // plans both.
        //
        // The fixture has a `website` row and a station row, so the key set is non-empty
        // and `register`'s other three conditions are satisfied -- which is what makes this
        // an assertion about the renderer rather than about the key set.
        assert!(
            !cfg.websites.is_empty() && !cfg.stations.is_empty(),
            "the key set must be non-empty, or this asserts the wrong thing: {} websites, \
             {} stations",
            cfg.websites.len(),
            cfg.stations.len()
        );
        assert!(
            ids.contains(&"snapshot:latest") && ids.contains(&"snapshot:warm"),
            "both snapshot components plan once the renderer exists: got {ids:?}"
        );
        // And the surface that reads them, planned unconditionally.
        assert!(ids.contains(&"api:legacy"), "got {ids:?}");
        // The station's listener, its re-arm (the fixture's station has a
        // `transport_host`), the pool, the writer, the rollup, the rollover, the two
        // snapshot components, `api:legacy`, the **one** listener, the website and the
        // asset tree.
        assert_eq!(ids.len(), 12, "got {ids:?}");
        assert!(
            ids.contains(&"http:listener") && ids.contains(&"web:static"),
            "both are planned unconditionally: got {ids:?}"
        );
        assert!(
            ids.iter().any(|id| id.starts_with("station:wll:udp:")),
            "got {ids:?}"
        );
        pool.close().await;
    }

    /// **Every window the three encoders declare is one `window:rollup` can resolve.**
    ///
    /// The half of that check `mw-output` cannot make: the ring's span and the maintained
    /// set are `mw-pipeline`'s constants, and this is the only crate that may name both
    /// (`ci/layers.toml` puts them in the same layer, so neither may name the other). The
    /// registry half — "does `metrics/registry.toml` declare this strategy for this metric"
    /// — is asserted inside `mw-output`.
    ///
    /// Why it is worth a test at all: `RollupState::one` routes a window shorter than the
    /// ring to the ring and everything else to the maintained set, which refuses anything
    /// but `Agg::Sum`. A declaration that falls between the two answers
    /// `WindowError::TooLong`, which classifies **`Fatal`** — so the component fails every
    /// occurrence, forever, and no test with a stubbed `WindowedView` can see it.
    #[test]
    fn every_declared_window_is_one_the_rollup_can_resolve() {
        use mw_output::encoder::{cwop, pws, wu};
        use mw_pipeline::rollup::ring::RING_SPAN;
        use mw_pipeline::rollup::state::MAINTAINED;
        use mw_types::Agg;

        for table in [cwop::REQUIRED, wu::REQUIRED, pws::REQUIRED] {
            for (metric, window, agg) in table.iter().copied() {
                match window.length() {
                    // Inside the ring: every strategy resolves, from bucket state.
                    Some(length) if length <= RING_SPAN => {}
                    // Outside it: the window must be one the rollup maintains, and a
                    // maintained window resolves only a total.
                    _ => {
                        assert!(
                            MAINTAINED.contains(&window),
                            "`({metric:?}, {window}, {agg})` is longer than the \
                             {}-second ring and is not in the rollup's maintained set",
                            RING_SPAN.as_secs()
                        );
                        assert_eq!(
                            agg,
                            Agg::Sum,
                            "`({metric:?}, {window})` is maintained, and \
                             `RollupState::maintained` resolves only `Agg::Sum`"
                        );
                    }
                }
            }
        }
    }

    /// **Every station the rollup holds has a resolved time zone**, which is what makes the
    /// three calendar-window declarations resolvable rather than `Fatal`.
    ///
    /// The invariant is real and is enforced three crates away, so it is worth pinning
    /// here. `WindowedView::resolve` answers `WindowError::NoZone` — `Fatal` — for a
    /// `Window::Day`, `Month` or `Year` declaration against a station with no zone, and all
    /// three encoder tables declare at least one. What stops that reaching a running
    /// uploader is that a *collecting* station with no usable zone makes `plan()` refuse the
    /// whole configuration: `rollup_stations` hands the unresolved `None` to
    /// `rain:rollover:<station>`'s `TzSource`, and `mw_supervisor::plan` answers
    /// `PlanError::MissingTimeZone`. (An unparseable zone is caught earlier still, by
    /// `attach_registry`.)
    #[tokio::test(flavor = "current_thread")]
    async fn a_collecting_station_with_no_time_zone_refuses_the_whole_configuration() {
        let scratch = Scratch::new("nozone");
        // The beaconing station, with its `timezone` column left NULL -- which
        // `mw_store::registry` treats as absent rather than as a problem, so startup
        // proceeds and `plan()` is where it is caught.
        let station = "INSERT INTO stations (station_key, owning_node_id, name, type,
                                             identity_kind, transport_kind, transport_dev,
                                             transport_baud, latitude, longitude)
                       VALUES ('backyard', x'07070707070707070707070707070707', 'backyard',
                               'davis', 'fixed', 'serial', '/dev/ttyUSB0', 19200,
                               45.1, -122.05)"
            .to_string();
        let pool = configure(&scratch, &[station]).await;
        let mut cfg = Config::load(&mut pool.acquire().await.expect("a connection"))
            .await
            .expect("the configuration reads");
        let node = cfg.node.clone().expect("the node row was inserted");
        let fanout = spool::open(&node).expect("the spool opens");
        let mut slots = Slots::new();
        let _registry =
            attach_registry(&mut cfg, &mut slots).expect("a blank zone is not a registry problem");
        attach_ingress(&mut cfg, &fanout).expect("the pipeline assembles");
        attach_listener(&mut cfg, Arc::new(String::new), &mut slots, None)
            .expect("the route tables mount");
        cfg.weather_database = Some(config_db::WeatherDatabase {
            url: secrecy::SecretString::from(
                "postgresql://mw@127.0.0.1:5432/modernweather".to_string(),
            ),
        });

        let roots = Roots {
            schedule: Arc::new(mw_supervisor::testing::MemoryScheduleStore::new()),
            stream: Some(Arc::clone(&fanout.stream) as Arc<dyn mw_component::ReadingStream>),
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let e = plan(&cfg, REGISTRARS, &ctx).expect_err("a station with no zone");
        let text = mw_types::DisplayChain(&e).to_string();
        assert!(text.contains("backyard"), "{text}");
        assert!(
            text.contains("time zone") || text.contains("timezone"),
            "{text}"
        );
        pool.close().await;
    }

    /// A station with coordinates and a callsign, which is what an APRS uploader needs.
    ///
    /// `timezone` is set for the reason [`broadcast_station`] records, and the three APRS
    /// columns are on the **station** row rather than the uploader because the callsign is
    /// public identity and the position is where the station is (**D-66**).
    fn beaconing_station(key: &str) -> String {
        format!(
            "INSERT INTO stations (station_key, owning_node_id, name, type, identity_kind,
                                   transport_kind, transport_dev, transport_baud,
                                   latitude, longitude, aprs_callsign, timezone)
             VALUES ('{key}', x'07070707070707070707070707070707', '{key}',
                     'davis', 'fixed', 'serial', '/dev/ttyUSB0', 19200,
                     45.1, -122.05, 'N0CALL', 'America/Denver')"
        )
    }

    /// **The rate-limited outputs plan, over the rollup's own `WindowedView`.**
    ///
    /// End to end through the real configuration database, because the half this covers is
    /// the wiring no unit test in `mw-output` can reach: `mw_store::config_db::uploaders`'
    /// three-table join, `declare_uploader`'s column-into-field copy, and the fact that
    /// `Config::set_rollup` is what both `OutputConfig::windowed_view` and
    /// `SnapshotConfig::snapshot_windows` answer from — so an uploader and `/latest` cannot
    /// disagree about what the five-minute wind average is.
    #[tokio::test(flavor = "current_thread")]
    async fn an_enabled_uploader_plans_a_component_over_the_rollups_own_view() {
        let scratch = Scratch::new("uploaders");
        let pool = configure(&scratch, &[beaconing_station("backyard")]).await;
        for sql in [
            "INSERT INTO uploader (id, kind, name, enabled, api_key, interval_seconds)
             VALUES (1, 'wunderground', 'backyard', 1, 'hunter2', 300)",
            "INSERT INTO station_service (station_key, service, uploader_id,
                                          remote_station_id)
             VALUES ('backyard', 'wunderground', 1, 'KWABACKYARD')",
            // A second uploader for the same station needs its own `uploader.name`: the
            // column is UNIQUE across kinds, because it is the component id's suffix and
            // `wu:backyard` has to survive a rename of anything else.
            //
            // A KISS beacon at fifteen minutes, which is above the RF floor O-8 fixed at
            // 600 s -- and at the Go default of 300 s this row would be a plan error.
            "INSERT INTO uploader (id, kind, name, enabled, interval_seconds,
                                   aprs_transport, aprs_kiss_tcp_address)
             VALUES (2, 'aprs', 'backyard-rf', 1, 900, 'kiss-tcp', 'tnc.lan:8001')",
            "INSERT INTO station_service (station_key, service, uploader_id)
             VALUES ('backyard', 'aprs', 2)",
            // Disabled, which is W5 §1.4's "Stop sharing with PWS Weather": the plan must
            // not contain it.
            "INSERT INTO uploader (id, kind, name, enabled, api_key)
             VALUES (3, 'pwsweather', 'backyard-pws', 0, 'hunter3')",
            "INSERT INTO station_service (station_key, service, uploader_id,
                                          remote_station_id)
             VALUES ('backyard', 'pwsweather', 3, 'BACKYARD')",
        ] {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the schema rejected a row: {e}\n{sql}"));
        }

        let mut cfg = Config::load(&mut pool.acquire().await.expect("a connection"))
            .await
            .expect("the configuration reads");
        assert_eq!(
            cfg.declared_uploaders.len(),
            2,
            "the disabled row is not read at all"
        );
        let node = cfg.node.clone().expect("the node row was inserted");
        let fanout = spool::open(&node).expect("the spool opens");
        let mut slots = Slots::new();
        let _registry = attach_registry(&mut cfg, &mut slots).expect("the registry publishes");
        attach_ingress(&mut cfg, &fanout).expect("the pipeline assembles");
        attach_listener(&mut cfg, Arc::new(String::new), &mut slots, None)
            .expect("the route tables mount");
        // The `weather_database` row, without which there is no `db:postgres`, no
        // `window:rollup` and therefore nothing to upload (**D-44**). Set on the loaded
        // configuration rather than inserted, for the reason
        // `a_weather_database_and_a_spool_plan_the_pool_and_the_writer` records.
        cfg.weather_database = Some(config_db::WeatherDatabase {
            url: secrecy::SecretString::from(
                "postgresql://mw@127.0.0.1:5432/modernweather".to_string(),
            ),
        });

        let roots = Roots {
            schedule: Arc::new(mw_supervisor::testing::MemoryScheduleStore::new()),
            stream: Some(Arc::clone(&fanout.stream) as Arc<dyn mw_component::ReadingStream>),
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx).expect("the uploaders plan");
        let ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        // The provider, and the two consumers of it this item adds.
        assert!(ids.contains(&"window:rollup"), "got {ids:?}");
        assert!(ids.contains(&"wu:backyard"), "got {ids:?}");
        assert!(ids.contains(&"aprs:backyard-rf"), "got {ids:?}");
        assert!(
            !ids.contains(&"pws:backyard-pws"),
            "a disabled uploader is not in the plan: got {ids:?}"
        );
        pool.close().await;
    }

    /// **A node that collects for nothing plans no uploader, and still starts.**
    ///
    /// The other direction of `OutputConfig::windowed_view`'s condition, and the one that
    /// would be a refusal rather than an absence if it were got wrong: the uploaders declare
    /// `WindowedView` as a hard need, `window:rollup` is not planned for a node with no
    /// collecting station, and a hard need nobody provides refuses the **whole**
    /// configuration (**D-44**) — so the daemon would not start at all.
    #[tokio::test(flavor = "current_thread")]
    async fn an_uploader_on_a_node_with_no_rollup_is_absent_rather_than_a_refusal() {
        let mut cfg = config(true);
        cfg.registry = Some(Arc::new(mw_store::ConfigRegistry::new(
            mw_store::RegistrySnapshot::default(),
        )));
        // `config(true)` has a weather database and no station rows, so
        // `rollup_stations()` is empty and `window:rollup` is not planned.
        assert!(mw_pipeline::RollupConfig::rollup_stations(&cfg).is_empty());
        cfg.declared_uploaders = vec![mw_output::UploaderRow {
            kind: "wunderground".to_string(),
            name: "backyard".to_string(),
            interval_seconds: None,
            api_endpoint: None,
            api_key: Some(secrecy::SecretString::from("hunter2")),
            station_key: Some("backyard".to_string()),
            remote_station_id: Some("KWABACKYARD".to_string()),
            latitude: None,
            longitude: None,
            aprs_callsign: None,
            station_enabled: Some(true),
            station_transport_kind: Some("serial".to_string()),
            aprs_passcode: None,
            aprs_symbol_table: None,
            aprs_symbol_code: None,
            aprs_comment: None,
            aprs_server: None,
            aprs_transport: None,
            aprs_kiss_serial_device: None,
            aprs_kiss_serial_baud: None,
            aprs_kiss_tcp_address: None,
            aprs_kiss_path: None,
            aprs_kiss_destination: None,
        }];
        let mut slots = Slots::new();
        attach_listener(&mut cfg, Arc::new(String::new), &mut slots, None)
            .expect("the route tables mount");

        let roots = Roots {
            schedule: Arc::new(mw_supervisor::testing::MemoryScheduleStore::new()),
            stream: None,
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx)
            .expect("a node with an uploader and no rollup still starts");
        let ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        assert!(!ids.contains(&"window:rollup"), "got {ids:?}");
        assert!(!ids.contains(&"wu:backyard"), "got {ids:?}");
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
        let mut slots = Slots::new();
        attach_listener(&mut cfg, Arc::new(String::new), &mut slots, None)
            .expect("the route tables mount");

        let roots = Roots {
            schedule: Arc::new(mw_supervisor::testing::MemoryScheduleStore::new()),
            stream: None,
            log: Arc::new(mw_supervisor::testing::MemoryLogControl::new("info")),
        };
        let ctx = PlanContext::new(Generation::ZERO).with_roots(roots.capabilities());
        let planned = plan(&cfg, REGISTRARS, &ctx).expect("a node that serves but does not ingest");
        let mut ids: Vec<&str> = planned
            .ids()
            .map(mw_component::ComponentId::as_str)
            .collect();
        ids.sort_unstable();
        // `writer:timescale` is absent, which is this test's subject. Neither snapshot
        // component is planned either, and for a different reason: this fixture has no
        // website and no station, so the `(website, station)` key set is empty and there
        // is nothing to fold into (`mw_pipeline::snapshot::register`). `api:legacy` goes
        // with them, because its hard `LatestSnapshot` would then be a need nobody
        // provides and `plan()` refuses the whole configuration for one -- and so does
        // every `web:site:<id>`, which declares the same hard need (§2.6).
        //
        // What **is** planned is the one listener and the asset tree, neither of which
        // needs anything: a node in this shape still serves `/metrics`, `/privacy` and
        // `/support`, which is §6.3's own asymmetry.
        assert_eq!(
            ids,
            vec!["db:postgres", "http:listener", "web:static"],
            "got {ids:?}"
        );
    }

    /// No weather database: neither component, and that is not an error (**D-44**).
    /// **The pairing tripwire, in both directions.**
    ///
    /// M4-3's code review found it untested while `docs/implementation-plan.md` advertised
    /// it as half of M4-1's constraint 1 being discharged -- which is exactly the shape
    /// `docs/reconciliation.md` G-1 records. Each direction is a different defect: a
    /// mounted table whose owner was not planned answers a `503` for ever on paths nobody
    /// will fill, and a planned provider with no mounted table reaches `Running` while its
    /// paths 404.
    #[test]
    fn the_mounted_tables_and_the_planned_providers_must_agree() {
        use mw_component::{Methods, RouteEntry, RouteManifest, Scope};

        let table = |owner: &str| {
            mw_http::RouteTable::new(
                mw_component::ComponentId::new(owner).expect("a legal id"),
                RouteManifest::new(
                    Scope::Any,
                    vec![RouteEntry::exact(format!("/{owner}"), Methods::READ)],
                ),
                axum::Router::new(),
            )
        };
        let mount = |owners: &[&str]| {
            let (mount, _) = mw_http::Mount::new(
                owners.iter().map(|o| table(o)).collect(),
                mw_http::HostTable::default(),
            )
            .expect("the tables mount");
            Arc::new(mount)
        };
        let planned = |owners: &[&str]| {
            owners
                .iter()
                .map(|owner| {
                    mw_component::ComponentSpec::new(
                        mw_component::ComponentId::new(*owner).expect("a legal id"),
                        mw_component::Trigger::Service,
                        mw_component::Builder::serve(|_| async { Ok(Stub) }),
                    )
                    .provides([mw_component::Provision::routes(
                        RouteManifest::new(
                            Scope::Any,
                            vec![RouteEntry::exact(format!("/{owner}"), Methods::READ)],
                        ),
                    )])
                })
                .collect::<Vec<_>>()
        };
        // The agreeing case, which is what the daemon does.
        let mut cfg = config(false);
        cfg.mount = Some(mount(&["a:one", "b:two"]));
        let agreeing = planned(&["a:one", "b:two"]);
        assert_route_tables_match_the_plan(&cfg, &agreeing).expect("they agree");

        // Mounted, not planned: the permanent `503`.
        let only_planned = planned(&["a:one"]);
        let err = assert_route_tables_match_the_plan(&cfg, &only_planned)
            .expect_err("`b:two` is mounted and nothing fills it");
        let text = format!("{err:#}");
        assert!(text.contains("b:two"), "{text}");
        assert!(text.contains("mounted-not-planned"), "{text}");

        // Planned, not mounted: a component that runs while its paths 404.
        cfg.mount = Some(mount(&["a:one"]));
        let err = assert_route_tables_match_the_plan(&cfg, &agreeing)
            .expect_err("`b:two` is planned and has no table");
        let text = format!("{err:#}");
        assert!(text.contains("b:two"), "{text}");
        assert!(text.contains("planned-not-mounted"), "{text}");

        // And the listener itself is mounted and never planned, by design (§5.10).
        cfg.mount = Some(mount(&["a:one", "b:two", mw_http::ID]));
        assert_route_tables_match_the_plan(&cfg, &agreeing)
            .expect("`http:listener` owns `/metrics` and provides nothing");
    }

    /// A `Serve` body that does nothing, for the specs above. They are never built.
    #[derive(Debug)]
    struct Stub;

    impl mw_component::Serve for Stub {
        fn serve(
            self: Box<Self>,
            cx: mw_component::Cx,
        ) -> mw_component::BoxFuture<'static, Result<(), mw_component::ComponentError>> {
            Box::pin(async move {
                cx.cancelled().await;
                Ok(())
            })
        }
    }

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
                "api:legacy",
                "http:listener",
                "snapshot:latest",
                "station:wll:suncrest:barn:rearm",
                "station:wll:suncrest:field",
                "station:wll:suncrest:roof:rearm",
                &format!("station:wll:udp:{port}")[..],
                "web:site:1",
                "web:static",
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
        let cfg = Config::load(&mut pool.acquire().await.expect("a connection"))
            .await
            .expect("the configuration reads");
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
        let reloaded = Config::load(&mut pool.acquire().await.expect("a connection"))
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
