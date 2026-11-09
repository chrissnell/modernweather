//! `window:rollup` and `rain:rollover:<station>` — **R2**'s half of the derived values.
//!
//! ```text
//!  observations_1m  ─ buckets ──▶ ┌───────────────┐
//!  observations_1h  ─ series ───▶ │ window:rollup │──┐
//!  1m/1h/1d         ─ totals ───▶ └───────────────┘  │   Arc<dyn WindowedView>
//!                                                    ├──▶ aprs:* pws:* wu:*  (M6-2)
//!  Calendar { daily 00:00, ─▶ ┌──────────────────┐   │    snapshot:latest     (M4-1)
//!             TzSource::Station } │ rain:rollover │──┘
//!                                 └──────────────────┘
//!                              both write one Arc<RollupState>
//! ```
//!
//! # The two components, and why there are two
//!
//! | Component | Trigger | Requires | Provides |
//! |---|---|---|---|
//! | `window:rollup` | `Every { 60 s, 10 s, Immediate, Fixed(5 s) }` | `ReadingStore` H | `WindowedView` |
//! | `rain:rollover:<station>` | `Calendar { daily 00:00, TzSource::Station, RunLatest }` | — | — |
//!
//! One component, one trigger (W2a §4.3's Aeris argument), and these are two triggers: a
//! 60-second accumulation and a daily boundary. W3 §2.5's consequence 2 writes the pair as
//! exactly that — "a `Calendar { daily 00:00, TzSource::Station }` component for the
//! boundary plus `Every { 60 s }` for the accumulation since it, so the boundary is correct
//! by construction" — and calls it `rain:totals`. `super::state`'s module documentation has
//! why the accumulating half is `window:rollup` here rather than a third component.
//!
//! # `Phase::Fixed(5 s)`, not `Phase::Aligned`, and the five seconds are measured
//!
//! `docs/component-model.md` §4.1 declares `Phase::Aligned` "because its input is the
//! 1-minute aggregate, which materialises on minute boundaries; spreading its phase would
//! read a bucket that is not there yet". The premise is right and the conclusion is three
//! seconds early: W4 **D-26** phase-locks the 1-minute refresh **3 seconds past** each
//! boundary with `end_offset => 1 second`, and measured the lag at **3.06–3.22 s**. A
//! component at offset zero therefore reads at the one instant the bucket it wants is
//! reliably absent.
//!
//! So the offset is [`PHASE`] — five seconds, which is after the measured lag and a long
//! way inside the 10-second deadline W2a assumption 14 gives this component. That is a
//! narrowing of `Phase::Aligned` rather than a departure from its reasoning: nothing is
//! spread, every station's rollup still fires on the same grid, and "next run at" is still
//! answerable to the second.
//!
//! It is also not load-bearing. The refresh is **incremental** — a tick asks for the
//! buckets after the newest one it holds — so a refresh that is late costs a minute of
//! staleness and never a value. The five seconds buy the steady state, not correctness.
//!
//! # What this item does not build
//!
//! The snow totals. `implementation-plan.md` M3-4's prose lists R2's whole set, including
//! "the snow totals", and those are four components over `Capability::SnowCache` (W3
//! §2a.7, `docs/component-model.md` §4.3) which `db:postgres` does not publish — its own
//! spec test on `main` says so, naming M5 and M6-3 as the items that bring it. `snow:*`
//! also needs the PELT changepoint detection and the RBF smoothing, which is a body of
//! arithmetic with its own done-criteria. M6-3 builds them.

pub mod calendar;
pub mod rain;
pub mod ring;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{
    BoxFuture, BucketLevel, BucketQuery, Builder, Capability, ComponentError, ComponentId,
    ComponentIdError, ComponentSpec, ConfigDigest, CronSpec, Cx, FirstRun, LocalTime, Misfire,
    Need, Phase, Piece, Provision, Publisher, ReadingStore, SeriesQuery, Store, Tick, TotalsQuery,
    Trigger, TzSource, WindowedView, Zone,
};
use mw_types::{MetricKey, StationKey, Strategy, Window, LEVELS};
use tracing::{info, warn};

use crate::clock::{Clock, SystemClock};
use crate::metrics as m;
use ring::{RING, RING_SPAN};
use state::{Boundaries, Rollover, RollupState, Windows, MAINTAINED};

/// `window:rollup`'s component id.
pub const ID: &str = "window:rollup";

/// The prefix of a `rain:rollover:<station>` component id.
pub const ROLLOVER_PREFIX: &str = "rain:rollover";

/// The accumulation period (W2a §4.1).
pub const PERIOD: Duration = Duration::from_secs(60);

/// The per-tick deadline (W2a assumption 14, W3 assumption 21).
///
/// Ten seconds from the minute boundary. W4 **D-26** measured the 1-minute aggregate's
/// refresh lag at 3.06–3.22 s against it, so a deadline miss here means the server is hung
/// rather than slow — the same reasoning `writer:timescale`'s 5 seconds has.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// How far past each minute boundary the tick fires. See the module documentation.
pub const PHASE: Duration = Duration::from_secs(5);

/// The rollover's per-occurrence deadline.
///
/// A rollover does no I/O at all — it computes three instants from a zone and swaps them in
/// — so this is three orders of magnitude of slack and a miss would mean the task never
/// got scheduled.
pub const ROLLOVER_DEADLINE: Duration = Duration::from_secs(30);

/// What this crate needs from the configuration to plan the rollup.
///
/// A trait rather than a type for the reason `mw_station::StationConfig` is one: `plan()` is
/// generic over the configuration and the binary chooses it (§1.3 rule 2), so a registrar
/// takes `&C` and each component crate names only the rows it plans from.
pub trait RollupConfig {
    /// Whether this node has a weather database. Without one there is no `db:postgres`, so
    /// `window:rollup`'s hard need would have no provider and `plan()` would refuse the
    /// whole configuration (**D-44**).
    fn has_weather_database(&self) -> bool;

    /// The shared state, built by `main` as a root resource.
    ///
    /// `None` is a node with no rollup, which is a node with no weather database. It
    /// travels through the registrar rather than the capability bus because **two**
    /// components write it and the bus holds one handle type per provider —
    /// `mw_station::config`'s module documentation has the whole of that argument, for
    /// `Ingress`.
    fn rollup_state(&self) -> Option<&Arc<RollupState>>;

    /// Every station this node collects for, with its resolved zone.
    ///
    /// `None` for a station with no usable `config.stations.timezone`. Such a station gets
    /// **no rollover component**, which `plan()` then reports as
    /// `PlanError::MissingTimeZone` naming it — see [`register`].
    fn rollup_stations(&self) -> Vec<(StationKey, Option<Zone>)>;
}

/// Declare `window:rollup` and one `rain:rollover:<station>` per station.
///
/// # A station with no time zone is a refusal, and it is the point of the item
///
/// A rollover component is emitted for **every** station, including one whose zone did not
/// resolve — and `plan()` then refuses the whole configuration with
/// `PlanError::MissingTimeZone` naming that station, because `TzSource::zone()` is `None`.
/// That is deliberate and it is not a convenience that got lost:
///
/// - **Skipping the component instead** is the defect this programme keeps finding. A
///   configured station whose rainfall totals silently never roll over produces no error, no
///   log line and a `dayrain` that only resets when the *database server's* midnight comes
///   round — which is precisely what `date_trunc('day', NOW())` does today and precisely
///   what makes the device's declared `timezone` decorative.
/// - **Defaulting to the deployment zone** is the same silence with an extra step. `TzSource`
///   has a `Deployment` variant for components that are not scoped to a station, and a
///   station's rainfall day is scoped to a station.
/// - The configuration schema's comment already says this is the answer: *"A station with a
///   `Calendar` component and no valid zone is a plan error rather than a silent fallback to
///   UTC"* (`migrations/config/20261102230000_config_schema_initial.sql`).
///
/// The operator-facing consequence, stated because it is a new requirement: **every station
/// in `config.stations` must have an IANA `timezone`**. The refusal names the station and
/// the fix is one `UPDATE`. A *malformed* zone is caught earlier and more precisely, at
/// `mw_store::RegistrySnapshot::new`, which refuses at startup with the station and the
/// typed value in the message.
pub fn register<C: RollupConfig + ?Sized>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), RollupPlanError> {
    if !cfg.has_weather_database() {
        return Ok(());
    }
    // The station set is checked before the state, because a node that collects for
    // nothing has no rollup whether or not the state was built -- the satellite shape (W7
    // §8.2) reaches `plan()` with neither.
    let stations = cfg.rollup_stations();
    if stations.is_empty() {
        // Nothing to roll up. Not an error and not a silence either: a node that serves
        // and does not collect is W7 §8.2's shape, and `db:postgres` is still planned for
        // it because the read path needs the pool.
        info!(
            "a weather database is configured and this node collects for no station, so \
             `window:rollup` is not planned -- there are no windows to maintain"
        );
        return Ok(());
    }
    let Some(state) = cfg.rollup_state() else {
        return Err(RollupPlanError::NoState);
    };
    out.push(spec(Arc::clone(state))?);
    for (station, zone) in stations {
        out.push(rollover_spec(Arc::clone(state), station, zone)?);
    }
    Ok(())
}

/// Why a set of stations does not produce a rollup plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RollupPlanError {
    /// `main` did not build the shared state before `plan()` ran.
    ///
    /// A bug in the boot sequence rather than a configuration error, reported as a
    /// registrar failure naming what is missing instead of a panic — the same shape
    /// `db:postgres`'s missing registry takes.
    #[error(
        "the window rollup's shared state has not been built, so `window:rollup` and \
         `rain:rollover:*` have nothing to write. `main` builds it beside the spool and \
         the station registry (crates/modernweather/src/boot.rs)."
    )]
    NoState,

    /// A station key that cannot name a component.
    #[error("station `{station}` cannot name a component")]
    BadId {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },
}

/// Declare `window:rollup`.
pub fn spec(state: Arc<RollupState>) -> Result<ComponentSpec, RollupPlanError> {
    let id = ComponentId::new(ID).map_err(|source| RollupPlanError::BadId {
        station: ID.to_string(),
        source,
    })?;
    // The station set and their zones are the whole of what this component closes over, so
    // they are the whole of the digest: adding a station or correcting a zone restarts it,
    // and a change elsewhere does not.
    let digest = ConfigDigest::of(&state_digest_input(&state));
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period: PERIOD,
            deadline: DEADLINE,
            // Warm up rather than waiting a minute for the first window: a restart would
            // otherwise leave every uploader in `Waiting { on: WindowedView }` for a
            // period, and this component's input is already in the database.
            first: FirstRun::Immediate,
            phase: Phase::Fixed(PHASE),
            floor: Duration::ZERO,
        },
        Builder::tick(move |cx| {
            let state = Arc::clone(&state);
            async move {
                let publisher = cx.publisher::<Arc<dyn WindowedView>>()?;
                // The **upcast to the half this component declared** (W2a §4.1b's
                // `ReadingStore H` against `writer:timescale`'s `H W`): after this line
                // the body holds a read handle and cannot store anything, even though both
                // resolved `Capability::ReadingStore`.
                let store: Arc<dyn ReadingStore> = cx.hard::<Arc<dyn Store>>()?;
                Ok(Rollup {
                    store,
                    state,
                    publisher,
                    clock: Arc::new(SystemClock),
                    published: false,
                })
            }
        }),
    )
    .requires([Need::hard(Capability::ReadingStore)])
    .provides([Provision::of(Capability::WindowedView)])
    .config_digest(digest))
}

/// Declare `rain:rollover:<station>`.
pub fn rollover_spec(
    state: Arc<RollupState>,
    station: StationKey,
    zone: Option<Zone>,
) -> Result<ComponentSpec, RollupPlanError> {
    let name = format!("{ROLLOVER_PREFIX}:{station}");
    let id = ComponentId::new(name.clone()).map_err(|source| RollupPlanError::BadId {
        station: station.to_string(),
        source,
    })?;
    let digest = ConfigDigest::of(&format!(
        "{station}:{}",
        zone.map(|z| z.name()).unwrap_or("<none>")
    ));
    Ok(ComponentSpec::new(
        id,
        Trigger::Calendar {
            // Local midnight, every day. One slot, so the occurrence key's slot index is
            // always 0 and the key is effectively the local date — which is what makes
            // fall-back's second pass recognisable (W2a Decision 13).
            spec: CronSpec::daily_at(LocalTime::MIDNIGHT),
            tz: TzSource::Station {
                station: station.clone(),
                zone,
            },
            // The default, and the reason this component works at all after a restart at
            // two in the afternoon: the supervisor makes up the most recent missed
            // occurrence, which for a daily schedule is this morning's local midnight, so
            // the boundaries are published within a second of reaching `Running` rather
            // than at the next one.
            misfire: Misfire::DEFAULT,
            deadline: ROLLOVER_DEADLINE,
        },
        Builder::tick(move |_cx| {
            let state = Arc::clone(&state);
            let station = station.clone();
            async move {
                Ok(RainRollover {
                    state,
                    station,
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    // **No needs at all, and that is the whole of why it is a second component.** The
    // boundary is a function of a zone and the clock; the accumulation is a function of the
    // database. A rollover that declared `ReadingStore` would sit in
    // `Waiting { on: ReadingStore }` through a database outage and the day's boundary would
    // be an hour late when the server came back, which is a wrong `dayrain` rather than a
    // stale one.
    .config_digest(digest))
}

/// What goes into `window:rollup`'s [`ConfigDigest`].
fn state_digest_input(state: &RollupState) -> String {
    let mut s = String::new();
    for (station, zone) in state.stations() {
        s.push_str(station.as_str());
        s.push('=');
        s.push_str(zone.map(|z| z.name()).unwrap_or("<none>"));
        s.push(';');
    }
    s
}

/// `window:rollup`'s body.
struct Rollup {
    store: Arc<dyn ReadingStore>,
    state: Arc<RollupState>,
    publisher: Publisher<Arc<dyn WindowedView>>,
    clock: Arc<dyn Clock>,
    published: bool,
}

impl std::fmt::Debug for Rollup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rollup")
            .field("stations", &self.state.len())
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

impl Tick for Rollup {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            // The newest **complete** minute, exclusive: the bucket starting here is still
            // being written to. Every window is measured back from it, so two stations
            // resolved in one tick are resolved against the same instant.
            let until = calendar::floor_to(now, BucketLevel::Minute.span());
            let stations: Vec<(StationKey, Option<Zone>)> =
                self.state.stations().map(|(k, z)| (k.clone(), z)).collect();
            for (station, _) in stations {
                self.refresh(&station, now, until).await?;
            }
            if !self.published {
                // Published on the first completed tick rather than in `build`, so a
                // consumer sits in `Waiting { on: WindowedView }` until there is something
                // to resolve rather than being handed a view that answers
                // `UnknownStation` for every station (W2a Decision 22's readiness, the
                // same shape `db:postgres` has).
                self.publisher
                    .publish(Arc::clone(&self.state) as Arc<dyn WindowedView>);
                self.published = true;
                info!(
                    stations = self.state.len(),
                    "the windowed view is usable: {} stations, a {}-minute ring and {} \
                     maintained windows per total metric",
                    self.state.len(),
                    RING,
                    MAINTAINED.len()
                );
            }
            Ok(())
        })
    }
}

impl Rollup {
    /// One station's ring and maintained totals.
    async fn refresh(
        &self,
        station: &StationKey,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<(), ComponentError> {
        // A station the state does not know cannot be reached from here -- the loop above
        // iterates the state's own keys -- so this is the empty ring a first tick starts
        // from, named rather than defaulted.
        let previous = match self.state.windows(station) {
            Some(windows) => windows,
            None => Arc::new(Windows::default()),
        };
        let horizon = until
            .checked_sub_signed(chrono::Duration::seconds(RING_SPAN.as_secs() as i64))
            .unwrap_or(until);
        // Incremental: everything after the newest bucket held, or the whole hour on the
        // first tick after a start. `+ 1` on the bucket's own instant, because `since` is
        // inclusive and the newest held bucket is not wanted again.
        let since = match previous.ring.newest() {
            Some(newest) => (newest + chrono::Duration::minutes(1)).max(horizon),
            None => horizon,
        };
        let mut ring = previous.ring.clone();
        let fetched = self
            .store
            .buckets(BucketQuery {
                station: station.clone(),
                since,
                limit: RING,
            })
            .await?;
        m::rollup_buckets_read(fetched.len() as u64);
        ring.extend(fetched.into_iter().filter(|b| b.bucket < until), horizon);

        let storm = self.storm(station, now).await?;
        let long = self
            .maintain(station, storm.map(|s| s.start), now, until)
            .await?;
        self.state.publish_windows(
            station,
            Windows {
                as_of: Some(now),
                until: Some(until),
                ring,
                long,
                storm_start: storm.map(|s| s.start),
            },
        );
        Ok(())
    }

    /// The current rain event, from the hourly series.
    async fn storm(
        &self,
        station: &StationKey,
        now: DateTime<Utc>,
    ) -> Result<Option<rain::Storm>, ComponentError> {
        let since = now
            .checked_sub_signed(chrono::Duration::seconds(
                rain::STORM_HORIZON.as_secs() as i64
            ))
            .unwrap_or(now);
        let series = self
            .store
            .series(SeriesQuery {
                station: station.clone(),
                metric: MetricKey::RainIncremental,
                level: BucketLevel::Hour,
                since,
                limit: rain::STORM_BUCKETS,
            })
            .await?;
        let storm = rain::current_storm(&series, now);
        if storm.is_some_and(|s| s.clamped) {
            // The dedupe layer throttles the repeat (§5.6), so a storm that fills the
            // horizon says so once rather than once a minute.
            warn!(
                %station,
                horizon_days = rain::STORM_HORIZON.as_secs() / 86_400,
                "this station has had no 24-hour dry spell within the storm horizon, so \
                 the storm total is measured from the horizon rather than from the start \
                 of the event"
            );
        }
        Ok(storm)
    }

    /// Every maintained window, for every metric that declares a total.
    async fn maintain(
        &self,
        station: &StationKey,
        storm_start: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<((MetricKey, Window), Option<f64>)>, ComponentError> {
        let boundaries = self.state.boundaries(station);
        let mut out = Vec::with_capacity(total_metrics().len() * MAINTAINED.len());
        for metric in total_metrics() {
            // One `totals` call per metric, carrying every window's pieces at once: the
            // store groups them by level, so this is one statement per level rather than
            // one per window.
            let mut pieces: Vec<Piece> = Vec::new();
            let mut spans: Vec<(Window, std::ops::Range<usize>)> = Vec::new();
            for window in MAINTAINED {
                let from = match window_from(window, boundaries, storm_start, until) {
                    Some(from) => from,
                    None => {
                        // No boundary yet: the rollover has not run, or there is no storm.
                        // `None` rather than a zero, because "we do not know when the day
                        // began" is not "no rain has fallen today".
                        out.push(((*metric, window), None));
                        continue;
                    }
                };
                let start = pieces.len();
                pieces.extend(calendar::decompose(from, until, now));
                spans.push((window, start..pieces.len()));
            }
            if pieces.is_empty() {
                continue;
            }
            let sums = self
                .store
                .totals(TotalsQuery {
                    station: station.clone(),
                    metric: *metric,
                    pieces: pieces.clone(),
                })
                .await?;
            m::rollup_pieces_summed(pieces.len() as u64);
            for (window, range) in spans {
                out.push(((*metric, window), combine(&sums[range])));
            }
        }
        Ok(out)
    }
}

/// The first instant of one maintained window.
fn window_from(
    window: Window,
    boundaries: Option<Boundaries>,
    storm_start: Option<DateTime<Utc>>,
    until: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match window {
        Window::Day => boundaries.map(|b| b.day),
        Window::Month => boundaries.map(|b| b.month),
        Window::Year => boundaries.map(|b| b.year),
        Window::Storm => storm_start,
        Window::Rolling(d) | Window::PerHour(d) => {
            until.checked_sub_signed(chrono::Duration::seconds(d.as_secs() as i64))
        }
    }
}

/// Add up one window's pieces.
///
/// **`None` is absent, not zero** (**D-26**). A window whose every piece is `None` has no
/// data and answers `None`; a window with one piece carrying 0.3 inches and two carrying
/// nothing answers 0.3 inches, because an hour with no rain contributes nothing to a total
/// rather than making the total unknown. `COALESCE(SUM(...), 0)` — which is what every one
/// of `rainfall_calc.go`'s queries writes — collapses the two cases into "0.0", and that is
/// how an offline gauge comes to report a dry year.
fn combine(pieces: &[Option<f64>]) -> Option<f64> {
    pieces.iter().flatten().copied().fold(None, |acc, v| {
        Some(match acc {
            Some(a) => a + v,
            None => v,
        })
    })
}

/// Every metric the registry gives a `total`, which is the set the long windows are
/// maintained for.
///
/// Derived rather than listed: `rain_incremental` and `solar_joules` today, and a third
/// total added to `metrics/registry.toml` is maintained with no edit here.
fn total_metrics() -> &'static [MetricKey] {
    use std::sync::OnceLock;
    static METRICS: OnceLock<Vec<MetricKey>> = OnceLock::new();
    METRICS.get_or_init(|| {
        let mut out: Vec<MetricKey> = Vec::new();
        for level in LEVELS.iter().filter(|l| l.name == "1m") {
            for c in level
                .columns
                .iter()
                .filter(|c| c.strategy == Strategy::Total)
            {
                if !out.contains(&c.metric) {
                    out.push(c.metric);
                }
            }
        }
        out
    })
}

/// `rain:rollover:<station>`'s body.
struct RainRollover {
    state: Arc<RollupState>,
    station: StationKey,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for RainRollover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RainRollover")
            .field("station", &self.station)
            .finish_non_exhaustive()
    }
}

impl Tick for RainRollover {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            // The supervisor fires this at the local midnight it computed, and the body
            // asks the clock rather than being told which occurrence it is. That is not
            // laziness: `Misfire::RunLatest` makes up this morning's occurrence at two in
            // the afternoon, and the local day to accumulate from is the one it is *now*,
            // not the one the occurrence key names.
            let now = self.clock.now();
            match self.state.roll_over(&self.station, now) {
                Rollover::Rolled { local_date } => {
                    m::rain_rolled_over();
                    info!(
                        station = %self.station,
                        %local_date,
                        "the station's local day has rolled over; its rainfall day, month \
                         and year totals accumulate from its own midnight"
                    );
                    Ok(())
                }
                Rollover::Already { local_date } => {
                    // Fall-back's second pass through local midnight, or an occurrence made
                    // up twice. W2a Decision 14: every `Calendar` user in this system is
                    // idempotent, and this is what that means here.
                    info!(
                        station = %self.station,
                        %local_date,
                        "this local date has already rolled over, so nothing changed"
                    );
                    Ok(())
                }
                Rollover::NoZone => {
                    // Unreachable through the supervisor: `plan()` refuses a `Calendar`
                    // component whose `TzSource` resolves no zone. Returned rather than
                    // ignored, because the alternative is a component that reports success
                    // having done nothing.
                    Err(ComponentError::tick(
                        "roll over the station's local day",
                        mw_component::WindowError::NoZone {
                            station: self.station.clone(),
                            window: Window::Day,
                        },
                    ))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
