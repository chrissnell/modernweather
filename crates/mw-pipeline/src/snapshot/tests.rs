//! The two specs, the fold, the seed, and **D-36**'s degradation — all over memory.
//!
//! Nothing here needs a database. What a database *is* needed for is the one half these
//! cannot assert — that `ReadingStore::latest` really reads the newest raw row back into a
//! `Reading` — and that lives in `crates/mw-store/tests/db.rs` behind the `db-tests` gate,
//! which fails rather than skips when there is no server (W3 Decision 84).
//!
//! The other half M4-1 owes is **D-48**'s precondition, which is a claim about the
//! *supervisor* rather than about this module: a consumer whose only hard need is
//! `LatestSnapshot` must reach `Running` with no `db:postgres` at all. That one is
//! `crates/modernweather/tests/snapshot_degraded.rs`, because `mw-supervisor` and
//! `mw-pipeline` are the same layer and the binary is the only place they meet.

use std::sync::Mutex;

use super::*;
use crate::testing::{admitted, website, CountingRender, MemoryRegistry};
use crate::TestClock;
use mw_component::{
    Bucket, BucketQuery, Bucketed, Encoding, SeriesQuery, StoreError, TotalsQuery, WebsiteView,
    WindowError,
};
use mw_types::WebsiteId;

fn at(s: &str) -> DateTime<Utc> {
    s.parse().expect("an RFC 3339 instant")
}

fn key() -> StationKey {
    StationKey::new("backyard")
}

fn websites() -> Vec<WebsiteView> {
    vec![
        website(1, "suncrest", "backyard"),
        website(2, "portal", "backyard"),
    ]
}

fn state() -> Arc<SnapshotState> {
    Arc::new(SnapshotState::new(
        websites(),
        [key(), StationKey::new("ridge")],
    ))
}

// ---------------------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------------------

/// **D-31's key is the cross product**, and the slots are allocated for all of it while the
/// entries are not: a key answers `None` until something reports for it, which is what makes
/// `/latest`'s 404 the cold-start answer.
#[test]
fn the_key_set_is_every_website_crossed_with_every_station() {
    let state = state();
    assert_eq!(state.len(), 4, "two websites x two stations");
    assert_eq!(state.populated(), 0, "nothing has reported");
    let keys: Vec<String> = state.keys().map(|k| k.to_string()).collect();
    assert_eq!(keys, vec!["1/backyard", "1/ridge", "2/backyard", "2/ridge"]);

    let configured = SnapshotKey::new(WebsiteId::new(1), key());
    assert!(state.get(&configured).is_none(), "no reading yet");
    assert!(
        state.subscribe(&configured).is_some(),
        "a configured key has a watch from the start, so SSE can attach before the first \
         reading"
    );
}

/// A key the configuration does not contain is absent from **both** accessors, and a write
/// to it is refused rather than growing the map — which is what W3 §2a.4's sizing argument
/// assumes.
#[test]
fn an_unconfigured_key_is_absent_and_unwritable() {
    let state = state();
    let unknown = SnapshotKey::new(WebsiteId::new(9), key());
    assert!(state.get(&unknown).is_none());
    assert!(state.subscribe(&unknown).is_none());

    let render = CountingRender::new();
    let snapshot = Arc::new(
        Snapshot::render(
            SnapshotSource {
                key: unknown.clone(),
                website: website(9, "ghost", "backyard"),
                reading: Reading::default(),
                meta: mw_component::StationMeta::EMPTY,
                display_name: None,
                windows: Resolved::new(at("2026-11-07T18:00:00Z"), []),
            },
            Staleness::Fresh,
            at("2026-11-07T18:00:00Z"),
            PayloadRev::of(1, mw_component::Generation::ZERO),
            render as Arc<dyn SnapshotRender>,
        )
        .expect("rendered"),
    );
    assert!(!state.publish(snapshot), "refused, not inserted");
    assert_eq!(state.populated(), 0);
}

// ---------------------------------------------------------------------------------------
// The specs
// ---------------------------------------------------------------------------------------

/// **D-48's precondition, as a property of the spec.** `snapshot:latest` declares no hard
/// requirement, so the supervisor cannot leave it `Waiting` — which is the whole of why a
/// `503` cannot appear on a frozen path.
#[test]
fn the_latest_spec_has_no_hard_requirement_and_provides_the_snapshot() {
    let render = CountingRender::new() as Arc<dyn SnapshotRender>;
    let spec =
        latest_spec(state(), MemoryRegistry::fixed("backyard"), render, None).expect("valid");
    assert_eq!(spec.id.as_str(), LATEST_ID);
    match &spec.trigger {
        Trigger::OnReading {
            filter,
            policy,
            deadline,
        } => {
            assert_eq!(*filter, ReadingFilter::All);
            assert_eq!(
                *policy,
                ReadingPolicy::Latest,
                "current conditions coalesce"
            );
            assert_eq!(*deadline, LATEST_DEADLINE);
        }
        other => panic!("expected OnReading, got {other:?}"),
    }
    assert_eq!(
        spec.requires,
        vec![Need::soft(Capability::WindowedView)],
        "soft and nothing else: a hard need here is a frozen route that can be unmounted"
    );
    assert_eq!(
        spec.hard_needs().count(),
        0,
        "D-48 reduces to this count being zero"
    );
    assert_eq!(
        spec.provides,
        vec![Provision::of(Capability::LatestSnapshot)]
    );
    assert!(
        !spec
            .requires
            .iter()
            .any(|n| n.capability == Capability::ReadingStore),
        "the component that renders the payload may not query either (D-35)"
    );
}

/// `snapshot:warm` is the one that reads, runs once, and is required by nothing — so its
/// hard need cannot cascade.
#[test]
fn the_warm_spec_runs_once_against_the_store_and_provides_nothing() {
    let render = CountingRender::new() as Arc<dyn SnapshotRender>;
    let spec = warm_spec(state(), MemoryRegistry::fixed("backyard"), render, None).expect("valid");
    assert_eq!(spec.id.as_str(), WARM_ID);
    assert_eq!(
        spec.trigger,
        Trigger::OnStartup {
            deadline: WARM_DEADLINE
        }
    );
    assert_eq!(
        spec.requires,
        vec![
            Need::hard(Capability::ReadingStore),
            Need::soft(Capability::WindowedView),
        ],
        "hard where D-37 writes soft; the module documentation has why"
    );
    assert!(
        spec.provides.is_empty(),
        "nothing requires this component, which is what makes its Waiting harmless"
    );
}

/// The two specs restart on a key-set change and on nothing else.
#[test]
fn the_config_digest_is_the_key_set() {
    let render = CountingRender::new() as Arc<dyn SnapshotRender>;
    let registry = MemoryRegistry::fixed("backyard");
    let a = latest_spec(
        state(),
        Arc::clone(&registry) as _,
        Arc::clone(&render),
        None,
    )
    .expect("valid");
    let b = latest_spec(
        state(),
        Arc::clone(&registry) as _,
        Arc::clone(&render),
        None,
    )
    .expect("valid");
    assert_eq!(a.config_digest, b.config_digest);

    let more = Arc::new(SnapshotState::new(
        websites(),
        [key(), StationKey::new("ridge"), StationKey::new("summit")],
    ));
    let c = latest_spec(more, registry as _, render, None).expect("valid");
    assert_ne!(
        a.config_digest, c.config_digest,
        "a station added to the configuration is a new key, so the component restarts"
    );
}

// ---------------------------------------------------------------------------------------
// The registrar
// ---------------------------------------------------------------------------------------

struct Cfg {
    db: bool,
    stream: bool,
    state: Option<Arc<SnapshotState>>,
    registry: Option<Arc<dyn StationRegistry>>,
    render: Option<Arc<dyn SnapshotRender>>,
    windows: Option<Arc<dyn WindowedView>>,
}

impl std::fmt::Debug for Cfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cfg")
            .field("db", &self.db)
            .field("stream", &self.stream)
            .field("render", &self.render.is_some())
            .finish_non_exhaustive()
    }
}

impl Cfg {
    fn full() -> Self {
        Self {
            db: true,
            stream: true,
            state: Some(state()),
            registry: Some(MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>),
            render: Some(CountingRender::new() as Arc<dyn SnapshotRender>),
            windows: None,
        }
    }
}

impl SnapshotConfig for Cfg {
    fn has_weather_database(&self) -> bool {
        self.db
    }
    fn has_reading_stream(&self) -> bool {
        self.stream
    }
    fn snapshot_state(&self) -> Option<&Arc<SnapshotState>> {
        self.state.as_ref()
    }
    fn snapshot_registry(&self) -> Option<&Arc<dyn StationRegistry>> {
        self.registry.as_ref()
    }
    fn snapshot_render(&self) -> Option<&Arc<dyn SnapshotRender>> {
        self.render.as_ref()
    }
    fn snapshot_windows(&self) -> Option<&Arc<dyn WindowedView>> {
        self.windows.as_ref()
    }
}

fn ids(cfg: &Cfg) -> Vec<String> {
    let mut out = Vec::new();
    register(cfg, &mut out).expect("a plannable configuration");
    out.iter().map(|s| s.id.to_string()).collect()
}

#[test]
fn a_node_with_everything_gets_both_components() {
    assert_eq!(ids(&Cfg::full()), vec![LATEST_ID, WARM_ID]);
}

/// The scope boundary this item ships with, named rather than implied: without M4-2's
/// renderer there are no bytes to pre-serialise, so neither component is planned.
#[test]
fn without_a_renderer_neither_component_is_planned() {
    let cfg = Cfg {
        render: None,
        ..Cfg::full()
    };
    assert!(ids(&cfg).is_empty());
}

/// A serve-only node (W7 §8.2) has no spool, so `OnReading` has no root to validate
/// against — but the seed still runs, which is the one thing that can populate its table.
#[test]
fn without_a_reading_stream_only_the_seed_is_planned() {
    let cfg = Cfg {
        stream: false,
        ..Cfg::full()
    };
    assert_eq!(ids(&cfg), vec![WARM_ID]);
}

/// A node with no weather database has no `db:postgres` and therefore no provider for
/// `ReadingStore`, so planning the seed would refuse the whole configuration (**D-44**).
/// `snapshot:latest` is unaffected, because it never queries.
#[test]
fn without_a_weather_database_only_the_fold_is_planned() {
    let cfg = Cfg {
        db: false,
        ..Cfg::full()
    };
    assert_eq!(ids(&cfg), vec![LATEST_ID]);
}

/// No websites or no stations is no keys, which is not an error and not a silence either.
#[test]
fn an_empty_key_set_plans_nothing() {
    let cfg = Cfg {
        state: Some(Arc::new(SnapshotState::new(Vec::new(), [key()]))),
        ..Cfg::full()
    };
    assert!(ids(&cfg).is_empty());

    let cfg = Cfg {
        state: Some(Arc::new(SnapshotState::new(websites(), Vec::new()))),
        ..Cfg::full()
    };
    assert!(ids(&cfg).is_empty());
}

/// A missing root resource is a registrar error naming what the boot sequence did not
/// build, not a panic.
#[test]
fn a_missing_root_resource_names_itself() {
    let mut out = Vec::new();
    let err = register(
        &Cfg {
            state: None,
            ..Cfg::full()
        },
        &mut out,
    )
    .expect_err("the state is a precondition");
    assert_eq!(err, SnapshotPlanError::NoState);
    assert!(err.to_string().contains("boot.rs"), "{err}");

    let err = register(
        &Cfg {
            registry: None,
            ..Cfg::full()
        },
        &mut out,
    )
    .expect_err("the registry is a precondition");
    assert_eq!(err, SnapshotPlanError::NoRegistry);
    assert!(err.to_string().contains("attach_registry"), "{err}");
}

/// The parts that need a real `tokio` runtime, and therefore cannot run under
/// `--cfg loom`.
///
/// That flag applies to every crate in the graph, and `tokio` gates its whole `net`
/// module behind `#![cfg(not(loom))]` — so `runtime::Builder::build` asserts the I/O
/// driver is disabled and a `#[tokio::test]` panics before its body runs. `super::super::
/// rollup::tests`'s `driven` module and `super::super::writer`'s `adapter` module carry
/// the same guard for the same reason, and `.github/workflows/pr.yml`'s loom job explains
/// the flag's reach in full.
///
/// **Nothing excluded here is a concurrency *model* — and the lock-free half is not
/// modelled anywhere either, which is worth saying plainly.** An earlier version of this
/// comment claimed the synchronous tests above cover it under loom. They do not: they
/// compile and run under `--cfg loom`, but they are single-threaded and they exercise the
/// real `arc_swap::ArcSwap`, which is **not** loom-instrumented —
/// `mw_supervisor::publish`'s module documentation states the consequence for its own cell,
/// that "a model built over it would explore exactly one interleaving while reporting
/// success". `mw_component::snapshot`'s tests are not run under loom at all; that job is
/// scoped `-p mw-supervisor -p mw-pipeline`.
///
/// What does cover the one interleaving that matters — a seed racing a fold on one key — is
/// `a_seed_racing_a_fold_can_never_move_a_key_backwards` in this module, a multi-threaded
/// `tokio` test that the `race` job runs under ThreadSanitizer. A loom model over the
/// publish protocol would be better and would follow `publish.rs`'s substitution pattern;
/// it is recorded as wanted rather than claimed as done.
#[cfg(not(loom))]
mod driven {
    use super::*;

    // ---------------------------------------------------------------------------------------
    // The fold
    // ---------------------------------------------------------------------------------------

    /// A [`WindowedView`] that answers from a table, or refuses.
    #[derive(Debug)]
    struct FakeView {
        as_of: Mutex<DateTime<Utc>>,
        gust: Mutex<Option<f64>>,
        refuse: Mutex<Option<WindowError>>,
    }

    impl FakeView {
        fn new(as_of: DateTime<Utc>, gust: Option<f64>) -> Arc<Self> {
            Arc::new(Self {
                as_of: Mutex::new(as_of),
                gust: Mutex::new(gust),
                refuse: Mutex::new(None),
            })
        }

        fn refusing(reason: WindowError) -> Arc<Self> {
            Arc::new(Self {
                as_of: Mutex::new(at("2026-11-07T18:00:00Z")),
                gust: Mutex::new(None),
                refuse: Mutex::new(Some(reason)),
            })
        }

        /// A provider that exists and has not ticked yet, so it answers `UnknownStation` —
        /// which is what `RollupState::resolve` does for a station whose windows are still
        /// the `Windows::default()` the state was constructed with.
        fn silent() -> Arc<Self> {
            Arc::new(Self {
                as_of: Mutex::new(at("2026-11-07T18:00:00Z")),
                gust: Mutex::new(None),
                refuse: Mutex::new(Some(WindowError::UnknownStation { station: key() })),
            })
        }

        /// The first completed tick: it stops refusing and starts answering.
        fn start_answering(&self, as_of: DateTime<Utc>, gust: Option<f64>) {
            *self.refuse.lock().expect("not poisoned") = None;
            *self.as_of.lock().expect("not poisoned") = as_of;
            *self.gust.lock().expect("not poisoned") = gust;
        }

        /// Freeze the provider: the handle keeps answering and its `as_of` stops moving, which
        /// is exactly what `window:rollup` looks like from here once it returns to `Waiting`.
        fn stopped_at(&self, when: DateTime<Utc>) {
            *self.as_of.lock().expect("not poisoned") = when;
        }
    }

    impl WindowedView for FakeView {
        fn resolve(
            &self,
            _station: &StationKey,
            want: &[Declaration],
        ) -> Result<Resolved, WindowError> {
            if let Some(reason) = self.refuse.lock().expect("not poisoned").clone() {
                return Err(reason);
            }
            let as_of = *self.as_of.lock().expect("not poisoned");
            let gust = *self.gust.lock().expect("not poisoned");
            Ok(Resolved::new(
                as_of,
                want.iter().map(|d| {
                    (
                        *d,
                        if d.0 == MetricKey::WindSpeed {
                            gust
                        } else {
                            None
                        },
                    )
                }),
            ))
        }
    }

    /// A [`ReadingStore`] whose only answered method is `latest`.
    #[derive(Debug, Default)]
    struct FakeStore {
        rows: Mutex<Vec<Reading>>,
        asked: Mutex<Vec<String>>,
        fail: bool,
    }

    impl FakeStore {
        fn with(rows: Vec<Reading>) -> Arc<Self> {
            Arc::new(Self {
                rows: Mutex::new(rows),
                ..Self::default()
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                fail: true,
                ..Self::default()
            })
        }
    }

    /// The write half, so a `FakeStore` can be published as the `Arc<dyn Store>` the bus
    /// actually carries (`mw_component::store`: the only handle type `ReadingStore` accepts
    /// is both halves).
    ///
    /// It refuses, which is the honest answer: this double is the read half, and a
    /// `snapshot:*` component that reached it would be a component that had grown a write
    /// path. Neither of them holds a `ReadingSink` after `build` -- both upcast to
    /// `Arc<dyn ReadingStore>` -- so this is unreachable from either body, and refusing
    /// rather than succeeding is what makes that a test failure instead of a silent write.
    impl mw_component::ReadingSink for FakeStore {
        fn store<'a>(
            &'a self,
            _batch: &'a [mw_types::Envelope],
        ) -> BoxFuture<'a, Result<mw_component::Stored, StoreError>> {
            Box::pin(async {
                Err(StoreError::rejected(std::io::Error::other(
                    "this double is the read half; a snapshot component must not store",
                )))
            })
        }
    }

    impl ReadingStore for FakeStore {
        /// The three aggregate reads have no caller in this module: `snapshot:warm` writes one
        /// query and `snapshot:latest` writes none. Answered as empty rather than
        /// `unimplemented!`, so that a future caller gets a wrong answer it can see instead of
        /// a panic inside a component tick.
        fn buckets<'a>(
            &'a self,
            _q: BucketQuery,
        ) -> BoxFuture<'a, Result<Vec<Bucket>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn totals<'a>(
            &'a self,
            _q: TotalsQuery,
        ) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn series<'a>(
            &'a self,
            _q: SeriesQuery,
        ) -> BoxFuture<'a, Result<Vec<Bucketed>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn mean_series<'a>(
            &'a self,
            _q: mw_component::MeanSeriesQuery,
        ) -> BoxFuture<'a, Result<Vec<mw_component::MeanBucket>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn latest<'a>(
            &'a self,
            q: LatestQuery,
        ) -> BoxFuture<'a, Result<Option<Reading>, StoreError>> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push(format!("latest {} since={}", q.station, q.since));
            if self.fail {
                return Box::pin(async {
                    Err(StoreError::unavailable(std::io::Error::other(
                        "the server is not answering",
                    )))
                });
            }
            // The bound is the store's, not the caller's: the fake applies it so that a test
            // asserting the recency rule is asserting the query rather than a filter the
            // component added afterwards.
            let found = self
                .rows
                .lock()
                .expect("not poisoned")
                .iter()
                .filter(|r| r.station_id == q.station.as_str() && r.time >= q.since)
                .max_by_key(|r| r.time)
                .cloned();
            Box::pin(async move { Ok(found) })
        }

        /// `/span` and `/almanac` have no caller in this module -- both are
        /// `api:legacy`'s. Answered as empty rather than `unimplemented!`, so a future
        /// caller gets a wrong answer it can see instead of a panic inside a component
        /// tick.
        fn span<'a>(
            &'a self,
            _q: mw_component::SpanQuery,
        ) -> BoxFuture<'a, Result<mw_component::Span, StoreError>> {
            Box::pin(async {
                Ok(mw_component::Span {
                    backfill: Vec::new(),
                    prefix: Vec::new(),
                    tail: None,
                })
            })
        }

        fn almanac<'a>(
            &'a self,
            _q: mw_component::AlmanacQuery,
        ) -> BoxFuture<'a, Result<Vec<mw_component::AlmanacRecord>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn folder(
        state: &Arc<SnapshotState>,
        render: &Arc<CountingRender>,
        windows: Option<Arc<dyn WindowedView>>,
    ) -> Folder {
        Folder::new(
            Arc::clone(state),
            MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>,
            Arc::clone(render) as Arc<dyn SnapshotRender>,
            windows,
            PayloadRev::of(render.revision(), mw_component::Generation::new(3)),
        )
    }

    fn cx(id: &str) -> Cx {
        let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
        Cx::new(
            ComponentId::new(id).expect("a valid id"),
            tokio_util::sync::CancellationToken::new(),
            mw_component::Generation::ZERO,
            rx,
        )
    }

    fn payload(state: &SnapshotState, website: i64, station: &str) -> String {
        let snapshot = state
            .get(&SnapshotKey::new(
                WebsiteId::new(website),
                StationKey::new(station),
            ))
            .unwrap_or_else(|| panic!("{website}/{station} has no snapshot"));
        String::from_utf8(snapshot.json().to_vec()).expect("the test renderer writes text")
    }

    /// **One reading becomes one payload per website**, which is D-31 as behaviour: the two
    /// websites get two entries for the one station that reported, and the station that did not
    /// report has none.
    #[tokio::test]
    async fn one_reading_is_folded_into_every_website_s_key() {
        let state = state();
        let render = CountingRender::new();
        let view = FakeView::new(at("2026-11-07T18:30:00Z"), Some(14.5));
        let mut body = Fold {
            folder: folder(&state, &render, Some(view as Arc<dyn WindowedView>)),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };

        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("the fold does no I/O");

        assert_eq!(state.populated(), 2, "two websites, one station");
        assert_eq!(payload(&state, 1, "backyard"), "1/backyard|29.92|14.5");
        assert_eq!(
            payload(&state, 2, "backyard"),
            "2/backyard|29.92|14.5",
            "the same reading and windows, rendered once per website"
        );
        assert!(
            state
                .get(&SnapshotKey::new(
                    WebsiteId::new(1),
                    StationKey::new("ridge")
                ))
                .is_none(),
            "a station that has not reported keeps answering 404"
        );
        // Rendered once per key, eagerly, and never a second time for the same version.
        assert_eq!(render.count(), 2);
        assert!(render.calls().iter().all(|(e, _)| *e == Encoding::Json));
    }

    /// **The newest envelope *per station*, not the newest envelope.** A batch carries the whole
    /// node's stream, so taking only `batch.newest()` would publish one station's conditions and
    /// silently drop the other's.
    #[tokio::test]
    async fn a_batch_carrying_two_stations_publishes_the_newest_of_each() {
        let state = state();
        let render = CountingRender::new();
        let mut body = Fold {
            folder: folder(&state, &render, None),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:05Z"))),
        };

        body.readings(
            ReadingBatch::new(vec![
                admitted("backyard", at("2026-11-07T18:30:00Z"), 29.90),
                admitted("ridge", at("2026-11-07T18:30:01Z"), 30.01),
                admitted("backyard", at("2026-11-07T18:30:02Z"), 29.92),
            ]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");

        assert_eq!(state.populated(), 4, "two websites x two stations");
        assert_eq!(
            payload(&state, 1, "backyard"),
            "1/backyard|29.92|-",
            "the second backyard reading won, not the first"
        );
        assert_eq!(payload(&state, 1, "ridge"), "1/ridge|30.01|-");
        assert_eq!(
            state
                .get(&SnapshotKey::new(WebsiteId::new(1), key()))
                .expect("published")
                .observed_at(),
            at("2026-11-07T18:30:02Z")
        );
    }

    /// A new reading is a new entity, so the `ETag` moves and the bytes are replaced.
    #[tokio::test]
    async fn a_later_reading_replaces_the_key_and_moves_the_etag() {
        let state = state();
        let render = CountingRender::new();
        let mut body = Fold {
            folder: folder(&state, &render, None),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };
        let k = SnapshotKey::new(WebsiteId::new(1), key());

        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.90,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");
        let first = state.get(&k).expect("published").etag().clone();

        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:02Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");
        let second = state.get(&k).expect("published");
        assert_ne!(&first, second.etag());
        assert!(second.etag().matches(second.etag().as_str()));
        assert!(
            !second.etag().matches(first.as_str()),
            "a cached copy of the previous version does not validate"
        );
        assert_eq!(payload(&state, 1, "backyard"), "1/backyard|29.92|-");
    }

    /// A fold wakes the key's `watch`, which is what SSE and `live:stream` attach to — and the
    /// subscriber never sees a table that lacks the snapshot it was woken for.
    #[tokio::test]
    async fn a_fold_wakes_the_keys_subscriber() {
        let state = state();
        let render = CountingRender::new();
        let k = SnapshotKey::new(WebsiteId::new(1), key());
        let mut rx = state.subscribe(&k).expect("a configured key");
        assert!(rx.borrow_and_update().is_none());

        let mut body = Fold {
            folder: folder(&state, &render, None),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");

        rx.changed().await.expect("the sender is alive");
        let seen = rx.borrow().clone().expect("a snapshot");
        assert_eq!(seen.observed_at(), at("2026-11-07T18:30:00Z"));
        assert!(
            Arc::ptr_eq(&seen, &state.get(&k).expect("also in the table")),
            "the watch and the table hold one object, so a subscriber cannot read a table \
             that does not have what woke it"
        );
    }

    // ---------------------------------------------------------------------------------------
    // D-36 — degradation
    // ---------------------------------------------------------------------------------------

    /// **D-36, the case it is actually about.** `window:rollup` goes to `Waiting` on a database
    /// outage, this component keeps the handle it resolved at build, and the values stop moving.
    /// What the snapshot does is keep the **last good** ones and say so.
    #[tokio::test]
    async fn a_provider_that_has_stopped_refreshing_keeps_its_last_good_values_and_degrades() {
        let state = state();
        let render = CountingRender::new();
        let view = FakeView::new(at("2026-11-07T18:30:00Z"), Some(14.5));
        let clock = TestClock::new(at("2026-11-07T18:30:02Z"));
        let mut body = Fold {
            folder: folder(
                &state,
                &render,
                Some(Arc::clone(&view) as Arc<dyn WindowedView>),
            ),
            clock: Arc::new(clock.clone()),
        };
        let k = SnapshotKey::new(WebsiteId::new(1), key());

        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");
        assert!(
            state.get(&k).expect("published").staleness().is_fresh(),
            "the provider ticked five seconds ago"
        );

        // The database goes down. The rollup stops refreshing; its handle keeps answering the
        // state it last published, with the `as_of` it was published at.
        view.stopped_at(at("2026-11-07T18:30:00Z"));
        clock.advance(chrono::Duration::seconds(208));
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:33:28Z"),
                29.80,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");

        let snapshot = state.get(&k).expect("published");
        assert_eq!(
            payload(&state, 1, "backyard"),
            "1/backyard|29.8|14.5",
            "the reading moved and the gust did not: the window-derived value is the last good \
             one, which is D-36's whole sentence"
        );
        match snapshot.staleness() {
            Staleness::Degraded { missing, since } => {
                assert_eq!(missing.as_slice(), &[Capability::WindowedView]);
                assert_eq!(
                    *since,
                    at("2026-11-07T18:30:00Z"),
                    "`since` is when the values stopped, not when it was noticed"
                );
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        assert!(
            !String::from_utf8(snapshot.json().to_vec())
                .expect("text")
                .contains("degraded"),
            "the indicator never reaches the frozen body -- it goes out through additive \
             headers, metrics and the management UI"
        );
    }

    /// The band around [`WINDOW_STALE_AFTER`]: one and two missed ticks are not an outage,
    /// three is.
    #[tokio::test]
    async fn one_late_tick_is_not_degradation() {
        let state = state();
        let render = CountingRender::new();
        let view = FakeView::new(at("2026-11-07T18:30:00Z"), Some(14.5));
        let k = SnapshotKey::new(WebsiteId::new(1), key());

        for (now, fresh) in [
            ("2026-11-07T18:31:05Z", true),  // one missed tick
            ("2026-11-07T18:32:05Z", true),  // two
            ("2026-11-07T18:32:31Z", false), // past 150 s
        ] {
            let mut body = Fold {
                folder: folder(
                    &state,
                    &render,
                    Some(Arc::clone(&view) as Arc<dyn WindowedView>),
                ),
                clock: Arc::new(TestClock::new(at(now))),
            };
            body.readings(
                ReadingBatch::new(vec![admitted("backyard", at(now), 29.92)]),
                &cx(LATEST_ID),
            )
            .await
            .expect("folded");
            assert_eq!(
                state.get(&k).expect("published").staleness().is_fresh(),
                fresh,
                "at {now}"
            );
        }
    }

    /// A provider that never published at all — a node whose database has not been up since it
    /// started. The reading itself is unaffected, which is W3 §2a.6's first table row.
    #[tokio::test]
    async fn with_no_window_provider_the_reading_is_still_served() {
        let state = state();
        let render = CountingRender::new();
        let mut body = Fold {
            folder: folder(&state, &render, None),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");

        let snapshot = state
            .get(&SnapshotKey::new(WebsiteId::new(1), key()))
            .expect("published");
        assert_eq!(payload(&state, 1, "backyard"), "1/backyard|29.92|-");
        assert_eq!(
            snapshot.staleness().missing(),
            &[Capability::WindowedView],
            "the payload is served and the absence is recorded"
        );
    }

    /// A declaration the provider refuses is not a reason to refuse the payload: the window
    /// fields are absent and everything the reading carries is served.
    #[tokio::test]
    async fn a_refused_declaration_costs_the_window_fields_and_nothing_else() {
        let state = state();
        let render = CountingRender::new();
        let view = FakeView::refusing(WindowError::UnknownStation { station: key() });
        let mut body = Fold {
            folder: folder(&state, &render, Some(view as Arc<dyn WindowedView>)),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("a refused window is not a failed fold");
        assert_eq!(payload(&state, 1, "backyard"), "1/backyard|29.92|-");
    }

    // ---------------------------------------------------------------------------------------
    // D-37 — the warm start
    // ---------------------------------------------------------------------------------------

    fn stored(station: &str, at_: DateTime<Utc>, barometer: f32) -> Reading {
        Reading {
            time: at_,
            station_id: station.to_string(),
            barometer: Some(barometer),
            ..Reading::default()
        }
    }

    fn warm(state: &Arc<SnapshotState>, store: Arc<FakeStore>, now: DateTime<Utc>) -> Warm {
        let render = CountingRender::new();
        Warm {
            store: store as Arc<dyn ReadingStore>,
            folder: folder(state, &render, None),
            clock: Arc::new(TestClock::new(now)),
        }
    }

    /// **D-37's point.** A restart inside the 10-minute window does not 404 a station that was
    /// reporting: the seed puts its newest row into every website's key.
    #[tokio::test]
    async fn the_seed_fills_every_key_for_a_station_inside_the_window() {
        let state = state();
        let store = FakeStore::with(vec![
            stored("backyard", at("2026-11-07T18:25:00Z"), 29.80),
            stored("backyard", at("2026-11-07T18:29:00Z"), 29.92),
            stored("ridge", at("2026-11-07T18:29:30Z"), 30.01),
        ]);
        let mut body = warm(&state, Arc::clone(&store), at("2026-11-07T18:30:00Z"));

        body.tick(&cx(WARM_ID)).await.expect("the store answers");

        assert_eq!(state.populated(), 4, "two websites x two stations");
        assert_eq!(
            payload(&state, 1, "backyard"),
            "1/backyard|29.92|-",
            "the newest row in the window, not the oldest"
        );
        assert_eq!(payload(&state, 2, "ridge"), "2/ridge|30.01|-");
        let asked = store.asked.lock().expect("not poisoned").clone();
        assert_eq!(asked.len(), 2, "one query per station, not per key");
        assert!(
            asked[0].contains("since=2026-11-07 18:20:00 UTC"),
            "the ten-minute recency bound is in the query: {}",
            asked[0]
        );
    }

    /// A station with nothing in the window stays absent, which is the same 404 a stale station
    /// produces — the bytes `contract/fixtures/latest-no-recent-data` already pins.
    #[tokio::test]
    async fn a_station_outside_the_window_is_not_seeded() {
        let state = state();
        let store = FakeStore::with(vec![stored("backyard", at("2026-11-07T18:00:00Z"), 29.92)]);
        let mut body = warm(&state, store, at("2026-11-07T18:30:00Z"));
        body.tick(&cx(WARM_ID)).await.expect("the store answers");
        assert_eq!(
            state.populated(),
            0,
            "half an hour old is outside W1 Decision 15's ten minutes"
        );
    }

    /// An empty store is not an error: the component finishes, the table stays empty, and
    /// `/latest` 404s until a reading arrives (**D-37**'s second sentence).
    #[tokio::test]
    async fn an_empty_store_leaves_an_empty_table_and_finishes() {
        let state = state();
        let mut body = warm(
            &state,
            FakeStore::with(Vec::new()),
            at("2026-11-07T18:30:00Z"),
        );
        body.tick(&cx(WARM_ID)).await.expect("nothing to seed");
        assert_eq!(state.populated(), 0);
    }

    /// A store that is reachable and failing is a tick error, which the supervisor turns into
    /// `Backoff` — distinct from an empty answer, because "the query failed" and "the station
    /// has not reported" are different facts and only one of them is worth retrying.
    #[tokio::test]
    async fn a_failing_store_is_a_retryable_tick_failure() {
        let state = state();
        let mut body = warm(&state, FakeStore::failing(), at("2026-11-07T18:30:00Z"));
        let err = body
            .tick(&cx(WARM_ID))
            .await
            .expect_err("the server is not answering");
        assert_eq!(mw_types::Classify::class(&err), mw_types::Class::RETRYABLE);
        assert_eq!(state.populated(), 0);
    }

    /// **The seed never moves a key backwards.** `OnStartup` has no hard edge to
    /// `snapshot:latest` and should not have one, so a seed can land after the first reading
    /// has already arrived — and replacing a two-second-old snapshot with a nine-minute-old one
    /// would be a visible jump backwards on the client.
    #[tokio::test]
    async fn a_seed_that_lands_late_does_not_overwrite_a_fresher_reading() {
        let state = state();
        let render = CountingRender::new();
        let mut fold = Fold {
            folder: folder(&state, &render, None),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };
        fold.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");

        let store = FakeStore::with(vec![stored("backyard", at("2026-11-07T18:21:00Z"), 28.00)]);
        let mut body = warm(&state, store, at("2026-11-07T18:30:03Z"));
        body.tick(&cx(WARM_ID)).await.expect("the store answers");

        assert_eq!(
            payload(&state, 1, "backyard"),
            "1/backyard|29.92|-",
            "the nine-minute-old row lost to the two-second-old reading"
        );
        assert_eq!(
            state
                .get(&SnapshotKey::new(WebsiteId::new(1), key()))
                .expect("published")
                .observed_at(),
            at("2026-11-07T18:30:00Z")
        );
    }

    /// The other direction: a seed into an empty key wins, and a *newer* stored row than what
    /// is in the table wins too — the rule is the instant, not which component asked.
    #[tokio::test]
    async fn a_seed_wins_an_empty_key_and_a_newer_instant() {
        let state = state();
        let k = SnapshotKey::new(WebsiteId::new(1), key());
        let store = FakeStore::with(vec![stored("backyard", at("2026-11-07T18:29:00Z"), 29.92)]);
        let mut body = warm(&state, Arc::clone(&store), at("2026-11-07T18:30:00Z"));
        body.tick(&cx(WARM_ID)).await.expect("seeded");
        assert_eq!(
            state.get(&k).expect("seeded").observed_at(),
            at("2026-11-07T18:29:00Z")
        );

        store.rows.lock().expect("not poisoned").push(stored(
            "backyard",
            at("2026-11-07T18:29:30Z"),
            30.00,
        ));
        let mut again = warm(&state, store, at("2026-11-07T18:30:00Z"));
        again.tick(&cx(WARM_ID)).await.expect("seeded again");
        assert_eq!(
            state.get(&k).expect("seeded").observed_at(),
            at("2026-11-07T18:29:30Z")
        );
    }

    /// **The regression test for the defect code review found: a window provider that
    /// publishes AFTER this component is built must still be read.**
    ///
    /// `snapshot:latest` has no hard requirement, so the supervisor builds it in the first
    /// reconcile pass — before `db:postgres` has connected, therefore before
    /// `window:rollup` can have run a tick, therefore before `WindowedView` exists on the
    /// bus. `BuildCx::soft` resolves **once**, and `mw_supervisor`'s `reconcile` never
    /// rebuilds a `Running` component when a soft capability later arrives: the
    /// `(State::Running, true)` arm refreshes the `degraded` label and returns.
    ///
    /// So a bus-resolved soft handle would be `None` for the life of every healthy
    /// process. Eight of the nine declarations in [`WINDOWS`] would be absent from
    /// `/latest` forever and every snapshot would report `Degraded`, pinning
    /// `mw_snapshot_degraded` at 1 on every key — which makes **D-36**'s only
    /// operator-facing signal a constant, and is the inverse of W3 §2a.6's first table row.
    ///
    /// The fix is that the handle comes from the registrar and the declaration stays,
    /// which is `mw_station`'s shape for `ReadingStream`. This is also the test shape the
    /// suite was missing: every other test here hands `Folder` a view that is already
    /// answering, which is exactly the case the defect did not occur in.
    #[tokio::test]
    async fn a_window_provider_that_publishes_after_the_build_is_still_read() {
        let state = state();
        let render = CountingRender::new();
        let registry = MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>;

        // The live state `main` owns, handed over before `plan()` and therefore before
        // anything has ticked. It answers `UnknownStation` until then, which is the real
        // provider's own cold-start behaviour.
        let view = FakeView::silent();
        let spec = latest_spec(
            Arc::clone(&state),
            registry,
            Arc::clone(&render) as Arc<dyn SnapshotRender>,
            Some(Arc::clone(&view) as Arc<dyn WindowedView>),
        )
        .expect("valid");
        let (build, _bus) = build_cx(
            LATEST_ID,
            vec![Need::soft(Capability::WindowedView)],
            vec![Capability::LatestSnapshot],
            // **Nothing on the bus**, which is the state at the first reconcile pass.
            Vec::new(),
        );
        let mut body = match &spec.build {
            mw_component::Builder::OnReading(f) => f(build).await.expect("builds"),
            other => panic!("expected an OnReading builder, got {other:?}"),
        };

        // **Instants relative to the wall clock, not the fixed ones the rest of this
        // module uses.** `latest_spec`'s builder constructs its own `SystemClock`, so a
        // body built through the real builder cannot be handed a `TestClock` -- and a fixed
        // 2026-11-07 instant would be days stale against it, which would make the second
        // assertion below green for the wrong reason. The staleness *band* is asserted on a
        // stopped clock by `one_late_tick_is_not_degradation`; what this test needs from
        // the clock is only that `as_of` is recent.
        let now = Utc::now();

        // Before the provider has ticked: degraded, and the reading is served anyway.
        body.readings(
            ReadingBatch::new(vec![admitted("backyard", now, 29.92)]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");
        assert_eq!(payload(&state, 1, "backyard"), "1/backyard|29.92|-");
        assert!(!state
            .get(&SnapshotKey::new(WebsiteId::new(1), key()))
            .expect("published")
            .staleness()
            .is_fresh());

        // `window:rollup` completes its first tick and publishes. The same body, never
        // rebuilt, must now see the values -- which is the whole assertion.
        view.start_answering(now, Some(14.5));
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                now + chrono::Duration::seconds(2),
                29.90,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");
        assert_eq!(
            payload(&state, 1, "backyard"),
            "1/backyard|29.9|14.5",
            "a handle resolved off the bus would still be None here, and the gust would \
             still be absent for the life of the process"
        );
        assert!(
            state
                .get(&SnapshotKey::new(WebsiteId::new(1), key()))
                .expect("published")
                .staleness()
                .is_fresh(),
            "and the snapshot stops reporting Degraded, so `mw_snapshot_degraded` is a \
             signal rather than a constant"
        );
    }

    /// **A render failure must not take the component down.** Returning `Err` from the
    /// fold would revoke `LatestSnapshot`, send `api:legacy` to `Waiting`, unmount its
    /// route table and make a frozen path answer `503` for the length of the backoff —
    /// which is the outcome **D-48** forbids, and which publishing in `build` does not by
    /// itself prevent.
    ///
    /// Two properties: the batch is acknowledged, and the key keeps its previous bytes.
    #[tokio::test]
    async fn a_render_failure_keeps_the_previous_bytes_and_acknowledges_the_batch() {
        let state = state();
        let render = CountingRender::new();
        let mut body = Fold {
            folder: folder(&state, &render, None),
            clock: Arc::new(TestClock::new(at("2026-11-07T18:30:02Z"))),
        };
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:00Z"),
                29.92,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect("folded");
        assert_eq!(payload(&state, 1, "backyard"), "1/backyard|29.92|-");

        render.start_failing();
        body.readings(
            ReadingBatch::new(vec![admitted(
                "backyard",
                at("2026-11-07T18:30:04Z"),
                30.10,
            )]),
            &cx(LATEST_ID),
        )
        .await
        .expect(
            "a render failure is a counted per-key non-event; an Err here would unmount \
             the frozen route table",
        );
        assert_eq!(
            payload(&state, 1, "backyard"),
            "1/backyard|29.92|-",
            "the last good bytes stay served, which is closer to D-36 than a 503"
        );
        assert_eq!(
            state
                .get(&SnapshotKey::new(WebsiteId::new(1), key()))
                .expect("published")
                .observed_at(),
            at("2026-11-07T18:30:00Z"),
            "and the key did not half-advance"
        );
    }

    /// **The monotonic guard is inside the `rcu`, so it is an invariant and not a race.**
    /// Asserted by driving the compare-and-swap directly with two concurrent writers: a
    /// compare-then-write would let the older seed land on top of the newer fold.
    ///
    /// **`std::thread::scope`, not `tokio::spawn`.** `clippy.toml` denies
    /// `tokio::task::spawn` outside `mw-supervisor` (**D-42**: a `JoinHandle` is owned only
    /// by `TaskTable`), and nothing here needs a runtime anyway — `Folder::fold` and
    /// `SnapshotState::store` are both synchronous, so two OS threads are the shorter and
    /// more honest way to make them race.
    #[test]
    fn a_seed_racing_a_fold_can_never_move_a_key_backwards() {
        let state = state();
        let render = CountingRender::new();
        let k = SnapshotKey::new(WebsiteId::new(1), key());
        let folder = folder(&state, &render, None);

        // 64 rounds, each a seed of an older instant racing a fold of a newer one on the
        // same key. Any one round is very likely to be ordered anyway; what the repetition
        // buys is the chance that at least one lands in the window a compare-then-write
        // would lose — and under ThreadSanitizer, the `race` job watches all of them.
        for round in 0..64i64 {
            let base = at("2026-11-07T18:30:00Z") + chrono::Duration::seconds(round * 10);
            let old = base;
            let new = base + chrono::Duration::seconds(5);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    folder
                        .fold(&stored("backyard", new, 29.92), &key(), new, false)
                        .expect("rendered")
                });
                scope.spawn(|| {
                    folder
                        .fold(&stored("backyard", old, 28.00), &key(), old, true)
                        .expect("rendered")
                });
            });

            assert_eq!(
                state.get(&k).expect("published").observed_at(),
                new,
                "round {round}: the seed is older, so whatever the interleaving the key \
                 must hold the fold's instant"
            );
        }
    }

    // -----------------------------------------------------------------------------------
    // The builders
    // -----------------------------------------------------------------------------------

    /// A capability bus that records rather than routing — the same double
    /// `super::super::rollup::tests` uses.
    #[derive(Debug, Default)]
    struct RecordingBus(Mutex<Vec<(Capability, bool)>>);

    impl mw_component::CapabilityPublisher for RecordingBus {
        fn publish(
            &self,
            _provider: &ComponentId,
            capability: Capability,
            _handle: Arc<dyn std::any::Any + Send + Sync>,
        ) {
            self.0
                .lock()
                .expect("not poisoned")
                .push((capability, true));
        }

        fn revoke(&self, _provider: &ComponentId, capability: Capability) {
            self.0
                .lock()
                .expect("not poisoned")
                .push((capability, false));
        }
    }

    /// One capability slot, already published by `provider`.
    fn slot(
        capability: Capability,
        provider: &str,
        handle: Arc<dyn std::any::Any + Send + Sync>,
    ) -> (
        Capability,
        tokio::sync::watch::Receiver<Arc<mw_component::CapabilitySlot>>,
    ) {
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(mw_component::CapabilitySlot {
            providers: vec![mw_component::Published {
                provider: ComponentId::new(provider).expect("a valid id"),
                handle,
            }],
        }));
        // The sender outlives the receiver for as long as the test needs it, which is the
        // one call to `build` below. Leaked rather than held, because a `BuildCx` owns its
        // receivers and threading the senders back out would be scaffolding with no
        // assertion attached.
        std::mem::forget(tx);
        (capability, rx)
    }

    fn build_cx(
        id: &str,
        needs: Vec<Need>,
        provides: Vec<Capability>,
        slots: Vec<(
            Capability,
            tokio::sync::watch::Receiver<Arc<mw_component::CapabilitySlot>>,
        )>,
    ) -> (mw_component::BuildCx, Arc<RecordingBus>) {
        let bus = Arc::new(RecordingBus::default());
        let access = mw_component::CapabilityAccess::new(
            Arc::from(needs),
            Arc::from(provides),
            slots,
            Arc::clone(&bus) as Arc<dyn mw_component::CapabilityPublisher>,
        );
        (mw_component::BuildCx::new(cx(id), access), bus)
    }

    /// **D-48's precondition at its source: the handle is published by `build`**, before any
    /// reading has been folded and whether or not `WindowedView` is available.
    ///
    /// `snapshot_degraded.rs` asserts the consequence through the real supervisor — the
    /// frozen route table's owner reaches `Running` — and this asserts the cause, in the one
    /// line that would be easy to "tidy" into the first successful fold.
    #[tokio::test]
    async fn the_latest_builder_publishes_before_the_first_reading() {
        let state = state();
        let registry = MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>;
        let render = CountingRender::new() as Arc<dyn SnapshotRender>;
        let spec = latest_spec(Arc::clone(&state), registry, render, None).expect("valid");

        let (build, bus) = build_cx(
            LATEST_ID,
            vec![Need::soft(Capability::WindowedView)],
            vec![Capability::LatestSnapshot],
            // No slot for `WindowedView`: the rollup has not published, which is the
            // cold-start case.
            Vec::new(),
        );
        let body = match &spec.build {
            mw_component::Builder::OnReading(f) => f(build).await.expect("builds"),
            other => panic!("expected an OnReading builder, got {other:?}"),
        };
        drop(body);

        assert_eq!(
            bus.0.lock().expect("not poisoned").as_slice(),
            &[(Capability::LatestSnapshot, true)],
            "published once, in build, with an empty table and no window provider"
        );
        assert_eq!(
            state.populated(),
            0,
            "and the table it published is empty, so `/latest` answers its documented 404"
        );
    }

    /// `snapshot:warm`'s builder resolves the store **as the read half**, which is the
    /// upcast W2a §4.1b's `H` against `H W` is expressed as: after `build` the body holds
    /// an `Arc<dyn ReadingStore>` and cannot reach `ReadingSink::store` even though both
    /// resolved `Capability::ReadingStore`.
    #[tokio::test]
    async fn the_warm_builder_upcasts_the_store_to_its_read_half() {
        let state = state();
        let registry = MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>;
        let render = CountingRender::new() as Arc<dyn SnapshotRender>;
        let spec = warm_spec(Arc::clone(&state), registry, render, None).expect("valid");

        let store = FakeStore::with(vec![stored("backyard", at("2026-11-07T18:29:00Z"), 29.92)]);
        let handle: Arc<dyn Store> = store as Arc<dyn Store>;
        let (build, bus) = build_cx(
            WARM_ID,
            vec![
                Need::hard(Capability::ReadingStore),
                Need::soft(Capability::WindowedView),
            ],
            Vec::new(),
            vec![slot(
                Capability::ReadingStore,
                "db:postgres",
                Arc::new(handle) as Arc<dyn std::any::Any + Send + Sync>,
            )],
        );
        let mut body = match &spec.build {
            mw_component::Builder::Tick(f) => f(build).await.expect("the store is published"),
            other => panic!("expected a Tick builder, got {other:?}"),
        };
        assert!(
            bus.0.lock().expect("not poisoned").is_empty(),
            "this component provides nothing, so it publishes nothing"
        );

        // It reads. The clock is the system one here, so the seed only proves the handle
        // was resolved and the query was issued -- the recency assertions are above, on a
        // stopped clock.
        body.tick(&cx(WARM_ID)).await.expect("the store answers");
    }
}

// ---------------------------------------------------------------------------------------
// The declaration table
// ---------------------------------------------------------------------------------------

/// [`WINDOWS`] is reviewable against W3 §2a.6's table, which is the property a declaration
/// list exists to have. The two that would be wrong in a way nothing else catches are
/// asserted by name.
#[test]
fn the_declaration_table_matches_the_contract() {
    assert_eq!(WINDOWS.len(), 9);
    assert!(
        WINDOWS.contains(&(
            MetricKey::WindSpeed,
            Window::Rolling(Window::TEN_MINUTES),
            Agg::Max
        )),
        "wind gust is the maximum over ten minutes, not a mean"
    );
    assert!(
        WINDOWS.contains(&(
            MetricKey::RainIncremental,
            Window::PerHour(Window::TEN_MINUTES),
            Agg::Sum
        )),
        "the rain rate is `sum(rainincremental) over 10 minutes x 6` and NOT the rainrate \
         column the station reported -- handlers.go:344 overwrites it, so the contract \
         promises the extrapolation"
    );
    assert!(
        !WINDOWS.iter().any(|(m, _, _)| *m == MetricKey::RainRate),
        "reading the reported column would be a bytes-visible change"
    );
    // Every maintained window the rollup keeps for a total metric is asked for, so the
    // declaration table and the provider's `MAINTAINED` list cannot drift apart silently.
    for window in crate::rollup::state::MAINTAINED {
        assert!(
            WINDOWS
                .iter()
                .any(|(m, w, a)| *m == MetricKey::RainIncremental
                    && *w == window
                    && *a == Agg::Sum),
            "{window} is maintained for every total metric and nothing reads it"
        );
    }
}

/// **The anti-drift check two doc comments name and nobody had written.**
///
/// `provides_latest_snapshot` is a second statement of a condition `register` already owns,
/// and `crates/mw-api-legacy` plans itself on the answer. The two were not the same: the
/// predicate ended `(has_reading_stream() || has_weather_database())` while only
/// `latest_spec` provides the capability and it is pushed only on `has_reading_stream()`.
/// A node with a weather database and no spool therefore got `api:legacy` planned with a
/// hard need nobody provides, and `plan()` refused the whole configuration -- a daemon that
/// exits before `READY=1`, on the shape W7 §8.2 calls a serve-only node.
///
/// Over every combination of the conditions rather than a sample, because the defect was in
/// one corner of it.
#[test]
fn the_predicate_agrees_with_what_register_plans() {
    for bits in 0..32u8 {
        let cfg = Cfg {
            render: (bits & 1 != 0).then(|| CountingRender::new() as Arc<dyn SnapshotRender>),
            state: (bits & 2 != 0).then(state),
            registry: (bits & 4 != 0)
                .then(|| MemoryRegistry::fixed("backyard") as Arc<dyn StationRegistry>),
            stream: bits & 8 != 0,
            db: bits & 16 != 0,
            windows: None,
        };
        let mut specs = Vec::new();
        // `register` returns an error only for a configuration it considers malformed
        // rather than unplanned; either way nothing provides the capability.
        let planned = register(&cfg, &mut specs).is_ok()
            && specs.iter().any(|s| {
                s.provides
                    .iter()
                    .any(|p| p.capability == Capability::LatestSnapshot)
            });
        assert_eq!(
            provides_latest_snapshot(&cfg),
            planned,
            "bits {bits:05b} (render/state/registry/stream/db): the predicate says {} and \
             register plans {}. `api:legacy` declares a hard LatestSnapshot on the \
             predicate's word, so a `true` here with no provider is a daemon that refuses \
             to start.",
            provides_latest_snapshot(&cfg),
            planned
        );
    }
}

/// The corner that shipped, named on its own so a regression says what it is.
#[test]
fn a_serve_only_node_does_not_claim_a_latest_snapshot_provider() {
    let cfg = Cfg {
        stream: false,
        db: true,
        ..Cfg::full()
    };
    assert_eq!(
        ids(&cfg),
        vec![WARM_ID],
        "the seed, and nothing that provides"
    );
    assert!(
        !provides_latest_snapshot(&cfg),
        "`snapshot:warm` writes the table and provides nothing: the capability is Exclusive \
         and two providers would be a plan error"
    );
}
