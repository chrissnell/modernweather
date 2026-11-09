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
        return render(BTreeMap::new());
    };

    let mut rows = match store
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
    if let Some(air) = website
        .air_quality_station
        .as_ref()
        .filter(|_| website.air_quality_enabled)
    {
        if air != &station.key {
            match store
                .almanac(AlmanacQuery {
                    station: air.clone(),
                })
                .await
            {
                Ok(air_rows) => {
                    for (record, row) in index(air_rows) {
                        if AIR_QUALITY_RECORDS.contains(&record.as_str()) {
                            rows.insert(record, row);
                        }
                    }
                }
                Err(e) => return store_failed(&e),
            }
        }
    }

    render(rows)
}

/// The six records `/almanac` reads from the website's air-quality device rather than from
/// the station the request named.
const AIR_QUALITY_RECORDS: [&str; 6] = [
    "high_pm25",
    "high_pm10_in",
    "high_co2",
    "high_tvoc",
    "high_aqi_pm25",
    "high_aqi_pm10",
];

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
fn render(mut rows: BTreeMap<String, AlmanacRecord>) -> Response<Body> {
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
        high_pm25: take("high_pm25"),
        high_pm10_in: take("high_pm10_in"),
        high_co2: take("high_co2"),
        high_tvoc: take("high_tvoc"),
        high_aqi_pm25: take("high_aqi_pm25"),
        high_aqi_pm10: take("high_aqi_pm10"),
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
