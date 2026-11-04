//! The two conversions between the domain [`Reading`] and the wire one.
//!
//! **One list, three uses.** [`reading_fields!`] is the only place a metric's three names
//! appear together — its `MetricKey` variant, its field on `mw_types::Reading`, and its
//! field on the prost-generated message — and the encoder, the decoder and the
//! completeness test are all expansions of it. A metric added to
//! `metrics/registry.toml` and not added here does not compile the test, which is the
//! property that matters: the alternative is a 212-field mapping maintained in two places,
//! and the Go implementation's 591 lines of exactly that are what this crate exists to
//! replace. `codegen-drift` gates `generated/` against the registry and
//! [`every_registry_metric_is_mapped`](tests::every_registry_metric_is_mapped) gates this
//! file against `generated/`.
//!
//! **Why a conversion exists at all**, given that the spool stores the wire bytes: because
//! `Sink::deliver` takes `&[mw_types::Envelope]` (docs/service-architecture.md §2.3), so
//! the read side of the spool produces domain envelopes. The uplink's re-encode is the
//! same two functions in the other direction, which is what keeps "the collector and the
//! satellite agree about wind chill" a property of one mapping rather than two.
//!
//! **Three places where the two representations are not the same shape**, each a
//! consequence of the registry rather than a choice here:
//!
//! 1. **`int4` columns with a `uint32` wire type** — 57 of them: counts, percentages and
//!    relay states. [`i32::cast_unsigned`] and [`u32::cast_signed`] are bit-preserving in
//!    both directions, so the round trip is exact even for the negative value no sensor
//!    should ever report. A `try_into().unwrap_or(0)` would have been a zero-fill with no
//!    literal for `ci/layers.toml`'s `one-zero-fill` rule to match.
//! 2. **`time` is required in the domain type and `optional` on the wire**, because it is a
//!    `google.protobuf.Timestamp` and a message field always carries presence. An envelope
//!    whose reading has no timestamp is rejected rather than defaulted
//!    ([`WireError::MissingTime`]): defaulting would write a row at the Unix epoch.
//! 3. **`stationName` and `stationType` are proto3 `string` without presence** (D-89
//!    deliberately excludes dimensions), so an absent name and an empty one are the same
//!    bytes. Empty decodes to `None`. Nothing reads either field off a reading — that comes
//!    from the station registry — and both are on their way out entirely (OQ-W4-2).

use chrono::{DateTime, Utc};
#[cfg(test)]
use mw_types::MetricKey;
use mw_types::Reading;

use crate::wire;

/// Why a wire reading could not become a domain reading.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    /// `readingTimestamp` was absent. The domain type has no way to say "no time", and
    /// substituting one would write a row at the Unix epoch under a real station key.
    #[error("the reading carries no readingTimestamp")]
    MissingTime,

    /// A `google.protobuf.Timestamp` that is not a point in time — seconds beyond
    /// `chrono`'s range, or `nanos` outside `0..1_000_000_000`.
    #[error("{field} is not a representable instant ({seconds}s + {nanos}ns)")]
    BadTimestamp {
        /// Which field.
        field: &'static str,
        /// The seconds it claimed.
        seconds: i64,
        /// The nanoseconds it claimed.
        nanos: i32,
    },
}

impl mw_types::Classify for WireError {
    /// Both variants are `Ignorable`: a malformed envelope from a peer is a counted
    /// rejection and not a failure of the component that received it (**D-46**), and
    /// re-reading the same bytes cannot change the answer.
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Ignorable
    }
}

/// Every metric, with its three names and the shape of its conversion.
///
/// `$emit` is a macro that takes the whole list; see the module doc. The four
/// `[[dimension]]` fields are **not** here, because they are not metrics — they have no
/// `MetricKey` variant, two of them are required on one side and optional on the other,
/// and each is handled once by hand below rather than through a fifth conversion kind
/// that exists for a single field.
macro_rules! reading_fields {
    ($emit:ident) => {
        $emit! {
            // MetricKey variant     mw_types::Reading     wire::Reading           kind
        Barometer,             barometer,               barometer,               f32;
        InTemp,                in_temp,                 inside_temperature,      f32;
        InHumidity,            in_humidity,             inside_humidity,         f32;
        OutTemp,               out_temp,                outside_temperature,     f32;
        OutHumidity,           out_humidity,            outside_humidity,        f32;
        WindSpeed,             wind_speed,              wind_speed,              f32;
        WindSpeed10,           wind_speed10,            wind_speed10,            f32;
        WindDir,               wind_dir,                wind_direction,          f32;
        WindChill,             wind_chill,              wind_chill,              f32;
        HeatIndex,             heat_index,              heat_index,              f32;
        ExtraTemp1,            extra_temp1,             extra_temp1,             f32;
        ExtraTemp2,            extra_temp2,             extra_temp2,             f32;
        ExtraTemp3,            extra_temp3,             extra_temp3,             f32;
        ExtraTemp4,            extra_temp4,             extra_temp4,             f32;
        ExtraTemp5,            extra_temp5,             extra_temp5,             f32;
        ExtraTemp6,            extra_temp6,             extra_temp6,             f32;
        ExtraTemp7,            extra_temp7,             extra_temp7,             f32;
        SoilTemp1,             soil_temp1,              soil_temp1,              f32;
        SoilTemp2,             soil_temp2,              soil_temp2,              f32;
        SoilTemp3,             soil_temp3,              soil_temp3,              f32;
        SoilTemp4,             soil_temp4,              soil_temp4,              f32;
        LeafTemp1,             leaf_temp1,              leaf_temp1,              f32;
        LeafTemp2,             leaf_temp2,              leaf_temp2,              f32;
        LeafTemp3,             leaf_temp3,              leaf_temp3,              f32;
        LeafTemp4,             leaf_temp4,              leaf_temp4,              f32;
        ExtraHumidity1,        extra_humidity1,         extra_humidity1,         f32;
        ExtraHumidity2,        extra_humidity2,         extra_humidity2,         f32;
        ExtraHumidity3,        extra_humidity3,         extra_humidity3,         f32;
        ExtraHumidity4,        extra_humidity4,         extra_humidity4,         f32;
        ExtraHumidity5,        extra_humidity5,         extra_humidity5,         f32;
        ExtraHumidity6,        extra_humidity6,         extra_humidity6,         f32;
        ExtraHumidity7,        extra_humidity7,         extra_humidity7,         f32;
        RainRate,              rain_rate,               rain_rate,               f32;
        RainIncremental,       rain_incremental,        rain_incremental,        f32;
        StormRain,             storm_rain,              storm_rain,              f32;
        StormStart,            storm_start,             storm_start,             ts;
        DayRain,               day_rain,                day_rain,                f32;
        MonthRain,             month_rain,              month_rain,              f32;
        YearRain,              year_rain,               year_rain,               f32;
        SolarWatts,            solar_watts,             solar_watts,             f32;
        PotentialSolarWatts,   potential_solar_watts,   potential_solar_watts,   f32;
        SolarJoules,           solar_joules,            solar_joules,            f32;
        Uv,                    uv,                      uv,                      f32;
        Radiation,             radiation,               radiation,               f32;
        DayEt,                 day_et,                  day_et,                  f32;
        MonthEt,               month_et,                month_et,                f32;
        YearEt,                year_et,                 year_et,                 f32;
        SoilMoisture1,         soil_moisture1,          soil_moisture1,          f32;
        SoilMoisture2,         soil_moisture2,          soil_moisture2,          f32;
        SoilMoisture3,         soil_moisture3,          soil_moisture3,          f32;
        SoilMoisture4,         soil_moisture4,          soil_moisture4,          f32;
        LeafWetness1,          leaf_wetness1,           leaf_wetness1,           f32;
        LeafWetness2,          leaf_wetness2,           leaf_wetness2,           f32;
        LeafWetness3,          leaf_wetness3,           leaf_wetness3,           f32;
        LeafWetness4,          leaf_wetness4,           leaf_wetness4,           f32;
        InsideAlarm,           inside_alarm,            inside_alarm,            u32;
        RainAlarm,             rain_alarm,              rain_alarm,              u32;
        OutsideAlarm1,         outside_alarm1,          outside_alarm1,          u32;
        OutsideAlarm2,         outside_alarm2,          outside_alarm2,          u32;
        ExtraAlarm1,           extra_alarm1,            extra_alarm1,            u32;
        ExtraAlarm2,           extra_alarm2,            extra_alarm2,            u32;
        ExtraAlarm3,           extra_alarm3,            extra_alarm3,            u32;
        ExtraAlarm4,           extra_alarm4,            extra_alarm4,            u32;
        ExtraAlarm5,           extra_alarm5,            extra_alarm5,            u32;
        ExtraAlarm6,           extra_alarm6,            extra_alarm6,            u32;
        ExtraAlarm7,           extra_alarm7,            extra_alarm7,            u32;
        ExtraAlarm8,           extra_alarm8,            extra_alarm8,            u32;
        SoilLeafAlarm1,        soil_leaf_alarm1,        soil_leaf_alarm1,        u32;
        SoilLeafAlarm2,        soil_leaf_alarm2,        soil_leaf_alarm2,        u32;
        SoilLeafAlarm3,        soil_leaf_alarm3,        soil_leaf_alarm3,        u32;
        SoilLeafAlarm4,        soil_leaf_alarm4,        soil_leaf_alarm4,        u32;
        TxBatteryStatus,       tx_battery_status,       tx_battery_status,       u32;
        ConsBatteryVoltage,    cons_battery_voltage,    cons_battery_voltage,    f32;
        StationBatteryVoltage, station_battery_voltage, station_battery_voltage, f32;
        ForecastIcon,          forecast_icon,           forecast_icon,           u32;
        ForecastRule,          forecast_rule,           forecast_rule,           u32;
        Sunrise,               sunrise,                 sunrise,                 ts;
        Sunset,                sunset,                  sunset,                  ts;
        SnowDistance,          snow_distance,           snow_distance,           f32;
        SnowDepth,             snow_depth,              snow_depth,              f32;
        ExtraFloat1,           extra_float1,            extra_float1,            f32;
        ExtraFloat2,           extra_float2,            extra_float2,            f32;
        ExtraFloat3,           extra_float3,            extra_float3,            f32;
        ExtraFloat4,           extra_float4,            extra_float4,            f32;
        ExtraFloat5,           extra_float5,            extra_float5,            f32;
        ExtraFloat6,           extra_float6,            extra_float6,            f32;
        ExtraFloat7,           extra_float7,            extra_float7,            f32;
        ExtraFloat8,           extra_float8,            extra_float8,            f32;
        ExtraFloat9,           extra_float9,            extra_float9,            f32;
        ExtraFloat10,          extra_float10,           extra_float10,           f32;
        ExtraText1,            extra_text1,             extra_text1,             text;
        ExtraText2,            extra_text2,             extra_text2,             text;
        ExtraText3,            extra_text3,             extra_text3,             text;
        ExtraText4,            extra_text4,             extra_text4,             text;
        ExtraText5,            extra_text5,             extra_text5,             text;
        ExtraText6,            extra_text6,             extra_text6,             text;
        ExtraText7,            extra_text7,             extra_text7,             text;
        ExtraText8,            extra_text8,             extra_text8,             text;
        ExtraText9,            extra_text9,             extra_text9,             text;
        ExtraText10,           extra_text10,            extra_text10,            text;
        Temp1,                 temp1,                   temp1,                   f32;
        Temp2,                 temp2,                   temp2,                   f32;
        Temp3,                 temp3,                   temp3,                   f32;
        Temp4,                 temp4,                   temp4,                   f32;
        Temp5,                 temp5,                   temp5,                   f32;
        Temp6,                 temp6,                   temp6,                   f32;
        Temp7,                 temp7,                   temp7,                   f32;
        Temp8,                 temp8,                   temp8,                   f32;
        Temp9,                 temp9,                   temp9,                   f32;
        Temp10,                temp10,                  temp10,                  f32;
        SoilTemp5,             soil_temp5,              soil_temp5,              f32;
        SoilTemp6,             soil_temp6,              soil_temp6,              f32;
        SoilTemp7,             soil_temp7,              soil_temp7,              f32;
        SoilTemp8,             soil_temp8,              soil_temp8,              f32;
        SoilTemp9,             soil_temp9,              soil_temp9,              f32;
        SoilTemp10,            soil_temp10,             soil_temp10,             f32;
        Humidity1,             humidity1,               humidity1,               f32;
        Humidity2,             humidity2,               humidity2,               f32;
        Humidity3,             humidity3,               humidity3,               f32;
        Humidity4,             humidity4,               humidity4,               f32;
        Humidity5,             humidity5,               humidity5,               f32;
        Humidity6,             humidity6,               humidity6,               f32;
        Humidity7,             humidity7,               humidity7,               f32;
        Humidity8,             humidity8,               humidity8,               f32;
        Humidity9,             humidity9,               humidity9,               f32;
        Humidity10,            humidity10,              humidity10,              f32;
        SoilHum1,              soil_hum1,               soil_hum1,               f32;
        SoilHum2,              soil_hum2,               soil_hum2,               f32;
        SoilHum3,              soil_hum3,               soil_hum3,               f32;
        SoilHum4,              soil_hum4,               soil_hum4,               f32;
        SoilHum5,              soil_hum5,               soil_hum5,               f32;
        SoilHum6,              soil_hum6,               soil_hum6,               f32;
        SoilHum7,              soil_hum7,               soil_hum7,               f32;
        SoilHum8,              soil_hum8,               soil_hum8,               f32;
        SoilHum9,              soil_hum9,               soil_hum9,               f32;
        SoilHum10,             soil_hum10,              soil_hum10,              f32;
        LeafWetness5,          leaf_wetness5,           leaf_wetness5,           f32;
        LeafWetness6,          leaf_wetness6,           leaf_wetness6,           f32;
        LeafWetness7,          leaf_wetness7,           leaf_wetness7,           f32;
        LeafWetness8,          leaf_wetness8,           leaf_wetness8,           f32;
        SoilTens1,             soil_tens1,              soil_tens1,              f32;
        SoilTens2,             soil_tens2,              soil_tens2,              f32;
        SoilTens3,             soil_tens3,              soil_tens3,              f32;
        SoilTens4,             soil_tens4,              soil_tens4,              f32;
        Gdd,                   gdd,                     gdd,                     i32;
        Etos,                  etos,                    etos,                    f32;
        Etrs,                  etrs,                    etrs,                    f32;
        Leak1,                 leak1,                   leak1,                   u32;
        Leak2,                 leak2,                   leak2,                   u32;
        Leak3,                 leak3,                   leak3,                   u32;
        Leak4,                 leak4,                   leak4,                   u32;
        BattOut,               batt_out,                batt_out,                u32;
        BattIn,                batt_in,                 batt_in,                 u32;
        Batt1,                 batt1,                   batt1,                   u32;
        Batt2,                 batt2,                   batt2,                   u32;
        Batt3,                 batt3,                   batt3,                   u32;
        Batt4,                 batt4,                   batt4,                   u32;
        Batt5,                 batt5,                   batt5,                   u32;
        Batt6,                 batt6,                   batt6,                   u32;
        Batt7,                 batt7,                   batt7,                   u32;
        Batt8,                 batt8,                   batt8,                   u32;
        Batt9,                 batt9,                   batt9,                   u32;
        Batt10,                batt10,                  batt10,                  u32;
        Batt25,                batt_25,                 batt25,                  u32;
        BattLightning,         batt_lightning,          batt_lightning,          u32;
        BatLeak1,              bat_leak1,               bat_leak1,               u32;
        BatLeak2,              bat_leak2,               bat_leak2,               u32;
        BatLeak3,              bat_leak3,               bat_leak3,               u32;
        BatLeak4,              bat_leak4,               bat_leak4,               u32;
        BattSm1,               batt_sm1,                batt_sm1,                u32;
        BattSm2,               batt_sm2,                batt_sm2,                u32;
        BattSm3,               batt_sm3,                batt_sm3,                u32;
        BattSm4,               batt_sm4,                batt_sm4,                u32;
        BattCo2,               batt_co2,                batt_co2,                u32;
        BattCellgateway,       batt_cellgateway,        batt_cell_gateway,       u32;
        BaromRelIn,            barom_rel_in,            barom_rel_in,            f32;
        BaromAbsIn,            barom_abs_in,            barom_abs_in,            f32;
        Relay1,                relay1,                  relay1,                  u32;
        Relay2,                relay2,                  relay2,                  u32;
        Relay3,                relay3,                  relay3,                  u32;
        Relay4,                relay4,                  relay4,                  u32;
        Relay5,                relay5,                  relay5,                  u32;
        Relay6,                relay6,                  relay6,                  u32;
        Relay7,                relay7,                  relay7,                  u32;
        Relay8,                relay8,                  relay8,                  u32;
        Relay9,                relay9,                  relay9,                  u32;
        Relay10,               relay10,                 relay10,                 u32;
        Pm25,                  pm25,                    pm25,                    f32;
        Pm2524h,               pm25_24h,                pm25_24h,                f32;
        Pm25In,                pm25_in,                 pm25_in,                 f32;
        Pm25In24h,             pm25_in_24h,             pm25_in24h,              f32;
        Pm25InAqin,            pm25_in_aqin,            pm25_in_aqin,            f32;
        Pm25In24hAqin,         pm25_in_24h_aqin,        pm25_in24h_aqin,         f32;
        Pm10InAqin,            pm10_in_aqin,            pm10_in_aqin,            f32;
        Pm10In24hAqin,         pm10_in_24h_aqin,        pm10_in24h_aqin,         f32;
        Co2,                   co2,                     co2,                     f32;
        Co2InAqin,             co2_in_aqin,             co2_in_aqin,             i32;
        Co2In24hAqin,          co2_in_24h_aqin,         co2_in24h_aqin,          i32;
        PmInTempAqin,          pm_in_temp_aqin,         pm_in_temp_aqin,         f32;
        PmInHumidityAqin,      pm_in_humidity_aqin,     pm_in_humidity_aqin,     i32;
        AqiPm25Aqin,           aqi_pm25_aqin,           aqi_pm25aqin,            i32;
        AqiPm2524hAqin,        aqi_pm25_24h_aqin,       aqi_pm2524h_aqin,        i32;
        AqiPm10Aqin,           aqi_pm10_aqin,           aqi_pm10aqin,            i32;
        AqiPm1024hAqin,        aqi_pm10_24h_aqin,       aqi_pm1024h_aqin,        i32;
        AqiPm25In,             aqi_pm25_in,             aqi_pm25_in,             i32;
        AqiPm25In24h,          aqi_pm25_in_24h,         aqi_pm25_in24h,          i32;
        LightningDay,          lightning_day,           lightning_day,           i32;
        LightningHour,         lightning_hour,          lightning_hour,          i32;
        LightningTime,         lightning_time,          lightning_time,          ts;
        LightningDistance,     lightning_distance,      lightning_distance,      f32;
        Tz,                    tz,                      tz,                      text;
        DateUtc,               date_utc,                date_utc,                i64;
        }
    };
}

/// Domain value -> wire value, per conversion kind.
macro_rules! encode {
    (f32, $src:expr) => {
        $src
    };
    (i32, $src:expr) => {
        $src
    };
    (i64, $src:expr) => {
        $src
    };
    // Bit-preserving, so the round trip is exact; see the module doc.
    (u32, $src:expr) => {
        $src.map(i32::cast_unsigned)
    };
    (text, $src:expr) => {
        $src.clone()
    };
    (ts, $src:expr) => {
        $src.map(timestamp)
    };
}

/// Wire value -> domain value, per conversion kind.
macro_rules! decode {
    (f32, $src:expr, $field:expr) => {
        $src
    };
    (i32, $src:expr, $field:expr) => {
        $src
    };
    (i64, $src:expr, $field:expr) => {
        $src
    };
    (u32, $src:expr, $field:expr) => {
        $src.map(u32::cast_signed)
    };
    (text, $src:expr, $field:expr) => {
        $src.clone()
    };
    (ts, $src:expr, $field:expr) => {
        $src.as_ref().map(|t| instant($field, t)).transpose()?
    };
}

/// The encoder, the decoder and the key roster, from the one list.
macro_rules! conversions {
    ($($variant:ident, $dom:ident, $wire:ident, $kind:ident;)*) => {
        fn encode_metrics(from: &Reading, into: &mut wire::Reading) {
            $( into.$wire = encode!($kind, from.$dom); )*
        }

        fn decode_metrics(from: &wire::Reading, into: &mut Reading) -> Result<(), WireError> {
            $( into.$dom = decode!($kind, from.$wire, stringify!($wire)); )*
            Ok(())
        }

        /// Every `MetricKey` this file maps. The test below asserts it is all of them.
        #[cfg(test)]
        const MAPPED: &[MetricKey] = &[$(MetricKey::$variant,)*];
    };
}

reading_fields!(conversions);

/// `DateTime<Utc>` -> `google.protobuf.Timestamp`.
///
/// Total: `chrono`'s range is a subset of the 64-bit second range the message allows, and
/// `timestamp_subsec_nanos` is below 2e9 even on a leap second, which fits an `i32`.
fn timestamp(at: DateTime<Utc>) -> wire::Timestamp {
    wire::Timestamp {
        seconds: at.timestamp(),
        nanos: i32::try_from(at.timestamp_subsec_nanos()).unwrap_or(i32::MAX),
    }
}

/// `google.protobuf.Timestamp` -> `DateTime<Utc>`, which is partial: a peer can send any
/// pair of integers.
fn instant(field: &'static str, t: &wire::Timestamp) -> Result<DateTime<Utc>, WireError> {
    let nanos = u32::try_from(t.nanos).ok();
    nanos
        .and_then(|n| DateTime::from_timestamp(t.seconds, n))
        .ok_or(WireError::BadTimestamp {
            field,
            seconds: t.seconds,
            nanos: t.nanos,
        })
}

/// The two halves of the presence-less dimension mapping, side by side so the asymmetry is
/// one decision in one place rather than four call sites that happen to agree.
///
/// `stationName` and `stationType` are proto3 `string` without presence — D-89 excludes
/// dimensions deliberately — so an absent name and an empty one are the same bytes and no
/// encoding can tell them apart. Empty decodes to `None`, which is the direction that keeps
/// a `NULL` column a `NULL`.
mod presence_less {
    /// `Option<String>` -> the bytes. `None` is the empty string, because there is nothing
    /// else for it to be.
    pub fn encode(s: &Option<String>) -> String {
        match s {
            Some(text) => text.clone(),
            None => String::new(),
        }
    }

    /// The bytes -> `Option<String>`. Empty is absent.
    pub fn decode(s: &str) -> Option<String> {
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }
}

impl From<&Reading> for wire::Reading {
    /// Total, because the domain type is the narrower of the two.
    fn from(r: &Reading) -> Self {
        let mut out = wire::Reading {
            reading_timestamp: Some(timestamp(r.time)),
            station_id: r.station_id.clone(),
            station_name: presence_less::encode(&r.station_name),
            station_type: presence_less::encode(&r.station_type),
            ..wire::Reading::default()
        };
        encode_metrics(r, &mut out);
        out
    }
}

impl TryFrom<&wire::Reading> for Reading {
    type Error = WireError;

    fn try_from(w: &wire::Reading) -> Result<Self, Self::Error> {
        let time = w
            .reading_timestamp
            .as_ref()
            .ok_or(WireError::MissingTime)
            .and_then(|t| instant("readingTimestamp", t))?;
        let mut out = Reading {
            time,
            station_id: w.station_id.clone(),
            station_name: presence_less::decode(&w.station_name),
            station_type: presence_less::decode(&w.station_type),
            ..Reading::default()
        };
        decode_metrics(w, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::METRICS;
    use prost::Message as _;

    /// The guard that makes the hand-written list safe: a `[[metric]]` added to
    /// `metrics/registry.toml` and not added to `reading_fields!` fails here, naming it.
    ///
    /// Without this, the failure mode is silent and permanent in the way **D-89**'s own
    /// comment describes — a new metric would be dropped by every hop and every restart,
    /// and nothing would say so.
    #[test]
    fn every_registry_metric_is_mapped() {
        let mapped: std::collections::BTreeSet<_> =
            MAPPED.iter().map(|k| format!("{k:?}")).collect();
        let missing: Vec<_> = METRICS
            .iter()
            .filter(|m| !mapped.contains(&format!("{:?}", m.key)))
            .map(|m| m.name)
            .collect();
        assert!(
            missing.is_empty(),
            "these registry metrics have no line in reading_fields!: {missing:?}"
        );
        assert_eq!(
            MAPPED.len(),
            METRICS.len(),
            "reading_fields! maps {} metrics and the registry has {}",
            MAPPED.len(),
            METRICS.len()
        );
    }

    #[test]
    fn no_metric_is_mapped_twice() {
        let mut seen: Vec<String> = MAPPED.iter().map(|k| format!("{k:?}")).collect();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "a metric appears twice in reading_fields!"
        );
    }

    fn populated() -> Reading {
        Reading {
            time: DateTime::from_timestamp(1_762_000_000, 123_456_789).expect("a real instant"),
            station_id: "backyard".into(),
            station_name: Some("Back Yard".into()),
            station_type: Some("davis".into()),
            // One of each conversion kind, so the round trip exercises all six.
            barometer: Some(29.92),                                  // f32
            out_humidity: Some(41.5),                                // f32
            lightning_day: Some(7),                                  // i32
            date_utc: Some(1_762_000_000_123),                       // i64
            inside_alarm: Some(3),                                   // u32
            tz: Some("America/Denver".into()),                       // text
            storm_start: DateTime::from_timestamp(1_761_000_000, 0), // ts
            ..Reading::default()
        }
    }

    #[test]
    fn a_populated_reading_round_trips_exactly() {
        let before = populated();
        let encoded = wire::Reading::from(&before);
        let after = Reading::try_from(&encoded).expect("our own encoding decodes");
        assert_eq!(before, after);
    }

    /// **D-89**, end to end and through the bytes rather than through the structs: the
    /// whole reason the generator emits `optional` is that an unreported metric must not
    /// come back as `0.0`. This is the assertion the spool depends on, because the record
    /// payload is this message (W7 D-11).
    #[test]
    fn an_unreported_metric_encodes_absent_and_decodes_to_none() {
        let mut r = populated();
        r.out_temp = None;
        r.out_humidity = None;
        let bytes = wire::Reading::from(&r).encode_to_vec();
        let decoded = wire::Reading::decode(bytes.as_slice()).expect("our own bytes");
        assert_eq!(decoded.outside_temperature, None, "absent on the wire");
        let back = Reading::try_from(&decoded).expect("decodes");
        assert_eq!(
            back.out_temp, None,
            "and NULL rather than 0.0 in the column"
        );
        assert_eq!(back.out_humidity, None);
        // The one that is present is still present, so the test is not passing because
        // everything decoded to None.
        assert_eq!(back.barometer, Some(29.92));
    }

    /// A metric that really is zero is distinguishable from one that was not reported,
    /// which is the other half of the same claim.
    #[test]
    fn a_reported_zero_is_not_an_absence() {
        let mut r = populated();
        r.out_temp = Some(0.0);
        let bytes = wire::Reading::from(&r).encode_to_vec();
        let decoded = wire::Reading::decode(bytes.as_slice()).expect("our own bytes");
        assert_eq!(
            decoded.outside_temperature,
            Some(0.0),
            "a reported zero is on the wire, which is what `optional` buys"
        );
        assert_eq!(
            Reading::try_from(&decoded).expect("decodes").out_temp,
            Some(0.0)
        );
    }

    /// The 57 `int4`/`uint32` fields, at the two values where a cast could go wrong.
    #[test]
    fn the_signed_to_unsigned_fields_round_trip_at_the_edges() {
        for value in [i32::MIN, -1, 0, 1, i32::MAX] {
            let mut r = populated();
            r.inside_alarm = Some(value);
            let back = Reading::try_from(&wire::Reading::from(&r)).expect("decodes");
            assert_eq!(back.inside_alarm, Some(value), "value {value}");
        }
    }

    #[test]
    fn a_reading_with_no_timestamp_is_rejected_rather_than_epoched() {
        let mut w = wire::Reading::from(&populated());
        w.reading_timestamp = None;
        assert_eq!(Reading::try_from(&w), Err(WireError::MissingTime));
    }

    #[test]
    fn an_unrepresentable_timestamp_names_its_field() {
        let mut w = wire::Reading::from(&populated());
        w.storm_start = Some(wire::Timestamp {
            seconds: i64::MIN,
            nanos: -1,
        });
        let err = Reading::try_from(&w).expect_err("not an instant");
        assert!(
            err.to_string().contains("stormStart") || err.to_string().contains("storm_start"),
            "{err}"
        );
    }

    /// The two presence-less dimensions: empty is absent, because the bytes cannot say
    /// which it was.
    #[test]
    fn an_empty_station_name_decodes_as_absent() {
        let mut r = populated();
        r.station_name = None;
        r.station_type = None;
        let w = wire::Reading::from(&r);
        assert_eq!(w.station_name, "");
        let back = Reading::try_from(&w).expect("decodes");
        assert_eq!(back.station_name, None);
        assert_eq!(back.station_type, None);
    }
}
