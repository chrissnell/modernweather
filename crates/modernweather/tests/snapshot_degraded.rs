//! **D-48's invariant, as far as M4-1 can take it: with the database down, a frozen route
//! table mounts and its handler answers from memory.**
//!
//! The decision reads: *"A `503` from an unmounted route can never be observed on a frozen
//! path, and that is a tested invariant. `api:legacy` declares only `LatestSnapshot` as a
//! hard requirement, and `snapshot:latest` starts successfully with `ReadingStore`
//! unavailable, so the legacy route table mounts unconditionally. With the database down,
//! every frozen route returns its contract response — never `503`."*
//!
//! `docs/reconciliation.md` C-W2a-28 records why it is an invariant rather than an
//! argument: both W2a (assumption 28) and W3 (assumption 30) handed the question to W1,
//! which ran in the same stage and could not answer it, so the plan made it a test.
//! `implementation-plan.md` M4-1 is the item that owes it.
//!
//! # What this test asserts, and what it cannot
//!
//! The decision has two halves. The first is **structural** and is entirely M4-1's: can a
//! component whose only hard need is `LatestSnapshot` be left `Waiting` when there is no
//! `db:postgres` at all? If it can, its route table is unmounted and a frozen path answers
//! `503`, and no amount of handler code fixes that. That half is what runs here, against
//! the real supervisor, the real `snapshot:latest`, and a stand-in that declares exactly
//! what W3 §6.2 says `api:legacy` declares — `LatestSnapshot` hard, `HttpRoutes` provided
//! with the five frozen patterns — and answers out of the capability it is handed.
//!
//! The second half is **byte-level**: `/latest` answering
//! `404 no recent weather data available for this station\n` rather than `503`, for each of
//! the five frozen routes. That needs the frozen routes to exist, and they do:
//! `crates/mw-api-legacy/tests/degraded.rs` drives the real router with all three soft
//! capability slots empty and asserts each one's contract bytes (M4-2), and
//! `cargo xtask gate conformance` has replayed all 70 fixtures against the Rust binary since
//! the same item. What is here is the precondition that half rests on, plus the two things
//! the handler reads: `None` before any reading, and bytes after one, with no store handle
//! anywhere in reach.
//!
//! **Why it lives in the binary's tests.** `mw-supervisor` and `mw-pipeline` are the same
//! layer in `ci/layers.toml` and the layout gate rejects a same-layer dependency in either
//! direction, dev-dependencies included — so the supervisor cannot be driven against the
//! real `snapshot:latest` anywhere else. That is §1.3 rule 1 working as designed: the
//! binary is the only place a capability consumer and its provider meet. `spool_fanout.rs`
//! is here for the same reason.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mw_component::{
    Builder, Capability, ComponentSpec, Generation, LatestSnapshot, Methods, Need, Provision,
    ReadingStream, RouteEntry, RouteManifest, Scope, Serve, SnapshotKey, SnapshotRender, State,
    StationRegistry, Trigger, WaitingOn, WebsiteView,
};
use mw_pipeline::testing::{config, id, node, website, CountingRender, MemoryRegistry, Scratch};
use mw_pipeline::{Clock, Draft, SnapshotState, Spool, SpoolStream, TestClock};
use mw_supervisor::plan::{validate, PlanContext};
use mw_supervisor::testing::{MemoryLogControl, MemoryScheduleStore};
use mw_supervisor::{Roots, Supervisor};
use mw_types::{StationKey, WebsiteId};
use tokio_util::sync::CancellationToken;

/// §3.6's default grace, which is what "shutdown completes" is measured against.
const GRACE: Duration = Duration::from_secs(10);

/// The five frozen routes `docs/reconciliation.md` §6 narrowed the contract to — the ones
/// the iOS client calls. The stand-in declares exactly these, so "the frozen route table
/// mounted" is a statement about the real set rather than about one pattern.
const FROZEN: [&str; 5] = [
    "/latest",
    "/span/{span}",
    "/snow",
    "/stationinfo",
    "/forecast/{span}",
];

fn websites() -> Vec<WebsiteView> {
    vec![website(1, "suncrest", "backyard")]
}

fn snapshot_state() -> Arc<SnapshotState> {
    Arc::new(SnapshotState::new(
        websites(),
        [StationKey::new("backyard")],
    ))
}

fn key() -> SnapshotKey {
    SnapshotKey::new(WebsiteId::new(1), StationKey::new("backyard"))
}

/// A stand-in for `db:postgres` that is **planned and never publishes** — which is what a
/// down database actually looks like.
///
/// This distinction is the one the first draft of this test got wrong. "The database is
/// down" is not "`db:postgres` is absent from the plan": a node with a `weather_database`
/// row plans the pool unconditionally, and `plan()` would refuse the whole configuration if
/// it did not — `snapshot:warm`'s hard `ReadingStore` would be `Unprovided`. What the
/// outage changes is that `mw_store::pg` publishes its handle only **after** a successful
/// connect and a **D-62** version assertion, so with the server unreachable the capability
/// is declared, planned, and never available. This body reproduces exactly that: it holds
/// the provision and never calls `publish`.
fn db_postgres_down() -> ComponentSpec {
    ComponentSpec::new(
        id("db:postgres"),
        Trigger::Service,
        Builder::serve(|_cx| async { Ok(Connecting) }),
    )
    .provides([Provision::of(Capability::ReadingStore)])
    .grace(GRACE)
}

#[derive(Debug)]
struct Connecting;

impl Serve for Connecting {
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

/// What the stand-in handler saw when it was asked, in the order it was asked.
///
/// `Option<usize>` per request: `None` is the 404 the contract has bytes for, `Some(n)` is
/// a payload of `n` bytes. A `503` is not representable, which is the point — the component
/// that would have produced one is the *router*, and it only produces one for a path whose
/// owner is not `Running`.
type Served = Arc<Mutex<Vec<Option<usize>>>>;

/// A stand-in for `api:legacy` (M4-2): the declarations W3 §6.2 gives it, and a body that
/// answers out of the one capability it is handed.
///
/// `Trigger::Service`, because `http:listener` and the route tables it mounts are services
/// (W3 §6.1). What makes this a faithful stand-in is not the body but the two lines of
/// declaration: `LatestSnapshot` **hard** and no `ReadingStore` at all, so the handler
/// *cannot* query (**D-35**) and the supervisor's own ordering rules decide whether its
/// routes mount.
fn api_legacy(served: Served, asked: tokio::sync::mpsc::UnboundedReceiver<()>) -> ComponentSpec {
    let asked = Arc::new(tokio::sync::Mutex::new(asked));
    ComponentSpec::new(
        id("api:legacy"),
        Trigger::Service,
        Builder::serve(move |cx| {
            let served = Arc::clone(&served);
            let asked = Arc::clone(&asked);
            async move {
                // The only handle. There is no `cx.hard::<Arc<dyn Store>>()` available here
                // even if the body wanted one: the spec below does not declare
                // `Capability::ReadingStore`, so `BuildCx::hard` would answer
                // `UndeclaredNeed`.
                let snapshot = cx.hard::<Arc<dyn LatestSnapshot>>()?;
                Ok(Handler {
                    snapshot,
                    served,
                    asked,
                })
            }
        }),
    )
    .requires([Need::hard(Capability::LatestSnapshot)])
    .provides([Provision::routes(RouteManifest::new(
        Scope::Any,
        FROZEN
            .iter()
            .map(|p| RouteEntry::exact(*p, Methods::READ))
            .collect(),
    ))])
    .grace(GRACE)
}

struct Handler {
    snapshot: Arc<dyn LatestSnapshot>,
    served: Served,
    asked: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<()>>>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler").finish_non_exhaustive()
    }
}

impl Serve for Handler {
    fn serve(
        self: Box<Self>,
        cx: mw_component::Cx,
    ) -> mw_component::BoxFuture<'static, Result<(), mw_component::ComponentError>> {
        Box::pin(async move {
            let mut asked = self.asked.lock().await;
            loop {
                tokio::select! {
                    _ = cx.cancelled() => return Ok(()),
                    request = asked.recv() => {
                        if request.is_none() {
                            return Ok(());
                        }
                        // The whole of the handler's reading half: one map lookup through
                        // the capability, no query, nothing to wait on.
                        let answer = self.snapshot.get(&key()).map(|s| s.json().len());
                        self.served.lock().expect("not poisoned").push(answer);
                    }
                }
            }
        })
    }
}

fn roots(stream: Arc<SpoolStream>) -> Roots {
    Roots {
        schedule: Arc::new(MemoryScheduleStore::new()),
        stream: Some(stream as Arc<dyn ReadingStream>),
        log: Arc::new(MemoryLogControl::default()),
    }
}

fn state_of(control: &dyn mw_component::ComponentControl, name: &str) -> State {
    control
        .component(&id(name))
        .unwrap_or_else(|| panic!("{name} is in the plan"))
        .state
}

/// **The invariant.** No `db:postgres`, so `ReadingStore` is never published and
/// `WindowedView` never is either — the database is down from boot, which is the worst case
/// the decision names.
///
/// Five assertions, in the order the failure would bite:
///
/// 1. `snapshot:latest` reaches `Running` and publishes `LatestSnapshot`.
/// 2. The frozen route table's owner reaches `Running`, so its five patterns are mounted —
///    which is what makes a `503` on a frozen path unreachable.
/// 3. Before any reading, the handler's answer is **absent**, which is the documented 404
///    and not a 503.
/// 4. After one reading admitted through the real spool, the same handler answers bytes —
///    from memory, holding no store handle.
/// 5. `snapshot:warm` is the only component left waiting, and it is required by nothing,
///    so its `Waiting` reaches no frozen path.
#[tokio::test(flavor = "current_thread")]
async fn with_no_database_the_frozen_route_table_mounts_and_answers_from_memory() {
    let scratch = Scratch::new("snapshot-degraded");
    let clock = TestClock::fixed();
    let spool = Spool::open(
        config(scratch.spool()),
        node(1),
        Arc::new(clock.clone()) as Arc<dyn Clock>,
    )
    .expect("a fresh spool");
    let stream = Arc::new(SpoolStream::new(Arc::clone(&spool)));
    let shutdown = CancellationToken::new();

    let state = snapshot_state();
    let registry = MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>;
    let render = CountingRender::new() as Arc<dyn SnapshotRender>;
    let served: Served = Arc::new(Mutex::new(Vec::new()));
    let (ask, asked) = tokio::sync::mpsc::unbounded_channel();

    // The plan a node with a weather database really produces: the pool, the rollup, the
    // two snapshot components, and the owner of the frozen route table. The pool and the
    // rollup are the outage -- both are planned, and neither ever publishes.
    let plan = validate(
        vec![
            db_postgres_down(),
            mw_pipeline::rollup::spec(Arc::new(mw_pipeline::RollupState::new([(
                StationKey::new("backyard"),
                None,
            )])))
            .expect("a valid spec")
            .grace(GRACE),
            mw_pipeline::snapshot::latest_spec(
                Arc::clone(&state),
                Arc::clone(&registry),
                Arc::clone(&render),
                // **No window view**, which is the honest shape of this outage: `main`
                // builds the rollup state only for a node that collects, and
                // `window:rollup` is in `Waiting { ReadingStore }` below either way. The
                // handle being present-but-silent is covered in `mw-pipeline`'s own tests.
                None,
            )
            .expect("a valid spec")
            .grace(GRACE),
            mw_pipeline::snapshot::warm_spec(Arc::clone(&state), registry, render, None)
                .expect("a valid spec")
                .grace(GRACE),
            api_legacy(Arc::clone(&served), asked),
        ],
        &PlanContext::new(Generation::new(1)),
    )
    .expect("every hard need has a provider in the plan; none of them is available");

    let (supervisor, control, _plans) =
        Supervisor::new(roots(Arc::clone(&stream)), shutdown.clone());
    let stop = shutdown.clone();
    let driver = {
        let spool = Arc::clone(&spool);
        let clock = clock.clone();
        let state = Arc::clone(&state);
        async move {
            for _ in 0..256 {
                tokio::task::yield_now().await;
            }

            // **1.** The provider started with no database in sight.
            assert!(
                matches!(
                    state_of(&*control, "snapshot:latest"),
                    State::Running { .. }
                ),
                "snapshot:latest has no hard requirement, so it cannot be Waiting; got {:?}",
                state_of(&*control, "snapshot:latest")
            );

            // **2.** And therefore the frozen route table's owner started too, which is the
            // whole of D-48: an unmounted declared path answers 503 naming what it waits
            // on, and a frozen path may not.
            assert!(
                matches!(state_of(&*control, "api:legacy"), State::Running { .. }),
                "the frozen route table must mount unconditionally; got {:?}",
                state_of(&*control, "api:legacy")
            );

            // **3.** With nothing reported, the answer is absent -- the 404 whose bytes
            // `contract/fixtures/latest-no-recent-data` already pins, not a 503.
            ask.send(()).expect("the handler is running");
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                served.lock().expect("not poisoned").as_slice(),
                &[None],
                "a cold table answers 404, which is a contract response"
            );

            // **4.** One reading through the real spool, and the same handler answers bytes.
            spool
                .admit_local(&[Draft {
                    station: StationKey::new("backyard"),
                    observed_at: clock.now(),
                    received_at: clock.now(),
                    reading: Arc::new(mw_pipeline::testing::reading(29.92)),
                }])
                .await
                .expect("admit waits on nothing but its own fsync");
            for _ in 0..256 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                state.populated(),
                1,
                "the fold reached the one configured key with the database still down"
            );
            ask.send(()).expect("the handler is running");
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            let answers = served.lock().expect("not poisoned").clone();
            assert_eq!(answers.len(), 2);
            assert!(
                answers[1].is_some_and(|n| n > 0),
                "the handler served pre-serialised bytes out of memory, with no store \
                 handle in reach; got {:?}",
                answers[1]
            );

            // **D-36**: the window-derived values are absent and the fact is recorded,
            // because `window:rollup` never published -- and the reading itself is served
            // anyway, which is W3 §2a.6's first table row.
            let snapshot = state.get(&key()).expect("published");
            assert_eq!(
                snapshot.staleness().missing(),
                &[Capability::WindowedView],
                "the staleness indicator names what is not answering"
            );

            // **5.** The two components that need the database are waiting on it, and
            // nothing a frozen route depends on is among them.
            assert!(
                matches!(state_of(&*control, "window:rollup"), State::Waiting { .. }),
                "the window provider is Waiting, which is what makes the degradation real \
                 rather than simulated; got {:?}",
                state_of(&*control, "window:rollup")
            );
            match state_of(&*control, "snapshot:warm") {
                State::Waiting { on } => assert_eq!(
                    on,
                    vec![WaitingOn {
                        capability: Capability::ReadingStore,
                        // Named, which is what the management UI renders as a sentence: it
                        // waits on the database rather than on anything in the read path.
                        provider: Some(id("db:postgres")),
                    }],
                ),
                other => panic!("expected Waiting, got {other:?}"),
            }

            stop.cancel();
        }
    };

    let (report, ()) = tokio::join!(supervisor.run(plan), driver);
    assert_eq!(
        report.aborted, 0,
        "shutdown completed inside the grace: {report:?}"
    );
}
