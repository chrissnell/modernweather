//! [`RollupState`] — the maintained values, and the [`WindowedView`] over them.
//!
//! **R2 in one object.** A derived value that is a function of a window of readings is
//! never computed per request; it is maintained by a component and read from memory (W3
//! §2.5). This is the memory. Two components write it and they write disjoint halves:
//!
//! | Writer | Writes | When |
//! |---|---|---|
//! | `window:rollup` | the ring and the maintained totals | `Every { 60 s }` |
//! | `rain:rollover:<station>` | the local day / month / year boundaries | `Calendar { daily 00:00, TzSource::Station }` |
//!
//! That split is W3 §2.5's consequence 2, which describes the pair as "a
//! `Calendar { daily 00:00, TzSource::Station }` component for the boundary plus
//! `Every { 60 s }` for the accumulation since it, **so the boundary is correct by
//! construction**". It calls the pair `rain:totals`; here the accumulating half is
//! `window:rollup`, because the capability inventory (W2a §2.1) has no capability for the
//! rainfall windows and `Window::DAY` is one of the windows `WindowedView` resolves — so
//! the accumulation has to be in `WindowedView`'s provider or it is not readable at all.
//! Recorded as **A-M3-4-2** in `docs/reconciliation.md`.
//!
//! # Why the boundary is an event rather than a calculation
//!
//! The accumulating half could compute the local midnight itself on every tick, and that
//! would be correct. What it would not be is **observable**: "exactly one rollover per
//! local day" is a claim about a thing that happens, and a function nobody counts has no
//! occurrences to count. Keeping it a `Calendar` occurrence buys three things the
//! calculation does not: the supervisor's own `OccurrenceKey` deduplication, which is what
//! makes fall-back's second pass a no-op (W2a Decision 13); `Misfire::RunLatest`, which is
//! what makes a restart at 14:00 local still know when the day began; and a `last run` /
//! `next run` an operator can read.
//!
//! # Why `ArcSwap` and not a `Mutex`
//!
//! A reader must never wait on the writer (**D-28**, §2a.2). The readers here are
//! `snapshot:latest` folding a window value into the `/latest` payload and the three
//! rate-limited uploaders; the writer is a tick that holds a database connection. One
//! `ArcSwap` per station, swapped once a tick, and the ring's buckets are behind `Arc`s so
//! that building the next state is sixty pointer bumps rather than sixty copies of a
//! 624-value row.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, NaiveDate, Utc};
use mw_component::{Declaration, Resolved, WindowError, WindowedView, Zone};
use mw_types::{Agg, MetricKey, StationKey, Window};

use super::calendar;
use super::ring::{ColumnPlan, Ring, RING_SPAN};

/// The windows this item maintains outside the ring, for every metric that declares a
/// total.
///
/// **R2's list, less the snow totals.** `window:rollup` keeps wind gust and the rain rate
/// in the ring (ten minutes of buckets) and these seven as sums; the snow tiers are
/// `snow:events`, `snow:smoothing`, `snow:storm` and `snow:season` over
/// `Capability::SnowCache` (W3 §2a.7, `docs/component-model.md` §4.3), which `db:postgres`
/// does not publish yet and `implementation-plan.md` M6-3 builds.
///
/// Uniform over metrics rather than special-cased to rainfall: the set is every metric
/// whose registry entry declares `total`, which is `rain_incremental` and `solar_joules`
/// today. A per-metric list here would be a second place to edit when a third total
/// arrives, and the station's local boundaries are a property of the station rather than
/// of the metric.
pub const MAINTAINED: [Window; 7] = [
    Window::Day,
    Window::Month,
    Window::Year,
    Window::Storm,
    Window::Rolling(Duration::from_secs(24 * 3600)),
    Window::Rolling(Duration::from_secs(48 * 3600)),
    Window::Rolling(Duration::from_secs(72 * 3600)),
];

/// The first instant of a station's local day, month and year.
///
/// Published by `rain:rollover:<station>` and read by `window:rollup`. `local_date` is which
/// day they belong to, which is how a second occurrence for the same local date is
/// recognised as the one that has already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundaries {
    /// The local date these boundaries are for.
    pub local_date: NaiveDate,
    /// The first instant of that local day.
    pub day: DateTime<Utc>,
    /// The first instant of the local month it falls in.
    pub month: DateTime<Utc>,
    /// The first instant of the local year it falls in.
    pub year: DateTime<Utc>,
}

impl Boundaries {
    /// The boundaries of the local day `at` falls in, in `zone`.
    ///
    /// `None` only if the zone cannot place a local midnight at all, which
    /// [`Zone::at`]'s own documentation says is a time of day that is not one — so in
    /// practice never, and reported rather than unwrapped.
    pub fn of(zone: Zone, at: DateTime<Utc>) -> Option<Self> {
        Some(Self {
            local_date: zone.date_of(at),
            day: calendar::day_start(zone, at)?,
            month: calendar::month_start(zone, at)?,
            year: calendar::year_start(zone, at)?,
        })
    }
}

/// What one rollover occurrence did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rollover {
    /// The local date changed, so the day's accumulation starts again. The only outcome
    /// that increments [`RollupState::rollovers`].
    Rolled {
        /// The local date now being accumulated.
        local_date: NaiveDate,
    },
    /// The same local date as last time — fall-back's second pass through local midnight,
    /// or a misfire made up twice. A no-op, which is what **W2a Decision 14**'s "every
    /// `Calendar` user in this system is idempotent" means for this one.
    Already {
        /// The local date that was already current.
        local_date: NaiveDate,
    },
    /// The station has no usable zone, so there is no local midnight to roll over at.
    ///
    /// Not reachable through the supervisor — `plan()` refuses a `Calendar` component whose
    /// `TzSource` resolves no zone (`PlanError::MissingTimeZone`), so a station without one
    /// has no rollover component at all. Represented because the state is shared and a
    /// direct caller can ask.
    NoZone,
}

/// A station's maintained windows, as one immutable snapshot.
#[derive(Debug, Clone, Default)]
pub struct Windows {
    /// When the tick that produced this ran, which is what [`Resolved::as_of`] carries.
    pub as_of: Option<DateTime<Utc>>,
    /// The newest complete minute the windows are measured back from, exclusive.
    pub until: Option<DateTime<Utc>>,
    /// Recent 1-minute buckets, for every window up to an hour.
    pub ring: Ring,
    /// The maintained totals, keyed by the metric and the window they answer.
    pub long: Vec<((MetricKey, Window), Option<f64>)>,
    /// When the current rain event began, or `None` while there is no event.
    pub storm_start: Option<DateTime<Utc>>,
}

impl Windows {
    fn long(&self, metric: MetricKey, window: Window) -> Option<Option<f64>> {
        self.long
            .iter()
            .find(|((m, w), _)| *m == metric && *w == window)
            .map(|(_, v)| *v)
    }
}

/// One station's slot: what is configured about it, and what is maintained for it.
#[derive(Debug)]
struct Station {
    zone: Option<Zone>,
    windows: ArcSwap<Windows>,
    boundaries: ArcSwap<Option<Boundaries>>,
    rollovers: AtomicU64,
}

/// The maintained window values for every configured station.
///
/// Held by `main` as a root resource and handed to both components through the registrar,
/// the way `Ingress` is (`mw_station::config`'s module documentation has why that is not a
/// shortcut): the two components that write it are built independently and the capability
/// bus holds one handle *type* per provider, so the shared state cannot travel on it.
#[derive(Debug)]
pub struct RollupState {
    stations: BTreeMap<StationKey, Station>,
    plan: ColumnPlan,
}

impl RollupState {
    /// The state for a fixed set of stations and their resolved zones.
    ///
    /// The set is fixed at construction because it comes from `config.stations`, and a
    /// station added while the daemon runs is not resolvable until it restarts — which is
    /// true of every configuration change until `ConfigAdmin` exists (M7-1) and is recorded
    /// at `mw_store::registry` for the same reason.
    pub fn new(stations: impl IntoIterator<Item = (StationKey, Option<Zone>)>) -> Self {
        Self {
            stations: stations
                .into_iter()
                .map(|(key, zone)| {
                    (
                        key,
                        Station {
                            zone,
                            windows: ArcSwap::from_pointee(Windows::default()),
                            boundaries: ArcSwap::from_pointee(None),
                            rollovers: AtomicU64::new(0),
                        },
                    )
                })
                .collect(),
            plan: ColumnPlan::new(),
        }
    }

    /// Every station and its zone, in key order — what a tick iterates.
    pub fn stations(&self) -> impl Iterator<Item = (&StationKey, Option<Zone>)> {
        self.stations.iter().map(|(k, s)| (k, s.zone))
    }

    /// How many stations are maintained.
    pub fn len(&self) -> usize {
        self.stations.len()
    }

    /// Whether this node has no stations, which is a node that only serves.
    pub fn is_empty(&self) -> bool {
        self.stations.is_empty()
    }

    /// The registry-derived column plan, so a tick and a resolve share one.
    pub fn plan(&self) -> &ColumnPlan {
        &self.plan
    }

    /// One station's zone, if it has a usable one.
    pub fn zone(&self, station: &StationKey) -> Option<Zone> {
        self.stations.get(station).and_then(|s| s.zone)
    }

    /// One station's current windows.
    pub fn windows(&self, station: &StationKey) -> Option<Arc<Windows>> {
        self.stations.get(station).map(|s| s.windows.load_full())
    }

    /// One station's current boundaries, if a rollover has published any.
    pub fn boundaries(&self, station: &StationKey) -> Option<Boundaries> {
        self.stations
            .get(station)
            .and_then(|s| *s.boundaries.load_full())
    }

    /// Replace one station's windows. `window:rollup`'s tick, and nothing else.
    pub fn publish_windows(&self, station: &StationKey, windows: Windows) {
        if let Some(slot) = self.stations.get(station) {
            slot.windows.store(Arc::new(windows));
        }
    }

    /// Move one station's calendar boundaries to the local day `at` falls in.
    ///
    /// `rain:rollover:<station>`'s occurrence, and nothing else. Idempotent by local date:
    /// a second call for the same local date is [`Rollover::Already`], which is what makes
    /// fall-back's second pass through a local midnight a no-op even if the supervisor's
    /// own `OccurrenceKey` deduplication were not there.
    pub fn roll_over(&self, station: &StationKey, at: DateTime<Utc>) -> Rollover {
        let Some(slot) = self.stations.get(station) else {
            return Rollover::NoZone;
        };
        let Some(zone) = slot.zone else {
            return Rollover::NoZone;
        };
        let Some(next) = Boundaries::of(zone, at) else {
            return Rollover::NoZone;
        };
        let current = slot.boundaries.load_full();
        if current.is_some_and(|b| b.local_date == next.local_date) {
            return Rollover::Already {
                local_date: next.local_date,
            };
        }
        slot.boundaries.store(Arc::new(Some(next)));
        slot.rollovers.fetch_add(1, Ordering::Relaxed);
        Rollover::Rolled {
            local_date: next.local_date,
        }
    }

    /// How many times one station's day has rolled over since the process started.
    ///
    /// The observable behind M3-4's done-criterion — "a DST transition in a station's zone
    /// produces **exactly one** daily rollover" — and `mw_rain_rollovers_total`'s value.
    pub fn rollovers(&self, station: &StationKey) -> u64 {
        self.stations
            .get(station)
            .map(|s| s.rollovers.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// The first instant of the window, for the windows that have one.
    ///
    /// `Ok(None)` means there is nothing to accumulate from *yet* and not that the window
    /// is wrong: `Window::Storm` has no start while there is no storm, and the calendar
    /// windows have none until the rollover's first occurrence — which the supervisor makes
    /// up on startup, because `Misfire::RunLatest`'s grace is 24 hours and a daily
    /// occurrence is never more than that old.
    pub fn window_start(
        &self,
        station: &StationKey,
        window: Window,
    ) -> Result<Option<DateTime<Utc>>, WindowError> {
        let slot = self
            .stations
            .get(station)
            .ok_or_else(|| WindowError::UnknownStation {
                station: station.clone(),
            })?;
        match window {
            Window::Day | Window::Month | Window::Year => {
                if slot.zone.is_none() {
                    return Err(WindowError::NoZone {
                        station: station.clone(),
                        window,
                    });
                }
                Ok(slot.boundaries.load_full().map(|b| match window {
                    Window::Month => b.month,
                    Window::Year => b.year,
                    _ => b.day,
                }))
            }
            Window::Storm => Ok(slot.windows.load().storm_start),
            Window::Rolling(d) | Window::PerHour(d) => {
                let until = slot.windows.load().until;
                Ok(until.and_then(|u| {
                    u.checked_sub_signed(chrono::Duration::seconds(d.as_secs() as i64))
                }))
            }
        }
    }
}

impl WindowedView for RollupState {
    fn resolve(&self, station: &StationKey, want: &[Declaration]) -> Result<Resolved, WindowError> {
        let slot = self
            .stations
            .get(station)
            .ok_or_else(|| WindowError::UnknownStation {
                station: station.clone(),
            })?;
        let windows = slot.windows.load_full();
        let (Some(as_of), Some(until)) = (windows.as_of, windows.until) else {
            // No tick has completed, so there is nothing to resolve from. The same answer
            // as an unknown station on purpose: a consumer that cannot tell "this station
            // does not exist" from "nothing has been read for it yet" would report a flat
            // zero for both, and `UnknownStation`'s message names the second case.
            return Err(WindowError::UnknownStation {
                station: station.clone(),
            });
        };
        let mut values = Vec::with_capacity(want.len());
        for (metric, window, agg) in want.iter().copied() {
            let value = self.one(station, slot, &windows, until, metric, window, agg)?;
            values.push(((metric, window, agg), value));
        }
        Ok(Resolved::new(as_of, values))
    }
}

impl RollupState {
    /// One declaration, against one station's maintained state.
    #[allow(clippy::too_many_arguments)]
    fn one(
        &self,
        station: &StationKey,
        slot: &Station,
        windows: &Windows,
        until: DateTime<Utc>,
        metric: MetricKey,
        window: Window,
        agg: Agg,
    ) -> Result<Option<f64>, WindowError> {
        match window {
            // Short enough for the ring: every strategy, resolved from bucket state.
            Window::Rolling(d) if d <= RING_SPAN => {
                windows.ring.resolve(&self.plan, metric, agg, d, until)
            }
            // A rate is a total extrapolated, so only a total can be one.
            Window::PerHour(d) => {
                if agg != Agg::Sum {
                    return Err(WindowError::NotARate { window, agg });
                }
                let total = if d <= RING_SPAN {
                    windows
                        .ring
                        .resolve(&self.plan, metric, Agg::Sum, d, until)?
                } else {
                    Self::maintained(windows, metric, Window::Rolling(d), agg, window)?
                };
                // in-per-10-minutes to in-per-hour is the frozen contract's `x 6`
                // (`rainfall_calc.go:160`), written as the ratio it is so that a different
                // window is not a different constant.
                let per_hour = 3600.0 / (d.as_secs().max(1) as f64);
                Ok(total.map(|t| t * per_hour))
            }
            // Maintained: the calendar windows, the storm and 24/48/72 hours.
            Window::Day | Window::Month | Window::Year | Window::Storm | Window::Rolling(_) => {
                if window.needs_zone() && slot.zone.is_none() {
                    return Err(WindowError::NoZone {
                        station: station.clone(),
                        window,
                    });
                }
                Self::maintained(windows, metric, window, agg, window)
            }
        }
    }

    /// A maintained total, or the reason there is none for that declaration.
    fn maintained(
        windows: &Windows,
        metric: MetricKey,
        window: Window,
        agg: Agg,
        asked: Window,
    ) -> Result<Option<f64>, WindowError> {
        if agg != Agg::Sum {
            // The maintained set is totals, because a total is the only aggregate whose
            // long window is a sum of sub-window sums. A daily *mean* temperature is a
            // `/span` query, not a maintained value — and answering `None` would read as
            // "the station does not report this".
            return Err(WindowError::TooLong {
                metric,
                window: asked,
                held_secs: RING_SPAN.as_secs(),
            });
        }
        windows.long(metric, window).ok_or(WindowError::TooLong {
            metric,
            window: asked,
            held_secs: RING_SPAN.as_secs(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn zone(name: &str) -> Zone {
        Zone::new(name).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn state(name: &str) -> (RollupState, StationKey) {
        let key = StationKey::new("backyard");
        (RollupState::new([(key.clone(), Some(zone(name)))]), key)
    }

    /// **M3-4's second done-criterion, in the state that counts it.** `America/Havana`'s
    /// local midnight does not exist on the March date and happens twice on the November
    /// one. Firing the rollover at every instant across both transitions produces **one**
    /// rollover per local date — not two on the ambiguous day and not zero on the
    /// nonexistent one.
    #[test]
    fn a_dst_transition_in_the_stations_zone_produces_exactly_one_daily_rollover() {
        for (name, span) in [
            // The ambiguous midnight: 00:00 happens at 04:00Z and again at 05:00Z.
            (
                "America/Havana",
                ("2026-10-30T00:00:00Z", "2026-11-04T00:00:00Z"),
            ),
            // The nonexistent midnight: the clock goes 00:00 -> 01:00.
            (
                "America/Havana",
                ("2026-03-06T00:00:00Z", "2026-03-11T00:00:00Z"),
            ),
            // Chile, where the spring-forward transition is also at midnight.
            (
                "America/Santiago",
                ("2026-09-04T00:00:00Z", "2026-09-09T00:00:00Z"),
            ),
        ] {
            let (state, key) = state(name);
            let z = zone(name);
            let (from, to) = (at(span.0), at(span.1));
            // Every ten minutes across the span: far more often than the Calendar fires,
            // which is the point -- a rollover that happened on any of these instants
            // must still be one per local date.
            let mut dates = std::collections::BTreeSet::new();
            let mut now = from;
            while now < to {
                match state.roll_over(&key, now) {
                    Rollover::Rolled { local_date } => {
                        assert!(
                            dates.insert(local_date),
                            "{name}: {local_date} rolled over twice"
                        );
                    }
                    Rollover::Already { local_date } => {
                        assert!(dates.contains(&local_date), "{name}: {local_date}");
                    }
                    Rollover::NoZone => panic!("{name} is a zone"),
                }
                now += chrono::Duration::minutes(10);
            }
            // One per local date in the span, and the count agrees with the counter.
            let local_dates: std::collections::BTreeSet<NaiveDate> = {
                let mut s = std::collections::BTreeSet::new();
                let mut now = from;
                while now < to {
                    s.insert(z.date_of(now));
                    now += chrono::Duration::minutes(10);
                }
                s
            };
            assert_eq!(dates, local_dates, "{name}");
            assert_eq!(
                state.rollovers(&key),
                dates.len() as u64,
                "{name}: one rollover per local date"
            );
        }
    }

    /// The fall-back case on its own, at the instants that matter: 00:00 local happens at
    /// 04:00Z and again at 05:00Z, and the second pass is [`Rollover::Already`] with the
    /// boundary unmoved.
    #[test]
    fn the_second_pass_through_an_ambiguous_midnight_moves_nothing() {
        let (state, key) = state("America/Havana");
        assert_eq!(
            state.roll_over(&key, at("2026-11-01T04:00:00Z")),
            Rollover::Rolled {
                local_date: NaiveDate::from_ymd_opt(2026, 11, 1).expect("a date")
            }
        );
        let after_first = state.boundaries(&key).expect("published");
        assert_eq!(after_first.day, at("2026-11-01T04:00:00Z"));

        assert_eq!(
            state.roll_over(&key, at("2026-11-01T05:00:00Z")),
            Rollover::Already {
                local_date: NaiveDate::from_ymd_opt(2026, 11, 1).expect("a date")
            },
            "the second pass through 00:00 local is the same local date"
        );
        assert_eq!(state.boundaries(&key).expect("published"), after_first);
        assert_eq!(state.rollovers(&key), 1);
    }

    /// The nonexistent case: the day begins at 01:00 local, which is the first instant
    /// that exists — so the day's accumulation starts there rather than being skipped.
    #[test]
    fn a_day_whose_midnight_does_not_exist_still_rolls_over_once() {
        let (state, key) = state("America/Havana");
        assert_eq!(
            state.roll_over(&key, at("2026-03-08T12:00:00Z")),
            Rollover::Rolled {
                local_date: NaiveDate::from_ymd_opt(2026, 3, 8).expect("a date")
            }
        );
        let b = state.boundaries(&key).expect("published");
        assert_eq!(b.day, at("2026-03-08T05:00:00Z"));
        assert_eq!(b.month, at("2026-03-01T05:00:00Z"));
        assert_eq!(b.year, at("2026-01-01T05:00:00Z"));
        assert_eq!(state.rollovers(&key), 1);
    }

    /// Before the first tick there is nothing to resolve, and the answer names the station
    /// rather than reporting an empty window — which a consumer would render as a zero.
    #[test]
    fn nothing_resolves_until_a_tick_has_completed() {
        let (state, key) = state("America/Denver");
        let e = state
            .resolve(
                &key,
                &[(
                    MetricKey::WindSpeed,
                    Window::of(Window::TEN_MINUTES),
                    Agg::Max,
                )],
            )
            .expect_err("no tick has run");
        assert!(matches!(e, WindowError::UnknownStation { .. }));

        let unknown = StationKey::new("not-configured");
        let e = state
            .resolve(&unknown, &[])
            .expect_err("not a station here");
        assert!(matches!(e, WindowError::UnknownStation { .. }));
    }

    /// The maintained set is totals. A daily **mean** is `/span`'s query, and asking this
    /// view for one is a named refusal rather than an absent value.
    #[test]
    fn a_calendar_window_of_something_that_is_not_a_total_is_refused_by_name() {
        let (state, key) = state("America/Denver");
        state.publish_windows(
            &key,
            Windows {
                as_of: Some(at("2026-11-06T15:00:05Z")),
                until: Some(at("2026-11-06T15:00:00Z")),
                long: vec![((MetricKey::RainIncremental, Window::Day), Some(0.42))],
                ..Windows::default()
            },
        );
        let resolved = state
            .resolve(&key, &[(MetricKey::RainIncremental, Window::Day, Agg::Sum)])
            .expect("the day's rain is maintained");
        assert_eq!(
            resolved.value((MetricKey::RainIncremental, Window::Day, Agg::Sum)),
            Ok(Some(0.42))
        );
        assert_eq!(resolved.as_of(), at("2026-11-06T15:00:05Z"));

        let e = state
            .resolve(&key, &[(MetricKey::OutTemp, Window::Day, Agg::Mean)])
            .expect_err("a daily mean temperature is not maintained");
        assert!(matches!(e, WindowError::TooLong { .. }));
    }

    /// `Window::PerHour` is the rain rate, and it is a total extrapolated — so declaring
    /// it with any other aggregate is refused. The frozen contract's figure is
    /// `sum(rainincremental over 10 minutes) x 6`, and the multiplier here is the ratio
    /// rather than a literal so that a different window is not a different constant.
    #[test]
    fn a_rate_is_a_total_extrapolated_to_an_hour_and_nothing_else_is() {
        let (state, key) = state("America/Denver");
        // Half an inch over the maintained 24-hour window, extrapolated: the ring is
        // empty, so this exercises the maintained arm of the rate.
        state.publish_windows(
            &key,
            Windows {
                as_of: Some(at("2026-11-06T15:00:05Z")),
                until: Some(at("2026-11-06T15:00:00Z")),
                long: vec![(
                    (
                        MetricKey::RainIncremental,
                        Window::Rolling(Duration::from_secs(24 * 3600)),
                    ),
                    Some(2.4),
                )],
                ..Windows::default()
            },
        );
        let day_rate = Window::PerHour(Duration::from_secs(24 * 3600));
        let resolved = state
            .resolve(&key, &[(MetricKey::RainIncremental, day_rate, Agg::Sum)])
            .expect("the 24-hour total is maintained");
        let rate = resolved
            .value((MetricKey::RainIncremental, day_rate, Agg::Sum))
            .expect("declared")
            .expect("maintained");
        assert!(
            (rate - 0.1).abs() < 1e-12,
            "2.4 inches in 24 hours is 0.1 inches an hour, got {rate}"
        );

        let e = state
            .resolve(
                &key,
                &[(
                    MetricKey::OutTemp,
                    Window::PerHour(Window::TEN_MINUTES),
                    Agg::Mean,
                )],
            )
            .expect_err("a mean cannot be extrapolated");
        assert!(matches!(e, WindowError::NotARate { .. }));
    }

    /// The maintained set is exactly R2's list less the snow tiers, and the snow tiers are
    /// absent for a reason that is checkable: `db:postgres` does not publish
    /// `Capability::SnowCache` yet, which `mw_store::pg`'s own spec test asserts.
    #[test]
    fn the_maintained_windows_are_r2s_list_less_the_snow_tiers() {
        assert_eq!(
            MAINTAINED.to_vec(),
            vec![
                Window::Day,
                Window::Month,
                Window::Year,
                Window::Storm,
                Window::Rolling(Duration::from_secs(24 * 3600)),
                Window::Rolling(Duration::from_secs(48 * 3600)),
                Window::Rolling(Duration::from_secs(72 * 3600)),
            ]
        );
        // Every maintained window is longer than the ring, which is why it is maintained
        // rather than resolved from bucket state.
        for w in MAINTAINED {
            match w.length() {
                Some(d) => assert!(d > RING_SPAN, "{w} fits in the ring"),
                None => assert!(w == Window::Storm || w.needs_zone(), "{w}"),
            }
        }
    }

    /// A station with no zone has no calendar boundary, and the message names the column to
    /// set. Unreachable through the supervisor — `plan()` refuses the rollover component —
    /// and reachable for a direct caller, which is what this asserts.
    #[test]
    fn a_station_with_no_zone_has_no_calendar_window() {
        let key = StationKey::new("zoneless");
        let state = RollupState::new([(key.clone(), None)]);
        assert_eq!(
            state.roll_over(&key, at("2026-11-06T15:00:00Z")),
            Rollover::NoZone
        );
        assert_eq!(state.rollovers(&key), 0);
        let e = state
            .window_start(&key, Window::Day)
            .expect_err("no zone, no local midnight");
        assert!(matches!(e, WindowError::NoZone { .. }));
        assert!(e.to_string().contains("config.stations.timezone"), "{e}");
        // A rolling window needs no zone and is still unavailable until a tick has run.
        assert_eq!(
            state.window_start(&key, Window::of(Window::TEN_MINUTES)),
            Ok(None)
        );
    }
}
