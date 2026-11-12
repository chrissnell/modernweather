//! The plan, and the four bodies against a recording store.
//!
//! The seams are the two capability handle traits, which is what `ci/layers.toml`'s ban on
//! `sqlx` and `mw-store` in this crate buys: every one of the seven numbers is asserted
//! against a constructed bucket series, with no database anywhere. Go's equivalent
//! arithmetic is four PL/pgSQL functions reachable only through a live TimescaleDB, which
//! is why none of it has a test today.

use std::collections::BTreeMap;
use std::sync::Mutex;

use mw_component::{Access, DepthEstimate, EstimateWindow, SnowTotals, Strength};

use super::*;
use crate::clock::TestClock;

// ---------------------------------------------------------------------------------------
// The configuration and the store, recorded.

struct Cfg {
    database: bool,
    stations: Vec<SnowStation>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            database: true,
            stations: vec![gauge_row("snowgauge", Some(1500.0))],
        }
    }
}

impl SnowConfig for Cfg {
    fn has_weather_database(&self) -> bool {
        self.database
    }
    fn snow_stations(&self) -> Vec<SnowStation> {
        self.stations.clone()
    }
}

fn denver() -> Zone {
    Zone::new("America/Denver").expect("a real zone")
}

fn gauge_row(key: &str, base_distance_mm: Option<f64>) -> SnowStation {
    SnowStation {
        key: StationKey::new(key),
        base_distance_mm,
        zone: Some(denver()),
    }
}

fn gauge(key: &str) -> Gauge {
    Gauge {
        key: StationKey::new(key),
        base_distance_mm: 1500.0,
        zone: denver(),
    }
}

/// A fixed instant with a known local date in Denver: 2026-11-12T20:00:00Z is 13:00 MST,
/// so the station's local midnight is 2026-11-12T07:00:00Z.
fn now() -> DateTime<Utc> {
    "2026-11-12T20:00:00Z"
        .parse::<DateTime<chrono::FixedOffset>>()
        .expect("an RFC 3339 instant")
        .with_timezone(&Utc)
}

/// The station's local midnight for [`now`].
fn midnight() -> DateTime<Utc> {
    units::local_midnight(now(), denver()).expect("a real midnight")
}

/// A `ReadingStore` that answers one canned series per level and records every query.
#[derive(Debug, Default)]
struct Series {
    by_level: BTreeMap<&'static str, Vec<MeanBucket>>,
    asked: Mutex<Vec<String>>,
    broken: bool,
}

impl Series {
    fn with(mut self, level: BucketLevel, buckets: Vec<MeanBucket>) -> Self {
        self.by_level.insert(level.as_str(), buckets);
        self
    }

    fn broken() -> Self {
        Self {
            broken: true,
            ..Self::default()
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("not poisoned").clone()
    }
}

impl ReadingStore for Series {
    fn buckets<'a>(
        &'a self,
        _q: mw_component::BucketQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::Bucket>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn totals<'a>(
        &'a self,
        _q: mw_component::TotalsQuery,
    ) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn series<'a>(
        &'a self,
        _q: mw_component::SeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::Bucketed>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn mean_series<'a>(
        &'a self,
        q: MeanSeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<MeanBucket>, StoreError>> {
        self.asked.lock().expect("not poisoned").push(format!(
            "{} {:?} since={} until={:?} limit={}",
            q.level, q.metric, q.since, q.until, q.limit
        ));
        if self.broken {
            return Box::pin(async {
                Err(StoreError::unavailable(std::io::Error::other(
                    "connection reset by peer",
                )))
            });
        }
        // A `let ... else` and not an `Option::map(...).unwrap_or*`, because the two checks
        // that guard this crate disagree about the latter: clippy's defaulting lint wants
        // the `Default`-valued form, and `ci/layers.toml`'s `forbidden_source` rejects that
        // spelling outright in `mw-pipeline` -- a defaulted `Option<f32>` is §1.7's
        // zero-fill with a different name. Naming the emptiness is what both are asking
        // for.
        let Some(buckets) = self.by_level.get(q.level.as_str()) else {
            // A level this test did not seed: no buckets, which is what a station that has
            // never reported at that cadence looks like.
            return Box::pin(async { Ok(Vec::new()) });
        };
        let out: Vec<MeanBucket> = buckets
            .iter()
            .filter(|b| b.bucket >= q.since && q.until.is_none_or(|end| b.bucket < end))
            .take(q.limit)
            .copied()
            .collect();
        Box::pin(async move { Ok(out) })
    }
    fn latest<'a>(
        &'a self,
        _q: mw_component::LatestQuery,
    ) -> BoxFuture<'a, Result<Option<mw_types::Reading>, StoreError>> {
        Box::pin(async { Ok(None) })
    }
    fn span<'a>(
        &'a self,
        _q: mw_component::SpanQuery,
    ) -> BoxFuture<'a, Result<mw_component::Span, StoreError>> {
        Box::pin(async {
            Err(StoreError::rejected(std::io::Error::other(
                "no snow component reads a span",
            )))
        })
    }
    fn almanac<'a>(
        &'a self,
        _q: mw_component::AlmanacQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::AlmanacRecord>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// A `SnowCache` that holds estimates in memory and records what it was told to write.
#[derive(Debug, Default)]
struct Cache {
    estimates: Mutex<BTreeMap<DateTime<Utc>, f32>>,
    patches: Mutex<Vec<SnowPatch>>,
    events: Mutex<Vec<(u32, Vec<SnowEvent>)>>,
    replacements: Mutex<Vec<(DateTime<Utc>, usize)>>,
}

impl Cache {
    fn patches(&self) -> Vec<SnowPatch> {
        self.patches.lock().expect("not poisoned").clone()
    }

    fn only_patch(&self) -> SnowPatch {
        let patches = self.patches();
        assert_eq!(patches.len(), 1, "expected exactly one write: {patches:?}");
        patches[0]
    }

    fn seed(&self, from: DateTime<Utc>, count: usize, depth_in: f32) {
        let mut estimates = self.estimates.lock().expect("not poisoned");
        for i in 0..count {
            estimates.insert(from + chrono::Duration::minutes(5 * i as i64), depth_in);
        }
    }
}

impl SnowCache for Cache {
    fn totals<'a>(
        &'a self,
        _station: &'a StationKey,
    ) -> BoxFuture<'a, Result<Option<SnowTotals>, StoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn store_totals<'a>(
        &'a self,
        _station: &'a StationKey,
        patch: SnowPatch,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        self.patches.lock().expect("not poisoned").push(patch);
        Box::pin(async { Ok(()) })
    }

    fn newest_estimate<'a>(
        &'a self,
        _station: &'a StationKey,
    ) -> BoxFuture<'a, Result<Option<DepthEstimate>, StoreError>> {
        let newest = self
            .estimates
            .lock()
            .expect("not poisoned")
            .iter()
            .next_back()
            .map(|(&bucket, &depth_in)| DepthEstimate { bucket, depth_in });
        Box::pin(async move { Ok(newest) })
    }

    fn estimate_before<'a>(
        &'a self,
        _station: &'a StationKey,
        at: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<Option<DepthEstimate>, StoreError>> {
        let before = self
            .estimates
            .lock()
            .expect("not poisoned")
            .range(..at)
            .next_back()
            .map(|(&bucket, &depth_in)| DepthEstimate { bucket, depth_in });
        Box::pin(async move { Ok(before) })
    }

    fn replace_estimates<'a>(
        &'a self,
        _station: &'a StationKey,
        from: DateTime<Utc>,
        rows: &'a [DepthEstimate],
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        self.replacements
            .lock()
            .expect("not poisoned")
            .push((from, rows.len()));
        let mut estimates = self.estimates.lock().expect("not poisoned");
        estimates.retain(|&bucket, _| bucket < from);
        for row in rows {
            estimates.insert(row.bucket, row.depth_in);
        }
        Box::pin(async { Ok(()) })
    }

    fn estimate_window<'a>(
        &'a self,
        _station: &'a StationKey,
        from: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<Option<EstimateWindow>, StoreError>> {
        let estimates = self.estimates.lock().expect("not poisoned");
        let window =
            estimates
                .range(from..)
                .fold(
                    None::<EstimateWindow>,
                    |acc, (&bucket, &depth_in)| match acc {
                        None => Some(EstimateWindow {
                            first_at: bucket,
                            first_in: depth_in,
                            min_in: depth_in,
                            max_in: depth_in,
                        }),
                        Some(mut w) => {
                            w.min_in = w.min_in.min(depth_in);
                            w.max_in = w.max_in.max(depth_in);
                            Some(w)
                        }
                    },
                );
        Box::pin(async move { Ok(window) })
    }

    fn replace_events<'a>(
        &'a self,
        _station: &'a StationKey,
        hours: u32,
        events: &'a [SnowEvent],
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        self.events
            .lock()
            .expect("not poisoned")
            .push((hours, events.to_vec()));
        Box::pin(async { Ok(()) })
    }
}

/// The `Cx` a `Tick` body is handed.
fn cx() -> Cx {
    let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
    Cx::new(
        ComponentId::new(DEPTH_ID).expect("a component id"),
        tokio_util::sync::CancellationToken::new(),
        mw_component::Generation::ZERO,
        rx,
    )
}

/// A bucket series at one cadence, from `from`, of `(distance mm)` values.
fn buckets(
    from: DateTime<Utc>,
    every: chrono::Duration,
    values: &[Option<f64>],
) -> Vec<MeanBucket> {
    values
        .iter()
        .enumerate()
        .map(|(i, &mean)| MeanBucket {
            bucket: from + every * i as i32,
            mean,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The plan.

/// Three shared components and one season total per gauge — §4.3's four rows, with the
/// `Calendar` one per-station because a station's zone is.
#[test]
fn one_gauge_is_three_shared_components_and_one_season_total() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "snow:depth",
            "snow:smoothing",
            "snow:events",
            "snow:season:snowgauge",
        ]
    );
}

/// Two gauges share the three iterating components and get one season total each — which is
/// `window:rollup`'s shape, with `rain:rollover:<station>`'s exception for the `Calendar`.
#[test]
fn two_gauges_share_three_components_and_have_two_season_totals() {
    let cfg = Cfg {
        stations: vec![
            gauge_row("snowgauge", Some(1500.0)),
            gauge_row("summit", Some(2000.0)),
        ],
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("a plannable configuration");
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "snow:depth",
            "snow:smoothing",
            "snow:events",
            "snow:season:snowgauge",
            "snow:season:summit",
        ]
    );
}

#[test]
fn no_weather_database_plans_nothing() {
    let cfg = Cfg {
        database: false,
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("not an error");
    assert!(out.is_empty(), "D-44: no database, no db:postgres");
}

#[test]
fn no_snow_gauge_plans_nothing() {
    let cfg = Cfg {
        stations: Vec::new(),
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("not an error");
    assert!(out.is_empty(), "the ordinary case");
}

/// A gauge with no base distance is **skipped** — the schema makes that state
/// representable on purpose — and the other gauge is still planned.
#[test]
fn a_gauge_with_no_base_distance_is_skipped_and_the_others_are_planned() {
    let cfg = Cfg {
        stations: vec![
            gauge_row("uncalibrated", None),
            gauge_row("snowgauge", Some(1500.0)),
        ],
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("not an error");
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "snow:depth",
            "snow:smoothing",
            "snow:events",
            "snow:season:snowgauge",
        ],
        "no season total for the uncalibrated gauge"
    );
}

/// A gauge with no base distance **and nothing else** plans nothing at all.
#[test]
fn the_only_gauge_having_no_base_distance_plans_nothing() {
    let cfg = Cfg {
        stations: vec![gauge_row("uncalibrated", None)],
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("not an error");
    assert!(out.is_empty());
}

/// A gauge with no usable zone is a refusal naming the station: "today" and "this season"
/// are both station-local (**D-24**), and a silent fallback to UTC is the defect W3 §2.5's
/// consequence 2 records for the rainfall rollovers.
#[test]
fn a_gauge_with_no_time_zone_is_refused_by_name() {
    let cfg = Cfg {
        stations: vec![SnowStation {
            key: StationKey::new("snowgauge"),
            base_distance_mm: Some(1500.0),
            zone: None,
        }],
        ..Cfg::default()
    };
    let mut out = Vec::new();
    let e = register(&cfg, &mut out).expect_err("a gauge with no local midnight");
    let SnowPlanError::MissingTimeZone { station } = &e else {
        panic!("expected a zone refusal, got {e:?}");
    };
    assert_eq!(station, "snowgauge");
    assert!(e.to_string().contains("timezone"), "the fix is named: {e}");
}

/// §4.3's triggers, as declared.
#[test]
fn the_triggers_are_the_tables() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    let trigger = |id: &str| {
        out.iter()
            .find(|s| s.id.as_str() == id)
            .expect(id)
            .trigger
            .clone()
    };
    assert!(matches!(
        trigger("snow:depth"),
        Trigger::Every { period, deadline, first: FirstRun::AtPhase, phase: Phase::Spread, .. }
            if period == DEPTH_PERIOD && deadline == DEPTH_DEADLINE
    ));
    for id in ["snow:smoothing", "snow:events"] {
        assert!(
            matches!(
                trigger(id),
                Trigger::Every { period, deadline, .. }
                    if period == SLOW_PERIOD && deadline == SLOW_DEADLINE
            ),
            "{id}"
        );
    }
    let Trigger::Calendar {
        spec,
        tz,
        misfire,
        deadline,
    } = trigger("snow:season:snowgauge")
    else {
        panic!("the season total is not a Calendar");
    };
    assert_eq!(spec, CronSpec::daily_at(SEASON_AT));
    assert_eq!(
        tz,
        TzSource::Station {
            station: StationKey::new("snowgauge"),
            zone: Some(denver()),
        }
    );
    assert_eq!(misfire, Misfire::DEFAULT);
    assert_eq!(deadline, SEASON_DEADLINE);
}

/// `SnowCache` **H W** and `ReadingStore` **H** — §4.3's table. The season total declares
/// only the first, because it is the only handle it resolves.
#[test]
fn the_needs_are_the_tables_and_the_season_total_declares_only_what_it_uses() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    for spec in &out {
        assert!(spec.provides.is_empty(), "{} provides nothing", spec.id);
        let cache = spec
            .requires
            .iter()
            .find(|n| n.capability == Capability::SnowCache)
            .unwrap_or_else(|| panic!("{} requires SnowCache", spec.id));
        assert_eq!(cache.strength, Strength::Hard, "{}", spec.id);
        assert_eq!(cache.access, Access::Write, "{}", spec.id);
        let reads_store = spec
            .requires
            .iter()
            .any(|n| n.capability == Capability::ReadingStore);
        if spec.id.as_str().starts_with(SEASON_PREFIX) {
            assert!(
                !reads_store,
                "the season total reads the estimate series through SnowCache alone"
            );
            assert_eq!(spec.requires.len(), 1);
        } else {
            assert!(reads_store, "{} reads buckets", spec.id);
            assert_eq!(spec.requires.len(), 2);
        }
    }
}

/// A digest that did not move is a component the plan diff does not restart.
#[test]
fn the_digest_follows_the_gauges_their_calibration_and_their_zones() {
    let digest_of = |cfg: &Cfg| {
        let mut out = Vec::new();
        register(cfg, &mut out).expect("a plannable configuration");
        out.iter()
            .find(|s| s.id.as_str() == DEPTH_ID)
            .expect("the depth refresh")
            .config_digest
    };
    let base = digest_of(&Cfg::default());
    assert_eq!(base, digest_of(&Cfg::default()), "pure");
    assert_ne!(
        base,
        digest_of(&Cfg {
            stations: vec![gauge_row("snowgauge", Some(1501.0))],
            ..Cfg::default()
        }),
        "a re-measured gauge"
    );
    assert_ne!(
        base,
        digest_of(&Cfg {
            stations: vec![SnowStation {
                zone: Some(Zone::new("America/Phoenix").expect("a real zone")),
                ..gauge_row("snowgauge", Some(1500.0))
            }],
            ..Cfg::default()
        }),
        "a gauge in another zone has another midnight"
    );
    assert_ne!(
        base,
        digest_of(&Cfg {
            stations: vec![
                gauge_row("snowgauge", Some(1500.0)),
                gauge_row("summit", Some(2000.0)),
            ],
            ..Cfg::default()
        }),
        "a second gauge"
    );
}

// ---------------------------------------------------------------------------------------
// snow:depth.

fn depth_body(series: Series, cache: Arc<Cache>) -> Depth {
    Depth {
        gauges: Arc::from(vec![gauge("snowgauge")]),
        cache: cache as Arc<dyn SnowCache>,
        store: Arc::new(series) as Arc<dyn ReadingStore>,
        clock: Arc::new(TestClock::new(now())),
    }
}

/// The snowfall rate's two-sample rule: the newest within five minutes and the one nearest
/// thirty minutes ago, extrapolated from half an hour to an hour.
#[test]
fn the_snowfall_rate_is_the_half_hour_delta_doubled() {
    // Two samples: one at now-30m reading 1300 mm, one at now-1m reading 1275 mm. Twenty
    // five millimetres in half an hour is 50 mm an hour, which is 2.0 inches.
    let series = vec![
        MeanBucket {
            bucket: now() - chrono::Duration::minutes(30),
            mean: Some(1300.0),
        },
        MeanBucket {
            bucket: now() - chrono::Duration::minutes(1),
            mean: Some(1275.0),
        },
    ];
    assert_eq!(rate(&series, now()), Some(2.0));
}

/// Both samples are required. `None` leaves the stored rate alone, where Go writes zero —
/// a gauge that has stopped reporting has an unknown rate, not a zero one.
#[test]
fn the_rate_needs_both_samples_and_is_absent_rather_than_zero_without_them() {
    let fresh = MeanBucket {
        bucket: now() - chrono::Duration::minutes(1),
        mean: Some(1275.0),
    };
    let lagged = MeanBucket {
        bucket: now() - chrono::Duration::minutes(30),
        mean: Some(1300.0),
    };
    assert_eq!(rate(&[fresh], now()), None, "no older sample");
    assert_eq!(rate(&[lagged], now()), None, "no fresh sample");
    assert_eq!(rate(&[], now()), None, "nothing at all");
    // A fresh sample that is not fresh enough is not a sample.
    let stale = MeanBucket {
        bucket: now() - chrono::Duration::minutes(6),
        mean: Some(1275.0),
    };
    assert_eq!(
        rate(&[lagged, stale], now()),
        None,
        "six minutes is too old"
    );
    // And an older sample outside the two-minute tolerance is not one either.
    let drifted = MeanBucket {
        bucket: now() - chrono::Duration::minutes(34),
        mean: Some(1300.0),
    };
    assert_eq!(rate(&[drifted, fresh], now()), None);
}

/// Settling is not negative snowfall: Go's `if snowfall_30min <= 0 then return 0`.
#[test]
fn a_shrinking_pack_has_a_rate_of_zero_and_not_a_negative_one() {
    let series = vec![
        MeanBucket {
            bucket: now() - chrono::Duration::minutes(30),
            mean: Some(1275.0),
        },
        MeanBucket {
            bucket: now() - chrono::Duration::minutes(1),
            mean: Some(1300.0),
        },
    ];
    assert_eq!(rate(&series, now()), Some(0.0));
}

// ---------------------------------------------------------------------------------------
// snow:smoothing.

fn smoothing_body(series: Series, cache: Arc<Cache>) -> Smoothing {
    Smoothing {
        gauges: Arc::from(vec![gauge("snowgauge")]),
        cache: cache as Arc<dyn SnowCache>,
        store: Arc::new(series) as Arc<dyn ReadingStore>,
        clock: Arc::new(TestClock::new(now())),
    }
}

// ---------------------------------------------------------------------------------------
// snow:events.

// ---------------------------------------------------------------------------------------
// snow:season.

// ---------------------------------------------------------------------------------------
// The tables.

/// The four chart ranges are the four the `snow_events.hours` `CHECK` permits, and every
/// one of them stays inside the detector's point cap.
#[test]
fn every_event_window_fits_the_detectors_bound() {
    let hours: Vec<u32> = EVENT_WINDOWS.iter().map(|w| w.hours).collect();
    assert_eq!(hours, vec![24, 72, 168, 744]);
    for window in EVENT_WINDOWS {
        // Go's own `days = hours / 24 + 1`.
        assert_eq!(window.days as u32, window.hours / 24 + 1, "{window:?}");
        let points = (window.days as u64 * 86_400) / window.level.span().as_secs();
        assert!(
            points <= pelt::MAX_POINTS as u64,
            "{window:?} is {points} buckets, over the {} cap",
            pelt::MAX_POINTS
        );
        assert_eq!(window.kernel % 2, 1, "an odd median kernel: {window:?}");
        // About five hours of smoothing at either level, which is Go's
        // `getSmoothingWindow`: 61 five-minute buckets is 305 minutes and 5 hourly ones is
        // 300, and the comment it carries says "~5 hours" for exactly that reason.
        let smoothed = window.kernel as u64 * window.level.span().as_secs();
        assert!(
            (18_000..=18_300).contains(&smoothed),
            "{window:?} smooths over {smoothed} seconds"
        );
    }
}

/// One pass of the smoother stays under the store's own row cap, so a backfill is chunked
/// rather than silently truncated to its oldest buckets.
#[test]
fn a_smoothing_pass_stays_under_the_stores_row_cap() {
    const _: () = assert!(
        PASS_BUCKETS <= 10_000,
        "a pass must stay under mw_store::reader::MAX_ROWS, or a backfill is silently \
         truncated to its oldest buckets"
    );
    let covered = PASS_BUCKETS * MAX_PASSES * BucketLevel::FiveMinute.span().as_secs() as usize;
    assert!(
        covered > 365 * 86_400,
        "one tick covers a season: {covered} seconds"
    );
}

/// Every class the classifier can produce is in the roster the migration's `CHECK` mirrors.
#[test]
fn the_event_classes_are_the_migrations_four() {
    let names: Vec<&str> = EVENT_CLASSES.iter().map(|c| c.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "accumulation",
            "plateau",
            "redistribution",
            "spike_then_settle"
        ]
    );
    let contributing: Vec<&str> = EVENT_CLASSES
        .iter()
        .filter(|c| c.contributes())
        .map(|c| c.as_str())
        .collect();
    assert_eq!(
        contributing,
        vec!["accumulation", "spike_then_settle"],
        "which is what the table ties a non-null accumulation_mm to"
    );
}

/// The parts that need a real `tokio` runtime, and therefore cannot run under `--cfg loom`.
///
/// That flag applies to every crate in the graph, and `tokio` gates its whole `net` module
/// behind `#![cfg(not(loom))]` -- so `runtime::Builder::build` asserts the I/O driver is
/// disabled and a `#[tokio::test]` panics before its body runs. `super::super::rollup::tests`'
/// `driven` module, `super::super::snapshot::tests`' and `super::super::writer`'s `adapter`
/// carry the same guard for the same reason, and `.github/workflows/pr.yml`'s loom job
/// explains the flag's reach in full.
///
/// **Nothing excluded here is a concurrency model.** These four components hold no
/// lock-free state of their own: each tick reads a bucket series and writes a cache row
/// through a capability handle, and every shared datum is behind the store's own pool. What
/// they are is `async` because the handles are, which is a property of the seam and not of
/// the arithmetic -- and the arithmetic, which is the part worth modelling, is in
/// `super::threshold`, `super::smoothing`, `super::pelt` and `super::units`, all of which
/// are synchronous and all of which run under loom.
#[cfg(not(loom))]
mod driven {
    use super::*;

    /// The four columns `snow:depth` owns, and only those four.
    #[tokio::test]
    async fn the_depth_refresh_writes_its_four_columns_and_no_others() {
        // A gauge 1500 mm above bare ground, currently reading 1246.0 mm -- so 254 mm of snow,
        // which is ten inches.
        let minutes = buckets(
            now() - chrono::Duration::minutes(59),
            chrono::Duration::minutes(1),
            &(0..60)
                .map(|i| Some(if i < 30 { 1300.0 } else { 1246.0 }))
                .collect::<Vec<_>>(),
        );
        // Since midnight, at the five-minute level: a 60 mm step, which the quick rule counts.
        let five = buckets(
            midnight(),
            chrono::Duration::minutes(5),
            &[Some(1306.0), Some(1306.0), Some(1246.0), Some(1246.0)],
        );
        let cache = Arc::new(Cache::default());
        let series = Series::default()
            .with(BucketLevel::Minute, minutes)
            .with(BucketLevel::FiveMinute, five);
        let mut body = depth_body(series, Arc::clone(&cache));
        body.tick(&cx()).await.expect("a successful occurrence");

        let patch = cache.only_patch();
        assert_eq!(patch.depth, Some(10.0), "254 mm is ten inches");
        assert_eq!(patch.today, Some(2.4), "60 mm since midnight");
        assert_eq!(
            patch.storm, None,
            "no hourly buckets were seeded, so the storm total is unknown rather than zero"
        );
        assert!(patch.fall_rate.is_some(), "both rate samples were present");
        assert!(patch.source_through.is_some());
        // The columns the other three components own are untouched.
        assert_eq!(patch.last24, None);
        assert_eq!(patch.last72, None);
        assert_eq!(patch.season, None);
    }

    /// **`/snow` has no recency filter and must not acquire one.** A tick that finds no reading
    /// writes no row at all, so the stored depth persists — which is what reproduces "an offline
    /// gauge returns its last known depth indefinitely" (W1 Decision 16).
    #[tokio::test]
    async fn a_gauge_with_no_recent_reading_leaves_its_row_alone() {
        let cache = Arc::new(Cache::default());
        let mut body = depth_body(Series::default(), Arc::clone(&cache));
        body.tick(&cx()).await.expect("a successful occurrence");
        assert!(
            cache.patches().is_empty(),
            "an empty window writes nothing -- not a row of zeros"
        );
    }

    /// A gauge that has never reported gets **no row**, which is what keeps "no row at all"
    /// (fixture `snow-device-never-reported`) distinguishable from "a row of zeros".
    #[tokio::test]
    async fn nothing_computable_means_no_row_rather_than_a_row_of_zeros() {
        let cache = Arc::new(Cache::default());
        // A bucket exists but carries no reading, which is a gauge that is configured and
        // silent.
        let minutes = buckets(
            now() - chrono::Duration::minutes(10),
            chrono::Duration::minutes(1),
            &[None, None, None],
        );
        let mut body = depth_body(
            Series::default().with(BucketLevel::Minute, minutes),
            Arc::clone(&cache),
        );
        body.tick(&cx()).await.expect("a successful occurrence");
        assert!(cache.patches().is_empty());
    }

    /// A store failure is a failed occurrence, so the supervisor records it and the next tick
    /// is the retry — rather than a tick that quietly wrote nothing.
    #[tokio::test]
    async fn a_store_failure_fails_the_occurrence() {
        let cache = Arc::new(Cache::default());
        let mut body = depth_body(Series::broken(), Arc::clone(&cache));
        let e = body.tick(&cx()).await.expect_err("the read failed");
        assert!(matches!(e, ComponentError::Tick { .. }), "{e:?}");
        assert!(cache.patches().is_empty());
    }

    /// The storm total uses the **station's** base distance.
    ///
    /// `calculate_storm_snowfall` carries `base_dist := 1798.0; -- TODO: Pass this as parameter
    /// from application`, so under Go every station's storm total is computed against the
    /// owner's own gauge height. At 1500 mm the same readings give a different depth series
    /// entirely, and this asserts the station's number is used.
    #[tokio::test]
    async fn the_storm_total_is_computed_against_the_stations_own_calibration() {
        // Hourly buckets: flat at 1300, then a step to 1200 -- a hundred millimetres, which the
        // quick rule counts whole. Against Go's hard-coded 1798 the depths would be 498 and 598
        // and the ACCUMULATION would be the same 100 mm; what differs is the bare-ground filter,
        // so the case that separates them is a reading just inside this station's base.
        let hourly = buckets(
            now() - chrono::Duration::hours(23),
            chrono::Duration::hours(1),
            &[
                // 1499 is within 2 mm of this station's 1500 base, so it is bare ground and is
                // dropped. Under Go's 1798 it would be 299 mm of snow.
                Some(1499.0),
                Some(1300.0),
                Some(1200.0),
            ],
        );
        let cache = Arc::new(Cache::default());
        let minutes = buckets(
            now() - chrono::Duration::minutes(1),
            chrono::Duration::minutes(1),
            &[Some(1200.0)],
        );
        let mut body = depth_body(
            Series::default()
                .with(BucketLevel::Hour, hourly)
                .with(BucketLevel::Minute, minutes),
            Arc::clone(&cache),
        );
        body.tick(&cx()).await.expect("a successful occurrence");
        let patch = cache.only_patch();
        // 1300 -> 1200 is a 100 mm step: 3.9 inches. The dropped 1499 contributes nothing.
        assert_eq!(patch.storm, Some(3.9));
    }

    /// The reads are the three levels and nothing else, each bounded.
    #[tokio::test]
    async fn the_depth_refresh_reads_three_levels_with_bounded_limits() {
        let series = Series::default();
        let store = Arc::new(series);
        let mut body = Depth {
            gauges: Arc::from(vec![gauge("snowgauge")]),
            cache: Arc::new(Cache::default()) as Arc<dyn SnowCache>,
            store: Arc::clone(&store) as Arc<dyn ReadingStore>,
            clock: Arc::new(TestClock::new(now())),
        };
        body.tick(&cx()).await.expect("a successful occurrence");
        let asked = store.asked();
        assert_eq!(asked.len(), 3, "{asked:#?}");
        assert!(asked[0].starts_with("1m "), "{}", asked[0]);
        assert!(asked[1].starts_with("5m "), "{}", asked[1]);
        assert!(asked[2].starts_with("1h "), "{}", asked[2]);
        for query in &asked {
            assert!(query.contains("SnowDistance"), "{query}");
            let limit: usize = query
                .rsplit("limit=")
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX);
            assert!(limit > 0 && limit <= 301, "{query}");
        }
    }

    /// The first pass for a gauge backfills from the season's first instant and writes the
    /// estimate series, then reads the totals off it.
    #[tokio::test]
    async fn the_first_smoothing_pass_backfills_from_the_season_start() {
        // Twelve hours of five-minute buckets ending now: a hundred millimetres accumulating.
        let count = 144;
        let values: Vec<Option<f64>> = (0..count)
            .map(|i| Some(1500.0 - 100.0 * f64::from(i) / f64::from(count)))
            .collect();
        let five = buckets(
            now() - chrono::Duration::hours(12),
            chrono::Duration::minutes(5),
            &values,
        );
        let cache = Arc::new(Cache::default());
        let store = Arc::new(Series::default().with(BucketLevel::FiveMinute, five));
        let mut body = Smoothing {
            gauges: Arc::from(vec![gauge("snowgauge")]),
            cache: Arc::clone(&cache) as Arc<dyn SnowCache>,
            store: Arc::clone(&store) as Arc<dyn ReadingStore>,
            clock: Arc::new(TestClock::new(now())),
        };
        body.tick(&cx()).await.expect("a successful occurrence");

        let replacements = cache.replacements.lock().expect("not poisoned").clone();
        // Six weeks from 1 October at five-minute buckets is more than one pass's 8 000, so
        // the backfill is chunked -- which is the bound `MAX_PASSES` exists to put on the work
        // one tick does, and it still finished inside this tick.
        assert_eq!(replacements.len(), 2, "{replacements:?}");
        let season = units::season_start(now(), denver()).expect("a real boundary");
        assert_eq!(replacements[0].0, season, "it started at the season");
        // The second pass starts inside the first by the smoothing overlap.
        assert!(replacements[1].0 > season, "{replacements:?}");
        let written: usize = replacements.iter().map(|(_, rows)| rows).sum();
        assert!(written > 100, "it wrote the series: {written} rows");

        let patch = cache.only_patch();
        assert!(patch.last24.is_some(), "a 24-hour total");
        assert!(patch.last72.is_some());
        assert!(patch.season.is_some(), "§2a.7's re-derivation");
        // The columns `snow:depth` owns are untouched.
        assert_eq!(patch.depth, None);
        assert_eq!(patch.today, None);
        assert_eq!(patch.storm, None);
        assert_eq!(patch.fall_rate, None);
    }

    /// An incremental pass starts six hours behind the newest estimate, not at the season.
    #[tokio::test]
    async fn an_incremental_pass_re_derives_the_trailing_overlap() {
        let cache = Arc::new(Cache::default());
        // A gauge with estimates up to an hour ago.
        let newest = now() - chrono::Duration::hours(1);
        cache.seed(newest - chrono::Duration::hours(6), 73, 4.0);
        let five = buckets(
            now() - chrono::Duration::hours(7),
            chrono::Duration::minutes(5),
            &(0..84).map(|_| Some(1400.0)).collect::<Vec<_>>(),
        );
        let mut body = smoothing_body(
            Series::default().with(BucketLevel::FiveMinute, five),
            Arc::clone(&cache),
        );
        body.tick(&cx()).await.expect("a successful occurrence");
        let replacements = cache.replacements.lock().expect("not poisoned").clone();
        assert_eq!(replacements.len(), 1);
        assert_eq!(
            replacements[0].0,
            newest - chrono::Duration::hours(6),
            "six hours behind the newest estimate, which is the smoothing overlap"
        );
    }

    /// `snowlast24` is `max - min` over the estimates, and an empty window leaves the stored
    /// total alone rather than zeroing it.
    #[tokio::test]
    async fn the_totals_are_the_spread_of_the_estimates_and_absent_when_there_are_none() {
        let cache = Arc::new(Cache::default());
        let body = smoothing_body(Series::default(), Arc::clone(&cache));
        // Nothing stored: every total is absent.
        let patch = body
            .totals(&gauge("snowgauge"), now())
            .await
            .expect("a successful read");
        assert_eq!(patch.last24, None);
        assert_eq!(patch.last72, None);
        assert_eq!(patch.season, None);
        // Four inches at the season's start, rising to ten over the last day.
        let season = units::season_start(now(), denver()).expect("a real boundary");
        cache.seed(season, 10, 4.0);
        cache.seed(now() - chrono::Duration::hours(2), 10, 10.0);
        let patch = body
            .totals(&gauge("snowgauge"), now())
            .await
            .expect("a successful read");
        assert_eq!(patch.last24, Some(0.0), "the last day is flat at ten");
        assert_eq!(patch.last72, Some(0.0));
        assert_eq!(
            patch.season,
            Some(6.0),
            "ten at the peak, less four at the start"
        );
    }

    /// Four windows per gauge, each written — including an empty one, which is what removes the
    /// bands the last pass found.
    #[tokio::test]
    async fn the_event_pass_writes_every_chart_range() {
        let cache = Arc::new(Cache::default());
        let mut body = Events {
            gauges: Arc::from(vec![gauge("snowgauge")]),
            cache: Arc::clone(&cache) as Arc<dyn SnowCache>,
            store: Arc::new(Series::default()) as Arc<dyn ReadingStore>,
            clock: Arc::new(TestClock::new(now())),
        };
        body.tick(&cx()).await.expect("a successful occurrence");
        let written = cache.events.lock().expect("not poisoned").clone();
        let ranges: Vec<u32> = written.iter().map(|(hours, _)| *hours).collect();
        assert_eq!(ranges, vec![24, 72, 168, 744]);
        for (hours, events) in &written {
            assert!(events.is_empty(), "{hours} h had no readings");
        }
    }

    /// A storm in the window is written as contributing events, and only the two classes that
    /// contribute are stored — which is what the table's own `CHECK` ties to the column.
    #[tokio::test]
    async fn a_storm_is_written_as_contributing_events_only() {
        // Twelve hours of hourly buckets with a 150 mm accumulation in the middle, long enough
        // for the 72-hour window's hour level to see it.
        let mut values: Vec<Option<f64>> = vec![Some(1500.0 - 1.0); 24];
        for (i, value) in values.iter_mut().enumerate().skip(8).take(8) {
            *value = Some(1499.0 - 20.0 * f64::from(i as i32 - 7));
        }
        for value in values.iter_mut().skip(16) {
            *value = Some(1499.0 - 160.0);
        }
        let hourly = buckets(
            now() - chrono::Duration::hours(24),
            chrono::Duration::hours(1),
            &values,
        );
        let cache = Arc::new(Cache::default());
        let mut body = Events {
            gauges: Arc::from(vec![gauge("snowgauge")]),
            cache: Arc::clone(&cache) as Arc<dyn SnowCache>,
            store: Arc::new(Series::default().with(BucketLevel::Hour, hourly))
                as Arc<dyn ReadingStore>,
            clock: Arc::new(TestClock::new(now())),
        };
        body.tick(&cx()).await.expect("a successful occurrence");
        let written = cache.events.lock().expect("not poisoned").clone();
        let seventy_two = written
            .iter()
            .find(|(hours, _)| *hours == 72)
            .map(|(_, events)| events.clone())
            .expect("the 72-hour range");
        assert!(!seventy_two.is_empty(), "the storm was detected");
        for event in &seventy_two {
            assert!(
                event.class.contributes(),
                "only the two contributing classes are stored: {event:?}"
            );
            assert!(
                event.accumulation_mm.is_some_and(|mm| mm > 0.0),
                "with an accumulation, which the table's CHECK requires: {event:?}"
            );
            assert!(event.end >= event.start, "{event:?}");
        }
    }

    /// The season total is `max - first` since 1 October in the **station's** zone, and it
    /// writes exactly one column.
    #[tokio::test]
    async fn the_season_total_writes_one_column() {
        let cache = Arc::new(Cache::default());
        let season = units::season_start(now(), denver()).expect("a real boundary");
        cache.seed(season, 5, 2.0);
        cache.seed(season + chrono::Duration::days(20), 5, 30.0);
        let mut body = Season {
            gauge: gauge("snowgauge"),
            cache: Arc::clone(&cache) as Arc<dyn SnowCache>,
            clock: Arc::new(TestClock::new(now())),
        };
        body.tick(&cx()).await.expect("a successful occurrence");
        let patch = cache.only_patch();
        assert_eq!(patch.season, Some(28.0), "thirty at the peak, less two");
        assert_eq!(patch.depth, None);
        assert_eq!(patch.last24, None);
        assert_eq!(patch.last72, None);
        assert_eq!(patch.today, None);
    }

    /// A gauge with no estimates writes nothing at all, which is the same "leave it alone"
    /// rule the depth refresh follows.
    #[tokio::test]
    async fn a_season_with_no_estimates_leaves_the_row_alone() {
        let cache = Arc::new(Cache::default());
        let mut body = Season {
            gauge: gauge("snowgauge"),
            cache: Arc::clone(&cache) as Arc<dyn SnowCache>,
            clock: Arc::new(TestClock::new(now())),
        };
        body.tick(&cx()).await.expect("a successful occurrence");
        assert!(cache.patches().is_empty());
    }

    /// A season that began with snow on the ground does not count that pack as having fallen
    /// during it — which is why the subtrahend is the **first** estimate and not the smallest.
    #[tokio::test]
    async fn the_season_subtracts_the_first_estimate_and_not_the_smallest() {
        let cache = Arc::new(Cache::default());
        let season = units::season_start(now(), denver()).expect("a real boundary");
        // Ten inches on the ground at the season's start, melting to one, then thirty.
        cache.seed(season, 2, 10.0);
        cache.seed(season + chrono::Duration::days(5), 2, 1.0);
        cache.seed(season + chrono::Duration::days(20), 2, 30.0);
        let total = season_total(
            &(Arc::clone(&cache) as Arc<dyn SnowCache>),
            &StationKey::new("snowgauge"),
            season,
        )
        .await
        .expect("a successful read");
        assert_eq!(total, Some(20.0), "thirty less the ten it started with");
    }
}
