//! `/snow` -- the website's gauge, and the depth the request may redirect.
//!
//! # Q-8 is the whole shape of this handler
//!
//! `stationname` and all six totals come from the **website's** `snow_station_key`,
//! whatever the request asked for; `?station=` and `?station_id=` redirect the **depth
//! lookup only**. So `/snow?station=<a weather station>` returns that station's (absent)
//! depth under the snow gauge's name, with the snow gauge's totals. That is not a defect to
//! fix -- it is what the website's chart reads.
//!
//! # No recency filter, deliberately
//!
//! There is none in the Go query and there must not be one here: an offline gauge keeps
//! returning its last known depth indefinitely (fixture `snow-device-stale`, a gauge that
//! went silent three days before capture). A configured-but-never-reporting gauge returns
//! seven zeros with status `200` (fixture `snow-device-never-reported`), which is why
//! `snow_totals_cache`'s every column is `NOT NULL DEFAULT 0` and why a station with no row
//! at all renders the same bytes.
//!
//! # Where the numbers come from, and why not from the snapshot
//!
//! `SnowCache`, and nothing else (W1 assumption 12). The snapshot would be the obvious
//! source for the depth and is the wrong one twice over: it holds nothing for a station
//! outside the ten-minute window -- which is exactly the gauge this endpoint has to answer
//! for -- and its `snowdepth` is in **millimetres** (Q-4) while this endpoint's is in
//! inches.

use axum::body::Body;
use axum::http::{Response, StatusCode};
use mw_component::{ETag, PayloadRev, SnowTotals};

use crate::component::Handlers;
use crate::config::StationView;
use crate::dto::{to_json_line, zf, SnowResponse};
use crate::params::Params;
use crate::reply::{self, Cache};

/// The `503` message `/snow` gives, which is `/latest`'s.
const NO_WEBSITE: &str =
    "Weather data is not available until at least one weather website is configured";

/// Serve `/snow`.
pub async fn handle(h: &Handlers, host: &str, params: &Params) -> Response<Body> {
    let website = match h.website(host, NO_WEBSITE) {
        Ok(w) => w,
        Err(response) => return *response,
    };

    // The website's own toggle, checked before anything is read: a website with snow off
    // has no gauge to report and the body is a plain-text `404` rather than an empty
    // object.
    if !website.snow_enabled {
        return reply::text(
            StatusCode::NOT_FOUND,
            "snow data not enabled for this website",
        );
    }
    let Some(gauge_key) = website.snow_station.as_ref() else {
        // Unrepresentable in the schema -- `CHECK (snow_enabled = 0 OR snow_station_key IS
        // NOT NULL)` -- and answered rather than asserted, because a frozen route may not
        // panic over a constraint.
        return reply::text(
            StatusCode::NOT_FOUND,
            "snow data not enabled for this website",
        );
    };
    let gauge = h.directory.station_by_key(gauge_key);

    // Which station's DEPTH the request asked for. Q-8: this redirects the depth and
    // nothing else.
    let depth_station: Option<&StationView> = match params.non_empty("station_id") {
        Some(raw) => {
            let Ok(id) = raw.parse::<i64>() else {
                return reply::text(StatusCode::BAD_REQUEST, "invalid station_id");
            };
            match h.directory.station_by_id(id) {
                Some(s) => Some(s),
                None => return reply::text(StatusCode::NOT_FOUND, "station not found"),
            }
        }
        None => match params.non_empty("station") {
            Some(name) => match h.directory.station_by_name(name) {
                Some(s) => Some(s),
                None => return reply::text(StatusCode::NOT_FOUND, "station not found"),
            },
            None => gauge,
        },
    };

    let Some(cache) = h.snow.load() else {
        // The database is down. **This is not a `503`** -- D-48 forbids one on a frozen
        // path -- so the contract's all-zero `200` is what a gauge with nothing cached
        // already produces, and that is what an unavailable cache produces too. The
        // `X-MW-Degraded` header is where the difference is visible.
        return render(h, gauge, None, None, &[mw_component::Capability::SnowCache]);
    };

    // Two reads, because Q-8 says the depth and the totals can come from two stations. One
    // when they are the same station, which is every request that names none.
    let totals = match gauge_key_of(gauge) {
        Some(key) => match cache.totals(key).await {
            Ok(totals) => totals,
            Err(e) => return store_failed(&e),
        },
        None => None,
    };
    let depth = match depth_station {
        Some(station) if Some(&station.key) != gauge_key_of(gauge) => {
            match cache.totals(&station.key).await {
                Ok(row) => row,
                Err(e) => return store_failed(&e),
            }
        }
        // The same station, so the row already read carries the depth.
        _ => totals,
    };
    render(h, gauge, totals, depth, &[])
}

/// The gauge's key, when the website's binding resolves to a configured station.
fn gauge_key_of(gauge: Option<&StationView>) -> Option<&mw_types::StationKey> {
    gauge.map(|s| &s.key)
}

/// The response body: the gauge's name, the requested station's depth, the gauge's totals.
fn render(
    h: &Handlers,
    gauge: Option<&StationView>,
    totals: Option<SnowTotals>,
    depth: Option<SnowTotals>,
    missing: &[mw_component::Capability],
) -> Response<Body> {
    let totals = totals.unwrap_or_default();
    let dto = SnowResponse {
        // **Q-8**: always the website's gauge, by NAME -- the device name, which is what
        // `stationname` is everywhere on this surface.
        station_name: gauge.map(|s| s.name.clone()).unwrap_or_default(),
        snow_depth: zf(Some(depth.unwrap_or_default().depth)),
        snow_today: zf(Some(totals.today)),
        snow_last24: zf(Some(totals.last24)),
        snow_last72: zf(Some(totals.last72)),
        snow_season: zf(Some(totals.season)),
        snow_storm: zf(Some(totals.storm)),
        snowfall_rate: zf(Some(totals.fall_rate)),
    };
    let body = match to_json_line(&dto) {
        Ok(bytes) => bytes,
        // Go's own `error encoding snow data` branch, which is unreachable for a struct of
        // eight scalars and is the contract's bytes for it.
        Err(_) => {
            return reply::text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "error encoding snow data",
            )
        }
    };

    let response = reply::json(StatusCode::OK, Cache::NoStore, body);
    // **DEV-1**: an `ETag` over the identity of these bytes, which is the gauge and when
    // the cache was written. `computed_at` is read for this and for nothing else.
    let Some(station) = gauge else {
        return response;
    };
    let etag = ETag::new(
        &station.key,
        totals.computed_at,
        PayloadRev::of(
            h.renderer.revision_for_etag(),
            mw_component::Generation::ZERO,
        ),
    );
    let staleness = if missing.is_empty() {
        mw_component::Staleness::Fresh
    } else {
        mw_component::Staleness::Degraded {
            missing: missing.iter().copied().collect(),
            since: chrono::Utc::now(),
        }
    };
    let (mut parts, body) = response.into_parts();
    if let Ok(additive) = reply::additive(
        Response::builder(),
        &etag,
        chrono::Utc::now()
            .signed_duration_since(totals.computed_at)
            .num_milliseconds() as f64
            / 1000.0,
        &staleness,
    )
    .body(())
    {
        for (name, value) in additive.headers() {
            parts.headers.insert(name, value.clone());
        }
    }
    Response::from_parts(parts, body)
}

/// Go's `error fetching readings from DB`, which is this endpoint's own 500 message.
fn store_failed(e: &mw_component::StoreError) -> Response<Body> {
    tracing::warn!(
        err = %mw_types::DisplayChain(e),
        "the snow cache could not be read, so /snow answers its 500"
    );
    reply::text(
        StatusCode::INTERNAL_SERVER_ERROR,
        "error fetching readings from DB",
    )
}
