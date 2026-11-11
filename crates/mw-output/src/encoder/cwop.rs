//! The CWOP table, which is this work item's deliverable.
//!
//! > **Done when:** CWOP receives a **5-minute averaged** wind direction and speed with a
//! > **10-minute peak** gust — the protocol's requirement, and the defect D-30 as written
//! > would have carried forward.
//! > — `docs/implementation-plan.md` M6-2
//!
//! # The table
//!
//! | Metric | Window | Agg | CWOP field | Requirement |
//! |---|---|---|---|---|
//! | `WindDir` | 5 min | `CircularMean` | `ddd` | averaged direction |
//! | `WindSpeed` | 5 min | `Mean` | `sss` | averaged speed |
//! | `WindSpeed` | 10 min | `Max` | `g` | peak over the window |
//! | `OutTemp` | 1 min | `Mean` | `t` | current |
//! | `RainIncremental` | 1 h | `Sum` | `r` | last hour |
//! | `RainIncremental` | 24 h | `Sum` | `p` | last 24 hours |
//! | `RainIncremental` | local day | `Sum` | `P` | midnight-to-now, **local** |
//! | `OutHumidity` | 1 min | `Mean` | `h` | current |
//! | `Barometer` | 1 min | `Mean` | `b` | current |
//! | `SolarWatts` | 1 min | `Mean` | `L`/`l` | current |
//!
//! Ten rows, reviewable against the CWOP specification by somebody who does not read Rust,
//! which `docs/component-model.md` §4b.2 calls "the actual deliverable".
//!
//! # What each row replaces
//!
//! `docs/component-model.md` §4b.1 tabulates where one Go packet's values come from, and
//! three of the four are wrong:
//!
//! | Value | Go | Here |
//! |---|---|---|
//! | wind gust | `SELECT calculate_wind_gust(?)` from inside the radio component | a declared `(WindSpeed, 10 min, Max)` |
//! | wind direction | straight off the reading | `(WindDir, 5 min, CircularMean)` |
//! | wind speed | straight off the reading | `(WindSpeed, 5 min, Mean)` |
//! | day rain | `CalculateDailyRainfall` called inside the packet builder | `(RainIncremental, Day, Sum)` |
//!
//! The wind direction and speed are the defect the item exists to close: CWOP requires an
//! average and the packet carries an instantaneous sample, which is **a protocol violation
//! that looks like working software** — the packets are accepted, the numbers are wrong,
//! and nothing reports it (`DESIGN.md` A-W2a-1).
//!
//! `WindDir`'s row is also the only `CircularMean` in the program. It is not a new
//! mechanism: W0 verified the sine-sum/cosine-sum/count pattern composes through all three
//! aggregate levels to within 3 × 10⁻¹² degrees (**D-13**), and averaging a direction
//! arithmetically would read 180° for a wind oscillating about north.
//!
//! # `p` and `P` are different numbers
//!
//! `p` is the trailing 24 hours on the clock and `P` is since the station's **local**
//! midnight — `Window::Day`, whose boundary `rain:rollover:<station>` publishes in the
//! station's zone (W3 **D-24**). Today's `date_trunc('day', NOW())` truncates in the
//! *database server's* zone, which makes the station's configured `timezone` decorative and
//! sends a `P` field that rolls over at the wrong hour.

use mw_aprs::{InformationField, Position, Symbol, WeatherReport};
use mw_component::Declaration;
use mw_types::{Agg, MetricKey};

use super::{
    Observation, OutputEncoder, CURRENT, FIVE_MINUTES, ONE_DAY_ROLLING, ONE_HOUR, TEN_MINUTES,
};
use crate::error::EncodeError;

/// This encoder's name, for a metric label and a log field.
pub const SERVICE: &str = "aprs";

/// Averaged wind direction — CWOP's `ddd`.
pub const WIND_DIR: Declaration = (MetricKey::WindDir, FIVE_MINUTES, Agg::CircularMean);
/// Averaged wind speed — CWOP's `sss`.
pub const WIND_SPEED: Declaration = (MetricKey::WindSpeed, FIVE_MINUTES, Agg::Mean);
/// Peak gust over ten minutes — CWOP's `g`.
pub const WIND_GUST: Declaration = (MetricKey::WindSpeed, TEN_MINUTES, Agg::Max);
/// Temperature — CWOP's `t`.
pub const TEMPERATURE: Declaration = (MetricKey::OutTemp, CURRENT, Agg::Mean);
/// Rainfall in the last hour — CWOP's `r`.
pub const RAIN_HOUR: Declaration = (MetricKey::RainIncremental, ONE_HOUR, Agg::Sum);
/// Rainfall in the last 24 hours — CWOP's `p`.
pub const RAIN_24H: Declaration = (MetricKey::RainIncremental, ONE_DAY_ROLLING, Agg::Sum);
/// Rainfall since the station's local midnight — CWOP's `P`.
pub const RAIN_DAY: Declaration = (MetricKey::RainIncremental, mw_types::Window::Day, Agg::Sum);
/// Humidity — CWOP's `h`.
pub const HUMIDITY: Declaration = (MetricKey::OutHumidity, CURRENT, Agg::Mean);
/// Barometric pressure — CWOP's `b`.
pub const BAROMETER: Declaration = (MetricKey::Barometer, CURRENT, Agg::Mean);
/// Solar irradiance — CWOP's `L`/`l`.
pub const SOLAR: Declaration = (MetricKey::SolarWatts, CURRENT, Agg::Mean);

/// **The table.** Ten rows, in the order the information field writes them.
pub const REQUIRED: &[Declaration] = &[
    WIND_DIR,
    WIND_SPEED,
    WIND_GUST,
    TEMPERATURE,
    RAIN_HOUR,
    RAIN_24H,
    RAIN_DAY,
    HUMIDITY,
    BAROMETER,
    SOLAR,
];

/// The APRS/CWOP encoder: a position, a symbol and a comment, from configuration.
///
/// Holds no callsign and no passcode. Those belong to the **framing** — the TNC2 header and
/// the APRS-IS login — which is `crate::frame`'s, because `docs/component-model.md` §4b.4
/// puts the split at the information field: one encoder, two framings, three transports.
#[derive(Debug, Clone)]
pub struct Cwop {
    position: Position,
    symbol: Symbol,
    software: String,
    comment: Option<String>,
}

impl Cwop {
    /// Build the encoder from the station's position and the uploader's symbol and comment.
    pub fn new(
        position: Position,
        symbol: Symbol,
        software: impl Into<String>,
        comment: Option<String>,
    ) -> Self {
        Self {
            position,
            symbol,
            software: software.into(),
            comment,
        }
    }

    /// Where this station beacons from.
    pub fn position(&self) -> Position {
        self.position
    }
}

impl OutputEncoder for Cwop {
    type Payload = InformationField;

    fn service(&self) -> &'static str {
        SERVICE
    }

    fn required(&self) -> &[Declaration] {
        REQUIRED
    }

    fn encode(&self, observation: &Observation<'_>) -> Result<InformationField, EncodeError> {
        // Every absence stays an absence: `WeatherReport` renders the protocol's dots for
        // the four mandatory fields and omits the optional ones (`mw_aprs::weather`).
        let report = WeatherReport {
            wind_direction_deg: observation.value(WIND_DIR)?,
            wind_speed_mph: observation.value(WIND_SPEED)?,
            wind_gust_mph: observation.value(WIND_GUST)?,
            temperature_f: observation.value(TEMPERATURE)?,
            rain_last_hour_in: observation.value(RAIN_HOUR)?,
            rain_last_24h_in: observation.value(RAIN_24H)?,
            rain_since_midnight_in: observation.value(RAIN_DAY)?,
            humidity_percent: observation.value(HUMIDITY)?,
            barometer_in_hg: observation.value(BAROMETER)?,
            solar_watts_per_m2: observation.value(SOLAR)?,
        };
        // **A beacon with no measurements is still a beacon.** Unlike the HTTP uploaders,
        // APRS is a position report that carries weather: a station with no readings yet
        // still has a position worth announcing, and the dotted fields say truthfully that
        // it is reporting nothing. So there is no `NothingToSend` here, which is the one
        // place the three encoders differ in more than a table.
        report
            .encode_info(
                self.position,
                self.symbol,
                &self.software,
                self.comment.as_deref(),
            )
            .map_err(EncodeError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::tests_support::{obs_station, resolved};

    /// Every declaration in the CWOP table, resolved, with `values` filled in.
    fn all(values: &[(Declaration, Option<f64>)]) -> mw_component::Resolved {
        crate::encoder::tests_support::all(REQUIRED, values)
    }

    fn encoder() -> Cwop {
        Cwop::new(
            Position::new(45.1, -122.05).expect("a position"),
            Symbol::WEATHER,
            ".mw1.0",
            None,
        )
    }

    /// **The done-criterion, as a declaration.** The table says 5-minute averages and a
    /// 10-minute peak, which is what the protocol requires and what the Go packet does not
    /// send.
    #[test]
    fn the_table_declares_what_cwop_requires() {
        use std::time::Duration;
        assert_eq!(REQUIRED.len(), 10);

        // Wind direction: averaged over five minutes, and **circularly** -- an arithmetic
        // mean of a direction oscillating about north reads 180.
        assert_eq!(WIND_DIR.0, MetricKey::WindDir);
        assert_eq!(WIND_DIR.1.length(), Some(Duration::from_secs(300)));
        assert_eq!(WIND_DIR.2, Agg::CircularMean);

        // Wind speed: averaged over the same five minutes.
        assert_eq!(WIND_SPEED.0, MetricKey::WindSpeed);
        assert_eq!(WIND_SPEED.1.length(), Some(Duration::from_secs(300)));
        assert_eq!(WIND_SPEED.2, Agg::Mean);

        // Gust: the **peak** over ten minutes, not an average and not an instant.
        assert_eq!(WIND_GUST.0, MetricKey::WindSpeed);
        assert_eq!(WIND_GUST.1.length(), Some(Duration::from_secs(600)));
        assert_eq!(WIND_GUST.2, Agg::Max);

        // The two rain windows are different numbers: 24 hours on the clock, and since the
        // station's **local** midnight.
        assert_eq!(RAIN_24H.1.length(), Some(Duration::from_secs(24 * 3600)));
        assert_eq!(RAIN_DAY.1, mw_types::Window::Day);
        assert!(RAIN_DAY.1.needs_zone(), "the local day needs the zone");
        assert_ne!(RAIN_24H.1, RAIN_DAY.1);

        // Every row is distinct, because a duplicated declaration would make
        // `Resolved::value` ambiguous about which one an encoder meant.
        let mut seen = REQUIRED.to_vec();
        seen.sort_by_key(|(m, w, a)| (format!("{m:?}"), format!("{w}"), a.as_str()));
        seen.dedup();
        assert_eq!(seen.len(), REQUIRED.len(), "a declaration is duplicated");
    }

    /// The encoder reads exactly its table, so the resolution the uploader performs is
    /// sufficient — and nothing it reads is outside it.
    #[test]
    fn the_encoder_reads_exactly_its_own_table() {
        let station = obs_station();
        let full = all(&[
            (WIND_DIR, Some(181.0)),
            (WIND_SPEED, Some(5.0)),
            (WIND_GUST, Some(14.0)),
            (TEMPERATURE, Some(72.0)),
            (RAIN_HOUR, Some(0.02)),
            (RAIN_24H, Some(0.31)),
            (RAIN_DAY, Some(0.25)),
            (HUMIDITY, Some(55.0)),
            (BAROMETER, Some(29.92)),
            (SOLAR, Some(640.0)),
        ]);
        let obs = Observation::new(SERVICE, &station, &full);
        let info = encoder().encode(&obs).expect("encodes");
        assert_eq!(
            info.as_str(),
            "!4506.00N/12203.00W_181/005g014t072r002p031P025h55b10132L640.mw1.0"
        );
    }

    /// A station that has reported nothing still beacons its position, with every field
    /// dotted — which is the one way APRS differs from the HTTP uploaders.
    #[test]
    fn a_station_with_no_readings_still_beacons_a_position() {
        let station = obs_station();
        let empty = all(&[]);
        let obs = Observation::new(SERVICE, &station, &empty);
        assert_eq!(obs.present(), 0);
        let info = encoder().encode(&obs).expect("encodes");
        assert_eq!(info.as_str(), "!4506.00N/12203.00W_.../...g...t....mw1.0");
        assert!(!info.as_str().contains("t000"), "{info}");
    }

    /// Reading a window the uploader did not resolve is the encoder's bug, and the error
    /// says which encoder and which triple.
    #[test]
    fn a_missing_resolution_names_the_triple() {
        let station = obs_station();
        // Everything but the gust, which is the row the Go version took from SQL.
        let partial: Vec<(Declaration, Option<f64>)> = REQUIRED
            .iter()
            .filter(|d| **d != WIND_GUST)
            .map(|d| (*d, None))
            .collect();
        let values = resolved(&partial);
        let obs = Observation::new(SERVICE, &station, &values);
        let e = encoder()
            .encode(&obs)
            .expect_err("the gust was not resolved");
        assert!(
            matches!(
                e,
                EncodeError::MissingWindow {
                    metric: MetricKey::WindSpeed,
                    agg: Agg::Max,
                    ..
                }
            ),
            "{e}"
        );
    }

    /// The configured comment and symbol reach the field, and a comment that would break a
    /// frame is refused rather than truncated.
    #[test]
    fn the_configured_symbol_and_comment_reach_the_field() {
        let station = obs_station();
        let values = all(&[(TEMPERATURE, Some(41.0))]);
        let obs = Observation::new(SERVICE, &station, &values);

        let with_comment = Cwop::new(
            Position::new(45.1, -122.05).expect("a position"),
            Symbol::new('\\', '_').expect("an overlay symbol"),
            ".mw1.0",
            Some("Suncrest WA".to_string()),
        );
        let info = with_comment.encode(&obs).expect("encodes");
        assert_eq!(
            info.as_str(),
            "!4506.00N\\12203.00W_.../...g...t041.mw1.0 Suncrest WA"
        );

        let broken = Cwop::new(
            Position::new(45.1, -122.05).expect("a position"),
            Symbol::WEATHER,
            ".mw1.0",
            Some("line\none".to_string()),
        );
        let e = broken.encode(&obs).expect_err("refused");
        assert!(matches!(e, EncodeError::Field(_)), "{e}");
    }
}
