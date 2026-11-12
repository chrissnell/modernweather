//! The four `snow:*` components — `docs/component-model.md` §4.3, W3 §2a.7, **W2a D-33**.
//!
//! ```text
//!                observations_1m / _5m / _1h           snow_depth_estimates
//!                         │                                    ▲   │
//!     ┌───────────────────┼────────────────────┐                │   │
//!     ▼                   ▼                    ▼                │   ▼
//!  snow:depth        snow:smoothing        snow:events     snow:season
//!  Every { 30s }     Every { 15m }         Every { 15m }   Calendar { 00:10, Station }
//!     │                   │                    │                │
//!     │ depth             │ last24 last72      │ snow_events    │ season
//!     │ today             │ season (re-derived)│                │
//!     │ storm fall_rate   │                    │                │
//!     └───────────────────┴────────┬───────────┴────────────────┘
//!                                  ▼
//!                        snow_totals_cache  ──▶ /snow
//! ```
//!
//! # Four components, and which columns each owns
//!
//! **D-33**: *"the snow cache becomes three, because they have independent periods,
//! failures and rate budgets"* — three being the Go controller's three tickers (a 30-second
//! depth refresh, a 15-minute event pass and a 15-minute smoothing pass), plus
//! `snow:season`, which comes from a different place entirely
//! (`calculate_total_season_snowfall()`, which the Go `/snow` handler calls **per request**
//! on a cache miss and which scans months).
//!
//! `mw_component::SnowPatch` is what makes four writers of one row safe: each component
//! writes the columns it owns and leaves the rest alone, so a 30-second depth refresh
//! cannot revert a 15-minute total. Which component owns which column is that type's own
//! documentation, and the one column with two writers is `season` — see below.
//!
//! # Two places where the two design documents disagree, and what was done
//!
//! `docs/component-model.md` §4.3 and `docs/service-architecture.md` §2a.7 both decompose
//! the snow cache and they do not agree. Recorded as `docs/reconciliation.md`
//! **A-M6-3-3**; the resolutions are:
//!
//! 1. **`snowstorm` and `snowfallrate` are `snow:depth`'s, not a fifth component's.** §2a.7
//!    gives them `snow:storm` at `Every { 5 min }` and calls them "the fast-moving members
//!    of the expensive set"; §4.3 has no such row and gives `snow:depth` the 30-second
//!    tier. They are folded into `snow:depth` because they are **cheaper than the column it
//!    already computes**: the storm total is a dual-threshold walk over 24 hourly buckets
//!    and the rate is a subtraction between two 1-minute buckets, against `snowtoday`'s walk
//!    over up to 288. A separate component at 5 minutes would be strictly staler than the
//!    30-second tick that already reads the same buckets, and §4.3's four rows are what
//!    `docs/reconciliation.md` names as the count.
//! 2. **`season` has two writers, which is §2a.7's own request.** §4.3 gives `snow:season`
//!    `Calendar { daily 00:10, Station }`; §2a.7 asks for `Every { 15 min }` **plus** that
//!    `Calendar`, which is two triggers on one component and therefore forbidden by D-33.
//!    So the `Calendar` component is `snow:season` as §4.3 writes it, and the 15-minute
//!    re-derivation is done by `snow:smoothing` — which has just rewritten the very series
//!    the season total is a function of, so it is one more statement on a handle it already
//!    holds. Without it, a storm would show `snowtoday = 12"` beside a `snowseason` from
//!    yesterday, on a frozen response, for up to a day.
//!
//! # What these components are **not**
//!
//! - **Not `OnReading`.** Every one of the seven numbers is a function of a *window* of
//!   readings, which is **R2** (`docs/service-architecture.md` §2.5): maintained by a
//!   component, read from memory, never computed per request. A reading-driven version
//!   would recompute a season total per reading.
//! - **Not a wait-for-data loop.** The Go controller opens with
//!   `for !dataAvailable { select { case <-time.After(30 * time.Second): ... } }`, which is
//!   a component hand-rolling the scheduler. "Is this gauge producing data" is **data
//!   presence, not capability availability** (§2.2), so it is not a `Waiting` state either:
//!   the supervisor already ticks every 30 seconds, and a tick with nothing to compute
//!   writes no column and returns. The loop, its `hasRecentSnowData` query and its
//!   `stopChan` all disappear, and the component gains a `last run`, a `next run` and a
//!   `last error` it does not have today.
//! - **Not instrumented with a series of its own.** `crate::metrics` has none for these,
//!   deliberately: the supervisor's `mw_component_ticks_total{component,outcome}` is already
//!   the right grain, because one occurrence is one set of computations. `mw-output` needs
//!   two series because one upload occurrence can make three attempts; nothing here can.
//!
//! # No SQL, which is `ci/layers.toml`
//!
//! Every number here is computed in Rust from a bucket series read through
//! `mw_component::ReadingStore`, and written through `mw_component::SnowCache`. Go evaluates
//! the same arithmetic as four PL/pgSQL functions, two Go computers and a changepoint
//! detector, with the PL/pgSQL half reachable only through a live TimescaleDB — so none of
//! it had a unit test. `super::threshold`, `super::smoothing`, `super::pelt` and
//! `super::units` are the arithmetic, each tested against a constructed series.

pub mod pelt;
pub mod smoothing;
pub mod threshold;
pub mod units;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{
    BoxFuture, BucketLevel, Builder, Capability, ComponentError, ComponentId, ComponentIdError,
    ComponentSpec, ConfigDigest, CronSpec, Cx, DepthEstimate, EventClass, FirstRun, LocalTime,
    MeanBucket, MeanSeriesQuery, Misfire, Need, Phase, ReadingStore, SnowCache, SnowEvent,
    SnowPatch, Store, StoreError, Tick, Trigger, TzSource, Zone,
};
use mw_types::{MetricKey, StationKey};
use tracing::{debug, info, warn};

use crate::clock::{Clock, SystemClock};

/// `snow:depth`'s component id.
pub const DEPTH_ID: &str = "snow:depth";

/// `snow:smoothing`'s component id.
pub const SMOOTHING_ID: &str = "snow:smoothing";

/// `snow:events`' component id.
pub const EVENTS_ID: &str = "snow:events";

/// The prefix of a `snow:season:<station>` component id.
///
/// The one id in §4.3's table that needs a station suffix, for `rain:rollover:<station>`'s
/// reason: a `Calendar { ..., TzSource::Station }` is bound to **one** station's zone, so one
/// component cannot serve two. The other three iterate every gauge inside one tick, which is
/// `window:rollup`'s shape.
pub const SEASON_PREFIX: &str = "snow:season";

/// `snow:depth`'s period — §4.3's `Every { 30s }`.
pub const DEPTH_PERIOD: Duration = Duration::from_secs(30);

/// `snow:depth`'s per-tick deadline.
///
/// Four round trips per gauge: three bucket reads and an upsert. Twenty seconds is inside
/// the period and two orders of magnitude more than the reads take, so a miss means the
/// database is not answering.
pub const DEPTH_DEADLINE: Duration = Duration::from_secs(20);

/// The period of the two 15-minute passes — §4.3 and W3 §2a.7 agree on this one.
pub const SLOW_PERIOD: Duration = Duration::from_secs(900);

/// Their per-tick deadline — W3 §2a.7's `deadline 60 s`.
pub const SLOW_DEADLINE: Duration = Duration::from_secs(60);

/// When `snow:season` runs, in the station's zone — §4.3's `Calendar { daily 00:10 }`.
///
/// Ten past midnight and not midnight: the 1-day aggregate's refresh policy and the
/// rainfall rollover both land on the boundary, and the season total reads the smoothed
/// series rather than either, so ten minutes of clearance costs nothing and keeps three
/// things off one instant.
pub const SEASON_AT: LocalTime = LocalTime::new(0, 10);

/// `snow:season`'s per-occurrence deadline.
///
/// One aggregate over a season of estimates. `min(depth_in)`/`max(depth_in)` is **not** an
/// index lookup — no index covers `depth_in`, so it is a scan of the range across the
/// hypertable's chunks, which at the end of a long winter is on the order of tens of
/// thousands of rows. Two minutes is three orders of magnitude of slack on that, and Go's
/// equivalent is a PL/pgSQL loop over the same range with a 10-second timeout.
pub const SEASON_DEADLINE: Duration = Duration::from_secs(120);

/// How far back `snow:depth` looks for a reading, for the depth and the rate.
///
/// One hour, which covers the rate's 30-minute lag with room for a late bucket. **It is not
/// a recency filter on the response**: `/snow` has none and must not acquire one (W1
/// Decision 16, fixture `snow-device-stale`), and what reproduces "an offline gauge returns
/// its last known depth indefinitely" is that the cache row **persists** — a tick that finds
/// no reading writes no depth column rather than writing a zero.
pub const DEPTH_LOOKBACK: Duration = Duration::from_secs(3600);

/// How recent the newer of the snowfall rate's two readings must be.
///
/// `bucket >= current_ts - interval '5 minutes'` in `calculate_current_snowfall_rate`.
pub const RATE_FRESH: Duration = Duration::from_secs(300);

/// How far back the older of the snowfall rate's two readings is.
///
/// `past_ts := current_ts - interval '30 minutes'`.
pub const RATE_LAG: Duration = Duration::from_secs(1800);

/// How far from that instant the older reading may be.
///
/// `bucket BETWEEN past_ts - interval '2 minutes' AND past_ts + interval '2 minutes'`.
pub const RATE_TOLERANCE: Duration = Duration::from_secs(120);

/// How long the storm total's window is.
///
/// `calculate_storm_snowfall`'s `'24 hours'::interval`, over the hourly level — which is
/// what `get_new_snow_dual_threshold` reads.
pub const STORM_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// How far the smoothing pass re-derives behind its last estimate.
///
/// `startTime = lastEstTime.Time.Add(-6 * time.Hour)` — "overlap by 6 hours for smoothing
/// boundary". The quantile window is ±30 minutes, so six hours is an order of magnitude of
/// clearance, and the rate limiter's continuity across the seam is
/// `SnowCache::estimate_before`'s job rather than the overlap's.
pub const SMOOTHING_OVERLAP: Duration = Duration::from_secs(6 * 3600);

/// How many buckets one smoothing pass reads.
///
/// Under `mw_store::reader::MAX_ROWS` (10 000), which is the store's own cap on a bucket
/// read: a pass that asked for more would be silently truncated to the **oldest** 8 000 and
/// the newest buckets would never be estimated.
pub const PASS_BUCKETS: usize = 8_000;

/// How many passes one smoothing tick will make, per level.
///
/// The steady state is **one** — a 15-minute tick plus a six-hour overlap is about 80
/// buckets. The rest is the backfill, which [`BACKFILL`] bounds: at those levels a whole
/// season is 315 daily, 1 730 hourly and 3 744 five-minute buckets, so one pass per level
/// covers it and eight is slack rather than the operating point.
///
/// Go attempts the whole season in one pass, with its own `O(n²)` smoother, inside the same
/// 15-minute tick. Chunking it is what makes the work per tick bounded — and what makes
/// the deadline above a real bound rather than an aspiration.
pub const MAX_PASSES: usize = 8;

/// Which aggregate level the smoother reads for history of a given age, newest first.
///
/// **This is a retention constraint, not a resolution preference**, and getting it wrong is
/// the one way the season total can be silently too small.
/// `migrations/20261029234500_...sql` puts a retention policy on three of the four levels:
///
/// | Level | Retained | Read here for |
/// |---|---|---|
/// | `observations_5m` | 14 days | the newest 13 days |
/// | `observations_1h` | 90 days | 13 to 85 days old |
/// | `observations_1d` | **unbounded** | everything older |
///
/// A backfill that read the 5-minute level from the season's first instant — which is what
/// the obvious one-level loop does — would find **nothing** before the retention horizon,
/// write no estimate for it, and `snowseason` would then report only the newest fortnight:
/// 3″ instead of 60″ on a node commissioned in February, permanently, because the
/// incremental path never looks back further than [`SMOOTHING_OVERLAP`] again. Go does not
/// have this problem because its own `weather_5m` retention is six months.
///
/// Each horizon is inside its level's policy with margin, for the reason that migration
/// gives for sizing retention above the band it serves: a level is still being refreshed
/// for a while after its band closes.
///
/// **What mixing levels costs, stated.** An hourly or daily bucket is a *mean* distance over
/// its bucket, so a storm's peak inside one bucket is flattened — which makes the season
/// total conservative rather than wrong, and is what the Go SQL function chose deliberately
/// for the same number ("daily totals eliminate false positives from natural variance",
/// migration 016). The 24-hour and 72-hour totals are unaffected: their windows never reach
/// past the 5-minute band.
pub const BACKFILL: [(BucketLevel, Duration); 3] = [
    (BucketLevel::FiveMinute, Duration::from_secs(13 * 86_400)),
    (BucketLevel::Hour, Duration::from_secs(85 * 86_400)),
    // No horizon: `observations_1d` is "the only level read over all history", and is
    // deliberately unbounded.
    (BucketLevel::Day, Duration::MAX),
];

/// One chart range `snow:events` detects events for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventWindow {
    /// The `snow_events.hours` value — one of the four the column's `CHECK` permits.
    pub hours: u32,
    /// Which aggregate level the depth series is read from.
    pub level: BucketLevel,
    /// How many days back it is read, which is Go's `hours / 24 + 1`.
    pub days: i64,
    /// The median filter's kernel, in buckets.
    pub kernel: usize,
}

/// The four chart ranges, with the level and kernel each is detected at.
///
/// `hours` and `days` are Go's (`timeRanges := []int{24, 72, 168, 744}` and
/// `days = (hours / 24) + 1`). `kernel` is Go's `getSmoothingWindow` keyed on the level
/// rather than on a table name: 61 buckets at five minutes (five hours) and 5 at one hour
/// (also five hours).
///
/// # The level is a deviation for the three long windows, and it is measured
///
/// Go reads the 5-minute level for every window up to 168 hours. Both of the detector's
/// matrices are `O(n²)` in the bucket count (`super::pelt`), so that is 2 304 buckets for
/// the 168-hour window — a **42 MiB** kernel prefix sum and a **21 MiB** distance vector,
/// inside a 10-second budget, on a release target `ci/spike/probe` measured at ARM1176.
/// The faithful version does not complete there, and a detector that times out is a chart
/// with no bands on it.
///
/// What makes the hour level the right answer rather than a concession: the 5-minute series
/// is median-filtered with a **61-bucket** kernel before anything is detected, which is five
/// hours — so the signal being segmented is already hourly, and reading it hourly costs
/// resolution the filter has already removed. At these levels the largest window is 768
/// buckets: a 4.7 MiB prefix sum and a 2.4 MiB distance vector.
///
/// These events feed the website's accumulation bands. **No frozen response reads them**,
/// which is what makes the trade this item's to make.
pub const EVENT_WINDOWS: [EventWindow; 4] = [
    EventWindow {
        hours: 24,
        level: BucketLevel::FiveMinute,
        days: 2,
        kernel: 61,
    },
    EventWindow {
        hours: 72,
        level: BucketLevel::Hour,
        days: 4,
        kernel: 5,
    },
    EventWindow {
        hours: 168,
        level: BucketLevel::Hour,
        days: 8,
        kernel: 5,
    },
    EventWindow {
        hours: 744,
        level: BucketLevel::Hour,
        days: 32,
        kernel: 5,
    },
];

/// One snow gauge, as the `snow:*` components plan from it.
#[derive(Debug, Clone, PartialEq)]
pub struct SnowStation {
    /// Whose.
    pub key: StationKey,
    /// `config.stations.snow_base_distance_mm` — the distance from the sensor to bare
    /// ground. Every depth is `base - snowdistance`.
    ///
    /// `None` is a gauge with no calibration, which has no computable depth. See
    /// [`register`].
    pub base_distance_mm: Option<f64>,
    /// The station's resolved zone, for the local midnight and the season's boundary.
    ///
    /// `None` is a station with no usable `config.stations.timezone`, which [`register`]
    /// refuses by name.
    pub zone: Option<Zone>,
}

/// What this crate needs from the configuration to plan the snow components.
pub trait SnowConfig {
    /// Whether this node has a weather database. Without one there is no `db:postgres`, so
    /// the hard needs below would have no provider and `plan()` would refuse the whole
    /// configuration (**D-44**).
    fn has_weather_database(&self) -> bool;

    /// Every enabled snow gauge, in station order.
    fn snow_stations(&self) -> Vec<SnowStation>;
}

/// Declare the four snow components.
///
/// # A gauge with no base distance is skipped, and a gauge with no zone is refused
///
/// The two absences look alike and are not, which is why they get different answers:
///
/// - **No `snow_base_distance_mm`** is a *representable* state the schema went out of its
///   way to allow: the column is nullable with a `CHECK (> 0)` precisely so that "not
///   configured" can be told from "configured as zero", and `crate::enrich` already derives
///   no depth from such a reading (`docs/reconciliation.md` DEV-16, part f). There is
///   nothing to compute, so the gauge contributes no component — logged at `warn`, because
///   an operator who installed a gauge and did not measure its height gets zeros forever
///   and the Go daemon told them nothing at all.
/// - **No usable `timezone`** is a plan error naming the station, because every number
///   except the depth depends on a local boundary — "today" and "this season" are both
///   station-local (**D-24**) — and a silent fallback to UTC is the defect W3 §2.5's
///   consequence 2 records for the rainfall rollovers. `mw_pipeline::rollup::register` has
///   the full argument; the only difference is that this registrar refuses directly rather
///   than planning a zoneless `TzSource` for `plan()` to refuse, because three of the four
///   components here are not `Calendar` and would otherwise start without a zone.
pub fn register<C: SnowConfig + ?Sized>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), SnowPlanError> {
    if !cfg.has_weather_database() {
        return Ok(());
    }
    let mut gauges = Vec::new();
    for station in cfg.snow_stations() {
        let Some(base_distance_mm) = station.base_distance_mm else {
            warn!(
                station = %station.key,
                "not planning snow calculations for this gauge: it has no \
                 `snow_base_distance_mm`, so there is no bare-ground reference to measure a \
                 depth against and every total would be zero. Measure the sensor's height \
                 above bare ground and set the column."
            );
            continue;
        };
        let Some(zone) = station.zone else {
            return Err(SnowPlanError::MissingTimeZone {
                station: station.key.to_string(),
            });
        };
        gauges.push(Gauge {
            key: station.key,
            base_distance_mm,
            zone,
        });
    }
    if gauges.is_empty() {
        // The ordinary case: most nodes have no snow gauge. `info` and not `warn` -- there
        // is nothing to fix.
        debug!("no snow gauge is configured, so no `snow:*` component is planned");
        return Ok(());
    }
    let shared = Arc::<[Gauge]>::from(gauges);
    out.push(depth_spec(Arc::clone(&shared))?);
    out.push(smoothing_spec(Arc::clone(&shared))?);
    out.push(events_spec(Arc::clone(&shared))?);
    for gauge in shared.iter() {
        out.push(season_spec(gauge.clone())?);
    }
    info!(
        gauges = shared.len(),
        "planned the snow components: one depth refresh, one smoothing pass, one event pass, \
         and one season total per gauge"
    );
    Ok(())
}

/// Why a configuration does not produce a snow plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnowPlanError {
    /// A gauge with no usable `config.stations.timezone`.
    #[error(
        "snow gauge `{station}` has no usable IANA `timezone`, and every snow total except \
         the current depth is measured from a station-local boundary -- `snowtoday` from \
         local midnight and `snowseason` from 1 October. A fallback to UTC would make both \
         wrong by the station's offset, silently. Set `config.stations.timezone`."
    )]
    MissingTimeZone {
        /// Which station.
        station: String,
    },

    /// A component id that cannot be one.
    #[error("`{what}` cannot name a component")]
    BadId {
        /// What was being named.
        what: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },
}

/// One gauge, with everything resolved — the shape the bodies hold.
#[derive(Debug, Clone, PartialEq)]
struct Gauge {
    key: StationKey,
    base_distance_mm: f64,
    zone: Zone,
}

/// What goes into a shared component's [`ConfigDigest`].
///
/// The gauges, their calibration and their zones: a gauge added, re-measured or moved to
/// another zone restarts the component, and a change elsewhere does not.
fn digest_of(gauges: &[Gauge]) -> ConfigDigest {
    let mut s = String::new();
    for gauge in gauges {
        s.push_str(gauge.key.as_str());
        s.push('=');
        s.push_str(&format!("{:.3}", gauge.base_distance_mm));
        s.push('@');
        s.push_str(gauge.zone.name());
        s.push(';');
    }
    ConfigDigest::of(&s)
}

/// A component id, with the refusal naming what could not be one.
fn component_id(name: &str) -> Result<ComponentId, SnowPlanError> {
    ComponentId::new(name).map_err(|source| SnowPlanError::BadId {
        what: name.to_string(),
        source,
    })
}

/// The needs every snow component declares.
///
/// `SnowCache` **H W** and `ReadingStore` **H** — §4.3's table, verbatim. The `ReadingStore`
/// handle is the read half of the one `db:postgres` publishes, upcast in the builder so that
/// after that line the body **cannot** store a reading even though both halves share the
/// capability.
fn needs() -> [Need; 2] {
    [
        Need::hard_write(Capability::SnowCache),
        Need::hard(Capability::ReadingStore),
    ]
}

/// Declare `snow:depth`.
fn depth_spec(gauges: Arc<[Gauge]>) -> Result<ComponentSpec, SnowPlanError> {
    let id = component_id(DEPTH_ID)?;
    let digest = digest_of(&gauges);
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period: DEPTH_PERIOD,
            deadline: DEPTH_DEADLINE,
            // **Not `Immediate`**, and the reason is that there is nothing cold to start:
            // `snow_totals_cache` persists, so `/snow` serves the last computed depth from
            // the first request after a restart. Thirty seconds of staleness on a column
            // with no recency filter is invisible.
            first: FirstRun::AtPhase,
            // `Spread`, not `Aligned`. `window:rollup` needs a fixed offset past the minute
            // boundary because it reads the bucket that materialises *on* it; this reads
            // whatever is there and writes no column when the newest bucket has not
            // arrived, so there is no boundary to be early for.
            phase: Phase::Spread,
            floor: Duration::ZERO,
        },
        Builder::tick(move |cx| {
            let gauges = Arc::clone(&gauges);
            async move {
                let cache = cx.hard::<Arc<dyn SnowCache>>()?;
                // The upcast to the half this component declared, which is what makes
                // `ReadingStore H` a fact of the type rather than of the declaration.
                let store: Arc<dyn ReadingStore> = cx.hard::<Arc<dyn Store>>()?;
                Ok(Depth {
                    gauges,
                    cache,
                    store,
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    .requires(needs())
    // **W2a Decision 20's declaration for the snow half.** `/snow` depending on the gauge
    // having been read is *data presence*, not capability availability: modelling it as a
    // startup edge would mean the website does not serve until a gauge has reported, and
    // `/snow` has no recency filter precisely so that it keeps answering when one stops. So
    // it is declared here, rendered by `mw_api_mgmt::freshness` as "Snow totals last
    // calculated 3 minutes ago", and **never read by the scheduler**.
    //
    // Twice the period, which on a 30-second tick is a minute: unlike a forecast, these
    // numbers are cheap and a gauge that is reporting produces them every tick, so two
    // missed occurrences is already a component that is not keeping up.
    .freshness([mw_component::Freshness {
        capability: Capability::SnowCache,
        populated_by: component_id(DEPTH_ID)?,
        expected_max_age: DEPTH_PERIOD * 2,
    }])
    .config_digest(digest))
}

/// Declare `snow:smoothing`.
fn smoothing_spec(gauges: Arc<[Gauge]>) -> Result<ComponentSpec, SnowPlanError> {
    let id = component_id(SMOOTHING_ID)?;
    let digest = digest_of(&gauges);
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period: SLOW_PERIOD,
            deadline: SLOW_DEADLINE,
            first: FirstRun::AtPhase,
            phase: Phase::Spread,
            floor: Duration::ZERO,
        },
        Builder::tick(move |cx| {
            let gauges = Arc::clone(&gauges);
            async move {
                let cache = cx.hard::<Arc<dyn SnowCache>>()?;
                let store: Arc<dyn ReadingStore> = cx.hard::<Arc<dyn Store>>()?;
                Ok(Smoothing {
                    gauges,
                    cache,
                    store,
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    .requires(needs())
    .config_digest(digest))
}

/// Declare `snow:events`.
fn events_spec(gauges: Arc<[Gauge]>) -> Result<ComponentSpec, SnowPlanError> {
    let id = component_id(EVENTS_ID)?;
    let digest = digest_of(&gauges);
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period: SLOW_PERIOD,
            deadline: SLOW_DEADLINE,
            first: FirstRun::AtPhase,
            phase: Phase::Spread,
            floor: Duration::ZERO,
        },
        Builder::tick(move |cx| {
            let gauges = Arc::clone(&gauges);
            async move {
                let cache = cx.hard::<Arc<dyn SnowCache>>()?;
                let store: Arc<dyn ReadingStore> = cx.hard::<Arc<dyn Store>>()?;
                Ok(Events {
                    gauges,
                    cache,
                    store,
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    .requires(needs())
    .config_digest(digest))
}

/// Declare `snow:season:<station>`.
fn season_spec(gauge: Gauge) -> Result<ComponentSpec, SnowPlanError> {
    let name = format!("{SEASON_PREFIX}:{}", gauge.key);
    let id = component_id(&name)?;
    let digest = digest_of(std::slice::from_ref(&gauge));
    let station = gauge.key.clone();
    let zone = gauge.zone;
    Ok(ComponentSpec::new(
        id,
        Trigger::Calendar {
            // One slot, so the occurrence key's slot index is always 0 and the key is
            // effectively the local date -- which is what makes a fall-back hour's second
            // pass recognisable as already completed (W2a Decision 13).
            spec: CronSpec::daily_at(SEASON_AT),
            tz: TzSource::Station {
                station: station.clone(),
                // Resolved: `register` refuses a gauge with no zone, so this is never
                // `None` -- which is also why `plan()` cannot report
                // `PlanError::MissingTimeZone` for this component and why the registrar
                // has to.
                zone: Some(zone),
            },
            // The default, and what makes this correct after a restart at noon: the
            // supervisor makes up the most recent missed occurrence, so a node that was off
            // at ten past midnight recomputes the season total on the way up rather than
            // waiting a day.
            misfire: Misfire::DEFAULT,
            deadline: SEASON_DEADLINE,
        },
        Builder::tick(move |cx| {
            let gauge = gauge.clone();
            async move {
                let cache = cx.hard::<Arc<dyn SnowCache>>()?;
                Ok(Season {
                    gauge,
                    cache,
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    // **`ReadingStore` is declared and not used by this one**, which is §4.3's table and is
    // deliberate rather than copied: the season total is `max - first` over
    // `snow_depth_estimates`, which is `SnowCache`'s. Declaring a capability this body does
    // not resolve would make the ordering graph claim an edge that is not there, so it
    // declares `SnowCache` alone.
    .requires([Need::hard_write(Capability::SnowCache)])
    .config_digest(digest))
}

// ---------------------------------------------------------------------------------------
// The bodies.

/// `snow:depth`'s body: the current depth, today's new snow, the storm total and the rate.
struct Depth {
    gauges: Arc<[Gauge]>,
    cache: Arc<dyn SnowCache>,
    store: Arc<dyn ReadingStore>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Depth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Depth")
            .field("gauges", &self.gauges.len())
            .finish_non_exhaustive()
    }
}

impl Tick for Depth {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            for gauge in self.gauges.iter() {
                // Four round trips per gauge, so cancellation is observed between them
                // (**D-24**): a stop during a tick over six gauges should not wait for all
                // twenty-four.
                if cx.is_cancelled() {
                    return Err(ComponentError::Cancelled);
                }
                let patch = self.patch(gauge, now).await.map_err(|e| {
                    // The station is in the message, not only in a log field: a shared
                    // component with two gauges fails for one of them, and an operator
                    // reading `last_error` has no other way to learn which.
                    ComponentError::tick(format!("read snow gauge `{}`'s buckets", gauge.key), e)
                })?;
                write(&self.cache, gauge, patch).await?;
            }
            Ok(())
        })
    }
}

impl Depth {
    /// The four columns this component owns, for one gauge.
    async fn patch(&self, gauge: &Gauge, now: DateTime<Utc>) -> Result<SnowPatch, StoreError> {
        // One minute-level read serves both the depth and the rate: the rate's older
        // sample is thirty minutes back and the lookback is an hour.
        let minutes = self
            .store
            .mean_series(MeanSeriesQuery {
                station: gauge.key.clone(),
                metric: MetricKey::SnowDistance,
                level: BucketLevel::Minute,
                since: now - DEPTH_LOOKBACK,
                until: None,
                limit: buckets_in(DEPTH_LOOKBACK, BucketLevel::Minute),
            })
            .await?;

        let newest = minutes.iter().rev().find(|b| b.mean.is_some());
        // A tick that finds no reading writes **no depth column**, which is what makes the
        // row's persistence reproduce "an offline gauge returns its last known depth
        // indefinitely" (W1 Decision 16) without a recency filter anywhere.
        let depth = newest.and_then(|bucket| {
            let distance = bucket.mean?;
            Some(units::serving_inches(gauge.base_distance_mm - distance))
        });
        let fall_rate = rate(&minutes, now);

        // Today, from the station's own local midnight -- not the database server's, and
        // not Colorado's, which is what `get_new_snow_midnight` hard-codes.
        let today = match units::local_midnight(now, gauge.zone) {
            Some(midnight) => {
                let series = self
                    .store
                    .mean_series(MeanSeriesQuery {
                        station: gauge.key.clone(),
                        metric: MetricKey::SnowDistance,
                        level: BucketLevel::FiveMinute,
                        since: midnight,
                        until: None,
                        limit: buckets_in(Duration::from_secs(25 * 3600), BucketLevel::FiveMinute),
                    })
                    .await?;
                accumulated(&series, gauge.base_distance_mm)
            }
            // Unreachable: `register` refuses a gauge with no zone. Answered rather than
            // asserted, because a component body may not panic over a configuration value.
            None => None,
        };

        // The storm total: the same walk over the last twenty-four hourly buckets.
        //
        // **Go uses a hard-coded base distance here and this does not.**
        // `calculate_storm_snowfall` carries `base_dist := 1798.0; -- TODO: Pass this as
        // parameter from application`, so every station's storm total is computed against
        // the owner's own gauge height -- which for any other gauge is a depth series
        // offset by the difference between the two. The station's own calibration is used
        // instead; `docs/reconciliation.md` records it.
        let storm_series = self
            .store
            .mean_series(MeanSeriesQuery {
                station: gauge.key.clone(),
                metric: MetricKey::SnowDistance,
                level: BucketLevel::Hour,
                since: now - STORM_WINDOW,
                until: None,
                limit: buckets_in(STORM_WINDOW, BucketLevel::Hour),
            })
            .await?;
        let storm = accumulated(&storm_series, gauge.base_distance_mm);

        Ok(SnowPatch {
            depth,
            today,
            storm,
            fall_rate,
            source_through: newest.map(|b| b.bucket),
            ..SnowPatch::NOTHING
        })
    }
}

/// The current snowfall rate, inches per hour — `calculate_current_snowfall_rate`.
///
/// Two readings and a subtraction: the newest within [`RATE_FRESH`], and the one nearest
/// thirty minutes ago within [`RATE_TOLERANCE`] of it. `None` when either is missing, which
/// leaves the stored rate alone; Go returns `0.0` for the same case, and the difference is
/// deliberate — a gauge that has stopped reporting has an *unknown* rate, and writing zero
/// would claim it had stopped snowing.
///
/// Positive-only, which is Go's: `if snowfall_30min <= 0 then return 0`. A decreasing depth
/// is settling, not negative snowfall, and the rate is a rate **of snowfall**.
fn rate(minutes: &[MeanBucket], now: DateTime<Utc>) -> Option<f32> {
    let fresh_from = now - RATE_FRESH;
    let current = minutes
        .iter()
        .rev()
        .find(|b| b.mean.is_some() && b.bucket >= fresh_from)?;
    let target = now - RATE_LAG;
    let tolerance = chrono::Duration::from_std(RATE_TOLERANCE).ok()?;
    // Nearest to the target instant, which is Go's
    // `ORDER BY abs(extract(epoch from (bucket - past_ts))) LIMIT 1`.
    let past = minutes
        .iter()
        .filter(|b| b.mean.is_some() && (b.bucket - target).abs() <= tolerance)
        .min_by_key(|b| (b.bucket - target).abs())?;
    // Distances, so the subtraction is the other way round from a depth: the snow got
    // deeper when the sensor's distance to it got shorter.
    let fell_mm = past.mean? - current.mean?;
    if fell_mm <= 0.0 {
        return Some(0.0);
    }
    // Half an hour extrapolated to an hour, which is Go's `hourly_rate := snowfall_30min * 2`.
    Some(units::serving_inches(fell_mm * 2.0))
}

/// The dual-threshold walk over one bucket series, as the inches `/snow` serves.
///
/// `None` only for a window **the gauge reported nothing in**, which leaves the stored total
/// alone; `Some(0.0)` for a window it reported in and that holds no accumulation. That
/// distinction is the one the depth column makes and for the same reason: a gauge that has
/// stopped reporting has an *unknown* total, and writing zero would claim the snow had gone.
///
/// # The predicate is "did the gauge report", **not** "did the filter keep anything"
///
/// `threshold::depths_from` drops a bucket for two unrelated reasons — no reading in it, and
/// a distance within 2 mm of the base (bare ground, which migration 011 excludes so that a
/// zero depth cannot re-base the walk). Treating an empty *filtered* series as "unknown"
/// conflates them, and the conflation is not harmless on `snowtoday`: that window **resets
/// at local midnight**, so a gauge that is reporting bare ground all day never produces a
/// kept depth, never writes a column, and `/snow` serves yesterday's `snowtoday` — 2.0
/// inches on a clear Wednesday, and on every day after it until the next snowfall. The other
/// totals slide continuously and reach `Some(0.0)` before their window empties, which is why
/// this only ever showed up here.
///
/// So the emptiness that means "unknown" is counted on the **unfiltered** series, and bare
/// ground is a known zero. Go answers `0.0` for both, which is the other half of the same
/// trade: its gauge that goes offline at noon reports no snow today by one o'clock.
fn accumulated(series: &[MeanBucket], base_distance_mm: f64) -> Option<f32> {
    if !series.iter().any(|bucket| bucket.mean.is_some()) {
        return None;
    }
    let pairs: Vec<(DateTime<Utc>, Option<f64>)> =
        series.iter().map(|b| (b.bucket, b.mean)).collect();
    let depths: Vec<f64> = threshold::depths_from(&pairs, base_distance_mm)
        .into_iter()
        .map(|(_, mm)| mm)
        .collect();
    Some(units::serving_inches(threshold::accumulated_mm(&depths)))
}

/// How many buckets a window holds at one level, as a read's `limit`.
///
/// One more than the window needs, because the window's own first and last buckets are both
/// partly inside it.
fn buckets_in(window: Duration, level: BucketLevel) -> usize {
    (window.as_secs() / level.span().as_secs()) as usize + 1
}

/// Write a patch, unless it carries nothing.
///
/// **A patch with no column does not touch the row**, and that is the one behaviour worth
/// stating: `/snow` distinguishes "no row at all" from "a row of zeros" (fixture
/// `snow-device-never-reported` against `snow-device-stale`), so a tick that found nothing
/// to compute must not insert a row of zeros for a gauge that has never reported. It would
/// render identically today and it would destroy an operational distinction
/// `mw_component::SnowCache::totals` goes out of its way to keep.
async fn write(
    cache: &Arc<dyn SnowCache>,
    gauge: &Gauge,
    patch: SnowPatch,
) -> Result<(), ComponentError> {
    if patch.is_empty() {
        debug!(
            station = %gauge.key,
            "nothing to compute for this gauge on this tick, so its cache row is untouched"
        );
        return Ok(());
    }
    cache.store_totals(&gauge.key, patch).await.map_err(|e| {
        ComponentError::tick(
            format!("write snow gauge `{}`'s totals cache", gauge.key),
            e,
        )
    })
}

/// `snow:smoothing`'s body: the estimated depth series, and the totals read off it.
struct Smoothing {
    gauges: Arc<[Gauge]>,
    cache: Arc<dyn SnowCache>,
    store: Arc<dyn ReadingStore>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Smoothing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Smoothing")
            .field("gauges", &self.gauges.len())
            .finish_non_exhaustive()
    }
}

impl Tick for Smoothing {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            for gauge in self.gauges.iter() {
                // Cancellation observed between gauges (**D-24**): the backfill can be
                // several passes long and a stop should not have to wait for all of them.
                if cx.is_cancelled() {
                    return Err(ComponentError::Cancelled);
                }
                let estimated = self.estimate(gauge, now, cx).await.map_err(|e| {
                    ComponentError::tick(
                        format!("estimate snow gauge `{}`'s depth series", gauge.key),
                        e,
                    )
                })?;
                let patch = self.totals(gauge, now).await.map_err(|e| {
                    ComponentError::tick(
                        format!("read snow gauge `{}`'s estimated depth series", gauge.key),
                        e,
                    )
                })?;
                if estimated > 0 {
                    debug!(
                        station = %gauge.key,
                        estimates = estimated,
                        "re-derived the smoothed depth series"
                    );
                }
                write(&self.cache, gauge, patch).await?;
            }
            Ok(())
        })
    }
}

impl Smoothing {
    /// Re-derive the estimate series, in bounded passes. Returns how many rows were written.
    ///
    /// One phase per level in [`BACKFILL`], oldest history first, each phase chunked into at
    /// most [`MAX_PASSES`] passes of [`PASS_BUCKETS`]. In the steady state only the last
    /// phase has anything to do and it is one pass of about eighty buckets; the earlier
    /// phases exist for the cold start, where reading the 5-minute level over a whole season
    /// would read a level the schema deletes after fourteen days.
    async fn estimate(
        &self,
        gauge: &Gauge,
        now: DateTime<Utc>,
        cx: &Cx,
    ) -> Result<usize, StoreError> {
        // Where this tick starts: six hours behind the newest estimate, or the season's
        // first instant for a station that has none.
        let start = match self.cache.newest_estimate(&gauge.key).await? {
            Some(newest) => newest.bucket - overlap(),
            None => match units::season_start(now, gauge.zone) {
                Some(season) => {
                    info!(
                        station = %gauge.key,
                        from = %season,
                        "no smoothed depth estimates for this gauge, so backfilling from \
                         the season's first instant"
                    );
                    season
                }
                // Unreachable for the same reason `Depth::patch`'s is.
                None => return Ok(0),
            },
        };
        let mut written = 0;
        // Each phase's window ends where the next finer level's begins, so the phases
        // partition `[start, now]` with no gap and no double coverage beyond the overlap.
        // Walked coarsest first, so the rate limiter's carried state always exists by the
        // time a finer phase needs it.
        let mut phases: Vec<(BucketLevel, DateTime<Utc>, DateTime<Utc>)> = Vec::new();
        let mut phase_end = now;
        for (level, horizon) in BACKFILL {
            let phase_start = match horizon_start(now, horizon) {
                // A bounded level: this phase covers its own band, and anything older
                // belongs to the next, coarser one.
                Some(boundary) => boundary.max(start),
                // The unbounded level takes whatever is left.
                None => start,
            };
            if phase_start < phase_end {
                phases.push((level, phase_start, phase_end));
            }
            phase_end = phase_start;
            if phase_end <= start {
                break;
            }
        }
        phases.reverse();
        for (level, phase_start, phase_end) in phases {
            let pass = Duration::from_secs(level.span().as_secs() * PASS_BUCKETS as u64);
            let mut from = phase_start;
            for _ in 0..MAX_PASSES {
                // Observed inside the loop and not only between gauges: a cold-start
                // backfill is three phases of up to eight passes, and a stop should not
                // have to wait for all of them (**D-24**).
                if cx.is_cancelled() {
                    return Ok(written);
                }
                let until = (from + pass).min(phase_end);
                let series = self
                    .store
                    .mean_series(MeanSeriesQuery {
                        station: gauge.key.clone(),
                        metric: MetricKey::SnowDistance,
                        level,
                        since: from,
                        until: Some(until),
                        limit: PASS_BUCKETS,
                    })
                    .await?;
                // The rate limiter's carried state: the newest estimate from **before** this
                // pass's window. Re-read per pass, because the previous pass wrote one.
                let previous = self.cache.estimate_before(&gauge.key, from).await?;
                let rows = estimates(&series, gauge.base_distance_mm, previous);
                // **The replace is bounded above as well as below**, which the one-level
                // version did not need: a coarse phase must not delete the finer estimates
                // that follow it, and `until` is where the next phase begins.
                self.cache
                    .replace_estimates(&gauge.key, from, Some(until), &rows)
                    .await?;
                written += rows.len();
                if until >= phase_end {
                    break;
                }
                // The next pass starts inside this one, by the smoothing overlap: its first
                // samples are then limited against an estimate that is outside its own
                // window.
                from = until - overlap();
            }
        }
        Ok(written)
    }

    /// `snowlast24`, `snowlast72` and the re-derived `snowseason`.
    async fn totals(&self, gauge: &Gauge, now: DateTime<Utc>) -> Result<SnowPatch, StoreError> {
        let last24 = self
            .spread(gauge, now - chrono::Duration::hours(24))
            .await?;
        let last72 = self
            .spread(gauge, now - chrono::Duration::hours(72))
            .await?;
        // §2a.7's 15-minute re-derivation of the season total. `snow:season` owns the
        // column's daily recomputation; this is the same expression over the same handle,
        // and the module documentation has why both exist.
        let season = match units::season_start(now, gauge.zone) {
            Some(start) => season_total(&self.cache, &gauge.key, start).await?,
            None => None,
        };
        // **No `source_through`.** It has one owner, `snow:depth`, and the handle's own
        // documentation has why: it is one column with one meaning, and two components whose
        // newest buckets are at two cadences would overwrite each other's answer four times
        // a minute -- 11:59 from the depth refresh, 11:45 from this one, 11:59 again. The
        // fastest tier's is the one a staleness display wants.
        Ok(SnowPatch {
            last24,
            last72,
            season,
            ..SnowPatch::NOTHING
        })
    }

    /// `max - min` over one window of the estimate series, as served inches.
    ///
    /// `SmoothedComputer::computeAccumulation`. `None` for a window with no estimates in it,
    /// which leaves the stored total alone rather than zeroing it — a gauge whose readings
    /// stopped an hour ago has an *unknown* 24-hour total, and Go's `return 0, nil` on an
    /// empty window claims it had no snow.
    async fn spread(&self, gauge: &Gauge, from: DateTime<Utc>) -> Result<Option<f32>, StoreError> {
        let Some(window) = self.cache.estimate_window(&gauge.key, from).await? else {
            return Ok(None);
        };
        let spread = f64::from(window.max_in) - f64::from(window.min_in);
        Ok(Some(units::serving_from_inches(spread.max(0.0))))
    }
}

/// The instant a retention horizon begins, or `None` for an unbounded level.
fn horizon_start(now: DateTime<Utc>, horizon: Duration) -> Option<DateTime<Utc>> {
    let horizon = chrono::Duration::from_std(horizon).ok()?;
    now.checked_sub_signed(horizon)
}

/// The smoothing overlap as a `chrono` duration.
fn overlap() -> chrono::Duration {
    chrono::Duration::from_std(SMOOTHING_OVERLAP).unwrap_or_else(|_| chrono::Duration::hours(6))
}

/// One pass's estimates: the distance series, converted, smoothed and rate-limited.
fn estimates(
    series: &[MeanBucket],
    base_distance_mm: f64,
    previous: Option<DepthEstimate>,
) -> Vec<DepthEstimate> {
    let pairs: Vec<(DateTime<Utc>, Option<f64>)> =
        series.iter().map(|b| (b.bucket, b.mean)).collect();
    let samples: Vec<smoothing::Sample> = threshold::depths_from(&pairs, base_distance_mm)
        .into_iter()
        .map(|(at, depth_mm)| smoothing::Sample {
            at,
            depth_in: depth_mm / units::MM_PER_INCH,
        })
        .collect();
    if samples.is_empty() {
        return Vec::new();
    }
    let carried = previous.map(|prior| smoothing::Sample {
        at: prior.bucket,
        depth_in: f64::from(prior.depth_in),
    });
    let limited = smoothing::estimate(&samples, carried);
    samples
        .iter()
        .zip(limited)
        .map(|(sample, depth_in)| DepthEstimate {
            bucket: sample.at,
            depth_in: depth_in as f32,
        })
        .collect()
}

/// `max - first` over the estimate series since the season's start, as served inches.
///
/// `SmoothedComputer::ComputeSeasonal`, and the subtrahend is the **first** estimate rather
/// than the smallest: a season that began with snow on the ground did not have that pack
/// fall during it.
///
/// # An empty window is a known zero when the gauge has a series at all
///
/// `None` here leaves the stored total alone, and the only state that deserves it is a gauge
/// the smoother has never run for. A gauge **with** a series and nothing in this season is
/// the 1 October case, and it is the one that bites: on the day the season boundary moves,
/// every estimate is suddenly older than it, so an unconditional `None` leaves last season's
/// 182 inches on a frozen response — beside a correct `snowtoday: 0` — for the whole of
/// October, until the first flake. Go reports zero.
///
/// So the two are told apart by whether the gauge has a newest estimate at all, which is one
/// indexed lookup and the same question `Smoothing::estimate` already asks.
async fn season_total(
    cache: &Arc<dyn SnowCache>,
    station: &StationKey,
    start: DateTime<Utc>,
) -> Result<Option<f32>, StoreError> {
    let Some(window) = cache.estimate_window(station, start).await? else {
        return match cache.newest_estimate(station).await? {
            // A series exists and none of it is in this season: nothing has fallen yet.
            Some(_) => Ok(Some(0.0)),
            // The smoother has never run for this gauge, so the total is unknown.
            None => Ok(None),
        };
    };
    let accumulated = f64::from(window.max_in) - f64::from(window.first_in);
    Ok(Some(units::serving_from_inches(accumulated.max(0.0))))
}

/// `snow:events`' body: the classified accumulation segments per chart range.
struct Events {
    gauges: Arc<[Gauge]>,
    cache: Arc<dyn SnowCache>,
    store: Arc<dyn ReadingStore>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events")
            .field("gauges", &self.gauges.len())
            .finish_non_exhaustive()
    }
}

impl Tick for Events {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            for gauge in self.gauges.iter() {
                for window in EVENT_WINDOWS {
                    // Four windows per gauge, each an `O(n²)` detection: cancellation is
                    // observed between them so a stop does not wait for the longest.
                    if cx.is_cancelled() {
                        return Err(ComponentError::Cancelled);
                    }
                    let detected = self.detect(gauge, window, now).await.map_err(|e| {
                        ComponentError::tick(
                            format!(
                                "detect snow gauge `{}`'s events over {} hours",
                                gauge.key, window.hours
                            ),
                            e,
                        )
                    })?;
                    self.cache
                        .replace_events(&gauge.key, window.hours, &detected)
                        .await
                        .map_err(|e| {
                            ComponentError::tick(
                                format!("write snow gauge `{}`'s events", gauge.key),
                                e,
                            )
                        })?;
                    debug!(
                        station = %gauge.key,
                        hours = window.hours,
                        events = detected.len(),
                        "detected the snow accumulation events for this chart range"
                    );
                }
            }
            Ok(())
        })
    }
}

impl Events {
    /// One `(gauge, window)`'s contributing segments.
    async fn detect(
        &self,
        gauge: &Gauge,
        window: EventWindow,
        now: DateTime<Utc>,
    ) -> Result<Vec<SnowEvent>, StoreError> {
        let series = self
            .store
            .mean_series(MeanSeriesQuery {
                station: gauge.key.clone(),
                metric: MetricKey::SnowDistance,
                level: window.level,
                since: now - chrono::Duration::days(window.days),
                until: None,
                limit: pelt::MAX_POINTS,
            })
            .await?;
        let pairs: Vec<(DateTime<Utc>, Option<f64>)> =
            series.iter().map(|b| (b.bucket, b.mean)).collect();
        let depths = threshold::depths_from(&pairs, gauge.base_distance_mm);
        if depths.is_empty() {
            // No usable readings in the window, so no events -- and the empty write above
            // is what removes the ones the last pass found. A chart that kept drawing a
            // week-old band would be worse than one with no band.
            return Ok(Vec::new());
        }
        // Truncated to the newest buckets **here**, before the detector, because this is
        // where the instants live: `pelt::segments` enforces the same cap on its own input
        // (it is where the `O(n²)` matrices are allocated), and the segment indices it
        // returns are indices into whatever it was given -- so the instant list has to have
        // been cut the same way or a band would be labelled with another day's times.
        // `EVENT_WINDOWS` keeps every window under 800 buckets, so this is a no-op today.
        let depths = &depths[depths.len().saturating_sub(pelt::MAX_POINTS)..];
        let values: Vec<f64> = depths.iter().map(|&(_, mm)| mm).collect();
        let (_smoothed, segments) = pelt::segments(&values, window.kernel);
        Ok(segments
            .into_iter()
            // Go's `if seg.SnowMM > 0`, written as the class as well: the table's own
            // `CHECK` ties a non-null accumulation to exactly these two classes, so the
            // filter and the constraint cannot disagree.
            .filter(|segment| segment.class.contributes() && segment.snow_mm > 0.0)
            .map(|segment| SnowEvent {
                // The segment's own first and last bucket, which `classify` indexes into
                // the depth series and this maps back to instants.
                start: depths[segment.start].0,
                end: depths[segment.end - 1].0,
                class: segment.class,
                start_depth_mm: segment.start_depth_mm as f32,
                end_depth_mm: segment.end_depth_mm as f32,
                accumulation_mm: Some(segment.snow_mm as f32),
            })
            .collect())
    }
}

/// `snow:season:<station>`'s body: the season total, recomputed at the station's own
/// local 00:10.
struct Season {
    gauge: Gauge,
    cache: Arc<dyn SnowCache>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Season {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Season")
            .field("station", &self.gauge.key)
            .finish_non_exhaustive()
    }
}

impl Tick for Season {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            let Some(start) = units::season_start(now, self.gauge.zone) else {
                // Unreachable: `register` refuses a gauge with no zone.
                return Ok(());
            };
            let season = season_total(&self.cache, &self.gauge.key, start)
                .await
                .map_err(|e| {
                    ComponentError::tick(
                        format!(
                            "read snow gauge `{}`'s estimated depth series",
                            self.gauge.key
                        ),
                        e,
                    )
                })?;
            let patch = SnowPatch {
                season,
                ..SnowPatch::NOTHING
            };
            write(&self.cache, &self.gauge, patch).await?;
            if let Some(total) = season {
                info!(
                    station = %self.gauge.key,
                    season_start = %start,
                    inches = total,
                    "recomputed the season snowfall total"
                );
            }
            Ok(())
        })
    }
}

/// Every class a segment can be, for the exhaustiveness the table's `CHECK` mirrors.
///
/// Here rather than in `mw-component` because this is the crate that classifies: the handle
/// trait needs the enum and not the roster.
pub const EVENT_CLASSES: [EventClass; 4] = [
    EventClass::Accumulation,
    EventClass::Plateau,
    EventClass::Redistribution,
    EventClass::SpikeThenSettle,
];

#[cfg(test)]
mod tests;
