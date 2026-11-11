//! The PWS Weather table.
//!
//! The service with the same protocol as Weather Underground and a different field set:
//! no `_avg2m` names, and rainfall totals for the month and the year, which `window:rollup`
//! maintains in the station's local zone (**MAINTAINED** in `mw_pipeline::rollup::state`).
//!
//! # The table
//!
//! | Metric | Window | Agg | Field |
//! |---|---|---|---|
//! | `WindDir` | 1 min | `CircularMean` | `winddir` |
//! | `WindSpeed` | 1 min | `Mean` | `windspeedmph` |
//! | `WindSpeed` | 10 min | `Max` | `windgustmph` |
//! | `OutTemp` | 1 min | `Mean` | `tempf` |
//! | `OutHumidity` | 1 min | `Mean` | `humidity` |
//! | `Barometer` | 1 min | `Mean` | `baromin` |
//! | `RainIncremental` | 1 h | `Sum` | `rainin` |
//! | `RainIncremental` | local day | `Sum` | `dailyrainin` |
//! | `RainIncremental` | local month | `Sum` | `monthrainin` |
//! | `RainIncremental` | local year | `Sum` | `yearrainin` |
//! | `SolarWatts` | 1 min | `Mean` | `solarradiation` |
//!
//! The gust is a **declared ten-minute peak**. The Go uploader sends
//! `r.MaxWindSpeed` — a column on the arbitrary `weather_1m` bucket its "latest reading"
//! query returned, so it is the maximum over *some* minute in the last two rather than the
//! peak over a window the service can interpret.
//!
//! The month and year rows are new. The Go uploader sends neither, and both are values
//! **R2** maintains anyway for the frozen `/latest` body — so declaring them costs one line
//! each and stops the service's own month-to-date page disagreeing with ours.

use mw_component::Declaration;
use mw_types::{Agg, MetricKey, Window};

use super::account::Account;
use super::{
    integer, one_decimal, two_decimals, Observation, OutputEncoder, CURRENT, ONE_HOUR, TEN_MINUTES,
};
use crate::error::EncodeError;
use crate::form::Form;

/// This encoder's name, for a metric label and a log field.
pub const SERVICE: &str = "pwsweather";

/// `winddir`.
pub const WIND_DIR: Declaration = (MetricKey::WindDir, CURRENT, Agg::CircularMean);
/// `windspeedmph`.
pub const WIND_SPEED: Declaration = (MetricKey::WindSpeed, CURRENT, Agg::Mean);
/// `windgustmph`, the ten-minute peak.
pub const WIND_GUST: Declaration = (MetricKey::WindSpeed, TEN_MINUTES, Agg::Max);
/// `tempf`.
pub const TEMPERATURE: Declaration = (MetricKey::OutTemp, CURRENT, Agg::Mean);
/// `humidity`.
pub const HUMIDITY: Declaration = (MetricKey::OutHumidity, CURRENT, Agg::Mean);
/// `baromin`.
pub const BAROMETER: Declaration = (MetricKey::Barometer, CURRENT, Agg::Mean);
/// `rainin`.
pub const RAIN_HOUR: Declaration = (MetricKey::RainIncremental, ONE_HOUR, Agg::Sum);
/// `dailyrainin`, over the station's local day.
pub const RAIN_DAY: Declaration = (MetricKey::RainIncremental, Window::Day, Agg::Sum);
/// `monthrainin`, over the station's local month.
pub const RAIN_MONTH: Declaration = (MetricKey::RainIncremental, Window::Month, Agg::Sum);
/// `yearrainin`, over the station's local year.
pub const RAIN_YEAR: Declaration = (MetricKey::RainIncremental, Window::Year, Agg::Sum);
/// `solarradiation`.
pub const SOLAR: Declaration = (MetricKey::SolarWatts, CURRENT, Agg::Mean);

/// **The table**, in the order the query writes it.
pub const REQUIRED: &[Declaration] = &[
    WIND_DIR,
    WIND_SPEED,
    WIND_GUST,
    TEMPERATURE,
    HUMIDITY,
    BAROMETER,
    RAIN_HOUR,
    RAIN_DAY,
    RAIN_MONTH,
    RAIN_YEAR,
    SOLAR,
];

/// The PWS Weather encoder.
#[derive(Debug, Clone)]
pub struct PwsWeather {
    account: Account,
}

impl PwsWeather {
    /// Build the encoder from the pairing's station identifier and the account's
    /// credential.
    pub fn new(account: Account) -> Self {
        Self { account }
    }
}

impl OutputEncoder for PwsWeather {
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
        if let Some(v) = observation.value(WIND_GUST)? {
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
        if let Some(v) = observation.value(RAIN_MONTH)? {
            form.measurement("monthrainin", &two_decimals(v));
        }
        if let Some(v) = observation.value(RAIN_YEAR)? {
            form.measurement("yearrainin", &two_decimals(v));
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
    use crate::encoder::tests_support::{all, encoder_account, obs_station};

    fn encoder() -> PwsWeather {
        PwsWeather::new(encoder_account())
    }

    /// The three calendar windows are the station's own, which is what
    /// `rain:rollover:<station>` publishes the boundaries for.
    #[test]
    fn the_calendar_rain_windows_are_the_stations_local_ones() {
        assert_eq!(REQUIRED.len(), 11);
        for d in [RAIN_DAY, RAIN_MONTH, RAIN_YEAR] {
            assert!(d.1.needs_zone(), "{} needs the station's zone", d.1);
            assert_eq!(d.2, Agg::Sum, "a rainfall window is a total");
            assert_eq!(d.0, MetricKey::RainIncremental);
        }
        // The gust is a declared ten-minute peak rather than a column.
        assert_eq!(WIND_GUST.2, Agg::Max);
        assert_eq!(
            WIND_GUST.1.length(),
            Some(std::time::Duration::from_secs(600))
        );
    }

    #[test]
    fn a_full_upload_is_the_services_own_parameter_vocabulary() {
        let station = obs_station();
        let values = all(
            REQUIRED,
            &[
                (WIND_DIR, Some(181.4)),
                (WIND_SPEED, Some(5.26)),
                (WIND_GUST, Some(14.0)),
                (TEMPERATURE, Some(72.34)),
                (HUMIDITY, Some(55.6)),
                (BAROMETER, Some(29.9199)),
                (RAIN_HOUR, Some(0.02)),
                (RAIN_DAY, Some(0.25)),
                (RAIN_MONTH, Some(1.75)),
                (RAIN_YEAR, Some(28.5)),
                (SOLAR, Some(640.0)),
            ],
        );
        let obs = Observation::new(SERVICE, &station, &values);
        let form = encoder().encode(&obs).expect("encodes");
        assert_eq!(
            form.as_str(),
            "ID=KWABACKYARD&PASSWORD=hunter2&dateutc=2026-11-11%2005%3A00%3A00\
             &action=updateraw&winddir=181&windspeedmph=5.3&windgustmph=14.0&tempf=72.3\
             &humidity=56&baromin=29.92&rainin=0.02&dailyrainin=0.25&monthrainin=1.75\
             &yearrainin=28.50&solarradiation=640.00&softwaretype=modernweather-1.0"
        );
        assert_eq!(form.measurements(), 11);
    }

    /// The two services are not the same encoder with a different URL: their field sets
    /// differ, which is why §9.3's shared shape is the component body rather than the
    /// parameter list.
    #[test]
    fn the_two_http_tables_are_not_the_same_table() {
        use crate::encoder::wu;
        assert_ne!(REQUIRED, wu::REQUIRED);
        assert!(!REQUIRED.contains(&wu::WIND_SPEED_2M));
        assert!(!wu::REQUIRED.contains(&RAIN_MONTH));
    }

    #[test]
    fn a_station_with_no_values_is_not_uploaded() {
        let station = obs_station();
        let values = all(REQUIRED, &[]);
        let obs = Observation::new(SERVICE, &station, &values);
        let e = encoder().encode(&obs).expect_err("nothing to send");
        assert!(matches!(e, EncodeError::NothingToSend { .. }), "{e}");
    }
}
