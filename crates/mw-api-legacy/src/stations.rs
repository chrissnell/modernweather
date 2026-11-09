//! `/api/stations` -- coupled class, and the portal's own endpoint.
//!
//! `403` on every website but a portal, which is the first thing checked. The body is an
//! array of every station **with a location**, enabled or not, each one optionally carrying
//! the website it is bound to.
//!
//! # Three predicates, each one a line of the Go handler
//!
//! 1. **A station at `latitude == 0 AND longitude == 0` is excluded.** The Go columns are
//!    `NOT NULL DEFAULT 0`, so "no location" and "the Gulf of Guinea" are one value there;
//!    here they are nullable and `None` is excluded as well, which is the same station
//!    under a better spelling ([`crate::config::StationView::has_location`]).
//! 2. **A disabled station is included**, with `enabled: false`. The website's map draws it
//!    greyed out.
//! 3. **The nested `website` block appears only for a station bound to a non-portal website
//!    that has a hostname.** A portal has no station map of its own to link to, and a
//!    website with no hostname cannot be linked to at all.
//!
//! `protocol` and `port` come from the one listener, which is why this crate is handed a
//! [`crate::config::Listener`]: the block is a URL the website builds, so it has to say
//! where this server answers.

use axum::body::Body;
use axum::http::{Response, StatusCode};

use crate::component::Handlers;
use crate::dto::{to_json_line, zf64, StationSummary, StationWebsite};
use crate::reply::{self, Cache};

/// The `503` this endpoint gives when no website resolves: `/api/stations` (`handlers.go:985`).
const NO_WEBSITE: reply::NoWebsite = reply::NO_WEBSITE_STATIONS;

/// Serve `/api/stations`.
pub fn handle(h: &Handlers, host: &str) -> Response<Body> {
    let website = match h.website(host, NO_WEBSITE) {
        Ok(w) => w,
        Err(response) => return *response,
    };
    if !website.is_portal {
        return reply::text(
            StatusCode::FORBIDDEN,
            "stations API not available for this website",
        );
    }

    let listener = h.directory.listener();
    let stations: Vec<StationSummary> = h
        .directory
        .stations()
        .iter()
        .filter(|s| s.has_location())
        .map(|s| StationSummary {
            id: s.id,
            name: s.name.clone(),
            kind: s.kind.clone(),
            latitude: zf64(s.latitude),
            longitude: zf64(s.longitude),
            enabled: s.enabled,
            website: h
                .directory
                .website_of_station(s)
                .filter(|w| !w.is_portal && !w.hostname.is_empty())
                .map(|w| StationWebsite {
                    name: w.name.clone(),
                    hostname: w.hostname.clone(),
                    page_title: w.page_title.clone(),
                    protocol: listener.protocol().to_string(),
                    port: i64::from(listener.port),
                }),
        })
        .collect();

    match to_json_line(&stations) {
        Ok(body) => reply::json(StatusCode::OK, Cache::Unset, body),
        Err(_) => reply::text(StatusCode::INTERNAL_SERVER_ERROR, "error encoding stations"),
    }
}
