//! The WeatherLink Live local API's JSON, as types.
//!
//! One set of types for both transports, which is W2a §4a.5's prediction confirmed: the
//! UDP broadcast datagram and the body of `GET /v1/current_conditions` are **the same
//! document** — a `did`, a `ts` and an array of `conditions` records — differing only in
//! which fields each record populates. The broadcast carries the high-rate wind and rain
//! fields of one ISS record; the HTTP poll carries every record the unit has. So there is
//! one codec rather than two, and the Go tree has the same property for the same reason:
//! a single 427-line `data_transformer.go` serves both paths.
//!
//! **Every numeric field is `Option<f64>`, including the ones the device documents as
//! integers.** Two reasons, and neither is laziness. A JSON number that is `59` where the
//! previous reading was `59.4` must not become a decode failure, and `hum`, `moist_soil_*`
//! and `solar_rad` are each documented as one of the two and observed as the other. And
//! `null` is the normal value for a sensor the unit has not heard from, which is the fact
//! the whole `Option<f32>` chain exists to carry (**D-3**, **D-26**): a sensor that did not
//! report is not a sensor that reported zero.
//!
//! Unknown fields are **ignored**, not rejected. A firmware that adds a field must not stop
//! a station working, and `serde`'s default is already that; it is stated here because the
//! opposite (`deny_unknown_fields`) is one attribute away and would look like rigour.

use serde::Deserialize;

/// A decoded `current_conditions` document, from either transport.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Document {
    /// The unit's own serial — `001D0A700002`. The identity a broadcast frame claims
    /// (W2a §4a.4's `FromPayload`), and the only identity either transport carries.
    #[serde(default)]
    pub(crate) did: Option<String>,
    /// The device's measurement instant, Unix seconds.
    #[serde(default)]
    pub(crate) ts: Option<i64>,
    /// The sensor records. Absent in an error response, which is why it is not required.
    #[serde(default)]
    pub(crate) conditions: Vec<Condition>,
}

/// The unit's own error object.
///
/// A sibling of `data` rather than a status code: the unit answers `200` with a populated
/// `error` object, so a decoder that looked only at the transport would read a device error
/// as an empty reading. The two envelopes that carry it are declared at their decode sites
/// (`super::real_time` and `super::WllConditions::document`), because each also has to
/// accept the *unwrapped* shape the broadcast datagram uses and a generic `Response<T>`
/// cannot express both without an untagged enum that mis-parses one of them.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiError {
    #[serde(default)]
    pub(crate) code: i64,
    #[serde(default)]
    pub(crate) message: String,
}

/// `GET /v1/real_time`'s payload: which UDP port the unit will broadcast on, and for how
/// long before it stops.
#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct RealTime {
    #[serde(default)]
    pub(crate) broadcast_port: Option<u16>,
    #[serde(default)]
    pub(crate) duration: Option<i64>,
}

/// One sensor record.
///
/// The field set is the subset the mapping in `super::map` consumes, plus `rain_size`,
/// which is not a measurement but the unit the two rain measurements are expressed in.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Condition {
    /// 1 = ISS, 2 = leaf/soil, 3 = barometer, 4 = the unit's own temp/humidity.
    #[serde(default)]
    pub(crate) data_structure_type: Option<i64>,
    /// The transmitter id. Absent on the unit's own internal records (3 and 4), which is
    /// what makes "match by transmitter" and "match by record type" two different lookups.
    #[serde(default)]
    pub(crate) txid: Option<i64>,

    // --- ISS (type 1) ---
    #[serde(default)]
    pub(crate) temp: Option<f64>,
    #[serde(default)]
    pub(crate) hum: Option<f64>,
    #[serde(default)]
    pub(crate) dew_point: Option<f64>,
    #[serde(default)]
    pub(crate) wet_bulb: Option<f64>,
    #[serde(default)]
    pub(crate) heat_index: Option<f64>,
    #[serde(default)]
    pub(crate) wind_chill: Option<f64>,
    #[serde(default)]
    pub(crate) thw_index: Option<f64>,
    #[serde(default)]
    pub(crate) thsw_index: Option<f64>,
    #[serde(default)]
    pub(crate) wind_speed_last: Option<f64>,
    #[serde(default)]
    pub(crate) wind_dir_last: Option<f64>,
    #[serde(default)]
    pub(crate) wind_speed_avg_last_10_min: Option<f64>,
    /// The rain collector's increment: 1 = 0.01 in, 2 = 0.2 mm, 3 = 0.1 mm, 4 = 0.001 in.
    /// **Not** a measurement, and the reason the two rain fields below cannot be read
    /// without it.
    #[serde(default)]
    pub(crate) rain_size: Option<i64>,
    #[serde(default)]
    pub(crate) rain_rate_last: Option<f64>,
    #[serde(default)]
    pub(crate) rainfall_daily: Option<f64>,
    #[serde(default)]
    pub(crate) solar_rad: Option<f64>,
    #[serde(default)]
    pub(crate) uv_index: Option<f64>,
    #[serde(default)]
    pub(crate) trans_battery_flag: Option<i64>,

    // --- barometer (type 3) ---
    #[serde(default)]
    pub(crate) bar_absolute: Option<f64>,
    #[serde(default)]
    pub(crate) bar_sea_level: Option<f64>,

    // --- the unit's own temp/humidity (type 4) ---
    #[serde(default)]
    pub(crate) temp_in: Option<f64>,
    #[serde(default)]
    pub(crate) hum_in: Option<f64>,
    #[serde(default)]
    pub(crate) dew_point_in: Option<f64>,
    #[serde(default)]
    pub(crate) heat_index_in: Option<f64>,

    // --- leaf/soil (type 2) ---
    #[serde(default)]
    pub(crate) temp_1: Option<f64>,
    #[serde(default)]
    pub(crate) temp_2: Option<f64>,
    #[serde(default)]
    pub(crate) temp_3: Option<f64>,
    #[serde(default)]
    pub(crate) temp_4: Option<f64>,
    #[serde(default)]
    pub(crate) moist_soil_1: Option<f64>,
    #[serde(default)]
    pub(crate) moist_soil_2: Option<f64>,
    #[serde(default)]
    pub(crate) moist_soil_3: Option<f64>,
    #[serde(default)]
    pub(crate) moist_soil_4: Option<f64>,
    #[serde(default)]
    pub(crate) wet_leaf_1: Option<f64>,
    #[serde(default)]
    pub(crate) wet_leaf_2: Option<f64>,
}

/// The record types the unit emits, named rather than compared as integers.
pub(crate) mod structure {
    /// Integrated Sensor Suite: outdoor temperature, humidity, wind, rain, solar, UV.
    pub(crate) const ISS: i64 = 1;
    /// Leaf wetness and soil moisture/temperature station.
    pub(crate) const LEAF_SOIL: i64 = 2;
    /// The unit's internal barometer.
    pub(crate) const BAROMETER: i64 = 3;
    /// The unit's internal temperature and humidity.
    pub(crate) const TEMP_HUM: i64 = 4;
}

impl Condition {
    /// Which record type this is, defaulting to [`structure::ISS`].
    ///
    /// The broadcast datagram's records carry `data_structure_type` in every firmware
    /// observed; defaulting rather than refusing keeps a unit that omits it working, and
    /// ISS is the only type the broadcast contains.
    pub(crate) fn structure(&self) -> i64 {
        self.data_structure_type.unwrap_or(structure::ISS)
    }
}
