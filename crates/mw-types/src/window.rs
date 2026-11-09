//! [`Window`] and [`Agg`] — the two halves of what an output declares it needs.
//!
//! An `OutputEncoder` declares `&[(MetricKey, Window, Agg)]` (W2a Decision 48) and
//! `mw_component::WindowedView` resolves it (Decision 49). The triple lives here, in the
//! lowest layer, because the declaration has to be writable by `mw-aprs` — a *pure* crate
//! whose golden test must link without a database or a transport (W2a assumption 7) — and
//! `mw-component` is a layer above it.
//!
//! # `Agg` is exactly W2's five strategies, and `Last` is not one of them
//!
//! **D-50.** W2a §4b.2 sketched `{Last, Mean, CircularMean, Max, Min, Sum}` and claimed it
//! was a subset of what `metrics/registry.toml` expresses. It is not: W2 D-11 fixes the
//! strategy vocabulary at `mean, min, max, total, circular_mean` and excludes `last`
//! explicitly, because a last value is not a composable aggregate state and so has no
//! column at any level to resolve against. An output needing a metric's *current* value
//! reads `LatestSnapshot`; `WindowedView` exists for the windowed ones. [`Agg::Sum`] maps
//! onto the registry's `total` — the same strategy under two names, kept because `Sum` is
//! what the protocol specifications say and `total` is what the column is called.
//!
//! # `Window` is an enum rather than W2a's `struct Window(Duration)`, and that is this
//! item's whole subject
//!
//! The sketch in `docs/component-model.md` §4b.2 is `pub struct Window(Duration);` with
//! `Window::DAY` as one of its constants. **A station's local day is not a duration.** On
//! a spring-forward date it is 23 hours and on a fall-back date it is 25, so a `DAY`
//! expressed as `Duration::from_secs(86_400)` is wrong twice a year in exactly the way
//! `date_trunc('day', NOW())` is wrong every day — which is the defect
//! `implementation-plan.md` M3-4 exists to close. §4b.3 says as much in prose two
//! paragraphs later: *"`Window::DAY` is the one window that needs a time zone, and it takes
//! it from the station"*. A type that cannot hold a time zone cannot express that, so the
//! calendar windows are their own variants and [`Window::Rolling`] carries the duration.
//!
//! Recorded as a departure in `docs/reconciliation.md`'s *Proposed amendments* section
//! (**A-M3-4-2**) rather than applied to a decision: no `DESIGN.md` decision fixes
//! `Window`'s shape, so there is nothing to amend — but the sketch is what a reader will
//! have in mind.

use std::fmt;
use std::time::Duration;

use crate::Strategy;

/// Which window of readings a declaration is about.
///
/// Every variant is a **trailing** window ending at the resolver's notion of now. There is
/// no variant for an arbitrary historical range, deliberately: that is `/span`'s query and
/// `ReadingStore::span` is what serves it (**D-30**). A `WindowedView` that could be asked
/// about last March would be a second query path with no cache and no owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Window {
    /// The trailing `Duration`, on the clock. Weather Underground's `_avg2m` and `_10m`
    /// field names are two of these.
    Rolling(Duration),

    /// The trailing `Duration`, **extrapolated to one hour** — a rate rather than a total.
    ///
    /// Meaningful with [`Agg::Sum`] and with nothing else, because extrapolating a mean or
    /// a maximum is not an operation: `mw_component::WindowError::NotARate` is what a
    /// resolver answers for the rest. The one value in this system that needs it is the
    /// **rain rate**, which the frozen `/latest` body serves as
    /// `sum(rainincremental) over 10 minutes x 6` (`rainfall_calc.go:149`) and **not** from
    /// the `rainrate` column the station reported — `handlers.go:344` overwrites the
    /// column's value with the computed one, so the contract promises the extrapolation.
    ///
    /// A separate variant rather than a sixth [`Agg`], because **D-50** fixes the `Agg` set
    /// at W2's five strategies and a rate is not a sixth way to combine bucket state: it is
    /// the same `Sum` over the same buckets, reported per hour. What changes is the window's
    /// units, which is why it is the window that says so.
    PerHour(Duration),

    /// Since the first instant of the **station's** local day.
    ///
    /// 23 or 25 hours long on a transition date, which is the entire reason this is not a
    /// `Duration`. The zone comes from the station (W3 D-24); today
    /// `rainfall_calc.go:30`'s `date_trunc('day', NOW())` truncates in the *database
    /// server's* zone, which makes the device's declared `timezone` decorative.
    Day,

    /// Since the first instant of the station's local month.
    Month,

    /// Since the first instant of the station's local year.
    Year,

    /// Since the current rain event began, or unresolvable while there is no event.
    ///
    /// The **computed** storm window, which is a different fact from the station-reported
    /// `storm_start` column that Davis hardware supplies (W3 §2.5, consequence 1). The two
    /// share a name today and nothing distinguishes them.
    Storm,
}

impl Window {
    /// Ten minutes, which is the gust window CWOP asks for and the rain-rate window the
    /// frozen contract computes over.
    pub const TEN_MINUTES: Duration = Duration::from_secs(600);

    /// A trailing window of `d`.
    pub const fn of(d: Duration) -> Self {
        Window::Rolling(d)
    }

    /// The trailing length, for the two variants that have one on the clock.
    ///
    /// `None` for a calendar window, because its length is a function of a date and a zone
    /// and this type holds neither — which is what stops a caller computing one by
    /// subtraction.
    pub const fn length(self) -> Option<Duration> {
        match self {
            Window::Rolling(d) | Window::PerHour(d) => Some(d),
            Window::Day | Window::Month | Window::Year | Window::Storm => None,
        }
    }

    /// Whether resolving this needs the station's time zone.
    pub const fn needs_zone(self) -> bool {
        matches!(self, Window::Day | Window::Month | Window::Year)
    }

    /// The label a log field and a `WindowError` message carry. Internal vocabulary
    /// (**D-36**): an operator never reads it, and a metric label does.
    pub fn as_str(self) -> &'static str {
        match self {
            Window::Rolling(_) => "rolling",
            Window::PerHour(_) => "per_hour",
            Window::Day => "day",
            Window::Month => "month",
            Window::Year => "year",
            Window::Storm => "storm",
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Window::Rolling(d) => write!(f, "the last {}", Secs(*d)),
            Window::PerHour(d) => write!(f, "the last {}, per hour", Secs(*d)),
            Window::Day => f.write_str("the station's local day"),
            Window::Month => f.write_str("the station's local month"),
            Window::Year => f.write_str("the station's local year"),
            Window::Storm => f.write_str("the current storm"),
        }
    }
}

/// A duration rendered the way a window is spoken about — `10 minutes`, `72 hours`.
///
/// Its own type rather than a `format!` at each site, because a `Display` that printed
/// `600s` would put seconds in an operator-facing sentence about a ten-minute average.
struct Secs(Duration);

impl fmt::Display for Secs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.as_secs();
        match () {
            () if s >= 3600 && s.is_multiple_of(3600) => write!(f, "{} hours", s / 3600),
            () if s >= 60 && s.is_multiple_of(60) => write!(f, "{} minutes", s / 60),
            () => write!(f, "{s} seconds"),
        }
    }
}

/// How the window's readings are combined — W2's five strategies and no sixth (**D-50**).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agg {
    /// The arithmetic mean, from the level's `_sum` and `_count` (or `_stats`) columns.
    /// Never `avg(avg(x))`, which D-12 measured wrong in 658 of 868 real pairs.
    Mean,
    /// The least value in the window.
    Min,
    /// The greatest value in the window. Wind gust is this over ten minutes.
    Max,
    /// The window's total — the registry's `total` strategy under the name the protocol
    /// specifications use.
    Sum,
    /// The vector mean of an angle, from `_sin_sum`, `_cos_sum` and `_dir_count`.
    CircularMean,
}

impl Agg {
    /// Every variant, for exhaustive tests and for a resolver's own table.
    pub const ALL: &'static [Agg] = &[Agg::Mean, Agg::Min, Agg::Max, Agg::Sum, Agg::CircularMean];

    /// The registry strategy this resolves against.
    ///
    /// The mapping is total and that is the point of **D-50**: every `Agg` has a column at
    /// every level that declares it, so "the registry's per-metric strategy resolves an
    /// encoder's declaration" (W2 assumption 10) holds without a second table.
    pub const fn strategy(self) -> Strategy {
        match self {
            Agg::Mean => Strategy::Mean,
            Agg::Min => Strategy::Min,
            Agg::Max => Strategy::Max,
            Agg::Sum => Strategy::Total,
            Agg::CircularMean => Strategy::CircularMean,
        }
    }

    /// The `Agg` that resolves a declared strategy, if one does.
    ///
    /// Total in this direction too, which is what makes the two sets the same size and
    /// `Agg::Last`'s absence a closed question rather than a gap.
    pub const fn of_strategy(s: Strategy) -> Self {
        match s {
            Strategy::Mean => Agg::Mean,
            Strategy::Min => Agg::Min,
            Strategy::Max => Agg::Max,
            Strategy::Total => Agg::Sum,
            Strategy::CircularMean => Agg::CircularMean,
        }
    }

    /// The label a log field and a metric label carry.
    pub const fn as_str(self) -> &'static str {
        match self {
            Agg::Mean => "mean",
            Agg::Min => "min",
            Agg::Max => "max",
            Agg::Sum => "total",
            Agg::CircularMean => "circular_mean",
        }
    }
}

impl fmt::Display for Agg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **D-50 in both directions.** Every `Agg` maps onto a registry strategy and every
    /// registry strategy maps back, so there is no declaration an encoder can write that
    /// the registry cannot resolve and no column the registry generates that no `Agg`
    /// reaches. That round trip is what C-W2a-10 found missing, and `Agg::Last` is the
    /// variant whose absence makes it hold.
    #[test]
    fn the_agg_set_and_the_registrys_strategy_set_are_the_same_set() {
        for agg in Agg::ALL {
            assert_eq!(Agg::of_strategy(agg.strategy()), *agg);
        }
        for s in [
            Strategy::Mean,
            Strategy::Min,
            Strategy::Max,
            Strategy::Total,
            Strategy::CircularMean,
        ] {
            assert_eq!(Agg::of_strategy(s).strategy(), s);
        }
        assert_eq!(Agg::ALL.len(), 5);
        assert_eq!(Agg::Sum.as_str(), "total", "Sum IS the registry's total");
    }

    /// A calendar window refuses to report a length, which is what stops the 23-hour and
    /// 25-hour days being computed by subtraction somewhere that has no zone.
    #[test]
    fn only_a_clock_window_has_a_length() {
        assert_eq!(
            Window::of(Window::TEN_MINUTES).length(),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            Window::PerHour(Window::TEN_MINUTES).length(),
            Some(Duration::from_secs(600))
        );
        for calendar in [Window::Day, Window::Month, Window::Year, Window::Storm] {
            assert_eq!(calendar.length(), None, "{calendar:?}");
        }
        assert!(Window::Day.needs_zone());
        assert!(Window::Month.needs_zone());
        assert!(Window::Year.needs_zone());
        // The storm's start is a maintained instant, not a date in a zone.
        assert!(!Window::Storm.needs_zone());
        assert!(!Window::of(Duration::from_secs(60)).needs_zone());
    }

    #[test]
    fn a_window_reads_as_a_sentence() {
        assert_eq!(
            Window::of(Window::TEN_MINUTES).to_string(),
            "the last 10 minutes"
        );
        assert_eq!(
            Window::PerHour(Window::TEN_MINUTES).to_string(),
            "the last 10 minutes, per hour"
        );
        assert_eq!(
            Window::of(Duration::from_secs(72 * 3600)).to_string(),
            "the last 72 hours"
        );
        assert_eq!(
            Window::of(Duration::from_secs(90)).to_string(),
            "the last 90 seconds"
        );
        assert_eq!(Window::Day.to_string(), "the station's local day");
    }
}
