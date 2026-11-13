//! `ConfigAdmin`, implemented here because here is the only place all three halves may be
//! named.
//!
//! A committed configuration change needs SQL (`mw-store`), the pure `plan()`
//! (`mw-supervisor`) and the configuration *type* (this crate), and `ci/layers.toml` forbids
//! those three from naming each other -- `plan()` is generic over the configuration
//! precisely so the supervisor cannot know that a station exists. So the management API
//! declares the trait it needs ([`mw_api_mgmt::ConfigAdmin`]) and the binary implements it,
//! which is docs/service-architecture.md §1.3 rule 1 doing the job it was written for.
//!
//! # The whole of a save, in order
//!
//! ```text
//!   BEGIN                                   one write at a time (`writes`)
//!     snapshot()                            the configuration this write replaces
//!     apply(change)                         mw_store::config_admin, CHECKs and all
//!     Config::load(&mut tx)                 ← reads the PROPOSED rows, uncommitted
//!     assemble()                            registry, rollup, snapshot table, route tables
//!     plan() + stamp()                      pure; every structural error caught here
//!     diff(live, proposed)                  start / stop / restart / unchanged
//!   COMMIT  (or ROLLBACK for a preview)
//!     mount.swap(tables)                    only after the commit
//!     plans.apply(plan)                     the supervisor reconciles the diff
//! ```
//!
//! **A preview is the same function with `Commit::Preview`**, and the only difference is the
//! last three lines. That is what makes implementation-plan.md **M7-1**'s "a save's preview
//! names exactly the components that then restart" a property of the code rather than an
//! agreement two paths have to keep: there is one path.
//!
//! # Two orderings that are load-bearing
//!
//! **The live `Mount` is swapped after the commit, never before.** `http:listener` holds
//! that `Arc` and its digest covers only the socket set, so a second `Mount` would never be
//! served -- but swapping before `plan()` has validated would leave the router answering for
//! a configuration the database rolled back, which is the mounted-table-with-no-owner defect
//! `assert_route_tables_match_the_plan` exists to catch.
//!
//! **The generation is bumped before `plan()` runs, not after the commit.** A component's
//! `applied_generation` is the generation its *running instance was built from* (**D-53**),
//! so the plan the supervisor applies has to carry the new one or every restarted component
//! would report the old one and W5 §2.2's comparison would never come true.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mw_api_mgmt::admin::{Applied, Awaiting, Commit, ConfigAdmin, PlanOutline};
use mw_api_mgmt::error::ApiError;
use mw_api_mgmt::view::{ConfigView, Named};
use mw_api_mgmt::Vocabulary;
use mw_component::{BoxFuture, Generation};
use mw_supervisor::plan::{Plan, PlanContext, PlanDiff, RootSet};
use mw_supervisor::PlanSink;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::boot::{self, Slots};
use crate::spool::Fanout;
use crate::view;

/// Everything a re-plan needs that `main` owns.
pub struct Shared {
    /// The configuration database. **D-41** makes the pool `main`'s.
    pub pool: SqlitePool,
    /// `/metrics`' renderer, which every route-table build needs.
    pub render: mw_http::Render,
    /// The spool and the fan-out, for a node with an identity (W7 §8.2).
    pub fanout: Option<Arc<Fanout>>,
    /// The root capabilities `plan()` validates an `OnReading` component against.
    pub roots: RootSet,
    /// How a new plan reaches the running supervisor.
    pub plans: PlanSink,
    /// The **live** mounted router set, swapped in place after a commit.
    pub mount: Arc<mw_http::Mount>,
    /// The management API's router.
    ///
    /// Installed after construction rather than passed in, because the two are circular: the
    /// router holds this `ConfigAdmin`, and this needs the router in order to put the
    /// management socket back into every re-plan's configuration. A `OnceLock` rather than a
    /// `Mutex` because it is written once, by `main`, before anything can read it.
    pub router: std::sync::OnceLock<axum::Router>,
}

/// The shape of the plan in force: every component's id against its declarative
/// fingerprint.
///
/// A fingerprint map rather than the `Plan` itself, because a `Plan` holds a `Builder` per
/// component -- a boxed closure over the configuration -- and is deliberately not `Clone`.
/// The diff needs only what `mw_supervisor::plan::diff` reads: the ids and the fingerprints.
/// Keeping those means the plan can be handed to the supervisor by value, which is the only
/// way `PlanSink` takes one.
#[derive(Debug, Default)]
struct Shape(std::collections::BTreeMap<mw_component::ComponentId, mw_component::SpecFingerprint>);

impl Shape {
    fn of(plan: &Plan) -> Self {
        Self(
            plan.specs()
                .iter()
                .map(|s| (s.id.clone(), s.fingerprint()))
                .collect(),
        )
    }

    /// `mw_supervisor::plan::diff`, against a shape rather than against a plan.
    ///
    /// The same four buckets and the same rule in each: in the new plan and not the old is
    /// `added`, in both with a different fingerprint is `changed`, in both and identical is
    /// `unchanged` and **is not touched**, and in the old and not the new is `removed`.
    fn diff(&self, new: &Plan) -> PlanDiff {
        let mut out = PlanDiff::default();
        for spec in new.specs() {
            match self.0.get(&spec.id) {
                None => out.added.push(spec.id.clone()),
                Some(before) if *before == spec.fingerprint() => {
                    out.unchanged.push(spec.id.clone());
                }
                Some(_) => out.changed.push(spec.id.clone()),
            }
        }
        let after: std::collections::BTreeSet<&mw_component::ComponentId> =
            new.specs().iter().map(|s| &s.id).collect();
        for id in self.0.keys() {
            if !after.contains(id) {
                out.removed.push(id.clone());
            }
        }
        out
    }
}

/// The parts a write takes a lock over.
struct Writable {
    /// The root resources that outlive one plan, and the epoch that restarts everything.
    slots: Slots,
    /// The shape of the plan in force, for the diff.
    shape: Shape,
    /// The configuration generation of [`Writable::plan`]'s previous committed state, and
    /// the snapshot that would restore it.
    ///
    /// `None` before the first save, and after a revert: the scope is **one step**
    /// (W5 §2.4), so reverting consumes it rather than walking further back. Full
    /// configuration history is **OQ-W5-5**.
    previous: Option<Previous>,
    /// The management API's certificate resolver, when one is configured. Swapped in place
    /// for the public resolver's reason.
    mgmt_sni: Option<Arc<mw_http::SniResolver>>,
    /// The public SNI resolver, swapped in place after a commit.
    sni: Option<Arc<mw_http::SniResolver>>,
}

/// One step back.
struct Previous {
    /// The generation this would restore.
    generation: u64,
    /// The configuration as it was, as the changes that recreate it.
    snapshot: Vec<mw_store::Change>,
}

/// `ConfigAdmin` over the configuration database, the plan and the supervisor.
pub struct SqliteConfigAdmin {
    shared: Shared,
    /// The newest committed generation (**D-53**).
    ///
    /// Separate from [`Writable`] and atomic, because `ConfigAdmin::generation` is read by
    /// every runtime response and must not queue behind a save.
    generation: AtomicU64,
    /// What the newest committed change is still waiting for.
    ///
    /// Beside the generation rather than inside [`Writable`] for the same reason: §2.4's
    /// banner polls it while a save may still be in flight, and a read that queued behind
    /// the write it is reporting on would stall exactly when the operator is watching.
    awaiting: std::sync::Mutex<Arc<Awaiting>>,
    /// One write at a time. A `tokio` mutex rather than a `std` one because the critical
    /// section awaits: it holds a transaction open across a read of the whole configuration.
    writable: tokio::sync::Mutex<Writable>,
}

impl std::fmt::Debug for SqliteConfigAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteConfigAdmin")
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SqliteConfigAdmin {
    /// Assemble it over the state `main` has already built.
    ///
    /// `slots`, `plan` and the two resolvers are what the boot sequence produced, handed over
    /// rather than rebuilt: the first plan is the one every diff is taken against, and the
    /// slots are the seams every mounted route already points at.
    pub fn new(
        shared: Shared,
        slots: Slots,
        sni: Option<Arc<mw_http::SniResolver>>,
        mgmt_sni: Option<Arc<mw_http::SniResolver>>,
    ) -> Self {
        Self {
            shared,
            generation: AtomicU64::new(0),
            awaiting: std::sync::Mutex::new(Arc::new(Awaiting::default())),
            writable: tokio::sync::Mutex::new(Writable {
                slots,
                shape: Shape::default(),
                previous: None,
                mgmt_sni,
                sni,
            }),
        }
    }

    /// Install the management router. Called once, by `main`.
    pub fn install_router(&self, router: axum::Router) {
        let _ = self.shared.router.set(router);
    }

    /// Adopt the plan the supervisor is about to run as the one every diff is taken against.
    ///
    /// Separate from [`SqliteConfigAdmin::new`] because the two cannot be one call: the
    /// management router is in the configuration the first plan is derived from, and the
    /// router holds this object. Called once, by `main`, before the supervisor starts --
    /// so the window in which there is no shape to diff against is a window in which
    /// nothing can call in.
    pub async fn adopt(&self, plan: &Plan) {
        let mut w = self.writable.lock().await;
        w.shape = Shape::of(plan);
        self.generation
            .store(plan.generation().get(), Ordering::Release);
    }

    /// Record what the change that was just committed is waiting for.
    ///
    /// Written after the generation, so a reader that sees the new generation also sees the
    /// set that goes with it: the other order would leave a window in which the banner
    /// compared the new generation against the previous change's components.
    fn now_awaiting(&self, generation: u64, affects: &[String]) {
        let mut slot = self
            .awaiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Arc::new(Awaiting {
            generation,
            ids: affects.to_vec(),
        });
    }

    /// Try or make one change — the whole of the sequence in this module's documentation.
    async fn run(&self, change: mw_api_mgmt::Change, commit: Commit) -> Result<Applied, ApiError> {
        let warnings = change.warnings();
        let store_change = view::into_store_change(&change, &self.shared.pool)
            .await
            .map_err(|e| ApiError::Invalid {
                summary: "That change cannot be applied to this configuration.".to_string(),
                errors: e,
            })?;
        let mut w = self.writable.lock().await;
        let current = self.generation.load(Ordering::Acquire);
        let generation = if commit.is_save() {
            current + 1
        } else {
            current
        };

        let mut tx = self
            .shared
            .pool
            .begin()
            .await
            .map_err(|e| internal("The configuration database could not be opened.", &e))?;

        // Taken before the change, inside the same transaction, so it is the configuration
        // this write replaces and not one another connection committed in between. Read on
        // a preview too, because the read is the cheap half and branching here would make
        // the two paths differ in more than their last line.
        let before = mw_store::snapshot(&mut tx)
            .await
            .map_err(|e| internal("The configuration could not be read.", &e))?;

        mw_store::config_admin::apply(&mut tx, &store_change)
            .await
            .map_err(refused)?;

        // **The slots are rolled back with the transaction.** `assemble` rebuilds the shared
        // root state and bumps the epoch when the proposed configuration's fingerprint
        // differs -- which a preview must not leave behind, because the next save would then
        // compare against a fingerprint no committed configuration ever had and restart the
        // entire plan for an unrelated edit. The clone is `Arc`s and a small map; the
        // transaction is the expensive half of this function.
        let before_slots = w.slots.clone();
        let proposed = self.plan_from(&mut tx, generation, &mut w).await;
        let Planned {
            plan,
            mounted,
            cfg,
            mgmt_certs,
        } = match proposed {
            Ok(v) => v,
            Err(e) => {
                // Explicit rather than relying on the drop: a `Transaction` dropped without
                // a decision rolls back, and saying so is what stops a later edit turning a
                // refusal into a half-applied write.
                let _ = tx.rollback().await;
                w.slots = before_slots;
                return Err(e);
            }
        };

        let diff = w.shape.diff(&plan);
        let vocab = Vocabulary::new(&view::of(&cfg, generation));
        let outline = PlanOutline {
            start: named(&vocab, &diff.added),
            stop: named(&vocab, &diff.removed),
            restart: named(&vocab, &diff.changed),
            unchanged: diff.unchanged.len(),
        };

        if !commit.is_save() {
            let rolled = tx.rollback().await;
            w.slots = before_slots;
            rolled.map_err(|e| internal("The preview could not be rolled back.", &e))?;
            return Ok(Applied::new(
                Generation::new(generation),
                commit,
                outline,
                warnings,
            ));
        }

        if let Err(e) = tx.commit().await {
            w.slots = before_slots;
            return Err(internal("The change could not be saved.", &e));
        }

        // **After the commit, and in this order.** The router has to serve the saved
        // configuration before the supervisor starts components that answer through it;
        // the reverse would leave a component `Running` against a table that still
        // described the previous configuration.
        self.publish(&mut w, mounted, mgmt_certs);
        self.generation.store(generation, Ordering::Release);
        w.previous = Some(Previous {
            generation: current,
            snapshot: before,
        });
        w.shape = Shape::of(&plan);
        let applied = Applied::new(Generation::new(generation), commit, outline, warnings);
        // **Before the sink, not after.** A reader that sees the new generation has to see
        // the set that goes with it; recording this only on the success path would leave a
        // shutting-down daemon reporting the new generation against the previous change's
        // components, which is the skew this ordering exists to prevent.
        self.now_awaiting(generation, &applied.affects);
        if self.shared.plans.apply(plan).is_err() {
            return Err(ApiError::Conflict {
                summary: "The daemon is shutting down, so the change was saved but will \
                          take effect at the next start."
                    .to_string(),
                detail: None,
            });
        }
        info!(
            change = change.as_str(),
            generation,
            start = applied.diff.start.len(),
            stop = applied.diff.stop.len(),
            restart = applied.diff.restart.len(),
            unchanged = applied.diff.unchanged,
            "configuration committed"
        );
        Ok(applied)
    }

    /// Read the configuration back out of the open transaction, assemble it, and plan it.
    async fn plan_from(
        &self,
        tx: &mut sqlx::SqliteConnection,
        generation: u64,
        w: &mut Writable,
    ) -> Result<Planned, ApiError> {
        let mut cfg = crate::config::Config::load(tx)
            .await
            .map_err(|e| internal("The configuration could not be read.", &e))?;
        cfg.generation = Generation::new(generation);

        let mounted = boot::assemble(
            &mut cfg,
            Arc::clone(&self.shared.render),
            self.shared.fanout.as_deref(),
            &mut w.slots,
        )
        .map_err(|e| ApiError::Invalid {
            summary: "That configuration cannot be used.".to_string(),
            errors: plan_fields(&format!("{e:#}")),
        })?;

        // The management API's certificate, loaded from the **proposed** paths so that an
        // unreadable one is a refusal before the commit rather than a socket that accepts
        // connections and fails every handshake. Swapped into the live resolver by
        // `publish`, which runs only after the commit — `http:listener`'s digest covers
        // only whether a certificate is configured, so a changed *path* would otherwise
        // keep serving the previous certificate for the life of the process.
        let mgmt_certs = management_certs(&cfg)?;

        // The management socket goes back into every plan, because `http:listener`'s digest
        // covers it: without this, the first save would plan a listener with no management
        // port and the API would close the connection that made the request.
        cfg.set_management(mw_http::Management {
            addr: cfg.management_addr().map_err(|e| {
                ApiError::field(
                    "That is not an address to listen on.",
                    "management_listen_addr",
                    e.to_string(),
                )
            })?,
            router: self
                .shared
                .router
                .get()
                .cloned()
                .unwrap_or_else(axum::Router::new),
            // The resolver the listener will be built with: the live one when a certificate
            // is still configured, a new one when it has just been added, none when it has
            // been removed.
            tls: match (&w.mgmt_sni, mgmt_certs.is_some()) {
                // The live resolver, which `publish` swaps the new store into after the
                // commit. Handing a second resolver here would leave the running listener
                // consulting the old one whenever its digest did not change.
                (Some(live), true) => Some(Arc::clone(live)),
                // Newly configured, so there is no live resolver to swap into and the
                // listener is restarting anyway (its digest covers whether TLS is on). The
                // second load is two small files and happens only on this transition --
                // `CertStore` is not `Clone`, and the first one is owed to `publish`.
                (None, true) => Some(Arc::new(mw_http::SniResolver::new(
                    management_certs(&cfg)?.expect("the paths were readable a moment ago"),
                ))),
                (_, false) => None,
            },
        });
        // The live mount, not the one `mounted` will become: the plan's `http:listener`
        // closes over this `Arc`, and a swap after the commit is what reaches it.
        cfg.set_listener(Arc::clone(&self.shared.mount), w.sni.clone());

        let ctx =
            PlanContext::new(Generation::new(generation)).with_roots(self.shared.roots.clone());
        let plan = boot::plan_stamped(&cfg, &ctx, w.slots.epoch()).map_err(|e| {
            // W5 §7.2: a `plan()` error **is** a validation error, rendered on the field
            // that caused it -- which is only possible because `plan()` is pure and nothing
            // has been stopped when this is returned.
            let rendered = mw_types::DisplayChain(&e).to_string();
            ApiError::Invalid {
                summary: "That configuration cannot be used.".to_string(),
                errors: plan_fields(&rendered),
            }
        })?;

        // **Both directions of the pairing, on every re-plan and not only at boot.** A
        // mounted table whose owner was not planned answers a `503` for ever on paths
        // nobody will fill; a planned route provider with no mounted table reaches
        // `Running` while its paths 404. `plan()` rejects a duplicate path, so neither is
        // reachable through a configuration -- which is an argument for asserting it rather
        // than for dropping the assertion the boot sequence makes.
        if let Err(e) = boot::assert_mounted_tables_match(&mounted, plan.specs()) {
            return Err(ApiError::Internal {
                summary: "That configuration could be planned but not served.".to_string(),
                detail: Some(format!("{e:#}")),
            });
        }

        Ok(Planned {
            plan,
            mounted,
            cfg,
            mgmt_certs,
        })
    }

    /// Swap the route tables, the certificates and the station registry into the live
    /// objects.
    ///
    /// **Called only after a commit.** Everything here is visible to the running daemon the
    /// instant it happens, so a preview reaching this function would be a dry run that was
    /// not dry.
    fn publish(
        &self,
        w: &mut Writable,
        mounted: boot::Mounted,
        mgmt_certs: Option<mw_http::CertStore>,
    ) {
        let (tables, hosts, certs, registry) = mounted.into_parts();
        // The registry first: it is what resolves a frame to a station, and a route that
        // starts answering before the station directory agrees with it would reject the
        // first uploads from a station the operator just added.
        if let Some(snapshot) = registry {
            w.slots.publish_registry(snapshot);
        }
        match self.shared.mount.swap(tables, hosts) {
            Ok(report) => info!(
                sites = report.sites,
                paths = report.paths,
                "the route tables are swapped"
            ),
            // Unreachable on a configuration a plan accepted: `Mount`'s own errors are a
            // duplicate path and a second catch-all, and `plan()` rejects both (**D-51**).
            // Logged rather than returned because the configuration is already committed:
            // the honest report is that the save happened and the router did not follow.
            Err(e) => warn!(
                err = %mw_types::DisplayChain(&e),
                "the saved configuration could not be mounted; the previous route tables \
                 are still being served"
            ),
        }
        // The management API's own certificate, which `http:listener`'s digest covers only
        // as "configured or not". A path that changed without the flag changing would
        // otherwise keep serving the previous certificate for the life of the process.
        match (&w.mgmt_sni, mgmt_certs) {
            (Some(live), Some(store)) => live.swap(store),
            (None, Some(store)) => w.mgmt_sni = Some(Arc::new(mw_http::SniResolver::new(store))),
            (Some(_), None) => w.mgmt_sni = None,
            (None, None) => {}
        }
        match (&w.sni, certs) {
            // Swapped in place: `http:listener` holds the resolver and only restarts when
            // the socket set changes, so a new object would never be consulted.
            (Some(live), Some(store)) => live.swap(store),
            (None, Some(store)) => w.sni = Some(Arc::new(mw_http::SniResolver::new(store))),
            // TLS turned off. The listener's digest covers `https_port`, so it is restarting
            // and will bind plaintext; the resolver is left in place rather than emptied,
            // because emptying it would make the in-flight TLS handshakes on the socket
            // being replaced fail rather than complete.
            (_, None) => {}
        }
    }
}

/// The management API's certificate store, from the paths the **proposed** configuration
/// names.
///
/// Loaded before the commit so that an unreadable path is a refusal on its own field rather
/// than a socket that accepts connections and fails every handshake — which is `mw-http`'s
/// own rule for the public listener (§6.4: a configured-but-unreadable path is a startup
/// failure naming the path, never a silent substitution).
fn management_certs(cfg: &crate::config::Config) -> Result<Option<mw_http::CertStore>, ApiError> {
    let Some(paths) = cfg.management_cert() else {
        return Ok(None);
    };
    mw_http::CertStore::load(std::slice::from_ref(&paths), Some(""))
        .map(Some)
        .map_err(|e| {
            ApiError::field(
                "That certificate could not be loaded.",
                "management_tls_cert_path",
                mw_types::DisplayChain(&e).to_string(),
            )
        })
}

/// What a successful preview or save produced, before the commit decision.
struct Planned {
    plan: Plan,
    mounted: boot::Mounted,
    cfg: crate::config::Config,
    /// The management API's certificate, loaded from the **proposed** paths.
    mgmt_certs: Option<mw_http::CertStore>,
}

/// `start` / `stop` / `restart`, in the words a screen uses.
fn named(vocab: &Vocabulary, ids: &[mw_component::ComponentId]) -> Vec<Named> {
    let mut out: Vec<Named> = ids
        .iter()
        .map(|id| {
            let d = vocab.describe(id);
            Named {
                id: id.as_str().to_string(),
                display: d.one_line(),
                section: d.section,
                href: d.href,
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A `plan()` refusal, as the field it belongs on.
fn plan_fields(rendered: &str) -> Vec<mw_api_mgmt::FieldError> {
    let fields = mw_api_mgmt::admin::plan_refusal_as_field_errors(rendered);
    if !fields.is_empty() {
        return fields;
    }
    // No field matched. The message still has to reach the operator, so it lands on the
    // form rather than being swallowed -- an empty `errors` array with a generic summary is
    // the one outcome that leaves them with nothing to act on.
    vec![mw_api_mgmt::FieldError::new("", rendered.to_string())]
}

/// A write the database refused, as the status its audience needs.
fn refused(e: mw_store::ConfigAdminError) -> ApiError {
    match &e {
        // `404`, not `409`: every route that can raise this documents `404`, and the split
        // `crate::error` draws between them is the one a client branches on -- "already
        // gone, refresh the list" against "the server refuses, show the conflict". A stale
        // browser tab deleting a station somebody else deleted is the first case.
        mw_store::ConfigAdminError::NoSuchRow { what, id } => ApiError::NotFound {
            what,
            id: id.clone(),
        },
        mw_store::ConfigAdminError::Rejected { .. } => ApiError::Invalid {
            summary: "That configuration is not one this daemon can store.".to_string(),
            // The constraint's own text, which names the rule rather than the field. The
            // field-level messages are `mw_api_mgmt::change`'s and have already run; this is
            // the backstop under them, so it reports what the database said.
            errors: vec![mw_api_mgmt::FieldError::new(
                "",
                mw_types::DisplayChain(&e).to_string(),
            )],
        },
        mw_store::ConfigAdminError::Query { .. } => {
            internal("The configuration could not be written.", &e)
        }
    }
}

fn internal(summary: &str, e: &dyn std::error::Error) -> ApiError {
    ApiError::Internal {
        summary: summary.to_string(),
        detail: Some(mw_types::DisplayChain(e).to_string()),
    }
}

impl ConfigAdmin for SqliteConfigAdmin {
    fn read(&self) -> BoxFuture<'_, Result<ConfigView, ApiError>> {
        Box::pin(async move {
            let mut conn = self
                .shared
                .pool
                .acquire()
                .await
                .map_err(|e| internal("The configuration database could not be opened.", &e))?;
            let cfg = crate::config::Config::load(&mut conn)
                .await
                .map_err(|e| internal("The configuration could not be read.", &e))?;
            Ok(view::of(&cfg, self.generation.load(Ordering::Acquire)))
        })
    }

    fn generation(&self) -> Generation {
        Generation::new(self.generation.load(Ordering::Acquire))
    }

    fn awaiting(&self) -> Awaiting {
        Arc::unwrap_or_clone(Arc::clone(
            &self
                .awaiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ))
    }

    fn apply(
        &self,
        change: mw_api_mgmt::Change,
        commit: Commit,
    ) -> BoxFuture<'_, Result<Applied, ApiError>> {
        Box::pin(self.run(change, commit))
    }

    fn revert(&self, to_generation: u64) -> BoxFuture<'_, Result<Applied, ApiError>> {
        Box::pin(async move {
            let mut w = self.writable.lock().await;
            let Some(previous) = w.previous.take() else {
                return Err(ApiError::Conflict {
                    summary: "There is nothing to undo.".to_string(),
                    detail: Some(
                        "One step back is kept, and only for changes this daemon has made \
                         since it started."
                            .to_string(),
                    ),
                });
            };
            if previous.generation != to_generation {
                let kept = previous.generation;
                // Put it back: a request naming the wrong generation has not consumed the
                // step, and consuming it would make a stale browser tab destroy the undo
                // the operator is about to press.
                w.previous = Some(previous);
                return Err(ApiError::Conflict {
                    summary: "That is not the change this daemon can undo.".to_string(),
                    detail: Some(format!(
                        "One step back is kept, and it restores generation {kept}."
                    )),
                });
            }

            let current = self.generation.load(Ordering::Acquire);
            let generation = current + 1;

            // **The step survives every failure, not only the planning one.** An undo that
            // changed nothing must not be the operator's last one: a transient `SQLITE_BUSY`
            // on the way in would otherwise leave them stuck with a configuration they
            // explicitly asked to leave, and no way back. So the whole attempt has one exit
            // that keeps `previous` taken, and it is the one that committed.
            let before_slots = w.slots.clone();
            let attempt = self
                .attempt_revert(generation, &previous.snapshot, &mut w)
                .await;
            let (applied, plan, mounted, mgmt_certs) = match attempt {
                Ok(v) => v,
                Err(e) => {
                    w.slots = before_slots;
                    w.previous = Some(previous);
                    return Err(e);
                }
            };
            self.publish(&mut w, mounted, mgmt_certs);
            self.generation.store(generation, Ordering::Release);
            w.shape = Shape::of(&plan);
            self.now_awaiting(generation, &applied.affects);
            if self.shared.plans.apply(plan).is_err() {
                return Err(ApiError::Conflict {
                    summary: "The daemon is shutting down, so the previous configuration is \
                              back but will take effect at the next start."
                        .to_string(),
                    detail: None,
                });
            }
            info!(
                generation,
                restored = to_generation,
                "the previous configuration is back"
            );
            Ok(applied)
        })
    }
}

impl SqliteConfigAdmin {
    /// The transactional half of a revert: restore, re-plan, commit.
    ///
    /// Split out so that [`ConfigAdmin::revert`] has one success path and every failure
    /// lands in one place — which is what makes "a failed undo does not spend the undo" a
    /// property of the control flow rather than of remembering it at four returns.
    async fn attempt_revert(
        &self,
        generation: u64,
        snapshot: &[mw_store::Change],
        w: &mut Writable,
    ) -> Result<(Applied, Plan, boot::Mounted, Option<mw_http::CertStore>), ApiError> {
        let mut tx = self
            .shared
            .pool
            .begin()
            .await
            .map_err(|e| internal("The configuration database could not be opened.", &e))?;
        if let Err(e) = mw_store::restore(&mut tx, snapshot).await {
            let _ = tx.rollback().await;
            return Err(refused(e));
        }

        let planned = self.plan_from(&mut tx, generation, w).await;
        let Planned {
            plan,
            mounted,
            cfg,
            mgmt_certs,
        } = match planned {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        };
        let diff = w.shape.diff(&plan);
        let vocab = Vocabulary::new(&view::of(&cfg, generation));
        let outline = PlanOutline {
            start: named(&vocab, &diff.added),
            stop: named(&vocab, &diff.removed),
            restart: named(&vocab, &diff.changed),
            unchanged: diff.unchanged.len(),
        };
        tx.commit()
            .await
            .map_err(|e| internal("The previous configuration could not be put back.", &e))?;
        Ok((
            Applied::new(
                Generation::new(generation),
                Commit::Save,
                outline,
                Vec::new(),
            ),
            plan,
            mounted,
            mgmt_certs,
        ))
    }
}
