//! `/almanac` -- coupled class, and the cache is its only data source.
//!
//! W1 assumption 12: this reads `almanac_cache` **and nothing else**, and returns `{}`
//! rather than computing anything when the cache is empty. There is no fallback query here
//! and there must not be one -- a handler that recomputed on a miss would serve a different
//! number from the one the cache will serve a minute later, on an endpoint whose whole point
//! is that it is cheap.
//!
//! Every one of the seventeen fields is `omitempty` and a pointer, so **an absent row is an
//! absent key, never a null**, and no rows at all is `{}`. That falls out of the table's
//! shape -- one row per record per station -- rather than out of a branch here, which is
//! the reason W4 chose that shape over seventeen nullable column triplets.
//!
//! # Two stations, not one
//!
//! The particulate extremes (`high_pm25`, `high_pm10_in`, `high_co2`, `high_tvoc`,
//! `high_aqi_pm25`, `high_aqi_pm10`) are read from the **website's** air-quality device,
//! not from the station the request named. That is why a seeded air-quality station changes
//! `/almanac?station=davis-full` from eight keys to fourteen whichever station is asked
//! about, which `ci/conformance/README.md` records as a property of the capture rig.
//!
//! # The three snow records
//!
//! `deepest_snow`, `max_snow_hour` and `max_snow_day` are declared and were **never
//! assigned** in Go -- `handlers.go:1416` carries "Snow metrics temporarily disabled due to
//! performance issues" (Q-19). They are keys of the cache table here, so they appear if and
//! when something writes them; nothing does today, so the response is unchanged. Reading
//! the table rather than suppressing three of its rows is the right shape: the table is the
//! source of truth for what the cache holds, and a handler that filtered it would be a
//! second place that decided.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Response, StatusCode};
use mw_component::AlmanacQuery;

use crate::component::Handlers;
use crate::dto::{go_rfc3339, to_json_line, zf, Almanac, AlmanacRecord};
use crate::params::Params;
use crate::reply::{self, Cache};

/// The `503` this endpoint gives when no website resolves: `/almanac` (`handlers.go:1330`).
const NO_WEBSITE: reply::NoWebsite = reply::NO_WEBSITE_ALMANAC;

/// One hour, which is this endpoint's frozen `Cache-Control`.
const MAX_AGE: u32 = 3600;

/// Serve `/almanac`.
pub async fn handle(h: &Handlers, host: &str, params: &Params) -> Response<Body> {
    let website = match h.website(host, NO_WEBSITE) {
        Ok(w) => w,
        Err(response) => return *response,
    };

    // `station` is **required** here, unlike on `/latest`: there is no primary-device
    // fallback and an absent parameter is a `400`.
    let Some(name) = params.non_empty("station") else {
        return reply::text(StatusCode::BAD_REQUEST, "station parameter is required");
    };
    let Some(station) = h.directory.station_by_name(name) else {
        return reply::text(StatusCode::NOT_FOUND, "station not found");
    };

    let Some(store) = h.reading_store() else {
        // The database is down. `{}` is what an empty cache produces and is a response
        // this endpoint already has; a `503` is not (**D-48** governs the frozen five and
        // this one is coupled, but a different status for an unavailable database would
        // still be a second answer to the same question).
        return render(BTreeMap::new(), BTreeMap::new());
    };

    let rows = match store
        .almanac(AlmanacQuery {
            station: station.key.clone(),
        })
        .await
    {
        Ok(rows) => index(rows),
        Err(e) => return store_failed(&e),
    };

    // The particulate half, from the website's air-quality device. Only the six records
    // that device owns are taken from it, so a station that is both -- an AirGradient bound
    // as a website's primary device -- does not read its own rows twice.
    //
    // **It SUBSTITUTES rather than inserting, and that is the whole difference.** Go writes
    // `aqRecords = r` and then reads the four keys out of `aqRecords`
    // (`handlers.go:1383-1397`), so an air-quality device with no cached extremes makes
    // those keys **absent** even when the queried station has rows of its own. Merging the
    // device's rows over the station's left the station's in place, which is the
    // eight-keys-against-fourteen behaviour `ci/conformance/rust-seed/04-caches.sql` was
    // built to watch.
    //
    // **And a failed read is logged, not fatal.** Go's `else { log.Error(...) }` leaves
    // `aqRecords` as the station's own records and still answers `200`; returning a `500`
    // turned a transient second query into a failed response on a coupled endpoint.
    //
    // **The condition is the binding, not a flag.** Go tests
    // `website.AirQualityDeviceName != ""`. `air_quality_enabled` is a separate column that
    // `migrations/config` only constrains in one direction -- `enabled = 1` implies a key --
    // so `enabled = 0` with a key set is representable, and filtering on it diverged there.
    let mut air_quality = rows.clone();
    if let Some(air) = website.air_quality_station.as_ref() {
        if air != &station.key {
            match store
                .almanac(AlmanacQuery {
                    station: air.clone(),
                })
                .await
            {
                Ok(air_rows) => air_quality = index(air_rows),
                Err(e) => tracing::warn!(
                    err = %mw_types::DisplayChain(&e),
                    station = %air,
                    "the air-quality almanac cache could not be read, so /almanac answers \
                     with the queried station's own records"
                ),
            }
        }
    }

    render(rows, air_quality)
}

/// The six records `/almanac` reads from the website's air-quality device rather than from
/// the station the request named.
/// The four keys read from the air-quality device's records rather than the station's.
///
/// **Four, not six.** `high_aqi_pm25` and `high_aqi_pm10` are not read from the cache at
/// all: Go **derives** them from the PM extremes (`handlers.go:1402-1413`) with
/// `getOrCalculateAQIPM25`, carrying the PM record's own timestamp and no `wind_dir`. The
/// table's `CHECK` still permits those two `record` values, for a writer a later item may
/// add -- this handler ignores such a row, because the contract's bytes are a function of
/// the PM extreme and not of whatever was stored beside it.
const AIR_QUALITY_RECORDS: [&str; 4] = ["high_pm25", "high_pm10_in", "high_co2", "high_tvoc"];

/// The cache's rows, keyed by the JSON field they populate.
fn index(rows: Vec<mw_component::AlmanacRecord>) -> BTreeMap<String, AlmanacRecord> {
    rows.into_iter()
        .map(|r| {
            (
                r.record,
                AlmanacRecord {
                    value: zf(Some(r.value)),
                    timestamp: go_rfc3339(r.observed_at),
                    // `omitempty` on a `*float32`, so an absent direction is an absent key
                    // and never a null -- which is the difference between a client reading
                    // "no wind direction was recorded" and one failing to decode.
                    wind_dir: r.wind_dir,
                },
            )
        })
        .collect()
}

/// The response, with each row placed by the name the table's `CHECK` constrains it to.
///
/// The `CHECK` is the contract: a typo in a `record` value would silently drop a field from
/// a response rather than fail, which is what that constraint exists to prevent and why
/// this function can place by name without a fallback.
fn render(
    mut rows: BTreeMap<String, AlmanacRecord>,
    mut air_quality: BTreeMap<String, AlmanacRecord>,
) -> Response<Body> {
    let high_pm25 = air_quality.remove(AIR_QUALITY_RECORDS[0]);
    let high_pm10_in = air_quality.remove(AIR_QUALITY_RECORDS[1]);
    // **Derived, the way Go derives them.** AQI is monotonic in particulate concentration,
    // so the peak AQI is the AQI of the peak PM, carried at that reading's timestamp and
    // with no `wind_dir` -- which Go's own comment says in those words.
    let derive = |pm: Option<&AlmanacRecord>, f: fn(f32) -> Option<i32>| {
        pm.map(|record| AlmanacRecord {
            value: zf(f(record.value).map(|v| v as f32)),
            timestamp: record.timestamp.clone(),
            wind_dir: None,
        })
    };
    let high_aqi_pm25 = derive(high_pm25.as_ref(), mw_pipeline::enrich::aqi_pm25);
    let high_aqi_pm10 = derive(high_pm10_in.as_ref(), mw_pipeline::enrich::aqi_pm10);
    let mut take = |name: &str| rows.remove(name);
    let dto = Almanac {
        high_temp: take("high_temp"),
        low_temp: take("low_temp"),
        high_wind_speed: take("high_wind_speed"),
        max_rain_hour: take("max_rain_hour"),
        max_rain_day: take("max_rain_day"),
        low_barometer: take("low_barometer"),
        low_humidity: take("low_humidity"),
        deepest_snow: take("deepest_snow"),
        max_snow_hour: take("max_snow_hour"),
        max_snow_day: take("max_snow_day"),
        high_solar: take("high_solar"),
        // The four from the air-quality device's records, and the two derived from them.
        high_pm25,
        high_pm10_in,
        high_co2: air_quality.remove(AIR_QUALITY_RECORDS[2]),
        high_tvoc: air_quality.remove(AIR_QUALITY_RECORDS[3]),
        high_aqi_pm25,
        high_aqi_pm10,
    };
    match to_json_line(&dto) {
        Ok(body) => reply::json(StatusCode::OK, Cache::MaxAge(MAX_AGE), body),
        Err(_) => reply::text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "error querying almanac data",
        ),
    }
}

/// Go's `error querying almanac data`.
fn store_failed(e: &mw_component::StoreError) -> Response<Body> {
    tracing::warn!(
        err = %mw_types::DisplayChain(e),
        "the almanac cache could not be read"
    );
    reply::text(
        StatusCode::INTERNAL_SERVER_ERROR,
        "error querying almanac data",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(value: f32) -> AlmanacRecord {
        AlmanacRecord {
            value,
            timestamp: "2026-10-28T00:00:00Z".to_string(),
            wind_dir: Some(270.0),
        }
    }

    async fn body_of(r: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20)
            .await
            .expect("a small body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    /// **The air-quality device's records REPLACE the station's, they do not merge over
    /// them.**
    ///
    /// Go writes `aqRecords = r` and reads the four keys out of `aqRecords`
    /// (`handlers.go:1383-1397`), so a device with no cached extremes makes those keys
    /// absent even when the queried station has rows of its own. Merging left the station's
    /// in place, which is the eight-keys-against-fourteen behaviour
    /// `ci/conformance/rust-seed/04-caches.sql` was built to watch.
    #[tokio::test]
    async fn an_air_quality_device_with_no_rows_removes_the_stations_own() {
        let mut station_rows = BTreeMap::new();
        station_rows.insert("high_temp".to_string(), record(101.0));
        station_rows.insert("high_pm25".to_string(), record(35.0));

        // The device answered, with nothing in it.
        let body = body_of(render(station_rows.clone(), BTreeMap::new())).await;
        assert!(body.contains("\"high_temp\""), "{body}");
        assert!(
            !body.contains("\"high_pm25\""),
            "the AQ device has no row, so the key is absent even though the station has \
             one: {body}"
        );

        // And when it does answer with rows, those are the ones served.
        let mut device = BTreeMap::new();
        device.insert("high_pm25".to_string(), record(55.4));
        let body = body_of(render(station_rows, device)).await;
        assert!(body.contains("\"high_pm25\":{\"value\":55.4"), "{body}");
    }

    /// **`high_aqi_pm25` and `high_aqi_pm10` are derived from the PM extremes, not read.**
    ///
    /// `handlers.go:1402-1413`: AQI is monotonic in particulate concentration, so the peak
    /// AQI is the AQI of the peak PM, carried at that reading's timestamp and with **no**
    /// `wind_dir`. Reading them as cache rows made them appear if and only if something had
    /// written them, which is a different condition from Go's -- and let their value drift
    /// from the PM extreme beside them.
    #[tokio::test]
    async fn the_aqi_extremes_are_derived_from_the_particulate_extremes() {
        let mut device = BTreeMap::new();
        device.insert("high_pm25".to_string(), record(35.4));
        // A stored row the handler must ignore, which is the whole point: Go never reads one.
        device.insert("high_aqi_pm25".to_string(), record(7.0));

        let body = body_of(render(BTreeMap::new(), device)).await;
        let want = mw_pipeline::enrich::aqi_pm25(35.4).expect("in range");
        assert!(
            body.contains(&format!("\"high_aqi_pm25\":{{\"value\":{want}")),
            "derived from 35.4 µg/m³, which is AQI {want}, not the stored 7: {body}"
        );
        assert!(
            body.contains(&format!(
                "\"high_aqi_pm25\":{{\"value\":{want},\"timestamp\":\"2026-10-28T00:00:00Z\"}}"
            )),
            "the PM record's own timestamp, and no wind_dir -- note the `wind_dir` the \
             stored row carried is absent, which is Go's `WindDir: nil`: {body}"
        );
        // No PM extreme, no AQI extreme.
        let body = body_of(render(BTreeMap::new(), BTreeMap::new())).await;
        assert!(!body.contains("high_aqi_pm25"), "{body}");
        assert!(!body.contains("high_aqi_pm10"), "{body}");
    }
}
