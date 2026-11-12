//! The Weather Underground table, whose field names encode the windows.
//!
//! > *Weather Underground encodes the windows in its field names — `windspdmph_avg2m`,
//! > `windgustmph_10m`.*
//! > — `docs/component-model.md` §4b.1
//!
//! That is the whole argument for this item in one line. The service's own parameter names
//! say which window each number is over, the Go uploader sends an instantaneous sample
//! under `windspeedmph` and nothing under either windowed name, and under a declaration
//! table "the names stop being a thing you have to know and become a thing the code says"
//! (§4b.2).
//!
//! # The table
//!
//! | Metric | Window | Agg | Field |
//! |---|---|---|---|
//! | `WindDir` | 1 min | `CircularMean` | `winddir` |
//! | `WindSpeed` | 1 min | `Mean` | `windspeedmph` |
//! | `WindDir` | 2 min | `CircularMean` | `winddir_avg2m` |
//! | `WindSpeed` | 2 min | `Mean` | `windspdmph_avg2m` |
//! | `WindSpeed` | 10 min | `Max` | `windgustmph_10m`, and `windgustmph` |
//! | `OutTemp` | 1 min | `Mean` | `tempf` |
//! | `OutHumidity` | 1 min | `Mean` | `humidity` |
//! | `Barometer` | 1 min | `Mean` | `baromin` |
//! | `RainIncremental` | 1 h | `Sum` | `rainin` |
//! | `RainIncremental` | local day | `Sum` | `dailyrainin` |
//! | `SolarWatts` | 1 min | `Mean` | `solarradiation` |
//!
//! **One declaration, two field names** for the gust: the service documents `windgustmph`
//! as the current gust — over a software-chosen period — and `windgustmph_10m` as the fixed
//! ten-minute peak. This program has one gust, the ten-minute peak `window:rollup`
//! maintains, so sending it under both is inside the documented contract; sending an
//! instantaneous sample as `windgustmph` would be inventing a measurement.
//!
//! The Go Weather Underground uploader sends **no gust at all** — its seven parameters are
//! `winddir`, `windspeedmph`, `humidity`, `tempf`, `dailyrainin`, `baromin` and
//! `softwaretype` (`wunderground/controller.go:110-133`). The `MaxWindSpeed` column is the
//! *PWS* uploader's approximation (`pwsweather/controller.go:119`), which
//! `crate::encoder::pws` describes.

use mw_component::Declaration;
use mw_types::{Agg, MetricKey, Window};

use super::account::Account;
use super::{
    integer, one_decimal, two_decimals, Observation, OutputEncoder, CURRENT, ONE_HOUR, TEN_MINUTES,
    TWO_MINUTES,
};
use crate::error::EncodeError;
use crate::form::Form;

/// This encoder's name, for a metric label and a log field.
pub const SERVICE: &str = "wunderground";

/// `winddir`.
pub const WIND_DIR: Declaration = (MetricKey::WindDir, CURRENT, Agg::CircularMean);
/// `windspeedmph`.
pub const WIND_SPEED: Declaration = (MetricKey::WindSpeed, CURRENT, Agg::Mean);
/// `winddir_avg2m`.
pub const WIND_DIR_2M: Declaration = (MetricKey::WindDir, TWO_MINUTES, Agg::CircularMean);
/// `windspdmph_avg2m`.
pub const WIND_SPEED_2M: Declaration = (MetricKey::WindSpeed, TWO_MINUTES, Agg::Mean);
/// `windgustmph_10m`, and `windgustmph`.
pub const WIND_GUST_10M: Declaration = (MetricKey::WindSpeed, TEN_MINUTES, Agg::Max);
/// `tempf`.
pub const TEMPERATURE: Declaration = (MetricKey::OutTemp, CURRENT, Agg::Mean);
/// `humidity`.
pub const HUMIDITY: Declaration = (MetricKey::OutHumidity, CURRENT, Agg::Mean);
/// `baromin`.
pub const BAROMETER: Declaration = (MetricKey::Barometer, CURRENT, Agg::Mean);
/// `rainin`.
pub const RAIN_HOUR: Declaration = (MetricKey::RainIncremental, ONE_HOUR, Agg::Sum);
/// `dailyrainin`, over the station's **local** day.
pub const RAIN_DAY: Declaration = (MetricKey::RainIncremental, Window::Day, Agg::Sum);
/// `solarradiation`.
pub const SOLAR: Declaration = (MetricKey::SolarWatts, CURRENT, Agg::Mean);

/// **The table**, in the order the query writes it.
pub const REQUIRED: &[Declaration] = &[
    WIND_DIR,
    WIND_SPEED,
    WIND_DIR_2M,
    WIND_SPEED_2M,
    WIND_GUST_10M,
    TEMPERATURE,
    HUMIDITY,
    BAROMETER,
    RAIN_HOUR,
    RAIN_DAY,
    SOLAR,
];

/// The Weather Underground encoder.
#[derive(Debug, Clone)]
pub struct WeatherUnderground {
    account: Account,
}

impl WeatherUnderground {
    /// Build the encoder from the pairing's station identifier and the account's
    /// credential.
    pub fn new(account: Account) -> Self {
        Self { account }
    }
}

impl OutputEncoder for WeatherUnderground {
    type Payload = Form;

    fn service(&self) -> &'static str {
        SERVICE
    }

    fn required(&self) -> &[Declaration] {
        REQUIRED
    }

    fn encode(&self, observation: &Observation<'_>) -> Result<Form, EncodeError> {
        let mut form = Form::new();
        self.account.preamble(&mut form, observation.as_of());

        if let Some(v) = observation.value(WIND_DIR)? {
            form.measurement("winddir", &integer(v));
        }
        if let Some(v) = observation.value(WIND_SPEED)? {
            form.measurement("windspeedmph", &one_decimal(v));
        }
        if let Some(v) = observation.value(WIND_DIR_2M)? {
            form.measurement("winddir_avg2m", &integer(v));
        }
        if let Some(v) = observation.value(WIND_SPEED_2M)? {
            form.measurement("windspdmph_avg2m", &one_decimal(v));
        }
        if let Some(v) = observation.value(WIND_GUST_10M)? {
            form.measurement("windgustmph_10m", &one_decimal(v));
            form.measurement("windgustmph", &one_decimal(v));
        }
        if let Some(v) = observation.value(TEMPERATURE)? {
            form.measurement("tempf", &one_decimal(v));
        }
        if let Some(v) = observation.value(HUMIDITY)? {
            form.measurement("humidity", &integer(v));
        }
        if let Some(v) = observation.value(BAROMETER)? {
            form.measurement("baromin", &two_decimals(v));
        }
        if let Some(v) = observation.value(RAIN_HOUR)? {
            form.measurement("rainin", &two_decimals(v));
        }
        if let Some(v) = observation.value(RAIN_DAY)? {
            form.measurement("dailyrainin", &two_decimals(v));
        }
        if let Some(v) = observation.value(SOLAR)? {
            form.measurement("solarradiation", &two_decimals(v));
        }

        if form.measurements() == 0 {
            return Err(EncodeError::NothingToSend {
                service: SERVICE,
                station: observation.station().clone(),
                declared: observation.declared(),
            });
        }
        self.account.software(&mut form);
        Ok(form)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::tests_support::{all, encoder_account, obs_station, resolved};
    use mw_types::Classify;

    fn encoder() -> WeatherUnderground {
        WeatherUnderground::new(encoder_account())
    }

    /// The field names say the windows, which is this encoder's whole reason for existing.
    #[test]
    fn the_windowed_field_names_match_the_windows_declared_for_them() {
        use std::time::Duration;
        assert_eq!(REQUIRED.len(), 11);
        assert_eq!(WIND_SPEED_2M.1.length(), Some(Duration::from_secs(120)));
        assert_eq!(WIND_DIR_2M.1.length(), Some(Duration::from_secs(120)));
        assert_eq!(WIND_GUST_10M.1.length(), Some(Duration::from_secs(600)));
        assert_eq!(WIND_GUST_10M.2, Agg::Max);
        // A direction is averaged circularly wherever it is averaged.
        assert_eq!(WIND_DIR.2, Agg::CircularMean);
        assert_eq!(WIND_DIR_2M.2, Agg::CircularMean);
        // `dailyrainin` is the station's local day, which is what makes the configured
        // timezone load-bearing rather than decorative.
        assert_eq!(RAIN_DAY.1, Window::Day);
    }

    #[test]
    fn a_full_upload_is_the_services_own_parameter_vocabulary() {
        let station = obs_station();
        let values = all(
            REQUIRED,
            &[
                (WIND_DIR, Some(181.4)),
                (WIND_SPEED, Some(5.26)),
                (WIND_DIR_2M, Some(178.0)),
                (WIND_SPEED_2M, Some(4.78)),
                (WIND_GUST_10M, Some(14.0)),
                (TEMPERATURE, Some(72.34)),
                (HUMIDITY, Some(55.6)),
                (BAROMETER, Some(29.9199)),
                (RAIN_HOUR, Some(0.02)),
                (RAIN_DAY, Some(0.25)),
                (SOLAR, Some(640.0)),
            ],
        );
        let obs = Observation::new(SERVICE, &station, &values);
        let form = encoder().encode(&obs).expect("encodes");
        assert_eq!(
            form.as_str(),
            "ID=KWABACKYARD&PASSWORD=hunter2&dateutc=2026-11-11%2005%3A00%3A00\
             &action=updateraw&winddir=181&windspeedmph=5.3&winddir_avg2m=178\
             &windspdmph_avg2m=4.8&windgustmph_10m=14.0&windgustmph=14.0&tempf=72.3\
             &humidity=56&baromin=29.92&rainin=0.02&dailyrainin=0.25\
             &solarradiation=640.00&softwaretype=modernweather-1.0"
        );
        assert_eq!(form.measurements(), 12, "eleven windows, twelve parameters");
    }

    /// An absent window is an absent parameter, never a zero. A service that is sent
    /// `tempf=0` records 0 °F.
    #[test]
    fn an_absent_window_is_an_absent_parameter() {
        let station = obs_station();
        let values = all(REQUIRED, &[(TEMPERATURE, Some(41.0))]);
        let obs = Observation::new(SERVICE, &station, &values);
        let form = encoder().encode(&obs).expect("encodes");
        assert_eq!(
            form.as_str(),
            "ID=KWABACKYARD&PASSWORD=hunter2&dateutc=2026-11-11%2005%3A00%3A00\
             &action=updateraw&tempf=41.0&softwaretype=modernweather-1.0"
        );
        for absent in ["winddir", "windspeedmph", "baromin", "dailyrainin"] {
            assert!(!form.as_str().contains(absent), "{}", form.as_str());
        }
    }

    /// A station with nothing to report does not get an upload, which is the difference
    /// from APRS: an HTTP upload with no measurements tells the service the station is
    /// alive and reporting nothing.
    #[test]
    fn a_station_with_no_values_is_not_uploaded() {
        let station = obs_station();
        let values = all(REQUIRED, &[]);
        let obs = Observation::new(SERVICE, &station, &values);
        let e = encoder().encode(&obs).expect_err("nothing to send");
        assert!(
            matches!(e, EncodeError::NothingToSend { declared: 11, .. }),
            "{e}"
        );
        assert_eq!(e.class(), mw_types::Class::Ignorable);
    }

    /// Reading a window the uploader did not resolve names the triple.
    #[test]
    fn a_missing_resolution_is_the_encoders_bug() {
        let station = obs_station();
        let partial: Vec<(Declaration, Option<f64>)> = REQUIRED
            .iter()
            .filter(|d| **d != SOLAR)
            .map(|d| (*d, Some(1.0)))
            .collect();
        let values = resolved(&partial);
        let obs = Observation::new(SERVICE, &station, &values);
        let e = encoder().encode(&obs).expect_err("solar was not resolved");
        assert!(
            matches!(
                e,
                EncodeError::MissingWindow {
                    metric: MetricKey::SolarWatts,
                    ..
                }
            ),
            "{e}"
        );
    }
}
