//! The two specs, and the tick over a store that answers from memory.
//!
//! Nothing here needs a database. What a database *is* needed for is the half of M3-4's
//! first done-criterion that this cannot assert — that `observations_1m`'s bucket state
//! really is the sum and count of the raw readings — and that lives in
//! `crates/mw-store/tests/db.rs`, behind the `db-tests` gate, which fails rather than skips
//! when there is no server (W3 Decision 84).

use super::*;

fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn key() -> StationKey {
    StationKey::new("backyard")
}

// ---------------------------------------------------------------------------------------
// The specs
// ---------------------------------------------------------------------------------------

#[test]
fn the_rollup_spec_reads_the_store_and_provides_the_windowed_view() {
    let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
    let spec = spec(state).expect("a valid component id");
    assert_eq!(spec.id.as_str(), ID);
    match &spec.trigger {
        Trigger::Every {
            period,
            deadline,
            first,
            phase,
            floor,
        } => {
            assert_eq!(*period, PERIOD, "W2a §4.1's 60 seconds");
            assert_eq!(*deadline, DEADLINE, "W2a assumption 14's 10 seconds");
            assert_eq!(
                *first,
                FirstRun::Immediate,
                "a restart must not leave every uploader Waiting for a period"
            );
            assert_eq!(
                *phase,
                Phase::Fixed(PHASE),
                "five seconds past the boundary, after W4 D-26's measured 3.06-3.22 s \
                 refresh lag -- see the module documentation"
            );
            assert_eq!(*floor, Duration::ZERO);
            assert!(deadline < period, "plan() checks this; so does this test");
        }
        other => panic!("expected Every, got {other:?}"),
    }
    assert_eq!(
        spec.requires,
        vec![Need::hard(Capability::ReadingStore)],
        "W2a §4.1 writes it as ReadingStore H -- read, not write"
    );
    assert_eq!(spec.provides, vec![Provision::of(Capability::WindowedView)]);
}

#[test]
fn the_rollover_spec_is_a_daily_calendar_in_the_stations_zone_and_needs_nothing() {
    let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
    let spec = rollover_spec(state, key(), Some(zone("America/Denver"))).expect("valid");
    assert_eq!(spec.id.as_str(), "rain:rollover:backyard");
    match &spec.trigger {
        Trigger::Calendar {
            spec: cron,
            tz,
            misfire,
            deadline,
        } => {
            assert_eq!(*cron, CronSpec::daily_at(LocalTime::MIDNIGHT));
            assert_eq!(
                cron.slots_per_day(),
                1,
                "one slot, so the key is the local date"
            );
            assert_eq!(cron.problem(), None);
            assert_eq!(tz.station().map(|s| s.to_string()), Some("backyard".into()));
            assert_eq!(tz.zone().map(|z| z.name()), Some("America/Denver"));
            assert_eq!(
                *misfire,
                Misfire::DEFAULT,
                "RunLatest with a 24-hour grace is what makes a restart at 14:00 local \
                 still know when the day began"
            );
            assert_eq!(*deadline, ROLLOVER_DEADLINE);
        }
        other => panic!("expected Calendar, got {other:?}"),
    }
    assert!(
        spec.requires.is_empty(),
        "the boundary is a function of a zone and the clock; a rollover that declared \
         ReadingStore would be an hour late after a database outage"
    );
    assert!(spec.provides.is_empty());
}

/// **A station with no zone is a plan error naming it.** The component is emitted and
/// `plan()` refuses it, rather than the registrar skipping it — which would be a station
/// whose rainfall totals silently roll over in the server's zone, with no error and no log
/// line.
#[test]
fn a_station_with_no_zone_produces_a_component_plan_refuses() {
    let state = Arc::new(RollupState::new([(key(), None)]));
    let spec = rollover_spec(state, key(), None).expect("the id is still valid");
    let Trigger::Calendar { tz, .. } = &spec.trigger else {
        panic!("expected Calendar");
    };
    assert_eq!(tz.zone(), None, "which is what plan() refuses on");
    // The message `plan()` would render, asserted here because this crate is where the
    // decision to emit the component anyway was taken.
    let e = mw_component::PlanError::MissingTimeZone {
        id: spec.id.clone(),
        station: tz.station().cloned(),
    };
    let text = e.to_string();
    assert!(text.contains("rain:rollover:backyard"), "{text}");
    assert!(text.contains("`backyard`"), "{text}");
    assert!(text.contains("config.stations.timezone"), "{text}");
}

/// The digest is what makes a re-plan restart the rollup when a station is added or a zone
/// corrected, and not otherwise.
#[test]
fn the_rollup_digest_follows_the_station_set_and_their_zones() {
    let of = |stations: Vec<(StationKey, Option<Zone>)>| {
        spec(Arc::new(RollupState::new(stations)))
            .expect("valid")
            .config_digest
    };
    let denver = Some(zone("America/Denver"));
    let havana = Some(zone("America/Havana"));
    assert_eq!(of(vec![(key(), denver)]), of(vec![(key(), denver)]));
    assert_ne!(of(vec![(key(), denver)]), of(vec![(key(), havana)]));
    assert_ne!(of(vec![(key(), denver)]), of(vec![(key(), None)]));
    assert_ne!(
        of(vec![(key(), denver)]),
        of(vec![(key(), denver), (StationKey::new("roof"), denver)])
    );
}

// ---------------------------------------------------------------------------------------
// The registrar
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
struct Cfg {
    database: bool,
    state: Option<Arc<RollupState>>,
    stations: Vec<(StationKey, Option<Zone>)>,
}

impl RollupConfig for Cfg {
    fn has_weather_database(&self) -> bool {
        self.database
    }
    fn rollup_state(&self) -> Option<&Arc<RollupState>> {
        self.state.as_ref()
    }
    fn rollup_stations(&self) -> Vec<(StationKey, Option<Zone>)> {
        self.stations.clone()
    }
}

#[test]
fn the_registrar_emits_one_rollup_and_one_rollover_per_station() {
    let stations = vec![
        (key(), Some(zone("America/Denver"))),
        (StationKey::new("roof"), Some(zone("America/Havana"))),
    ];
    let cfg = Cfg {
        database: true,
        state: Some(Arc::new(RollupState::new(stations.clone()))),
        stations,
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("two stations with zones");
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "window:rollup",
            "rain:rollover:backyard",
            "rain:rollover:roof"
        ],
        "one accumulator for the node and one boundary per station"
    );
}

#[test]
fn a_node_with_no_weather_database_plans_no_rollup() {
    let cfg = Cfg {
        database: false,
        state: None,
        stations: vec![(key(), Some(zone("America/Denver")))],
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("no database, no rollup");
    assert!(
        out.is_empty(),
        "D-44: no db:postgres, no hard-need provider"
    );
}

#[test]
fn a_node_that_collects_for_no_station_plans_no_rollup_and_is_not_an_error() {
    let cfg = Cfg {
        database: true,
        state: Some(Arc::new(RollupState::new([]))),
        stations: Vec::new(),
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("a node that serves and does not collect");
    assert!(out.is_empty());
}

#[test]
fn a_boot_sequence_that_forgot_the_state_is_a_named_registrar_failure() {
    let cfg = Cfg {
        database: true,
        state: None,
        stations: vec![(key(), Some(zone("America/Denver")))],
    };
    let mut out = Vec::new();
    let e = register(&cfg, &mut out).expect_err("the state was never built");
    assert_eq!(e, RollupPlanError::NoState);
    assert!(e.to_string().contains("boot.rs"), "{e}");
}

/// The parts that need a real `tokio` runtime, and therefore cannot run under `--cfg loom`.
///
/// That flag applies to every crate in the graph, and `tokio` gates its whole `net` module
/// behind `#![cfg(not(loom))]` — so `runtime::Builder::build` asserts the I/O driver is
/// disabled and a `#[tokio::test]` panics before its body runs. `super::super::writer`'s
/// own `adapter` module carries the same guard for the same reason, and
/// `.github/workflows/pr.yml`'s loom job explains the flag's reach in full.
///
/// Nothing excluded here is a concurrency model. The lock-free half of this item —
/// `RollupState`'s `ArcSwap` publish and its `AtomicU64` — is in `super::state`, whose
/// tests are synchronous and **do** run under loom.
#[cfg(not(loom))]
mod driven {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use mw_component::{Bucket, Bucketed, StoreError};
    use mw_types::Agg;

    use crate::clock::TestClock;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    /// A `ReadingStore` that answers from memory, so the tick's arithmetic and its query shapes
    /// are both assertable without a server.
    #[derive(Debug, Default)]
    struct FakeStore {
        /// 1-minute buckets, oldest first.
        buckets: Vec<Bucket>,
        /// The hourly rain series the storm walk reads.
        series: Vec<Bucketed>,
        /// `rainincremental` per minute, which every `totals` piece is summed from — so the
        /// fake's answer is derived from one set of facts rather than being a second script the
        /// test has to keep consistent.
        rain: BTreeMap<DateTime<Utc>, f64>,
        /// Every query this store was asked, for the assertions about shape.
        asked: Mutex<Vec<String>>,
    }

    impl ReadingStore for FakeStore {
        fn buckets<'a>(&'a self, q: BucketQuery) -> BoxFuture<'a, Result<Vec<Bucket>, StoreError>> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push(format!("buckets since={} limit={}", q.since, q.limit));
            let out: Vec<Bucket> = self
                .buckets
                .iter()
                .filter(|b| b.bucket >= q.since)
                .take(q.limit)
                .cloned()
                .collect();
            Box::pin(async move { Ok(out) })
        }

        fn totals<'a>(
            &'a self,
            q: TotalsQuery,
        ) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
            for p in &q.pieces {
                self.asked
                    .lock()
                    .expect("not poisoned")
                    .push(format!("totals {} {} {}", p.level, p.from, p.until));
            }
            // Summed from the minute-level facts whatever level the piece names, which is the
            // decomposition's own exactness claim turned into the fake's behaviour: a correct
            // caller gets the same answer however it tiles the range.
            let out: Vec<Option<f64>> = q
                .pieces
                .iter()
                .map(|p| {
                    self.rain.range(p.from..p.until).map(|(_, v)| *v).fold(
                        None,
                        |acc: Option<f64>, v| {
                            Some(match acc {
                                Some(a) => a + v,
                                None => v,
                            })
                        },
                    )
                })
                .collect();
            Box::pin(async move { Ok(out) })
        }

        fn series<'a>(
            &'a self,
            q: SeriesQuery,
        ) -> BoxFuture<'a, Result<Vec<Bucketed>, StoreError>> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push(format!("series {} since={}", q.level, q.since));
            let out: Vec<Bucketed> = self
                .series
                .iter()
                .filter(|b| b.bucket >= q.since)
                .take(q.limit)
                .copied()
                .collect();
            Box::pin(async move { Ok(out) })
        }

        /// `window:rollup` never calls this -- the raw hypertable is the writer's business
        /// and this component reads composable aggregate state (`mw_component::store`).
        /// Recorded in `asked` rather than panicking, so that a tick which started reading
        /// raw rows fails an assertion here instead of aborting the test process.
        fn latest<'a>(
            &'a self,
            q: mw_component::LatestQuery,
        ) -> BoxFuture<'a, Result<Option<mw_types::Reading>, StoreError>> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push(format!("latest {}", q.station));
            Box::pin(async { Ok(None) })
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

    /// A bucket with the named 1-minute columns set, looked up through the registry.
    fn bucket(start: DateTime<Utc>, set: &[(&str, f64)]) -> Bucket {
        let columns = LEVELS
            .iter()
            .find(|l| l.name == "1m")
            .map(|l| l.columns)
            .expect("the 1-minute level");
        let mut values = vec![None; columns.len()];
        for (name, v) in set {
            let i = columns
                .iter()
                .position(|c| c.column == *name)
                .unwrap_or_else(|| panic!("{name} is not a 1-minute column"));
            values[i] = Some(*v);
        }
        Bucket {
            bucket: start,
            values,
        }
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

    /// `window:rollup`'s body, over a fake store and a stopped clock, with its publisher wired
    /// to a bus that records.
    fn rollup(
        store: Arc<FakeStore>,
        state: Arc<RollupState>,
        now: DateTime<Utc>,
    ) -> (Rollup, Arc<RecordingBus>) {
        use mw_component::{CapabilityAccess, CapabilityPublisher};
        let bus = Arc::new(RecordingBus::default());
        let access = CapabilityAccess::new(
            Arc::from(vec![Need::hard(Capability::ReadingStore)]),
            Arc::from(vec![Capability::WindowedView]),
            Vec::new(),
            Arc::clone(&bus) as Arc<dyn CapabilityPublisher>,
        );
        let build = mw_component::BuildCx::new(cx(ID), access);
        let publisher = build
            .publisher::<Arc<dyn WindowedView>>()
            .expect("the spec declares this provision");
        (
            Rollup {
                store: store as Arc<dyn ReadingStore>,
                state,
                publisher,
                clock: Arc::new(TestClock::new(now)),
                published: false,
            },
            bus,
        )
    }

    /// A capability bus that records rather than routing.
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

    // ---------------------------------------------------------------------------------------
    // The tick
    // ---------------------------------------------------------------------------------------

    /// **M3-4's first done-criterion, in the component.** Ten minutes of buckets with uneven
    /// counts resolve to the mean of the underlying readings. The database half of the same
    /// claim — that the buckets really are the sum and count of the raw rows — is
    /// `crates/mw-store/tests/db.rs`.
    #[tokio::test]
    async fn a_ten_minute_mean_over_the_view_is_the_mean_of_the_readings() {
        let now = at("2026-11-06T15:00:05Z");
        let buckets: Vec<Bucket> = (0..10)
            .map(|k| {
                let n = f64::from(k + 1);
                bucket(
                    at("2026-11-06T14:50:00Z") + chrono::Duration::minutes(i64::from(k)),
                    &[("outtemp_sum", 10.0 * n * n), ("outtemp_count", n)],
                )
            })
            .collect();
        let store = Arc::new(FakeStore {
            buckets,
            ..FakeStore::default()
        });
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
        let (mut body, bus) = rollup(Arc::clone(&store), Arc::clone(&state), now);

        body.tick(&cx(ID)).await.expect("the fake store answers");

        assert_eq!(
            bus.0.lock().expect("not poisoned").as_slice(),
            [(Capability::WindowedView, true)],
            "published on the first completed tick, not in build (W2a Decision 22)"
        );
        let declaration = (
            MetricKey::OutTemp,
            Window::of(Window::TEN_MINUTES),
            Agg::Mean,
        );
        let view: Arc<dyn WindowedView> = state as Arc<dyn WindowedView>;
        let got = view
            .resolve(&key(), &[declaration])
            .expect("a tick has completed")
            .value(declaration)
            .expect("declared")
            .expect("ten buckets of readings");
        assert!((got - 70.0).abs() < 1e-12, "Σsum/Σcount is 70, got {got}");
    }

    /// The refresh is incremental: the first tick asks for the hour and the second asks only
    /// for what it does not have. That is what keeps a 624-column read from being a megabyte a
    /// minute, and it is checkable from the queries the store was asked.
    #[tokio::test]
    async fn the_second_tick_asks_only_for_the_buckets_it_does_not_hold() {
        let store = Arc::new(FakeStore {
            buckets: (0..5)
                .map(|k| {
                    bucket(
                        at("2026-11-06T14:55:00Z") + chrono::Duration::minutes(k),
                        &[("windspeed_max", 10.0 + k as f64)],
                    )
                })
                .collect(),
            ..FakeStore::default()
        });
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
        let (mut body, _) = rollup(
            Arc::clone(&store),
            Arc::clone(&state),
            at("2026-11-06T15:00:05Z"),
        );
        body.tick(&cx(ID)).await.expect("answers");
        body.clock = Arc::new(TestClock::new(at("2026-11-06T15:01:05Z")));
        body.tick(&cx(ID)).await.expect("answers");

        let asked = store.asked.lock().expect("not poisoned");
        let bucket_queries: Vec<&String> =
            asked.iter().filter(|q| q.starts_with("buckets")).collect();
        assert_eq!(bucket_queries.len(), 2);
        assert!(
            bucket_queries[0].contains("since=2026-11-06 14:00:00 UTC"),
            "the first tick asks for the whole hour: {}",
            bucket_queries[0]
        );
        assert!(
            bucket_queries[1].contains("since=2026-11-06 15:00:00 UTC"),
            "the second asks for the minute after the newest it holds: {}",
            bucket_queries[1]
        );
    }

    /// The maintained rainfall windows, with the boundary the rollover published. The day total
    /// is the rain since the station's **local** midnight, which is seven hours of rain later
    /// than the server's.
    #[tokio::test]
    async fn the_day_total_accumulates_from_the_stations_local_midnight() {
        let now = at("2026-11-06T15:00:05Z");
        let until = at("2026-11-06T15:00:00Z");
        // A tenth of an inch in every hour of 2026-11-06, UTC.
        let rain: BTreeMap<DateTime<Utc>, f64> = (0..15)
            .map(|h| (at("2026-11-06T00:00:00Z") + chrono::Duration::hours(h), 0.1))
            .collect();
        let store = Arc::new(FakeStore {
            rain,
            ..FakeStore::default()
        });
        let denver = zone("America/Denver");
        let state = Arc::new(RollupState::new([(key(), Some(denver))]));
        // The rollover runs first, which is what the supervisor's misfire handling does on a
        // start: 2026-11-06 in Denver began at 07:00Z.
        assert!(matches!(
            state.roll_over(&key(), now),
            Rollover::Rolled { .. }
        ));
        assert_eq!(
            state.boundaries(&key()).expect("published").day,
            at("2026-11-06T07:00:00Z")
        );

        let (mut body, _) = rollup(Arc::clone(&store), Arc::clone(&state), now);
        body.tick(&cx(ID)).await.expect("answers");

        let day = (MetricKey::RainIncremental, Window::Day, Agg::Sum);
        let view: Arc<dyn WindowedView> = Arc::clone(&state) as Arc<dyn WindowedView>;
        let resolved = view.resolve(&key(), &[day]).expect("a tick has completed");
        let got = resolved.value(day).expect("declared").expect("it rained");
        // 07:00Z through 14:00Z inclusive is eight hourly tenths.
        assert!((got - 0.8).abs() < 1e-9, "expected 0.8 inches, got {got}");
        assert_eq!(resolved.as_of(), now);
        assert_eq!(until, at("2026-11-06T15:00:00Z"));
    }

    /// Before the rollover's first occurrence there is no boundary, so the day total is `None`
    /// — **not** zero. "We do not know when this station's day began" and "no rain has fallen
    /// today" are different facts, and `COALESCE(SUM(...), 0)` is what makes them the same one.
    #[tokio::test]
    async fn a_day_with_no_boundary_yet_is_absent_rather_than_zero() {
        let now = at("2026-11-06T15:00:05Z");
        let rain: BTreeMap<DateTime<Utc>, f64> = (0..15)
            .map(|h| (at("2026-11-06T00:00:00Z") + chrono::Duration::hours(h), 0.1))
            .collect();
        let store = Arc::new(FakeStore {
            rain,
            ..FakeStore::default()
        });
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
        let (mut body, _) = rollup(Arc::clone(&store), Arc::clone(&state), now);
        body.tick(&cx(ID)).await.expect("answers");

        let day = (MetricKey::RainIncremental, Window::Day, Agg::Sum);
        let view: Arc<dyn WindowedView> = Arc::clone(&state) as Arc<dyn WindowedView>;
        assert_eq!(
            view.resolve(&key(), &[day])
                .expect("a tick has completed")
                .value(day),
            Ok(None)
        );
        // The 24-hour window needs no boundary, so it is answered on the same tick.
        let day24 = (
            MetricKey::RainIncremental,
            Window::Rolling(Duration::from_secs(24 * 3600)),
            Agg::Sum,
        );
        let got = view
            .resolve(&key(), &[day24])
            .expect("a tick has completed")
            .value(day24)
            .expect("declared");
        assert!(got.is_some_and(|v| (v - 1.5).abs() < 1e-9), "{got:?}");
    }

    /// The storm window, end to end through the tick: the series says it has been raining since
    /// 11:00Z, so the storm total is the rain since then and the window's start is that hour.
    #[tokio::test]
    async fn the_storm_total_is_the_rain_since_the_event_began() {
        let now = at("2026-11-06T15:00:05Z");
        let rain: BTreeMap<DateTime<Utc>, f64> = (11..15)
            .map(|h| {
                (
                    at("2026-11-06T00:00:00Z") + chrono::Duration::hours(h),
                    0.25,
                )
            })
            .collect();
        let series: Vec<Bucketed> = (0..48)
            .map(|h| {
                let b = at("2026-11-04T15:00:00Z") + chrono::Duration::hours(h);
                Bucketed {
                    bucket: b,
                    total: rain.get(&b).copied(),
                }
            })
            .collect();
        let store = Arc::new(FakeStore {
            rain,
            series,
            ..FakeStore::default()
        });
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
        let (mut body, _) = rollup(Arc::clone(&store), Arc::clone(&state), now);
        body.tick(&cx(ID)).await.expect("answers");

        assert_eq!(
            state.windows(&key()).expect("a station").storm_start,
            Some(at("2026-11-06T11:00:00Z"))
        );
        let storm = (MetricKey::RainIncremental, Window::Storm, Agg::Sum);
        let view: Arc<dyn WindowedView> = Arc::clone(&state) as Arc<dyn WindowedView>;
        let got = view
            .resolve(&key(), &[storm])
            .expect("a tick has completed")
            .value(storm)
            .expect("declared")
            .expect("it is raining");
        assert!((got - 1.0).abs() < 1e-9, "four quarter-inches, got {got}");
    }

    /// The rain rate is the frozen contract's figure: the ten-minute total extrapolated to an
    /// hour. `handlers.go:344` overwrites the station's own `rainrate` column with it, so this
    /// is what the contract promises and the column is not.
    #[tokio::test]
    async fn the_rain_rate_is_the_ten_minute_total_extrapolated_to_an_hour() {
        let now = at("2026-11-06T15:00:05Z");
        // 0.01 inches in each of the last ten minutes: 0.1 in ten minutes, 0.6 in an hour.
        let buckets: Vec<Bucket> = (0..10)
            .map(|k| {
                bucket(
                    at("2026-11-06T14:50:00Z") + chrono::Duration::minutes(k),
                    &[("rainincremental_total", 0.01)],
                )
            })
            .collect();
        let store = Arc::new(FakeStore {
            buckets,
            ..FakeStore::default()
        });
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
        let (mut body, _) = rollup(Arc::clone(&store), Arc::clone(&state), now);
        body.tick(&cx(ID)).await.expect("answers");

        let rate = (
            MetricKey::RainIncremental,
            Window::PerHour(Window::TEN_MINUTES),
            Agg::Sum,
        );
        let view: Arc<dyn WindowedView> = Arc::clone(&state) as Arc<dyn WindowedView>;
        let got = view
            .resolve(&key(), &[rate])
            .expect("a tick has completed")
            .value(rate)
            .expect("declared")
            .expect("it is raining");
        assert!(
            (got - 0.6).abs() < 1e-9,
            "0.1 in ten minutes is 0.6 in/h, got {got}"
        );
    }

    /// A store failure is a tick failure naming the action, which the supervisor turns into
    /// `Failed` + `Backoff`. The previous state stays published, which is W3 §2a.6's graceful
    /// degradation: the window-derived values keep their last good value.
    #[tokio::test]
    async fn a_store_failure_leaves_the_last_good_windows_in_place() {
        #[derive(Debug)]
        struct Broken;
        impl ReadingStore for Broken {
            fn buckets<'a>(
                &'a self,
                _q: BucketQuery,
            ) -> BoxFuture<'a, Result<Vec<Bucket>, StoreError>> {
                Box::pin(async move {
                    Err(StoreError::unavailable(std::io::Error::other(
                        "connection reset by peer",
                    )))
                })
            }
            fn totals<'a>(
                &'a self,
                _q: TotalsQuery,
            ) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
                Box::pin(async move { Ok(Vec::new()) })
            }
            fn series<'a>(
                &'a self,
                _q: SeriesQuery,
            ) -> BoxFuture<'a, Result<Vec<Bucketed>, StoreError>> {
                Box::pin(async move { Ok(Vec::new()) })
            }
            /// Not reachable from this component; see `FakeStore`'s own note.
            fn latest<'a>(
                &'a self,
                _q: mw_component::LatestQuery,
            ) -> BoxFuture<'a, Result<Option<mw_types::Reading>, StoreError>> {
                Box::pin(async move { Ok(None) })
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

        let now = at("2026-11-06T15:00:05Z");
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Denver")))]));
        let gust = (
            MetricKey::WindSpeed,
            Window::of(Window::TEN_MINUTES),
            Agg::Max,
        );
        // A good tick first, so there is a last good value to keep.
        let store = Arc::new(FakeStore {
            buckets: vec![bucket(
                at("2026-11-06T14:59:00Z"),
                &[("windspeed_max", 31.5)],
            )],
            ..FakeStore::default()
        });
        let (mut body, _) = rollup(store, Arc::clone(&state), now);
        body.tick(&cx(ID)).await.expect("answers");

        let (mut broken, _) = {
            use mw_component::{CapabilityAccess, CapabilityPublisher};
            let bus = Arc::new(RecordingBus::default());
            let access = CapabilityAccess::new(
                Arc::from(vec![Need::hard(Capability::ReadingStore)]),
                Arc::from(vec![Capability::WindowedView]),
                Vec::new(),
                Arc::clone(&bus) as Arc<dyn CapabilityPublisher>,
            );
            let build = mw_component::BuildCx::new(cx(ID), access);
            let publisher = build
                .publisher::<Arc<dyn WindowedView>>()
                .expect("declared");
            (
                Rollup {
                    store: Arc::new(Broken) as Arc<dyn ReadingStore>,
                    state: Arc::clone(&state),
                    publisher,
                    clock: Arc::new(TestClock::new(now + chrono::Duration::minutes(1))),
                    published: true,
                },
                bus,
            )
        };
        let e = broken.tick(&cx(ID)).await.expect_err("the store is down");
        let chain = mw_types::DisplayChain(&e).to_string();
        assert!(chain.contains("could not be reached"), "{chain}");

        let view: Arc<dyn WindowedView> = state as Arc<dyn WindowedView>;
        assert_eq!(
            view.resolve(&key(), &[gust])
                .expect("the last good windows are still published")
                .value(gust),
            Ok(Some(31.5))
        );
    }

    /// Two stations are resolved against **one** instant, so a window read for one and a window
    /// read for the other in the same tick cover the same minutes. A tick that asked the clock
    /// per station would straddle a minute boundary once a minute at 0.5 Hz.
    #[tokio::test]
    async fn one_tick_resolves_every_station_against_the_same_minute() {
        let now = at("2026-11-06T15:00:05Z");
        let roof = StationKey::new("roof");
        let store = Arc::new(FakeStore {
            buckets: vec![bucket(
                at("2026-11-06T14:59:00Z"),
                &[("windspeed_max", 12.0)],
            )],
            ..FakeStore::default()
        });
        let state = Arc::new(RollupState::new([
            (key(), Some(zone("America/Denver"))),
            (roof.clone(), Some(zone("America/Havana"))),
        ]));
        let (mut body, _) = rollup(store, Arc::clone(&state), now);
        body.tick(&cx(ID)).await.expect("answers");

        let a = state.windows(&key()).expect("a station");
        let b = state.windows(&roof).expect("a station");
        assert_eq!(a.until, b.until);
        assert_eq!(a.until, Some(at("2026-11-06T15:00:00Z")));
        assert_eq!(a.as_of, b.as_of);
    }

    // ---------------------------------------------------------------------------------------
    // The rollover body
    // ---------------------------------------------------------------------------------------

    /// The rollover's body over the two awkward instants of a zone whose transition is at local
    /// midnight: the first occurrence rolls, the second pass through the same local midnight is
    /// a no-op, and the counter says one.
    #[tokio::test]
    async fn the_rollovers_body_is_idempotent_across_an_ambiguous_midnight() {
        let state = Arc::new(RollupState::new([(key(), Some(zone("America/Havana")))]));
        let mut body = RainRollover {
            state: Arc::clone(&state),
            station: key(),
            clock: Arc::new(TestClock::new(at("2026-11-01T04:00:00Z"))),
        };
        body.tick(&cx("rain:rollover:backyard"))
            .await
            .expect("the first pass rolls over");
        assert_eq!(state.rollovers(&key()), 1);

        body.clock = Arc::new(TestClock::new(at("2026-11-01T05:00:00Z")));
        body.tick(&cx("rain:rollover:backyard"))
            .await
            .expect("the second pass is a no-op and not a failure");
        assert_eq!(
            state.rollovers(&key()),
            1,
            "00:00 local happened twice and the day rolled over once"
        );

        body.clock = Arc::new(TestClock::new(at("2026-11-02T05:00:00Z")));
        body.tick(&cx("rain:rollover:backyard"))
            .await
            .expect("the next local day");
        assert_eq!(state.rollovers(&key()), 2);
    }

    /// A rollover for a station the state has no zone for fails rather than reporting success
    /// having done nothing. Unreachable through the supervisor, because `plan()` refuses the
    /// component — asserted because "it cannot happen" is what a silent `Ok` would be relying
    /// on.
    #[tokio::test]
    async fn a_rollover_with_no_zone_fails_rather_than_doing_nothing_quietly() {
        let state = Arc::new(RollupState::new([(key(), None)]));
        let mut body = RainRollover {
            state: Arc::clone(&state),
            station: key(),
            clock: Arc::new(TestClock::new(at("2026-11-06T15:00:00Z"))),
        };
        let e = body
            .tick(&cx("rain:rollover:backyard"))
            .await
            .expect_err("no zone, no boundary");
        let chain = mw_types::DisplayChain(&e).to_string();
        assert!(
            chain.contains("roll over the station's local day"),
            "{chain}"
        );
        assert!(chain.contains("config.stations.timezone"), "{chain}");
        assert_eq!(state.rollovers(&key()), 0);
    }

    /// The metrics this item owns are described and zeroed, so "no rollover has happened yet"
    /// is an assertion rather than an absent series.
    #[test]
    fn the_rollup_counters_are_registered_and_zeroed() {
        // `register()` is idempotent and this asserts the names exist in one place rather than
        // re-listing them; the recorder is installed by `mw-obs` and is absent here, so this
        // is the call not panicking plus the names being spelled once.
        crate::metrics::register();
        m::rollup_buckets_read(0);
        m::rollup_pieces_summed(0);
        m::rain_rolled_over();
    }

    /// The metrics that declare a total are what the long windows are maintained for, derived
    /// from the registry rather than listed. Two today; a third is maintained with no edit.
    #[test]
    fn the_maintained_metrics_are_the_registrys_totals() {
        let totals = total_metrics();
        assert!(
            totals.contains(&MetricKey::RainIncremental),
            "rain_incremental declares a total: {totals:?}"
        );
        for m in totals {
            let declared = LEVELS
                .iter()
                .find(|l| l.name == "1m")
                .map(|l| l.columns)
                .expect("the 1-minute level")
                .iter()
                .any(|c| c.metric == *m && c.strategy == Strategy::Total);
            assert!(declared, "{m:?} is maintained and declares no total");
        }
    }
}
