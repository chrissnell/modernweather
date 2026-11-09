//! [`WindowedView`] — the capability an output resolves its declaration against.
//!
//! W2a §4b.2's shape, with W2a Decision 50's structural half: `aprs:*`, `pws:*` and `wu:*`
//! require **this** and are never handed a [`crate::ReadingStore`], so they *cannot* run a
//! query. Today APRS runs `SELECT calculate_wind_gust(?)` from inside the radio component
//! with a comment saying "same as website" — one concept, two call sites, no owner — and
//! takes wind direction and speed straight off the latest reading, which CWOP requires to
//! be averaged.
//!
//! # Why `resolve` is synchronous
//!
//! The provider is `window:rollup`, an `Every { 60 s }` component holding a bounded ring of
//! recent 1-minute aggregate buckets per station (W2a §4b.3). The query happens on *its*
//! tick, inside *its* 10-second deadline; a consumer's `resolve` is arithmetic over memory.
//! A `fn` returning a `Result` rather than a future is that fact in a signature — the same
//! reasoning [`crate::StationRegistry`] is synchronous for. An implementation that wanted
//! to query would have to change this trait, which is the review it should get.
//!
//! # Why [`Resolved`]'s values are private
//!
//! An encoder reads them back through [`Resolved::value`], keyed by the exact
//! `(MetricKey, Window, Agg)` triple it declared, so **an encoder cannot read a window it
//! did not declare**. That is what makes `required()` reviewable against a protocol
//! specification by somebody who does not read Rust (W3 §2a.1), rather than a list that
//! documents a `&v` nobody checks.

use std::sync::Arc;

use mw_types::{Agg, Class, Classify, MetricKey, StationKey, Window};
use smallvec::SmallVec;

use crate::capability::Capability;
use crate::cx::CapabilityHandle;

/// One declared windowed value: which metric, over which window, combined how.
pub type Declaration = (MetricKey, Window, Agg);

/// How many declarations fit without spilling to the heap.
///
/// Sixteen, against the largest declaration table in the system: CWOP's is eight rows
/// (`docs/component-model.md` §4b.2) and Weather Underground's is of the same order, so a
/// resolve is a stack allocation for every caller this program has.
const INLINE: usize = 16;

/// `(Metric, Window, Agg)` resolution over recent aggregate buckets.
pub trait WindowedView: Send + Sync + 'static {
    /// Resolve every declaration for one station, or say why the whole set could not be.
    ///
    /// **A `None` value is not an error.** A metric the station has never reported has no
    /// bucket state to combine, and the window is empty rather than wrong — **D-26** one
    /// level up from a column. The errors are about the *declaration* and about the
    /// *station*: a strategy the registry does not declare for that metric, a window longer
    /// than the provider maintains, a station it has no zone for. Those are facts about the
    /// configuration and the build, so they are the same answer on every call and worth a
    /// name.
    fn resolve(&self, station: &StationKey, want: &[Declaration]) -> Result<Resolved, WindowError>;
}

impl CapabilityHandle for Arc<dyn WindowedView> {
    const CAPABILITY: Capability = Capability::WindowedView;
}

/// What one [`WindowedView::resolve`] answered.
///
/// Values are private and read through [`Resolved::value`]; see the module documentation.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    values: SmallVec<[(Declaration, Option<f64>); INLINE]>,
    /// The instant the provider's state was last refreshed from the store.
    ///
    /// Carried so that a caller can tell "the window is empty" from "the provider has not
    /// ticked yet", which is the distinction `Staleness::Degraded` is built on (W3 §2a.2)
    /// and the one a consumer would otherwise have to infer from an all-`None` answer.
    as_of: chrono::DateTime<chrono::Utc>,
}

impl Resolved {
    /// Build an answer. The provider's constructor; a consumer only reads.
    pub fn new(
        as_of: chrono::DateTime<chrono::Utc>,
        values: impl IntoIterator<Item = (Declaration, Option<f64>)>,
    ) -> Self {
        Self {
            values: values.into_iter().collect(),
            as_of,
        }
    }

    /// The value for one declaration.
    ///
    /// `Err(NotDeclared)` rather than `None` for a triple that was not asked for, because
    /// the two mean opposite things: `Ok(None)` is "the station does not report this" and
    /// an encoder emits nothing for it, while a triple missing from the request is an
    /// encoder reading outside its own declaration table.
    pub fn value(&self, declaration: Declaration) -> Result<Option<f64>, WindowError> {
        self.values
            .iter()
            .find(|(d, _)| *d == declaration)
            .map(|(_, v)| *v)
            .ok_or(WindowError::NotDeclaredInRequest {
                metric: declaration.0,
                window: declaration.1,
                agg: declaration.2,
            })
    }

    /// When the provider's state was last refreshed.
    pub fn as_of(&self) -> chrono::DateTime<chrono::Utc> {
        self.as_of
    }

    /// How many declarations were answered.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether nothing was asked for.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every declaration and its value, in the order they were declared.
    ///
    /// For a renderer that is driven by the table rather than naming each row —
    /// `/metrics`, the management UI's window panel — and **not** a way round
    /// [`Resolved::value`]: reading this still requires having the declaration list, which
    /// is the thing [`Resolved::value`] checks against.
    pub fn iter(&self) -> impl Iterator<Item = (Declaration, Option<f64>)> + '_ {
        self.values.iter().copied()
    }
}

/// Why a declaration could not be resolved.
///
/// Every variant is a property of the declaration, the registry or the station's
/// configuration — so it is the same answer on the next call, which is why every one of
/// them is `Fatal` rather than retried. A transient store failure never reaches here: the
/// provider owns the query, and a provider that cannot reach the database keeps serving its
/// last good state and records `Staleness::Degraded` (W3 §2a.6).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    /// The registry does not declare this strategy for this metric at the 1-minute level.
    ///
    /// Which is W2 D-11 doing its job: `(WindSpeed, 10 min, CircularMean)` has no
    /// `windspeed_sin_sum` to resolve against, and the declaration is wrong rather than the
    /// data being absent.
    #[error(
        "metrics/registry.toml does not declare `{agg}` for `{metric:?}` at the 1-minute \
         level, so there is no bucket state to combine. Either declare it in \
         `aggregates` for that metric and re-run `cargo xtask codegen`, or declare a \
         strategy the registry already has."
    )]
    NotDeclared {
        /// Which metric.
        metric: MetricKey,
        /// Over which window.
        window: Window,
        /// Combined how.
        agg: Agg,
    },

    /// The window is longer than the provider maintains for that metric.
    ///
    /// `window:rollup` keeps an hour of 1-minute buckets (W2a §4b.3) and maintains the
    /// calendar and multi-day rain windows separately, so a three-hour mean of a
    /// temperature has nothing behind it. Named rather than answered with `None`, because
    /// `None` would read as "the station does not report this".
    #[error(
        "`{window}` is longer than this view maintains for `{metric:?}`: the ring holds \
         {held_secs} seconds of 1-minute buckets, and the longer windows are maintained \
         only for the metrics that declare a total. Read it from `/span`, which is the \
         query-shaped endpoint, or add it to the maintained set."
    )]
    TooLong {
        /// Which metric.
        metric: MetricKey,
        /// The window asked for.
        window: Window,
        /// How much the ring holds.
        held_secs: u64,
    },

    /// `Window::PerHour` with an aggregate that cannot be extrapolated.
    #[error(
        "`{window}` is a rate, and only a total can be extrapolated to one hour — \
         `{agg}` cannot. Declare `total`, or declare a `Window::Rolling` of the same \
         length."
    )]
    NotARate {
        /// The window asked for.
        window: Window,
        /// The aggregate asked for.
        agg: Agg,
    },

    /// A calendar window for a station with no usable time zone.
    ///
    /// The rollover happens at the station's local midnight (W3 **D-24**), so without a
    /// zone there is no boundary to accumulate from — and falling back to UTC is exactly
    /// what `date_trunc('day', NOW())` does today and what this item exists to stop.
    #[error(
        "station `{station}` has no usable IANA time zone, so `{window}` has no boundary. \
         Set `config.stations.timezone` for it; a blank or absent value is what makes the \
         rollover happen in the server's zone instead of the station's."
    )]
    NoZone {
        /// Which station.
        station: StationKey,
        /// The window that needed the zone.
        window: Window,
    },

    /// The provider has no state for this station at all.
    ///
    /// A station that is configured and has never reported, or one that is not configured
    /// on this node. Distinct from an empty window for the reason [`WindowError::TooLong`]
    /// is: a caller that cannot tell them apart would report a flat zero for a station that
    /// does not exist.
    #[error(
        "this view holds nothing for station `{station}`. It is maintained for the \
         stations in `config.stations`, so either the key is not one of them or \
         `window:rollup` has not completed a tick since it started."
    )]
    UnknownStation {
        /// Which station.
        station: StationKey,
    },

    /// A triple read back from [`Resolved`] that was not in the request.
    #[error(
        "`({metric:?}, {window}, {agg})` was read from a resolution that did not declare \
         it. An output reads exactly the declarations its `required()` table names, which \
         is what makes that table reviewable against the protocol specification."
    )]
    NotDeclaredInRequest {
        /// Which metric.
        metric: MetricKey,
        /// Over which window.
        window: Window,
        /// Combined how.
        agg: Agg,
    },
}

impl Classify for WindowError {
    /// `Fatal` for every variant, and the enum's own documentation says why: each one is a
    /// property of the declaration or of the configuration, so the next call answers the
    /// same thing. Retrying a declaration the registry does not have would be a component
    /// in `Backoff` forever with the reason in `last_error` either way — but classified
    /// honestly, `mw_supervisor::task::interpret` has something true to read when it
    /// branches on more than `Ignorable`.
    fn class(&self) -> Class {
        Class::Fatal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    /// The property the privacy exists for: a triple that was not declared is an error and
    /// not a `None`, so an encoder cannot read a window it did not ask for and get a
    /// plausible-looking "no data".
    #[test]
    fn reading_an_undeclared_triple_is_an_error_and_not_an_absent_value() {
        let gust = (
            MetricKey::WindSpeed,
            Window::of(Window::TEN_MINUTES),
            Agg::Max,
        );
        let unreported = (
            MetricKey::SoilTemp4,
            Window::of(Window::TEN_MINUTES),
            Agg::Mean,
        );
        let resolved = Resolved::new(
            at("2026-11-06T15:00:00Z"),
            [(gust, Some(14.5)), (unreported, None)],
        );

        assert_eq!(resolved.value(gust), Ok(Some(14.5)));
        assert_eq!(
            resolved.value(unreported),
            Ok(None),
            "a metric the station does not report is an empty window, not a failure"
        );

        let never_asked = (MetricKey::Barometer, Window::Day, Agg::Mean);
        assert!(matches!(
            resolved.value(never_asked),
            Err(WindowError::NotDeclaredInRequest { .. })
        ));
        assert_eq!(resolved.len(), 2);
        assert!(!resolved.is_empty());
        assert_eq!(resolved.as_of(), at("2026-11-06T15:00:00Z"));
    }

    /// `iter` is driven by the declaration order, so a renderer produces the same rows in
    /// the same order on every tick — which is what makes a rendered table diffable.
    #[test]
    fn the_answer_keeps_the_declared_order() {
        let a = (
            MetricKey::WindDir,
            Window::of(Duration::from_secs(300)),
            Agg::CircularMean,
        );
        let b = (
            MetricKey::WindSpeed,
            Window::of(Duration::from_secs(300)),
            Agg::Mean,
        );
        let resolved = Resolved::new(at("2026-11-06T15:00:00Z"), [(a, Some(181.0)), (b, None)]);
        let order: Vec<Declaration> = resolved.iter().map(|(d, _)| d).collect();
        assert_eq!(order, vec![a, b]);
    }

    /// Every variant is `Fatal`, which is the enum's own claim and the thing a future
    /// `tower` retry layer would read.
    #[test]
    fn every_window_failure_is_a_fact_about_the_declaration_and_not_retried() {
        let station = StationKey::new("backyard");
        let errors = [
            WindowError::NotDeclared {
                metric: MetricKey::WindSpeed,
                window: Window::of(Window::TEN_MINUTES),
                agg: Agg::CircularMean,
            },
            WindowError::TooLong {
                metric: MetricKey::OutTemp,
                window: Window::of(Duration::from_secs(3 * 3600)),
                held_secs: 3600,
            },
            WindowError::NotARate {
                window: Window::PerHour(Window::TEN_MINUTES),
                agg: Agg::Mean,
            },
            WindowError::NoZone {
                station: station.clone(),
                window: Window::Day,
            },
            WindowError::UnknownStation {
                station: station.clone(),
            },
            WindowError::NotDeclaredInRequest {
                metric: MetricKey::Barometer,
                window: Window::Day,
                agg: Agg::Mean,
            },
        ];
        for e in &errors {
            assert_eq!(e.class(), Class::Fatal, "{e}");
            // §5.9: the message names what to do about it, so every one of them has to
            // say more than what went wrong.
            assert!(e.to_string().len() > 60, "{e}");
        }
    }

    /// The one message an operator is most likely to see, worded for them: it names the
    /// station and the column to set.
    #[test]
    fn the_missing_zone_message_names_the_station_and_the_column() {
        let e = WindowError::NoZone {
            station: StationKey::new("backyard"),
            window: Window::Day,
        };
        let text = e.to_string();
        assert!(text.contains("backyard"), "{text}");
        assert!(text.contains("config.stations.timezone"), "{text}");
        assert!(text.contains("the station's local day"), "{text}");
    }
}
