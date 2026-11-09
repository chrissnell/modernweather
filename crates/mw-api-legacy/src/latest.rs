//! `/latest` -- four things, and no queries.
//!
//! **D-34**'s four: resolve the key, load the snapshot, check W1 Decision 15's ten-minute
//! recency, write the bytes. There is no fifth, and there is no query -- `api:legacy` is
//! handed [`mw_component::LatestSnapshot`] and never a store handle, so the handler
//! *cannot* query (**D-35**). The payload was serialised once, when the reading was folded
//! (**D-33**), so the last step is a refcount bump rather than a copy.
//!
//! # The 404 that is about staleness, and the one that is about configuration
//!
//! Two different bodies, and the contract distinguishes them:
//!
//! | Request | Status and body |
//! |---|---|
//! | `?station_id=` that is not an integer | `400 invalid station_id` |
//! | `?station_id=` or `?station=` naming nothing configured | `404 station not found` |
//! | a configured station whose newest reading is older than ten minutes | `404 no recent weather data available for this station` |
//!
//! The third is **the only place in the contract where staleness becomes a `404`**, and it
//! is checked here rather than in the store because the ten minutes belongs to the
//! contract (`mw_component::LatestQuery::since` says so from the other side).
//!
//! # `format=msgpack` is accepted as a parameter and ignored
//!
//! **D-69** does not carry it forward. The parameter is not rejected -- a `400` for a query
//! parameter the server used to honour is a worse break than ignoring it -- so a request
//! with it gets the JSON body. The fixture that asserts the msgpack content type is
//! `retired`-class and expected to fail; see `contract/README.md`.

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use mw_component::SnapshotKey;

use crate::component::Handlers;
use crate::params::Params;
use crate::reply::{self, Cache};

/// The `503` this endpoint gives when no website resolves: `/latest`, which reaches this through `formatter.WriteResponse` after
/// `WriteHeader`, so Go labels the JSON body `text/plain` (`handlers.go:205`).
const NO_WEBSITE: reply::NoWebsite = reply::NO_WEBSITE_LATEST;

/// Serve `/latest`.
pub fn handle(h: &Handlers, host: &str, params: &Params, headers: &HeaderMap) -> Response<Body> {
    let website = match h.website(host, NO_WEBSITE) {
        Ok(w) => w,
        Err(response) => return *response,
    };

    // `station_id` is the preferred form and `station` the legacy one; neither is required,
    // and with neither the website's primary device answers.
    let station = match params.non_empty("station_id") {
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
            None => None,
        },
    };

    // Neither parameter: the website's primary device. A website with none configured has
    // no station to answer for, which the Go handler reaches as an empty station name and
    // a query that matches nothing -- the same `404` with the same bytes.
    let key = match station {
        Some(station) => SnapshotKey::new(website.id, station.key.clone()),
        None => match &website.primary_station {
            Some(key) => SnapshotKey::new(website.id, key.clone()),
            None => return no_recent_data(),
        },
    };

    let Some(snapshot) = h.snapshot.get(&key) else {
        return no_recent_data();
    };

    // W1 Decision 15. The snapshot keeps its last good values through a database outage
    // (**D-36**), so a station that stopped *reporting* is not the same thing as a database
    // that stopped answering -- and only the first of them is a `404`.
    let now = chrono::Utc::now();
    if now.signed_duration_since(snapshot.observed_at()) > crate::render::RECENCY {
        return no_recent_data();
    }

    // **DEV-1**: `If-None-Match` is honoured through `ETag::matches`, which is RFC 9110
    // §13.1.2's weak comparison rather than string equality. `Cache-Control` is unchanged
    // and is **not** "fixed": a client honouring `no-store` never sends the header, so the
    // `304` simply never fires for the iOS app as it ships.
    let etag = snapshot.etag();
    if let Some(inm) = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if etag.matches(inm) {
            let builder = reply::additive(
                Response::builder().status(StatusCode::NOT_MODIFIED),
                etag,
                snapshot.age_seconds(now),
                snapshot.staleness(),
            );
            // `304` carries the validators and no body, and keeps the frozen
            // `Cache-Control` so that a conditional request answers the same way an
            // unconditional one does.
            return builder
                .header(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-cache, no-store, must-revalidate"),
                )
                .header(
                    axum::http::header::VARY,
                    axum::http::HeaderValue::from_static("Accept-Encoding"),
                )
                .body(Body::empty())
                .unwrap_or_else(|_| reply::empty(StatusCode::NOT_MODIFIED));
        }
    }

    let payload = snapshot.json();
    let mut response = reply::json(StatusCode::OK, Cache::NoStore, payload.to_vec());
    // The additive headers are attached after the frozen ones, which is why this rebuilds
    // rather than passing them in: `reply::json` owns the frozen set, and a function that
    // took both would make it possible to pass an additive header where a frozen one
    // belongs.
    let (mut parts, body) = response.into_parts();
    let builder = reply::additive(
        Response::builder(),
        etag,
        snapshot.age_seconds(now),
        snapshot.staleness(),
    );
    if let Ok(additive) = builder.body(()) {
        for (name, value) in additive.headers() {
            parts.headers.insert(name, value.clone());
        }
    }
    response = Response::from_parts(parts, body);
    response
}

/// The one body W1 Decision 15 produces, and the same one a key nothing has reported for
/// produces (`LatestSnapshot::get`'s own documentation says so).
fn no_recent_data() -> Response<Body> {
    reply::text(
        StatusCode::NOT_FOUND,
        "no recent weather data available for this station",
    )
}
