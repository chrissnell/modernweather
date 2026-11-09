//! The `/latest` body, rendered once per reading version (**D-33**).
//!
//! [`Renderer`] is the `mw-api-legacy` half of [`mw_component::SnapshotRender`]: the
//! snapshot components live in `mw-pipeline`, which is a lower layer and may not name this
//! crate, so the trait is declared in `mw-component` and implemented here -- with the
//! binary as the one place the two meet (§1.3 rule 1).
//!
//! # What the renderer is handed, and what that buys
//!
//! A [`mw_component::SnapshotSource`]: the key, the website's bindings, the enriched
//! reading, the station's static metadata and the window-derived values **as the provider
//! last resolved them**. There is no store handle in it, which is the structural half of
//! **D-35**: the `/latest` handler cannot query because there is nothing in it to query
//! with, and this renderer cannot either.
//!
//! # The three things the Go read path computes per request, and must keep computing
//!
//! 1. **`windch` and `heatidx` are RECOMPUTED, never read from their columns.** This is a
//!    constraint rather than a choice (**DEV-9**, and the plan records it on this item):
//!    `transform.go:18` recomputes both from `OutTemp` plus wind speed or humidity on every
//!    request, and `metrics/registry.toml` marks both `legacy_read_time_computed`. Reading
//!    the column instead would emit `mean(windchill)` where the contract emits
//!    `windchill(mean(outtemp), mean(windspeed))` -- 33.64 against 46.04 for a two-reading
//!    bucket at (40 °F, 10 mph), which is not rounding.
//! 2. **The two AQI keys use the column only when it is positive**, and compute otherwise
//!    (`transform.go:209`). The enrich stage now populates the column from the same EPA
//!    table, so the stored branch returns what the computed branch would -- but the
//!    *branch* is the contract and is reproduced.
//! 3. **`sunrise`, `sunset` and the four lunar values are functions of `(date, lat, lon)`**
//!    and of no reading at all (W2a Decision 36). [`crate::sun`] has them.
//!
//! The formulae for the first two come from `mw_pipeline::enrich` rather than from a copy
//! here, which is the whole point of **DEV-9**'s constraint: one formula, asserted
//! bit-for-bit against the Go arithmetic in that module's own tests, used at ingest and at
//! read time. A second copy is exactly the defect the constraint exists to prevent.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Utc};
use mw_component::{
    Encoding, Payload, RenderError, Resolved, SnapshotRender, SnapshotSource, StationMeta, Zone,
};
use mw_types::{Agg, MetricKey, Reading, StationKey, Window};

use crate::dto::{to_json_line, zf, LegacyWeatherReading};
use crate::sun;

/// A number that changes when the bytes this renderer produces for a given source change.
///
/// Folded into [`mw_component::PayloadRev`] and therefore into the `ETag`, so a serialiser
/// change invalidates a cached response. **Bump it in the same commit as any change to the
/// emitted bytes** -- a new key, a changed unit, a different `omitempty` -- because an
/// `ETag` that did not move across a payload change is a `304` serving the old body.
const REVISION: u64 = 1;

/// The ten minutes W1 Decision 15 freezes: a reading older than this makes `/latest` a
/// `404` rather than a stale `200`.
///
/// Here rather than in `mw-store` because it belongs to the **contract** and the store
/// should not have a copy of it (`mw_component::LatestQuery::since` says so from the other
/// side). `snapshot:warm` seeds with the same bound, computed by its own caller.
pub const RECENCY: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// The frozen DTO's serialiser.
#[derive(Debug, Default)]
pub struct Renderer {
    /// Each station's resolved zone, for `sunrise` and `sunset`.
    ///
    /// **Taken from the registry snapshot's parse rather than re-parsed**, which is W2a
    /// Decision 11's own rule: the parse *is* the validation and it already happened there,
    /// so a second one would be a second answer to which midnight a station's day begins
    /// at -- and the binary refuses to boot over a zone that does not resolve, so there is
    /// no failure mode left here to handle. A station with no entry renders its sun times
    /// in UTC, which is the honest answer for a station nobody gave a zone.
    ///
    /// Not taken from [`StationMeta`], which carries the coordinates and not the zone.
    zones: BTreeMap<StationKey, Zone>,
    /// Each station's coordinates, for the **website's primary** station.
    ///
    /// [`StationMeta`] carries the queried station's, which is what `/latest` needs for its
    /// sun times -- but `moonCrescentAngle` is computed at the *primary device's* position
    /// (`handlers.go:387`), and the primary may not be the station the request named. So
    /// the renderer needs a position it was not handed, which is why this map exists
    /// alongside [`Renderer::zones`] rather than being read off the snapshot.
    positions: BTreeMap<StationKey, (f64, f64)>,
}

impl Renderer {
    /// The renderer over the station zones and positions the configuration declares.
    pub fn new(
        zones: BTreeMap<StationKey, Zone>,
        positions: BTreeMap<StationKey, (f64, f64)>,
    ) -> Self {
        Self { zones, positions }
    }

    /// How many stations have a configured zone, for the binary's boot log line.
    pub fn zones(&self) -> usize {
        self.zones.len()
    }

    /// [`REVISION`], for a surface that builds an `ETag` without going through a snapshot.
    ///
    /// `/snow` is the one: **DEV-1** puts a tag on it and its bytes are not a snapshot's,
    /// so it folds this crate's serialiser revision in by hand rather than through
    /// `Snapshot::render`. One constant, read from two places, so a bump invalidates both.
    pub fn revision_for_etag(&self) -> u64 {
        REVISION
    }

    /// The `/latest` DTO for one snapshot source.
    ///
    /// Public because the handler tests need it: asserting the 88-field mapping against a
    /// constructed source is what makes it checkable without a database.
    pub fn reading(&self, source: &SnapshotSource) -> LegacyWeatherReading {
        let r = &source.reading;
        let w = &source.windows;
        let meta = &source.meta;
        // **The website's primary station, whose zone formats the sun times and whose
        // position orients the moon.** `handlers.go:372` takes `loc` from
        // `getPrimaryDeviceConfigForWebsite(website)` and falls back to `time.Local`; the
        // *minutes* it formats come from the queried station. So a request naming a
        // non-primary station gets its own sunrise instant rendered in the primary's clock,
        // which reads as odd and is the contract.
        let primary = source.website.primary_station.as_ref();
        let zone = primary
            .and_then(|key| self.zones.get(key))
            .or_else(|| self.zones.get(&source.key.station))
            .map(|z| z.tz());

        let mut dto = LegacyWeatherReading {
            station_name: r.station_name.clone().unwrap_or_default(),
            station_type: r.station_type.clone().unwrap_or_default(),
            // Q-3: milliseconds since the epoch, as a bare JSON integer.
            reading_timestamp: r.time.timestamp_millis(),
            outside_temperature: zf(r.out_temp),
            extra_temp1: zf(r.extra_temp1),
            extra_temp2: zf(r.extra_temp2),
            extra_temp3: zf(r.extra_temp3),
            extra_temp4: zf(r.extra_temp4),
            extra_temp5: zf(r.extra_temp5),
            extra_temp6: zf(r.extra_temp6),
            extra_temp7: zf(r.extra_temp7),
            soil_temp1: zf(r.soil_temp1),
            soil_temp2: zf(r.soil_temp2),
            soil_temp3: zf(r.soil_temp3),
            soil_temp4: zf(r.soil_temp4),
            leaf_temp1: zf(r.leaf_temp1),
            leaf_temp2: zf(r.leaf_temp2),
            leaf_temp3: zf(r.leaf_temp3),
            leaf_temp4: zf(r.leaf_temp4),
            out_humidity: zf(r.out_humidity),
            extra_humidity1: zf(r.extra_humidity1),
            extra_humidity2: zf(r.extra_humidity2),
            extra_humidity3: zf(r.extra_humidity3),
            extra_humidity4: zf(r.extra_humidity4),
            extra_humidity5: zf(r.extra_humidity5),
            extra_humidity6: zf(r.extra_humidity6),
            extra_humidity7: zf(r.extra_humidity7),
            // **Q-1**: `ohum` and `outhumidity` are one measurement emitted twice. The
            // website reads `ohum` and the iOS client decodes both, so neither may be
            // dropped.
            outside_humidity: zf(r.out_humidity),
            // **The rain rate is a window, not the column.** `handlers.go:344` overwrites
            // the station-reported `rainrate` with `sum(rainincremental)` over ten minutes
            // times six before serialising, so the contract promises the extrapolation.
            rain_rate: window(
                w,
                MetricKey::RainIncremental,
                Window::PerHour(Window::TEN_MINUTES),
                Agg::Sum,
            ),
            rain_incremental: zf(r.rain_incremental),
            // `period_rain` is a column of the **aggregate views only**: the Go `/latest`
            // query is `SELECT time AS bucket, *, … FROM weather`, so gorm leaves
            // `BucketReading.PeriodRain` at its zero value and the response carries `0`.
            // On `/span` the same key is the bucket's total. Reproduced rather than
            // improved -- a non-zero value here would be a new number on a frozen key.
            period_rain: zf(None),
            solar_watts: zf(r.solar_watts),
            potential_solar_watts: zf(r.potential_solar_watts),
            solar_joules: zf(r.solar_joules),
            uv: zf(r.uv),
            radiation: zf(r.radiation),
            storm_rain: zf(r.storm_rain),
            barometer: zf(r.barometer),
            wind_speed: zf(r.wind_speed),
            wind_direction: zf(r.wind_dir),
            cardinal_direction: cardinal(zf(r.wind_dir)).to_string(),
            inside_temperature: zf(r.in_temp),
            inside_humidity: zf(r.in_humidity),
            cons_battery_voltage: zf(r.cons_battery_voltage),
            station_battery_voltage: zf(r.station_battery_voltage),
            // **Q-4**: millimetres here and inches on `/snow` and `/span`. The column's
            // declared unit is `mm` and this passes it through unconverted, which is what
            // `transformLatestReadings` does.
            snow_depth: zf(r.snow_depth),
            snow_distance: zf(r.snow_distance),
            pm25: zf(r.pm25),
            co2: zf(r.co2),
            aqi_pm25_aqin: aqi_pm25(r),
            aqi_pm10_aqin: aqi_pm10(r),
            extra_float1: zf(r.extra_float1),
            extra_float2: zf(r.extra_float2),
            extra_float3: zf(r.extra_float3),
            extra_float4: zf(r.extra_float4),
            extra_float5: zf(r.extra_float5),
            extra_float6: zf(r.extra_float6),
            extra_float7: zf(r.extra_float7),
            extra_float8: zf(r.extra_float8),
            extra_float9: zf(r.extra_float9),
            extra_float10: zf(r.extra_float10),
            extra_text1: r.extra_text1.clone().unwrap_or_default(),
            extra_text2: r.extra_text2.clone().unwrap_or_default(),
            extra_text3: r.extra_text3.clone().unwrap_or_default(),
            extra_text4: r.extra_text4.clone().unwrap_or_default(),
            extra_text5: r.extra_text5.clone().unwrap_or_default(),
            extra_text6: r.extra_text6.clone().unwrap_or_default(),
            extra_text7: r.extra_text7.clone().unwrap_or_default(),
            extra_text8: r.extra_text8.clone().unwrap_or_default(),
            extra_text9: r.extra_text9.clone().unwrap_or_default(),
            extra_text10: r.extra_text10.clone().unwrap_or_default(),
            ..Default::default()
        };

        // The window-derived half: the five rainfall totals, the gust and the storm. All
        // nine declarations are `mw_pipeline::snapshot::WINDOWS`, resolved by
        // `window:rollup` on its own tick -- so a database outage degrades these and
        // nothing else (W3 §2a.6's first table row).
        dto.wind_gust = window(
            w,
            MetricKey::WindSpeed,
            Window::Rolling(Window::TEN_MINUTES),
            Agg::Max,
        );
        let day_rain = window(w, MetricKey::RainIncremental, Window::Day, Agg::Sum);
        // **Q-1 again**: `dayrain` and `rainday` are one value emitted twice.
        dto.day_rain = day_rain;
        dto.rainfall_day = day_rain;
        dto.month_rain = window(w, MetricKey::RainIncremental, Window::Month, Agg::Sum);
        dto.year_rain = window(w, MetricKey::RainIncremental, Window::Year, Agg::Sum);
        dto.rainfall24h = window(
            w,
            MetricKey::RainIncremental,
            Window::Rolling(DAY),
            Agg::Sum,
        );
        dto.rainfall48h = window(
            w,
            MetricKey::RainIncremental,
            Window::Rolling(TWO_DAYS),
            Agg::Sum,
        );
        dto.rainfall72h = window(
            w,
            MetricKey::RainIncremental,
            Window::Rolling(THREE_DAYS),
            Agg::Sum,
        );
        // The **computed** storm total, which is not the station-reported `storm_rain`
        // column two fields above it.
        dto.rainfall_storm = window(w, MetricKey::RainIncremental, Window::Storm, Agg::Sum);

        // Recomputed, never read from their columns -- see the module documentation.
        //
        // **Zero-filled BEFORE the formula, not after**, which is what `/span` already does
        // (`span.rs`'s own call) and what Go does by accident of its types. Go guards on
        // `latest.OutTemp <= 50 && latest.WindSpeed >= 3` over a struct GORM scanned a NULL
        // column into as `0`, so a station with a working anemometer and no outdoor
        // thermometer satisfies `0 <= 50` and gets `CalculateWindChill(0, 10)`. Guarding on
        // `Option` presence instead returned `0` there, which is a different number and a
        // different key: `-15.934471` against `0` for `out_temp` NULL and `wind_speed` 10,
        // and `82.38399` against `0` for `out_temp` 85 with `out_humidity` NULL.
        //
        // That state is the normal one for a sensor a station lacks, because **D-26** has the
        // store write NULL for an unreported metric rather than zero -- so the two endpoints
        // disagreeing was reachable from the ordinary case.
        dto.wind_chill = zf(mw_pipeline::enrich::wind_chill(
            zf(r.out_temp),
            zf(r.wind_speed),
        ));
        dto.heat_index = zf(mw_pipeline::enrich::heat_index(
            zf(r.out_temp),
            zf(r.out_humidity),
        ));

        self.sun_and_moon(&mut dto, r.time, &source.key.station, meta, zone, primary);
        dto
    }

    /// The six `(date, lat, lon)` keys.
    ///
    /// All six are `omitempty`, and the two sun times are **absent together**: a station
    /// with no coordinates has no sunrise to compute, and a polar day has none to report.
    /// The four lunar keys do not need coordinates -- only the crescent *rotation* does,
    /// and it falls back to the geocentric terminator without them, which is what the Go
    /// function does for its `(0, 0)` sentinel.
    fn sun_and_moon(
        &self,
        dto: &mut LegacyWeatherReading,
        at: DateTime<Utc>,
        station: &StationKey,
        meta: &StationMeta,
        zone: Option<chrono_tz::Tz>,
        primary: Option<&StationKey>,
    ) {
        // **The queried station's position, for the sun.** Go reads its sun times from
        // `GetSunTimes(stationName, dayOfYear)`, which is per station.
        let position = meta.coordinates();
        if let Some((lat, lon)) = position {
            // The day of year is the **station's**, not UTC's: a station in
            // `Pacific/Auckland` is a day ahead of UTC for part of its own day, and taking
            // the day from UTC would shift its sunrise by one.
            let local_day = match zone {
                Some(tz) => at.with_timezone(&tz).ordinal(),
                None => at.ordinal(),
            };
            match sun::sunrise_sunset(local_day, lat, lon) {
                Some((rise, set)) => match zone {
                    Some(tz) => {
                        dto.sunrise = sun::format_sun_time(rise, at, &tz);
                        dto.sunset = sun::format_sun_time(set, at, &tz);
                    }
                    None => {
                        dto.sunrise = sun::format_sun_time(rise, at, &Utc);
                        dto.sunset = sun::format_sun_time(set, at, &Utc);
                    }
                },
                None => tracing::debug!(
                    %station,
                    "the sun neither rises nor sets at this station today, so `/latest` \
                     omits both keys -- which is also what the Go handler does for a \
                     station whose sun-times row was never populated"
                ),
            }
        }

        // The moon is computed from the reading's own instant rather than from `now()`,
        // which is a departure the other way: the Go handler uses `time.Now()`. The
        // difference is at most the snapshot's age; the payload has to be a function of
        // the reading for the `ETag` to be a validator at all (§2a.5), and no client can
        // tell a moon phase ten minutes out from one that is not.
        // **The PRIMARY station's position, for the moon.** `handlers.go:387` passes
        // `getPrimaryDeviceConfigForWebsite(website)`'s latitude and longitude to
        // `CalculateCrescentAngle`, not the queried station's -- so `/latest?station=X` on a
        // website whose primary is Y computes the crescent's rotation at Y. Using the
        // queried station's instead changed `moonCrescentAngle` on a frozen endpoint, and
        // the fixtures that would notice set `body_equals: false` and assert shape only.
        //
        // Falls back to the queried station's position when the website names no primary or
        // the primary has no coordinates, which is the geocentric `(0, 0)` sentinel's
        // neighbourhood: Go passes `device.Latitude`/`Longitude`, which are zero for a row
        // that has none, and `CalculateCrescentAngle` treats `(0, 0)` as "no observer".
        let moon_position = primary
            .and_then(|key| self.positions.get(key).copied())
            .or(position);
        let moon = sun::moon(at, moon_position);
        dto.moon_phase_name = moon.phase_name.to_string();
        // Through `zf` like every other float on the body, which makes **DEV-11**
        // structural rather than a property of this file. The arithmetic in `sun::moon` is
        // bounded trig over `atan2`, so a non-finite is not reachable -- but these were the
        // only three floats on the frozen surface that could have reached `serde_json`
        // directly, where a non-finite becomes `null` and fails the iOS decode of the whole
        // object. `is_zero_f32` would not have omitted it either, because `NaN == 0.0` is
        // false.
        dto.moon_illumination = zf(Some(moon.illumination as f32));
        dto.moon_age = zf(Some(moon.age_days as f32));
        dto.moon_crescent_angle = zf(Some(moon.crescent_angle as f32));
    }
}

impl SnapshotRender for Renderer {
    fn revision(&self) -> u64 {
        REVISION
    }

    /// JSON and nothing else.
    ///
    /// `format=msgpack` is **not carried forward** (**D-69**), so declaring only
    /// `[Encoding::Json]` makes a request for it a named
    /// [`RenderError::EncodingUnsupported`] rather than an empty body -- and protobuf's
    /// consumer is `mw-grpc`, which is M6's.
    fn encodings(&self) -> &'static [Encoding] {
        &[Encoding::Json]
    }

    fn render(&self, encoding: Encoding, source: &SnapshotSource) -> Result<Payload, RenderError> {
        if encoding != Encoding::Json {
            return Err(RenderError::EncodingUnsupported {
                encoding,
                declared: Encoding::Json.to_string(),
            });
        }
        let dto = self.reading(source);
        to_json_line(&dto)
            .map(|bytes| Payload::from(bytes.into_boxed_slice()))
            .map_err(|e| RenderError::NotSerialisable {
                key: source.key.clone(),
                reason: e.to_string(),
            })
    }
}

/// The three multi-day rolling windows `/latest` serves.
const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// Forty-eight hours.
const TWO_DAYS: std::time::Duration = std::time::Duration::from_secs(48 * 3600);
/// Seventy-two hours.
const THREE_DAYS: std::time::Duration = std::time::Duration::from_secs(72 * 3600);

/// One window-derived value, zero-filled.
///
/// **A declaration the provider did not answer is the same as an absent value here**, and
/// that is deliberate: `Resolved::value` returns `Err(NotDeclaredInRequest)` for a triple
/// nobody asked for, which on this path can only mean that `mw_pipeline::snapshot::WINDOWS`
/// and this module have drifted apart. Zero-filling it keeps the frozen response intact --
/// a missing key fails the iOS decode of the whole object -- and
/// `every_window_this_renderer_reads_is_one_the_provider_declares` is what stops the drift
/// being silent.
fn window(resolved: &Resolved, metric: MetricKey, w: Window, agg: Agg) -> f32 {
    let value = resolved.value((metric, w, agg)).ok().flatten();
    // `f64` to `f32` on a rainfall total: the column is `float4` and the sum is accumulated
    // in double precision (**D-14**), so this narrowing is the one the contract's own type
    // performs and not a loss introduced here.
    zf(value.map(|v| v as f32))
}

/// Go's `headingToCardinalDirection`: sixteen points, 22.5 degrees each.
///
/// Always returns a non-empty string, including for a direction of 0 (`N`) -- which is why
/// `windcard` carries `omitempty` and is never absent in practice (Q-5).
///
/// The index truncates toward zero exactly as Go's `int(...)` conversion does, and the
/// modulo is what wraps 348.75-360 back onto `N`. A negative direction would index
/// negatively in Go and panic; `rem_euclid` here returns a point instead, which is a
/// difference only a value the schema does not permit could reach.
fn cardinal(heading: f32) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let index = ((heading + 11.25) / 22.5) as i32;
    POINTS[index.rem_euclid(16) as usize]
}

/// [`cardinal`], for `/span`, which renders the same key from a bucket's circular mean.
///
/// Crate-private rather than copied: one table and one rounding rule, so a change to the
/// sixteen-point mapping cannot apply to one endpoint and not the other.
pub(crate) fn cardinal_for_span(heading: f32) -> &'static str {
    cardinal(heading)
}

/// `transform.go:209`'s `getOrCalculateAQIPM25`: the column when it is positive, otherwise
/// computed from `pm25`.
fn aqi_pm25(r: &Reading) -> f32 {
    if let Some(stored) = r.aqi_pm25_aqin.filter(|v| *v > 0) {
        return stored as f32;
    }
    match r.pm25.filter(|v| *v > 0.0) {
        Some(pm25) => zf(mw_pipeline::enrich::aqi_pm25(pm25).map(|v| v as f32)),
        None => zf(None),
    }
}

/// `getOrCalculateAQIPM10`, with the oddity the Go source has and this reproduces: it reads
/// **`pm10_in_aqin`**, the AQIN device's indoor PM10, and not an outdoor PM10 the schema
/// does not have.
fn aqi_pm10(r: &Reading) -> f32 {
    if let Some(stored) = r.aqi_pm10_aqin.filter(|v| *v > 0) {
        return stored as f32;
    }
    match r.pm10_in_aqin.filter(|v| *v > 0.0) {
        Some(pm10) => zf(mw_pipeline::enrich::aqi_pm10(pm10).map(|v| v as f32)),
        None => zf(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::{SnapshotKey, WebsiteView};
    use mw_types::WebsiteId;

    fn source(reading: Reading, windows: Resolved) -> SnapshotSource {
        SnapshotSource {
            key: SnapshotKey::new(WebsiteId::new(1), StationKey::new("davis-full")),
            website: WebsiteView {
                id: WebsiteId::new(1),
                name: "Suncrest Live Weather".into(),
                primary_station: Some(StationKey::new("davis-full")),
                snow_enabled: true,
                snow_station: Some(StationKey::new("snowgauge")),
            },
            reading,
            meta: StationMeta {
                latitude: Some(39.900),
                longitude: Some(-105.500),
                altitude_m: Some(1800.0),
                snow_base_distance_mm: None,
            },
            windows,
        }
    }

    /// **The primary station's position orients the moon, the queried station's the sun.**
    ///
    /// `handlers.go:387` passes `getPrimaryDeviceConfigForWebsite(website)`'s latitude and
    /// longitude to `CalculateCrescentAngle`, so `/latest?station=X` on a website whose
    /// primary is Y computes the crescent at Y. Using the queried station's position changed
    /// `moonCrescentAngle` on a frozen endpoint, and the fixtures that would notice set
    /// `body_equals: false` and assert shape only.
    #[test]
    fn the_crescent_is_oriented_at_the_websites_primary_station() {
        let mut positions = BTreeMap::new();
        positions.insert(StationKey::new("davis-full"), (39.900, -105.500));
        positions.insert(StationKey::new("csi-minimal"), (39.800, -105.400));
        let renderer = Renderer::new(BTreeMap::new(), positions);

        // The request names `csi-minimal`; the website's primary is `davis-full`.
        let mut src = source(
            Reading {
                out_temp: Some(40.0),
                ..Reading::default()
            },
            resolved(),
        );
        src.key = SnapshotKey::new(WebsiteId::new(1), StationKey::new("csi-minimal"));
        src.meta.latitude = Some(39.800);
        src.meta.longitude = Some(-105.400);
        let at_primary = renderer.reading(&src).moon_crescent_angle;

        // The same request on a website that names no primary falls back to the queried
        // station, which is the only position there is.
        let mut without = src.clone();
        without.website.primary_station = None;
        let at_queried = renderer.reading(&without).moon_crescent_angle;

        assert_ne!(
            at_primary, at_queried,
            "the two positions are 0.1 degrees apart, so the rotation differs -- if these \
             are equal the primary is not being consulted"
        );
        // And the primary's answer is the one `davis-full` would have got for itself.
        let mut as_primary = src.clone();
        as_primary.key = SnapshotKey::new(WebsiteId::new(1), StationKey::new("davis-full"));
        as_primary.meta.latitude = Some(39.900);
        as_primary.meta.longitude = Some(-105.500);
        assert_eq!(
            renderer.reading(&as_primary).moon_crescent_angle,
            at_primary
        );
    }

    fn resolved() -> Resolved {
        Resolved::new(
            Utc::now(),
            mw_pipeline::snapshot::WINDOWS.map(|d| (d, Some(1.5))),
        )
    }

    /// **The invariant that keeps this module and the provider in step.** Every window this
    /// renderer reads has to be one `mw_pipeline::snapshot::WINDOWS` declares, or
    /// `Resolved::value` answers `NotDeclaredInRequest` and the key is silently zero
    /// forever -- which is the M4-1 defect in a different place.
    #[test]
    fn every_window_this_renderer_reads_is_one_the_provider_declares() {
        // Resolve with a DISTINCT value per declaration, so a renderer reading the wrong
        // triple reads the wrong number rather than coincidentally the right one.
        let values: Vec<(mw_component::Declaration, Option<f64>)> = mw_pipeline::snapshot::WINDOWS
            .iter()
            .enumerate()
            .map(|(i, d)| (*d, Some(i as f64 + 1.0)))
            .collect();
        let dto = Renderer::default().reading(&source(
            Reading::default(),
            Resolved::new(Utc::now(), values),
        ));
        // The nine declarations, in `WINDOWS` order, and the field each one lands in.
        assert_eq!(dto.wind_gust, 1.0, "WindSpeed/10m/Max");
        assert_eq!(dto.rain_rate, 2.0, "RainIncremental/PerHour(10m)/Sum");
        assert_eq!(dto.day_rain, 3.0, "RainIncremental/Day/Sum");
        assert_eq!(
            dto.rainfall_day, 3.0,
            "Q-1: the same value under a second key"
        );
        assert_eq!(dto.month_rain, 4.0);
        assert_eq!(dto.year_rain, 5.0);
        assert_eq!(dto.rainfall24h, 6.0);
        assert_eq!(dto.rainfall48h, 7.0);
        assert_eq!(dto.rainfall72h, 8.0);
        assert_eq!(dto.rainfall_storm, 9.0, "RainIncremental/Storm/Sum");
    }

    /// A provider that has never answered leaves every window-derived key at the
    /// contract's zero and changes nothing else, which is W3 §2a.6's first table row.
    #[test]
    fn an_unresolved_window_is_the_contracts_zero_and_nothing_else_changes() {
        let reading = Reading {
            out_temp: Some(62.7),
            ..Reading::default()
        };
        let dto = Renderer::default().reading(&source(reading, Resolved::new(Utc::now(), [])));
        assert_eq!(dto.wind_gust, 0.0);
        assert_eq!(dto.rain_rate, 0.0);
        assert_eq!(dto.rainfall_storm, 0.0);
        assert_eq!(
            dto.outside_temperature, 62.7,
            "the reading itself is unaffected"
        );
    }

    /// **Q-1**, both pairs, from one value each.
    #[test]
    fn the_two_duplicate_pairs_come_from_one_value() {
        let reading = Reading {
            out_humidity: Some(44.0),
            ..Reading::default()
        };
        let dto = Renderer::default().reading(&source(reading, resolved()));
        assert_eq!(dto.out_humidity, 44.0);
        assert_eq!(dto.outside_humidity, 44.0);
        assert_eq!(dto.day_rain, dto.rainfall_day);
    }

    /// **DEV-9's constraint**: wind chill is the formula over the reading's own values and
    /// not the stored column, which here holds a number no formula would produce.
    #[test]
    fn wind_chill_and_heat_index_are_recomputed_rather_than_read() {
        let reading = Reading {
            out_temp: Some(40.0),
            wind_speed: Some(10.0),
            wind_chill: Some(-999.0),
            out_humidity: Some(50.0),
            heat_index: Some(-999.0),
            ..Reading::default()
        };
        let dto = Renderer::default().reading(&source(reading, resolved()));
        assert_ne!(dto.wind_chill, -999.0, "the column was read");
        assert!(
            (dto.wind_chill - 33.64).abs() < 0.01,
            "the Go value for (40 °F, 10 mph) is 33.64, got {}",
            dto.wind_chill
        );
        // Heat index does not apply below 80 °F, so the Go read path leaves it at zero.
        assert_eq!(dto.heat_index, 0.0);
    }

    /// **One input absent is not both.** Go guards on the *values* of a struct GORM scanned
    /// NULL into as zero, so a station with a working anemometer and no outdoor thermometer
    /// gets `CalculateWindChill(0, 10)` and a real number. Guarding on `Option` presence
    /// returned `0` instead -- and `/span` already did it Go's way, so the two endpoints
    /// disagreed with each other.
    ///
    /// The expected values are `CalculateWindChill`/`CalculateHeatIndex` at `c3ec9f7`, run
    /// here: `windchill(0, 10) = -15.934471`, `heatindex(85, 0) = 82.38399`.
    #[test]
    fn a_reading_missing_one_of_the_two_inputs_is_computed_the_way_go_computed_it() {
        // An anemometer and no thermometer: `out_temp` NULL zero-fills to 0, which is
        // `<= 50`, so the formula applies.
        let dto = Renderer::default().reading(&source(
            Reading {
                out_temp: None,
                wind_speed: Some(10.0),
                ..Reading::default()
            },
            resolved(),
        ));
        assert_eq!(dto.wind_chill.to_bits(), 3246322584, "{}", dto.wind_chill);

        // A thermometer and no hygrometer: `out_humidity` NULL zero-fills to 0, and 85 °F
        // is `>= 80`, so the regression applies at zero humidity.
        let dto = Renderer::default().reading(&source(
            Reading {
                out_temp: Some(85.0),
                out_humidity: None,
                ..Reading::default()
            },
            resolved(),
        ));
        assert_eq!(dto.heat_index.to_bits(), 1118094490, "{}", dto.heat_index);

        // And the thresholds still bite on a zero-filled input: no wind means `wind_speed`
        // is 0, which is below 3, so Go returns 0 and so does this.
        let dto = Renderer::default().reading(&source(
            Reading {
                out_temp: Some(20.0),
                wind_speed: None,
                ..Reading::default()
            },
            resolved(),
        ));
        assert_eq!(dto.wind_chill, 0.0);
    }

    /// Q-4: millimetres on `/latest`, passed through from the column the enrich stage
    /// filled.
    #[test]
    fn latest_serves_the_snow_depth_in_millimetres() {
        let reading = Reading {
            snow_depth: Some(119.38),
            snow_distance: Some(1380.62),
            ..Reading::default()
        };
        let dto = Renderer::default().reading(&source(reading, resolved()));
        assert_eq!(dto.snow_depth, 119.38, "mm, unconverted");
        assert_eq!(dto.snow_distance, 1380.62);
    }

    /// Q-5: sixteen points, and `N` for a direction of zero -- which is why `windcard`
    /// carries `omitempty` and is nonetheless never absent.
    #[test]
    fn the_cardinal_direction_covers_the_circle() {
        assert_eq!(cardinal(0.0), "N");
        assert_eq!(cardinal(11.24), "N");
        assert_eq!(cardinal(11.26), "NNE");
        assert_eq!(cardinal(90.0), "E");
        assert_eq!(cardinal(180.0), "S");
        assert_eq!(cardinal(270.0), "W");
        assert_eq!(cardinal(349.0), "N", "348.75 wraps back onto north");
        assert_eq!(cardinal(360.0), "N");
        for degrees in 0..=3600 {
            let heading = degrees as f32 / 10.0;
            assert!(!cardinal(heading).is_empty(), "{heading}");
        }
    }

    /// The AQI branch: the column when positive, the formula otherwise, and zero when
    /// there is neither.
    #[test]
    fn the_aqi_keys_prefer_a_positive_column_and_compute_otherwise() {
        let stored = Reading {
            aqi_pm25_aqin: Some(42),
            pm25: Some(12.0),
            ..Reading::default()
        };
        assert_eq!(aqi_pm25(&stored), 42.0);

        let computed = Reading {
            aqi_pm25_aqin: Some(0),
            pm25: Some(12.0),
            ..Reading::default()
        };
        assert_eq!(
            aqi_pm25(&computed),
            mw_pipeline::enrich::aqi_pm25(12.0).expect("12 ug/m3 is on the table") as f32
        );

        assert_eq!(
            aqi_pm25(&Reading::default()),
            0.0,
            "neither, so the zero-fill"
        );
        assert_eq!(aqi_pm10(&Reading::default()), 0.0);
    }

    /// The renderer declares JSON and refuses the rest by name (**D-69**).
    #[test]
    fn only_json_is_produced_and_msgpack_is_a_named_refusal() {
        let renderer = Renderer::default();
        assert_eq!(renderer.encodings(), &[Encoding::Json]);
        let src = source(Reading::default(), resolved());
        assert!(renderer.render(Encoding::Json, &src).is_ok());
        let e = renderer
            .render(Encoding::MsgPack, &src)
            .expect_err("msgpack is not carried forward");
        assert!(matches!(
            e,
            RenderError::EncodingUnsupported {
                encoding: Encoding::MsgPack,
                ..
            }
        ));
        assert!(
            e.to_string().contains("deliberately not implemented"),
            "{e}"
        );
    }

    /// The rendered bytes end with one newline (Q-12) and parse as JSON.
    #[test]
    fn the_rendered_payload_is_newline_terminated() {
        let bytes = Renderer::default()
            .render(Encoding::Json, &source(Reading::default(), resolved()))
            .expect("renders");
        assert_eq!(bytes.last(), Some(&b'\n'));
        let text = std::str::from_utf8(&bytes).expect("utf-8");
        let _: serde_json::Value = serde_json::from_str(text).expect("valid JSON");
    }

    /// A station with coordinates gets all six `(date, lat, lon)` keys; one without gets
    /// the four lunar keys and neither sun time.
    #[test]
    fn the_sun_keys_need_coordinates_and_the_moon_keys_do_not() {
        let renderer = Renderer::default();
        let dto = renderer.reading(&source(Reading::default(), resolved()));
        assert!(!dto.sunrise.is_empty(), "a station at 39.9 N has a sunrise");
        assert!(!dto.sunset.is_empty());
        assert!(!dto.moon_phase_name.is_empty());

        let mut src = source(Reading::default(), resolved());
        src.meta = StationMeta::EMPTY;
        let dto = renderer.reading(&src);
        assert!(dto.sunrise.is_empty(), "no coordinates, no sunrise");
        assert!(dto.sunset.is_empty());
        assert!(
            !dto.moon_phase_name.is_empty(),
            "the moon phase is a function of the instant alone"
        );
    }
}
