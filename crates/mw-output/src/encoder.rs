//! [`OutputEncoder`] — W2a Decision 48, and the whole of what makes a rate-limited output
//! reviewable.
//!
//! > *An `OutputEncoder` declares `&[(Metric, Window, Agg)]` and receives a `WindowedView`;
//! > APRS's CWOP table is reviewable line by line against the specification.*
//! > — `docs/component-model.md` decision 48
//!
//! An encoder is a **table and a function**. Everything else — the schedule, the retry, the
//! rate limiting, the backoff, the connection, the SQL — is somewhere else, which is §9.3's
//! claim that a new upload service is "four files, and zero lines of scheduling, retry,
//! rate limiting, backoff or SQL".
//!
//! # Why `required()` and `encode()` cannot disagree
//!
//! The uploader resolves exactly `required()` against `WindowedView` and hands the encoder
//! a [`Resolved`], whose values are **private** and read back through `Resolved::value`
//! keyed by the triple that was asked for. So an encoder that reads a window it did not
//! declare gets `WindowError::NotDeclaredInRequest` rather than a plausible-looking
//! absence, which [`Observation::value`] turns into
//! `EncodeError::MissingWindow`. That is the mechanism behind "reviewable": the table is
//! not a comment about what the function reads, it is the only thing the function *can*
//! read.
//!
//! # `Window::Rolling(60 s)` is how an output reads a current value, and why
//!
//! **D-50** removes `Agg::Last` and says an output needing a metric's current value reads
//! `LatestSnapshot`. The three uploaders cannot: `docs/component-model.md` §4b's component
//! table gives `aprs:*`, `pws:*` and `wu:*` exactly `WindowedView` and `StationRegistry`,
//! and `LatestSnapshot` is keyed per `(website, station)` (**D-31**) — a key an uploader has
//! no way to form, because an uploader is configured per *station* and a station may appear
//! on several websites or none.
//!
//! So the current values are [`CURRENT`], the newest **complete one-minute mean**, which is
//! the finest thing `WindowedView` resolves: `window:rollup` holds 1-minute aggregate
//! buckets, so a one-minute mean is one bucket. That is also what the Go uploaders send
//! today, accidentally — their "latest reading" is a `weather_1m` row, which is a 1-minute
//! average of an arbitrary bucket inside a two-minute window with no `ORDER BY`
//! (`internal/database/client.go:121`). The difference is that this one is the *newest*
//! complete minute, deliberately, and says so in a declaration.
//!
//! Recorded as a deviation in `docs/reconciliation.md` (**A-M6-2-2**) rather than applied to
//! **D-50**, which is untouched: nothing here reads a window with `Agg::Last` and nothing
//! claims `LatestSnapshot` is the wrong home for an instantaneous value.

use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{Declaration, Resolved};
use mw_types::{StationKey, Window};

use crate::error::EncodeError;

pub mod account;
pub mod cwop;
pub mod pws;
pub mod wu;

/// The newest complete one-minute mean — how an output reads a "current" value.
///
/// See this module's documentation. One minute because that is `window:rollup`'s bucket
/// width, so this resolves against exactly one bucket and is the closest thing to an
/// instant the capability expresses.
pub const CURRENT: Window = Window::Rolling(Duration::from_secs(60));

/// Weather Underground's `_avg2m` window, which its field names say out loud.
pub const TWO_MINUTES: Window = Window::Rolling(Duration::from_secs(120));

/// CWOP's wind-averaging window.
pub const FIVE_MINUTES: Window = Window::Rolling(Duration::from_secs(300));

/// The gust window: CWOP's peak, and Weather Underground's `_10m`.
pub const TEN_MINUTES: Window = Window::of(Window::TEN_MINUTES);

/// The trailing hour, for `rainin` and CWOP's `r` field.
pub const ONE_HOUR: Window = Window::Rolling(Duration::from_secs(3600));

/// The trailing 24 hours, for CWOP's `p` field.
///
/// Distinct from [`mw_types::Window::Day`], which is the station's **local** day and is
/// what CWOP's `P` field wants. Both are maintained by `window:rollup`, and the two fields
/// exist in the protocol precisely because they are different numbers.
pub const ONE_DAY_ROLLING: Window = Window::Rolling(Duration::from_secs(24 * 3600));

/// What one occurrence has to encode from.
///
/// Holds no credential and no configuration: those are the encoder's own, captured when
/// `crate::plan` built it from the `uploader` row. What travels per occurrence is the
/// station, the resolved windows and the instant they were resolved at.
#[derive(Debug, Clone, Copy)]
pub struct Observation<'a> {
    service: &'static str,
    station: &'a StationKey,
    values: &'a Resolved,
}

impl<'a> Observation<'a> {
    /// Assemble one. `crate::uploader` is the only caller outside tests.
    pub fn new(service: &'static str, station: &'a StationKey, values: &'a Resolved) -> Self {
        Self {
            service,
            station,
            values,
        }
    }

    /// Which station this is about.
    pub fn station(&self) -> &StationKey {
        self.station
    }

    /// When the provider's state was last refreshed — the instant these values are *as of*.
    ///
    /// This is what goes in `dateutc`, rather than the wall clock. The two differ by up to
    /// one rollup period, and sending the wall clock would tell the service a one-minute-old
    /// average was measured now. The Go uploaders send `time.Now()`.
    pub fn as_of(&self) -> DateTime<Utc> {
        self.values.as_of()
    }

    /// One declared value.
    ///
    /// `Ok(None)` is an absence the encoder renders as the protocol's "unavailable"; an
    /// undeclared triple is [`EncodeError::MissingWindow`], which is a bug in the encoder
    /// rather than a configuration problem.
    pub fn value(&self, declaration: Declaration) -> Result<Option<f64>, EncodeError> {
        match self.values.value(declaration) {
            Ok(v) => Ok(v.filter(|f| f.is_finite())),
            Err(_) => Err(EncodeError::MissingWindow {
                service: self.service,
                metric: declaration.0,
                window: declaration.1,
                agg: declaration.2,
            }),
        }
    }

    /// How many of the resolved declarations hold a value.
    ///
    /// What decides `EncodeError::NothingToSend`: a station that has just started reporting
    /// resolves every declaration to `None`, and there is no point telling a service about
    /// that once a period.
    pub fn present(&self) -> usize {
        self.values
            .iter()
            .filter(|(_, v)| v.is_some_and(f64::is_finite))
            .count()
    }

    /// How many declarations were resolved.
    pub fn declared(&self) -> usize {
        self.values.len()
    }
}

/// A rate-limited output's protocol half: a declaration table and an encode.
///
/// `Send + Sync + 'static` because the encoder lives in the component body across
/// occurrences, and the body is moved into a task.
pub trait OutputEncoder: Send + Sync + 'static {
    /// What one occurrence transmits — a [`crate::Form`] for the HTTP uploaders, an
    /// `InformationField` for APRS.
    ///
    /// `Clone` because `tower::retry` clones the request for each attempt, which is what
    /// makes the retry layer able to sit **outside** the rate limiter (**D-57**).
    type Payload: Clone + Send + Sync + std::fmt::Debug + 'static;

    /// The service's name, for a metric label and a log field. A closed set in practice —
    /// one per `uploader.kind`.
    fn service(&self) -> &'static str;

    /// **The reviewable table.** Which metric, over which window, combined how.
    fn required(&self) -> &[Declaration];

    /// Build what this occurrence sends.
    fn encode(&self, observation: &Observation<'_>) -> Result<Self::Payload, EncodeError>;
}

/// Render a measurement the way a service's documentation writes it.
///
/// Three helpers rather than `format!` at each site, so that "`tempf` is one decimal and
/// `humidity` is an integer" is a visible property of the declaration table's neighbour
/// rather than a formatting string buried in a conditional.
pub(crate) fn integer(value: f64) -> String {
    format!("{}", value.round() as i64)
}

/// One decimal place — temperatures and wind speeds.
pub(crate) fn one_decimal(value: f64) -> String {
    format!("{value:.1}")
}

/// Two decimal places — rainfall and pressure, where the third digit is noise and the
/// second is not.
pub(crate) fn two_decimals(value: f64) -> String {
    format!("{value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Agg, MetricKey};

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    /// The windows this crate names are the ones `window:rollup` maintains, which is what
    /// stops a declaration table from being unresolvable.
    ///
    /// `window:rollup` holds an hour of 1-minute buckets and maintains the calendar windows
    /// and 24/48/72 hours as totals, so every rolling window here is inside the ring and
    /// every long one is in its maintained set.
    #[test]
    fn every_window_this_crate_declares_is_one_the_rollup_maintains() {
        const RING_SPAN: Duration = Duration::from_secs(3600);
        for w in [CURRENT, TWO_MINUTES, FIVE_MINUTES, TEN_MINUTES, ONE_HOUR] {
            let length = w.length().expect("a clock window");
            assert!(length <= RING_SPAN, "{w} is longer than the ring");
        }
        // The one long window, which is maintained rather than resolved from the ring --
        // and therefore only resolvable with `Agg::Sum`.
        assert_eq!(
            ONE_DAY_ROLLING.length(),
            Some(Duration::from_secs(24 * 3600))
        );
        assert_eq!(CURRENT.length(), Some(Duration::from_secs(60)));
    }

    /// An encoder cannot read outside its own table, and the failure says so.
    #[test]
    fn reading_an_undeclared_triple_names_the_encoder() {
        let declared = (MetricKey::OutTemp, CURRENT, Agg::Mean);
        let resolved = Resolved::new(at("2026-11-11T05:00:00Z"), [(declared, Some(72.4))]);
        let station = StationKey::new("backyard");
        let obs = Observation::new("wunderground", &station, &resolved);

        assert_eq!(obs.value(declared).expect("declared"), Some(72.4));
        assert_eq!(obs.as_of(), at("2026-11-11T05:00:00Z"));
        assert_eq!(obs.station().as_str(), "backyard");

        let never = (MetricKey::Barometer, CURRENT, Agg::Mean);
        let e = obs.value(never).expect_err("not declared");
        assert!(
            matches!(
                e,
                EncodeError::MissingWindow {
                    service: "wunderground",
                    ..
                }
            ),
            "{e}"
        );
    }

    /// A non-finite value is an absence rather than a number, because that is what
    /// arithmetic over an empty window produces and `0/0` is not a temperature.
    #[test]
    fn a_non_finite_value_is_an_absence() {
        let temp = (MetricKey::OutTemp, CURRENT, Agg::Mean);
        let humidity = (MetricKey::OutHumidity, CURRENT, Agg::Mean);
        let dir = (MetricKey::WindDir, FIVE_MINUTES, Agg::CircularMean);
        let resolved = Resolved::new(
            at("2026-11-11T05:00:00Z"),
            [(temp, Some(f64::NAN)), (humidity, None), (dir, Some(181.0))],
        );
        let station = StationKey::new("backyard");
        let obs = Observation::new("cwop", &station, &resolved);
        assert_eq!(obs.value(temp).expect("declared"), None);
        assert_eq!(obs.value(humidity).expect("declared"), None);
        assert_eq!(obs.declared(), 3);
        assert_eq!(obs.present(), 1);
    }

    #[test]
    fn a_measurement_renders_the_way_its_service_documents_it() {
        assert_eq!(integer(54.6), "55");
        assert_eq!(integer(-0.4), "0");
        assert_eq!(one_decimal(72.34), "72.3");
        assert_eq!(one_decimal(-5.0), "-5.0");
        assert_eq!(two_decimals(29.9199), "29.92");
        assert_eq!(two_decimals(0.0), "0.00");
    }
}

/// Helpers the three encoders' tests share.
///
/// One module rather than three copies, because what they share is the *shape* of a
/// resolution — "every declaration this encoder names, resolved, with these values filled
/// in" — and an encoder test that builds a partial resolution by hand would pass for the
/// wrong reason: `EncodeError::MissingWindow` and `Ok(None)` look alike from the outside and
/// mean opposite things.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use mw_component::Resolved;
    use secrecy::SecretString;

    /// The instant every encoder test resolves as of, so `dateutc` is a literal.
    pub(crate) const AS_OF: &str = "2026-11-11T05:00:00Z";

    /// The station every encoder test is about.
    pub(crate) fn obs_station() -> StationKey {
        StationKey::new("backyard")
    }

    /// An account with a credential a test can assert is redacted.
    pub(crate) fn encoder_account() -> account::Account {
        account::Account::new(
            "KWABACKYARD",
            SecretString::from("hunter2"),
            "modernweather-1.0",
        )
    }

    /// A resolution holding exactly the declarations given, in that order.
    pub(crate) fn resolved(values: &[(Declaration, Option<f64>)]) -> Resolved {
        let at: DateTime<Utc> = AS_OF.parse().expect("an RFC 3339 instant");
        Resolved::new(at, values.iter().copied())
    }

    /// A resolution holding **every** declaration in `table`, with `values` filled in and
    /// the rest absent — which is what the uploader hands an encoder.
    pub(crate) fn all(table: &[Declaration], values: &[(Declaration, Option<f64>)]) -> Resolved {
        let mut full: Vec<(Declaration, Option<f64>)> = table.iter().map(|d| (*d, None)).collect();
        for (declaration, value) in values {
            let row = full
                .iter_mut()
                .find(|(d, _)| d == declaration)
                .unwrap_or_else(|| panic!("{declaration:?} is not in this encoder's table"));
            row.1 = *value;
        }
        resolved(&full)
    }
}
