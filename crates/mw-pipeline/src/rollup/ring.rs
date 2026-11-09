//! The bounded ring of recent 1-minute buckets, and the arithmetic that rolls them up.
//!
//! **This is the only place in the program that knows the roll-up arithmetic**
//! (`docs/component-model.md` §4b.3). Wind gust stops being "a SQL function called from the
//! radio component with a comment saying *same as website*": `calculate_wind_gust` is one
//! of three call sites of one PL/pgSQL function, and the website computes the same thing a
//! fourth way.
//!
//! # The second payoff of composable state
//!
//! Because the 1-minute level stores `_sum` and `_count` rather than a finished average,
//! rolling ten buckets into a correct ten-minute mean is arithmetic: `Σsum / Σcount`.
//! **D-12** measured what happens otherwise — `avg(avg(x))` disagreed with the direct
//! average in 658 of 868 real bucket pairs and in all 56 uneven ones. There is no column
//! holding a finished average at any level, so the wrong answer is not expressible from
//! here; what this module has to get right is that it reads **both** columns.
//!
//! # What the ring holds, and why an hour
//!
//! Sixty buckets per station. W2a §4b.3's assumption 16 is that `window:rollup` never needs
//! a 1-minute bucket older than an hour, which is what makes the level's 7-day retention
//! invisible to it; the windows the outputs declare are two minutes, five minutes and ten
//! minutes (`windspdmph_avg2m`, CWOP's averaged direction, `windgustmph_10m`). The calendar
//! and multi-day rain windows are **not** in the ring — 72 hours of buckets per station is
//! 4,320 rows a tick — and are maintained as sums instead (`super::rain`).
//!
//! # Every column is found through the registry, never by name
//!
//! A [`ColumnPlan`] maps `(MetricKey, Agg)` onto the column *positions* the 1-minute level
//! declares, built once. `Agg::Mean` is two positions and `Agg::CircularMean` is three,
//! which is the thing a `(metric, agg)` lookup has to get right and the thing a hand-written
//! table would get wrong first.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{Bucket, WindowError};
use mw_types::{Agg, MetricKey, Strategy, LEVELS};

/// How many 1-minute buckets a station's ring holds.
///
/// Sixty — W2a §4b.3's hour. See the module documentation; it is a bound on what the
/// *short* windows can be, and `WindowError::TooLong` is what a longer one gets.
pub const RING: usize = 60;

/// The longest rolling window the ring can answer.
pub const RING_SPAN: Duration = Duration::from_secs(RING as u64 * 60);

/// Which column positions of the 1-minute level a `(metric, agg)` resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slots {
    /// The least value in the bucket.
    Min(usize),
    /// The greatest value in the bucket.
    Max(usize),
    /// The bucket's total.
    Total(usize),
    /// `_sum` and `_count`, finalised as `Σsum / Σcount` — never `avg(avg(x))` (**D-11**).
    SumCount { sum: usize, count: usize },
    /// `_sin_sum`, `_cos_sum` and `_dir_count`, finalised with an arctangent (**D-13**).
    Circular {
        sin: usize,
        cos: usize,
        count: usize,
    },
}

/// Every `(metric, agg)` the 1-minute level declares, and where its state is.
///
/// Built once from `mw_types::LEVELS`, so a metric added to `metrics/registry.toml` is
/// resolvable with no edit here — the property the Go implementation's 591 lines of
/// hand-written mapping did not have.
#[derive(Debug)]
pub struct ColumnPlan {
    slots: HashMap<(MetricKey, Agg), Slots>,
    /// Metrics whose mean is stored as one `_stats` column rather than a `_sum`/`_count`
    /// pair, which this module cannot finalise.
    ///
    /// `metrics/registry.toml`'s `mean_representation` is `"sum_count"` today and
    /// `"stats_agg"` is the other value the generator emits. A `_stats` column is a
    /// `timescaledb_toolkit` type, not a number, so it would not survive the reader's cast
    /// to `float8` either. Recorded rather than ignored: switching that default would
    /// otherwise turn every mean into a silent `None`, and this makes it a named refusal.
    stats_only: Vec<MetricKey>,
}

impl ColumnPlan {
    /// Build the plan from the registry's 1-minute level.
    pub fn new() -> Self {
        let columns = LEVELS
            .iter()
            .find(|l| l.name == "1m")
            .map(|l| l.columns)
            .expect("metrics/registry.toml declares the 1-minute level");
        let mut slots: HashMap<(MetricKey, Agg), Slots> = HashMap::new();
        let mut stats_only = Vec::new();
        for (i, c) in columns.iter().enumerate() {
            match c.strategy {
                Strategy::Min => {
                    slots.insert((c.metric, Agg::Min), Slots::Min(i));
                }
                Strategy::Max => {
                    slots.insert((c.metric, Agg::Max), Slots::Max(i));
                }
                Strategy::Total => {
                    slots.insert((c.metric, Agg::Sum), Slots::Total(i));
                }
                // The two multi-column strategies are assembled by suffix, because the
                // suffix is the only thing that distinguishes two columns that share a
                // metric and a strategy. The generator's own names are the contract:
                // `<col>_sum`/`<col>_count` and `<col>_sin_sum`/`<col>_cos_sum`/
                // `<col>_dir_count`.
                Strategy::Mean => {
                    let entry = slots
                        .entry((c.metric, Agg::Mean))
                        .or_insert(Slots::SumCount {
                            sum: usize::MAX,
                            count: usize::MAX,
                        });
                    if let Slots::SumCount { sum, count } = entry {
                        if c.column.ends_with("_count") {
                            *count = i;
                        } else if c.column.ends_with("_sum") {
                            *sum = i;
                        } else {
                            // `_stats`: one column carrying a toolkit aggregate state.
                            stats_only.push(c.metric);
                        }
                    }
                }
                Strategy::CircularMean => {
                    let entry =
                        slots
                            .entry((c.metric, Agg::CircularMean))
                            .or_insert(Slots::Circular {
                                sin: usize::MAX,
                                cos: usize::MAX,
                                count: usize::MAX,
                            });
                    if let Slots::Circular { sin, cos, count } = entry {
                        if c.column.ends_with("_sin_sum") {
                            *sin = i;
                        } else if c.column.ends_with("_cos_sum") {
                            *cos = i;
                        } else if c.column.ends_with("_dir_count") {
                            *count = i;
                        }
                    }
                }
            }
        }
        // A half-assembled multi-column strategy is a generator change this module has not
        // caught up with, and keeping it would resolve to a panic on an out-of-range index.
        // Dropping it makes it a `NotDeclared` refusal naming the metric and the strategy.
        slots.retain(|(metric, _), s| match s {
            Slots::SumCount { sum, count } => {
                let complete = *sum != usize::MAX && *count != usize::MAX;
                if !complete {
                    stats_only.push(*metric);
                }
                complete
            }
            Slots::Circular { sin, cos, count } => {
                *sin != usize::MAX && *cos != usize::MAX && *count != usize::MAX
            }
            _ => true,
        });
        // `MetricKey` is `Hash + Eq` and deliberately not `Ord`, and this list is expected
        // to be empty -- so a quadratic dedup over nothing is the right shape.
        let mut unique: Vec<MetricKey> = Vec::new();
        for m in stats_only {
            if !unique.contains(&m) {
                unique.push(m);
            }
        }
        Self {
            slots,
            stats_only: unique,
        }
    }

    /// How many `(metric, agg)` pairs the registry declares at the 1-minute level.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the registry declares nothing at all, which would be a generator failure.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Metrics whose mean this module cannot finalise. Empty today; see [`ColumnPlan`].
    pub fn stats_only(&self) -> &[MetricKey] {
        &self.stats_only
    }

    fn get(&self, metric: MetricKey, agg: Agg) -> Option<Slots> {
        self.slots.get(&(metric, agg)).copied()
    }
}

impl Default for ColumnPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// A station's recent 1-minute buckets, oldest first.
///
/// `Arc<Bucket>` so that publishing a new state is sixty pointer bumps rather than sixty
/// copies of a 624-value row: the tick clones the deque, pushes the new bucket and swaps the
/// result in, which is what lets a reader never wait on the writer (**D-28**).
#[derive(Debug, Clone, Default)]
pub struct Ring(VecDeque<Arc<Bucket>>);

impl Ring {
    /// An empty ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest bucket's start instant, which is where an incremental refresh resumes.
    pub fn newest(&self) -> Option<DateTime<Utc>> {
        self.0.back().map(|b| b.bucket)
    }

    /// How many buckets are held.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing has been read yet.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Add buckets, oldest first, and drop anything that has fallen out of the hour.
    ///
    /// Out-of-order and duplicate arrivals are rejected rather than merged: the only
    /// producer is an incremental read that asks for everything after [`Ring::newest`], so
    /// a bucket at or before the newest one is a replay — and inserting it would put the
    /// deque out of order, which every window below reads it as being.
    pub fn extend(&mut self, buckets: impl IntoIterator<Item = Bucket>, horizon: DateTime<Utc>) {
        for bucket in buckets {
            if self.newest().is_some_and(|newest| bucket.bucket <= newest) {
                continue;
            }
            self.0.push_back(Arc::new(bucket));
        }
        while self.0.front().is_some_and(|b| b.bucket < horizon) {
            self.0.pop_front();
        }
        while self.0.len() > RING {
            self.0.pop_front();
        }
    }

    /// The buckets at or after `from`.
    fn since(&self, from: DateTime<Utc>) -> impl Iterator<Item = &Arc<Bucket>> {
        self.0.iter().filter(move |b| b.bucket >= from)
    }

    /// Resolve one declaration over the trailing `window`, ending at `until`.
    ///
    /// `until` is exclusive and is the caller's floored notion of now, so the window is
    /// `[until - window, until)` — whole buckets, which is what makes the answer equal to
    /// the same aggregate computed directly from the raw readings in those minutes.
    pub fn resolve(
        &self,
        plan: &ColumnPlan,
        metric: MetricKey,
        agg: Agg,
        window: Duration,
        until: DateTime<Utc>,
    ) -> Result<Option<f64>, WindowError> {
        if window > RING_SPAN {
            return Err(WindowError::TooLong {
                metric,
                window: mw_types::Window::Rolling(window),
                held_secs: RING_SPAN.as_secs(),
            });
        }
        let slots = plan.get(metric, agg).ok_or(WindowError::NotDeclared {
            metric,
            window: mw_types::Window::Rolling(window),
            agg,
        })?;
        let from = until
            .checked_sub_signed(chrono::Duration::seconds(window.as_secs() as i64))
            .ok_or(WindowError::TooLong {
                metric,
                window: mw_types::Window::Rolling(window),
                held_secs: RING_SPAN.as_secs(),
            })?;
        let buckets = || self.since(from).filter(|b| b.bucket < until);
        Ok(match slots {
            // `fold` rather than `min_by`/`max_by` so that the `None`s are skipped and an
            // all-`None` window answers `None` instead of a sentinel.
            Slots::Min(i) => buckets().filter_map(|b| value(b, i)).fold(None, |acc, v| {
                Some(match acc {
                    Some(a) => f64::min(a, v),
                    None => v,
                })
            }),
            Slots::Max(i) => buckets().filter_map(|b| value(b, i)).fold(None, |acc, v| {
                Some(match acc {
                    Some(a) => f64::max(a, v),
                    None => v,
                })
            }),
            Slots::Total(i) => buckets().filter_map(|b| value(b, i)).fold(None, |acc, v| {
                Some(match acc {
                    Some(a) => a + v,
                    None => v,
                })
            }),
            Slots::SumCount { sum, count } => {
                let mut total = 0.0f64;
                let mut n = 0.0f64;
                for b in buckets() {
                    // Both or neither: a bucket with a sum and no count cannot contribute
                    // to a mean, and counting it as zero observations would bias the
                    // answer rather than omit it. The level nulls counts at zero
                    // (**D-16**), so "no count" is how an empty bucket presents.
                    if let (Some(s), Some(c)) = (value(b, sum), value(b, count)) {
                        total += s;
                        n += c;
                    }
                }
                // `mw_mean(total, n)`, which the SQL side finalises the same way: NULL for
                // a zero count rather than a division.
                if n > 0.0 {
                    Some(total / n)
                } else {
                    None
                }
            }
            Slots::Circular { sin, cos, count } => {
                let mut sin_sum = 0.0f64;
                let mut cos_sum = 0.0f64;
                let mut n = 0.0f64;
                for b in buckets() {
                    if let (Some(s), Some(c), Some(k)) =
                        (value(b, sin), value(b, cos), value(b, count))
                    {
                        sin_sum += s;
                        cos_sum += c;
                        n += k;
                    }
                }
                if n > 0.0 {
                    Some(circular_mean(sin_sum, cos_sum, n))
                } else {
                    // **Deliberately `None` where `mw_circular_mean()` returns 0.** That
                    // function's zero is a *contract* requirement: `/span`'s `windd` is a
                    // non-optional Swift `Double`, so a missing key fails the decode of
                    // the whole response (W1-BUG-1, OQ-W1-2 option (c)). This is not
                    // `/span`. A `WindowedView` answers what the window holds, and the
                    // frozen contract's zeros are produced one layer later by the one
                    // zero-filling function `ci/layers.toml` permits, in `mw-api-legacy`
                    // (§1.7).
                    None
                }
            }
        })
    }
}

/// One column of one bucket, or `None` if the bucket is narrower than the plan expects.
///
/// The index comes from [`ColumnPlan`] and the width from the reader's `SELECT`, which are
/// both derived from `mw_types::LEVELS` — so a mismatch means a build whose reader and
/// whose plan disagree. `None` rather than a panic, because the consequence of being wrong
/// here is an absent window value and the consequence of a panic is a component in
/// `Backoff` with the station's readings unserved.
fn value(bucket: &Bucket, index: usize) -> Option<f64> {
    bucket.values.get(index).copied().flatten()
}

/// The vector mean of an angle, in degrees on `[0, 360)`.
///
/// The same shape as the `mw_circular_mean()` the metric registry's generator emits,
/// division by `n` included: it is redundant under `atan2` and it is kept so that the two
/// finalisers are the same expression. W0 verified the pattern composes through the level hierarchy to
/// within 3e-12 degrees against a direct computation from raw (**D-13**).
fn circular_mean(sin_sum: f64, cos_sum: f64, n: f64) -> f64 {
    let degrees = (sin_sum / n).atan2(cos_sum / n).to_degrees();
    (degrees + 360.0) % 360.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn plan() -> ColumnPlan {
        ColumnPlan::new()
    }

    /// Build a bucket with the named columns set, by looking their positions up the way
    /// the plan does — so a test cannot hard-code an index that drifts.
    fn bucket(start: &str, set: &[(&str, f64)]) -> Bucket {
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
            bucket: at(start),
            values,
        }
    }

    /// Every metric the registry declares has a complete set of column positions for every
    /// strategy it declares, and the multi-column ones have all their columns. A metric
    /// with a half-assembled mean would resolve to `None` forever, which is the failure
    /// this plan is built once to make impossible.
    #[test]
    fn the_plan_covers_every_declared_strategy_with_all_of_its_columns() {
        let plan = plan();
        assert!(!plan.is_empty());
        assert!(
            plan.stats_only().is_empty(),
            "metrics/registry.toml's mean_representation is sum_count; these metrics have \
             no sum/count pair: {:?}",
            plan.stats_only()
        );
        let columns = LEVELS
            .iter()
            .find(|l| l.name == "1m")
            .map(|l| l.columns)
            .expect("the 1-minute level");
        // Every (metric, strategy) the level declares is reachable through the plan...
        let mut declared = std::collections::BTreeSet::new();
        for c in columns {
            declared.insert((
                format!("{:?}", c.metric),
                Agg::of_strategy(c.strategy).as_str(),
            ));
            assert!(
                plan.get(c.metric, Agg::of_strategy(c.strategy)).is_some(),
                "{:?} declares {:?} and the plan has no slots for it",
                c.metric,
                c.strategy
            );
        }
        // ...and nothing else is.
        assert_eq!(plan.len(), declared.len());
        // The two multi-column cases, named, because they are the ones a hand-written
        // table gets wrong.
        assert!(matches!(
            plan.get(MetricKey::OutTemp, Agg::Mean),
            Some(Slots::SumCount { .. })
        ));
        assert!(matches!(
            plan.get(MetricKey::WindDir, Agg::CircularMean),
            Some(Slots::Circular { .. })
        ));
        assert!(matches!(
            plan.get(MetricKey::RainIncremental, Agg::Sum),
            Some(Slots::Total(_))
        ));
        // And a strategy the registry does not declare for a metric is absent, which is
        // what makes `WindowError::NotDeclared` reachable.
        assert_eq!(plan.get(MetricKey::WindSpeed, Agg::CircularMean), None);
        assert_eq!(plan.get(MetricKey::OutTemp, Agg::Sum), None);
    }

    /// **The done-criterion's arithmetic, without a database.** Ten buckets of uneven
    /// counts roll into the mean of all the underlying readings, which `avg(avg(x))` would
    /// not be: the per-bucket means here are 10, 20, 30 ... and their average is 55, while
    /// the true mean of the readings is 70.
    #[test]
    fn a_ten_minute_mean_is_the_mean_of_the_readings_and_not_the_mean_of_the_means() {
        let mut ring = Ring::new();
        // Bucket k has k+1 readings averaging 10*(k+1): sum = 10*(k+1)^2.
        let buckets: Vec<Bucket> = (0..10)
            .map(|k| {
                let n = f64::from(k + 1);
                bucket(
                    &format!("2026-11-06T14:{:02}:00Z", 50 + k),
                    &[("outtemp_sum", 10.0 * n * n), ("outtemp_count", n)],
                )
            })
            .collect();
        ring.extend(buckets, at("2026-11-06T14:00:00Z"));
        assert_eq!(ring.len(), 10);

        let got = ring
            .resolve(
                &plan(),
                MetricKey::OutTemp,
                Agg::Mean,
                Duration::from_secs(600),
                at("2026-11-06T15:00:00Z"),
            )
            .expect("out_temp declares a mean")
            .expect("ten buckets of readings");
        // Σsum / Σcount = 10*Σ(k+1)^2 / Σ(k+1) = 10*385/55 = 70.
        assert!((got - 70.0).abs() < 1e-12, "Σsum/Σcount is 70, got {got}");
        let mean_of_means: f64 = (1..=10).map(f64::from).map(|n| 10.0 * n).sum::<f64>() / 10.0;
        assert!(
            (mean_of_means - 55.0).abs() < 1e-12,
            "the wrong answer is 55, and it is 15 away from the right one"
        );
    }

    /// Wind gust is `max(windspeed)` over ten minutes, and the max of the per-bucket maxima
    /// **is** the max of the readings — which is why the gust needs no raw access even
    /// though `calculate_wind_gust` reads raw.
    #[test]
    fn a_ten_minute_gust_is_the_greatest_per_bucket_maximum() {
        let mut ring = Ring::new();
        ring.extend(
            [
                bucket("2026-11-06T14:50:00Z", &[("windspeed_max", 9.5)]),
                bucket("2026-11-06T14:51:00Z", &[("windspeed_max", 22.25)]),
                // A minute the anemometer reported nothing in: skipped, not zero.
                bucket("2026-11-06T14:52:00Z", &[]),
                bucket("2026-11-06T14:53:00Z", &[("windspeed_max", 14.0)]),
            ],
            at("2026-11-06T14:00:00Z"),
        );
        let gust = ring
            .resolve(
                &plan(),
                MetricKey::WindSpeed,
                Agg::Max,
                Duration::from_secs(600),
                at("2026-11-06T15:00:00Z"),
            )
            .expect("wind_speed declares a max");
        assert_eq!(gust, Some(22.25));
    }

    /// The window is `[until - window, until)`, so a bucket outside it is not in the
    /// answer however recently it was read. Two windows over the same ring are what makes
    /// that visible.
    #[test]
    fn the_window_is_the_trailing_range_and_not_everything_held() {
        let mut ring = Ring::new();
        ring.extend(
            (0..30).map(|k| {
                bucket(
                    &format!("2026-11-06T14:{:02}:00Z", 30 + k),
                    &[("windspeed_max", f64::from(k))],
                )
            }),
            at("2026-11-06T14:00:00Z"),
        );
        let until = at("2026-11-06T15:00:00Z");
        let p = plan();
        assert_eq!(
            ring.resolve(
                &p,
                MetricKey::WindSpeed,
                Agg::Max,
                Duration::from_secs(600),
                until
            ),
            Ok(Some(29.0)),
            "the last ten minutes are 14:50..14:59, whose greatest k is 29"
        );
        assert_eq!(
            ring.resolve(
                &p,
                MetricKey::WindSpeed,
                Agg::Max,
                Duration::from_secs(120),
                until
            ),
            Ok(Some(29.0))
        );
        // Five minutes back from 14:50 is 14:45..14:49, whose greatest k is 19.
        assert_eq!(
            ring.resolve(
                &p,
                MetricKey::WindSpeed,
                Agg::Max,
                Duration::from_secs(300),
                at("2026-11-06T14:50:00Z")
            ),
            Ok(Some(19.0))
        );
    }

    /// An empty window is `None` and not zero, for every strategy — **D-26** one level up
    /// from a column. A gust of 0 mph and "the anemometer has not reported" are different
    /// facts, and `calculate_wind_gust`'s `COALESCE(max_windspeed, 0.0)` makes them the
    /// same one.
    #[test]
    fn an_empty_window_is_absent_and_never_zero() {
        let ring = Ring::new();
        let p = plan();
        let until = at("2026-11-06T15:00:00Z");
        let ten = Duration::from_secs(600);
        for (metric, agg) in [
            (MetricKey::WindSpeed, Agg::Max),
            (MetricKey::OutTemp, Agg::Min),
            (MetricKey::OutTemp, Agg::Mean),
            (MetricKey::RainIncremental, Agg::Sum),
            (MetricKey::WindDir, Agg::CircularMean),
        ] {
            assert_eq!(
                ring.resolve(&p, metric, agg, ten, until),
                Ok(None),
                "{metric:?} {agg}"
            );
        }
    }

    /// A bucket carrying a sum and no count cannot contribute to a mean. The level nulls
    /// counts at zero (**D-16**), so that is how an empty bucket presents — and treating
    /// the missing count as zero observations would bias the mean rather than omit the
    /// bucket.
    #[test]
    fn a_mean_needs_both_columns_of_the_pair() {
        let mut ring = Ring::new();
        ring.extend(
            [
                bucket(
                    "2026-11-06T14:58:00Z",
                    &[("outtemp_sum", 100.0), ("outtemp_count", 2.0)],
                ),
                // A sum with no count: impossible from the generator, and the arithmetic
                // says what it does about it rather than dividing by a guess.
                bucket("2026-11-06T14:59:00Z", &[("outtemp_sum", 999.0)]),
            ],
            at("2026-11-06T14:00:00Z"),
        );
        assert_eq!(
            ring.resolve(
                &plan(),
                MetricKey::OutTemp,
                Agg::Mean,
                Duration::from_secs(600),
                at("2026-11-06T15:00:00Z")
            ),
            Ok(Some(50.0))
        );
    }

    /// The circular mean is the vector one, which is the whole point: the average of 350°
    /// and 10° is 0°, not 180°. And an empty window is `None` here too, **unlike**
    /// `mw_circular_mean()`, whose 0 is a frozen-contract requirement rather than a value.
    #[test]
    fn a_circular_mean_averages_vectors_and_not_numbers() {
        let mut ring = Ring::new();
        let ten_deg: f64 = 10.0_f64.to_radians();
        let three_fifty: f64 = 350.0_f64.to_radians();
        ring.extend(
            [
                bucket(
                    "2026-11-06T14:58:00Z",
                    &[
                        ("winddir_sin_sum", ten_deg.sin()),
                        ("winddir_cos_sum", ten_deg.cos()),
                        ("winddir_dir_count", 1.0),
                    ],
                ),
                bucket(
                    "2026-11-06T14:59:00Z",
                    &[
                        ("winddir_sin_sum", three_fifty.sin()),
                        ("winddir_cos_sum", three_fifty.cos()),
                        ("winddir_dir_count", 1.0),
                    ],
                ),
            ],
            at("2026-11-06T14:00:00Z"),
        );
        let got = ring
            .resolve(
                &plan(),
                MetricKey::WindDir,
                Agg::CircularMean,
                Duration::from_secs(600),
                at("2026-11-06T15:00:00Z"),
            )
            .expect("wind_dir declares a circular mean")
            .expect("two buckets of directions");
        assert!(
            !(1e-9..=360.0 - 1e-9).contains(&got),
            "expected ~0 degrees, got {got}"
        );
        // The arithmetic mean of the two numbers, which is the wrong answer by 180.
        assert!(f64::abs(180.0 - (350.0 + 10.0) / 2.0) < 1e-12);
        // And the range is [0, 360) rather than (-180, 180].
        assert_eq!(circular_mean(-1.0, -1.0, 1.0).round(), 225.0);
        assert_eq!(circular_mean(0.0, 1.0, 1.0), 0.0);
        assert_eq!(circular_mean(1.0, 0.0, 1.0).round(), 90.0);
    }

    /// The ring is bounded by both the hour and the count, and a replayed bucket does not
    /// get in twice. The incremental refresh asks for everything after the newest, so a
    /// duplicate is a replay rather than new data — and inserting it would put the deque
    /// out of the order every window reads it as being.
    #[test]
    fn the_ring_is_bounded_and_rejects_a_replay() {
        let mut ring = Ring::new();
        ring.extend(
            (0..90).map(|k| {
                let minute = at("2026-11-06T13:30:00Z") + chrono::Duration::minutes(k);
                Bucket {
                    bucket: minute,
                    values: vec![None; 1],
                }
            }),
            at("2026-11-06T13:30:00Z"),
        );
        assert_eq!(ring.len(), RING, "ninety offered, sixty held");
        assert_eq!(ring.newest(), Some(at("2026-11-06T14:59:00Z")));

        // A replay of the newest and of one behind it: both ignored.
        let before = ring.len();
        ring.extend(
            [
                Bucket {
                    bucket: at("2026-11-06T14:59:00Z"),
                    values: vec![None; 1],
                },
                Bucket {
                    bucket: at("2026-11-06T14:30:00Z"),
                    values: vec![None; 1],
                },
            ],
            at("2026-11-06T14:00:00Z"),
        );
        assert_eq!(ring.len(), before);
        assert_eq!(ring.newest(), Some(at("2026-11-06T14:59:00Z")));

        // And the horizon drops what has aged out, which is what keeps a window over a
        // restarted daemon from including an hour-old bucket.
        ring.extend([], at("2026-11-06T14:50:00Z"));
        assert_eq!(ring.len(), 10, "14:50 through 14:59 inclusive");
    }

    /// A window longer than the ring is a named refusal rather than a quietly short answer,
    /// which is the distinction `WindowError`'s own documentation is about: `None` would
    /// read as "the station does not report this".
    #[test]
    fn a_window_longer_than_the_ring_is_refused_by_name() {
        let ring = Ring::new();
        let e = ring
            .resolve(
                &plan(),
                MetricKey::OutTemp,
                Agg::Mean,
                Duration::from_secs(3 * 3600),
                at("2026-11-06T15:00:00Z"),
            )
            .expect_err("three hours is longer than the hour the ring holds");
        assert!(matches!(e, WindowError::TooLong { held_secs, .. } if held_secs == 3600));
        assert_eq!(RING_SPAN.as_secs(), 3600);
    }

    /// A strategy the registry does not declare for that metric names the metric, the
    /// strategy and the fix — W2 D-11 doing its job rather than an empty answer.
    #[test]
    fn an_undeclared_strategy_is_refused_by_name() {
        let ring = Ring::new();
        let e = ring
            .resolve(
                &plan(),
                MetricKey::WindSpeed,
                Agg::CircularMean,
                Duration::from_secs(300),
                at("2026-11-06T15:00:00Z"),
            )
            .expect_err("wind_speed declares mean and max, not a circular mean");
        assert!(matches!(e, WindowError::NotDeclared { .. }));
        assert!(e.to_string().contains("circular_mean"), "{e}");
    }
}
