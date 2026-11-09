//! `/stationinfo` -- and **DEV-10**, the one signed deviation on the frozen surface.
//!
//! Takes no parameters: the website is selected entirely by the `Host` header.
//!
//! # The hazard this endpoint carried, and the two characters that close it
//!
//! `about_text` and `weather_device` are `omitempty` in Go and **non-optional** in the iOS
//! client. A website with no about text or no bound device therefore returns a body
//! `JSONDecoder` cannot decode **at all** -- the app loses the station list, the snow
//! device, the air-quality device and the radar token, not just the missing field -- and any
//! portal website is permanently in that state. `docs/reconciliation.md` **O-5**, option
//! (b), concurred by the owner: emit both keys. [`crate::dto::StationInfo`] is where the
//! two `omitempty`s are gone and why.
//!
//! The deviation is **additive from the client's point of view** -- a present key can only
//! help a non-optional property -- and the website guards both reads, so nothing that works
//! today stops working. It changes bytes, which is why it needed a signature rather than an
//! agent's judgement, and why the two affected fixtures carry a `deviation` block that the
//! Rust-target replay applies and the Go-target replay does not.
//!
//! # What `stations` lists
//!
//! **Every station with a location, enabled or not**, including the snow gauge and the
//! air-quality monitor -- the iOS client filters those out itself. The `type` is the
//! **configured** device type (`campbellscientific`), which is not the driver's
//! self-reported `stationtype` on `/latest` (`campbell`): `docs/legacy-api-contract.md`
//! §3.1 is explicit that the two sets must not be unified.

use axum::body::Body;
use axum::http::{Response, StatusCode};

use crate::component::Handlers;
use crate::dto::{to_json_line, zf64, StationInfo, StationInfoItem};
use crate::reply::{self, Cache};

/// The `503` message, which is `/latest`'s.
const NO_WEBSITE: &str =
    "Weather data is not available until at least one weather website is configured";

/// Serve `/stationinfo`.
pub fn handle(h: &Handlers, host: &str) -> Response<Body> {
    let website = match h.website(host, NO_WEBSITE) {
        Ok(w) => w,
        Err(response) => return *response,
    };

    let stations: Vec<StationInfoItem> = h
        .directory
        .stations()
        .iter()
        .filter(|s| s.has_location())
        .map(|s| StationInfoItem {
            id: s.id,
            name: s.name.clone(),
            kind: s.kind.clone(),
        })
        .collect();

    // The website's primary device, resolved to its surrogate id. **DEV-10**: `0` rather
    // than an absent key when there is none.
    let weather_device = website
        .primary_station
        .as_ref()
        .and_then(|key| h.directory.station_by_key(key))
        .map(|s| s.id)
        .unwrap_or(0);

    // The two optional device NAMES, which are `*string` in Go and absent when unset. Not
    // part of DEV-10: the Swift properties for both are optional, so an absent key is
    // decodable and the hazard does not apply.
    let snow_device = website
        .snow_station
        .as_ref()
        .filter(|_| website.snow_enabled)
        .and_then(|key| h.directory.station_by_key(key))
        .map(|s| s.name.clone());
    let air_quality_device = website
        .air_quality_station
        .as_ref()
        .filter(|_| website.air_quality_enabled)
        .and_then(|key| h.directory.station_by_key(key))
        .map(|s| s.name.clone());

    // The website's coordinates are its PRIMARY DEVICE's, which is what the Go handler
    // reads: a website is not a place and has no columns for one.
    let primary = website
        .primary_station
        .as_ref()
        .and_then(|key| h.directory.station_by_key(key));

    let dto = StationInfo {
        website_name: website.name.clone(),
        about_text: website.about_text.clone(),
        stations,
        weather_device,
        snow_device,
        air_quality_device,
        latitude: zf64(primary.and_then(|s| s.latitude)),
        longitude: zf64(primary.and_then(|s| s.longitude)),
        radar_enabled: website.radar_enabled,
        radar_token: website.radar_token.clone(),
    };

    match to_json_line(&dto) {
        // No `Cache-Control` at all, which is what this endpoint sets.
        Ok(body) => reply::json(StatusCode::OK, Cache::Unset, body),
        Err(_) => reply::text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "error encoding station info",
        ),
    }
}
