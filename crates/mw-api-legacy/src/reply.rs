//! The frozen response conventions: the header set, the four error-body shapes, and the
//! two additive headers.
//!
//! # Four error-body conventions, and they are all in the contract
//!
//! `docs/legacy-api-contract.md` §5 enumerates them, and the conformance suite asserts the
//! bytes of each:
//!
//! | Shape | `Content-Type` | Body | Where |
//! |---|---|---|---|
//! | plain text | `text/plain; charset=utf-8` | the message, newline-terminated | every endpoint's `400`/`403`/`404`/`500` |
//! | JSON labelled as text | `text/plain; charset=utf-8` | `{"error":…}` + `\n` | `/forecast`'s bad `period` and unknown station **name** (Q-10) |
//! | empty | **absent** | zero bytes | three `/forecast` paths (Q-11) |
//! | JSON | `application/json` | `{"error":…,"message":…}` + `\n` | the `503` when no website resolves |
//!
//! The second is not a mistake to tidy. `w.WriteHeader(400)` runs **before**
//! `formatter.WriteResponse`, so Go has already committed the content type its sniffer
//! chose; the body is valid JSON and the header says otherwise, and a client that switches
//! on the content type sees text.
//!
//! # `Vary: Accept-Encoding` is set here, explicitly, and that is deliberate
//!
//! Every one of the 70 fixtures asserts it, including the ones with no body and no content
//! type -- it comes from `gorilla/handlers.CompressHandler`, which wraps every route. This
//! crate sets it on every response instead, because the runner compares a header's **value**
//! exactly and `tower_http`'s `CompressionLayer` writes `vary: accept-encoding` in lower
//! case. **When M4-3 adds compression it must not also add a second `Vary`**: the value has
//! to stay `Accept-Encoding`, capitalised, and appending rather than replacing would make
//! it `Accept-Encoding, accept-encoding`.
//!
//! # The two additive headers (DEV-1, DEV-5)
//!
//! `ETag` on `/latest` and `/snow`, and `X-MW-Snapshot-Age` / `X-MW-Degraded` during a
//! database outage. Both are invisible to the conformance suite by a **verified property of
//! the runner** rather than by hope: `contract/conformance/src/lib.rs` iterates each
//! fixture's `assert.headers` as an allowlist and never enumerates a response's headers,
//! which is also why `Date` has never been a problem. What must not happen is "fixing"
//! `Cache-Control`, which is frozen at `no-cache, no-store, must-revalidate` -- a client
//! honouring `no-store` never sends `If-None-Match`, so the `304` simply never fires for
//! the iOS app as it ships, and that is fine.

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, Response, StatusCode};
use mw_component::{ETag, Staleness};

/// `X-MW-Snapshot-Age`: how old the reading behind a snapshot is, in seconds (**DEV-5**).
pub const SNAPSHOT_AGE: HeaderName = HeaderName::from_static("x-mw-snapshot-age");
/// `X-MW-Degraded`: which capabilities were not answering when the payload was folded.
pub const DEGRADED: HeaderName = HeaderName::from_static("x-mw-degraded");

/// What `Cache-Control` the endpoint sets, which is per endpoint and frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    /// `/latest` and `/snow`: `no-cache, no-store, must-revalidate`.
    NoStore,
    /// `/span` (the data interval minus one second) and `/almanac` (3600).
    MaxAge(u32),
    /// `/stationinfo`, `/forecast` and `/api/stations` set none at all.
    Unset,
}

impl Cache {
    fn value(self) -> Option<HeaderValue> {
        match self {
            Cache::NoStore => Some(HeaderValue::from_static(
                "no-cache, no-store, must-revalidate",
            )),
            // `max-age=<n>` is recomputed per request on `/span`, which is why the runner
            // compares it as a pattern rather than against the fixture's literal.
            Cache::MaxAge(n) => HeaderValue::try_from(format!("max-age={n}")).ok(),
            Cache::Unset => None,
        }
    }
}

/// `Vary: Accept-Encoding` -- see the module documentation.
fn vary(builder: axum::http::response::Builder) -> axum::http::response::Builder {
    builder.header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
}

/// A JSON body, newline-terminated, with CORS and the endpoint's `Cache-Control`.
///
/// `Access-Control-Allow-Origin: *` on every JSON response of this surface, which is what
/// ships: the public API is unauthenticated and stays that way (**D-35**'s own note), so
/// the wildcard is not the hazard it would be on the management API -- where A-W5-3
/// replaces it with an exact-match allowlist.
pub fn json(status: StatusCode, cache: Cache, body: Vec<u8>) -> Response<Body> {
    let mut builder = vary(Response::builder().status(status))
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .header(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    if let Some(value) = cache.value() {
        builder = builder.header(header::CACHE_CONTROL, value);
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| fallback(status))
}

/// Go's `http.Error`: the message, a newline, and `text/plain; charset=utf-8`.
///
/// `X-Content-Type-Options: nosniff` is what Go also sets and is **not** reproduced: no
/// fixture asserts it, and adding a header a client might switch on is a change this item
/// has no reason to make. Recorded rather than overlooked.
pub fn text(status: StatusCode, message: &str) -> Response<Body> {
    let mut body = String::with_capacity(message.len() + 1);
    body.push_str(message);
    body.push('\n');
    vary(Response::builder().status(status))
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .body(Body::from(body))
        .unwrap_or_else(|_| fallback(status))
}

/// A JSON body served as `text/plain` -- Q-10, `/forecast`'s bad `period` and its unknown
/// station name.
///
/// The caller passes the JSON text because there are exactly two of them and both are
/// one-key objects whose bytes the fixtures assert; building them through `serde_json`
/// would introduce an escaping question the contract has already answered.
pub fn json_as_text(status: StatusCode, json_body: &str) -> Response<Body> {
    text(status, json_body)
}

/// Zero bytes, no `Content-Type`, no trailing newline -- Q-11, the three `/forecast` paths
/// that call `w.WriteHeader(...)` and return.
///
/// `Body::empty()` rather than `String::new()`, which `axum` would label
/// `text/plain; charset=utf-8`: the **absence** of the header is what three fixtures assert.
pub fn empty(status: StatusCode) -> Response<Body> {
    vary(Response::builder().status(status))
        .body(Body::empty())
        .unwrap_or_else(|_| fallback(status))
}

/// The `503` for a request that resolves to no website at all.
///
/// A JSON body with two keys, and the only `503` the frozen surface has. **It is
/// unreachable on a configured node and that is D-48's subject**: this fires when
/// `config.website` is empty, which is a configuration with no public website rather than a
/// database that is down. A `503` because a *capability* is missing is the response D-48
/// forbids on these paths, and the component's declarations are what make it unreachable.
///
/// The message differs per endpoint in the Go tree (`Weather data is not available…` on
/// `/latest` and `/span`, `Forecast data is not available…` on `/forecast`), which the
/// caller supplies.
pub fn no_website(message: &str) -> Response<Body> {
    // Two keys, in this order, assembled rather than serialised: the body is three string
    // literals and a message, and the only escaping it can need is of a message this crate
    // chose.
    let body =
        format!("{{\"error\":\"No weather websites configured\",\"message\":\"{message}\"}}\n");
    json(
        StatusCode::SERVICE_UNAVAILABLE,
        Cache::Unset,
        body.into_bytes(),
    )
}

/// Attach the additive headers a snapshot-backed response carries (**DEV-1**, **DEV-5**).
///
/// `ETag` always; the two `X-MW-*` only when the snapshot is degraded, because a header
/// that is always present carries no signal and `X-MW-Degraded: ` would be one.
///
/// **The staleness never reaches the frozen body**, which is the whole of **D-36**: there
/// is no field for it, inventing one is a contract change, and this is its only path to a
/// client.
pub fn additive(
    builder: axum::http::response::Builder,
    etag: &ETag,
    age_seconds: f64,
    staleness: &Staleness,
) -> axum::http::response::Builder {
    let mut builder = match HeaderValue::try_from(etag.as_str()) {
        Ok(value) => builder.header(header::ETAG, value),
        // `ETag::new` escapes the station key into RFC 9110's `etagc`, so this is
        // unreachable. Dropping the header rather than failing the response is the right
        // answer for an unreachable branch on a frozen route: an absent additive header
        // costs a `304` nobody asks for, and a 5xx costs the response (**D-48**).
        Err(_) => builder,
    };
    if !staleness.is_fresh() {
        if let Ok(value) = HeaderValue::try_from(format!("{age_seconds:.0}")) {
            builder = builder.header(SNAPSHOT_AGE, value);
        }
        let missing = staleness
            .missing()
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(",");
        if let Ok(value) = HeaderValue::try_from(missing) {
            builder = builder.header(DEGRADED, value);
        }
    }
    builder
}

/// The response a `Response::builder()` that could not be built falls back to.
///
/// Unreachable: every header name and value above is either a constant or has already been
/// validated. It exists because the alternative is an `expect` on a frozen route, and
/// `docs/service-architecture.md` §1.7's whole subject is that this layer must not be the
/// one that panics.
fn fallback(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use mw_component::{Capability, PayloadRev};
    use mw_types::StationKey;

    fn headers(r: &Response<Body>) -> HeaderMap {
        r.headers().clone()
    }

    async fn body_of(r: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(r.into_body(), 64 * 1024)
            .await
            .expect("a small body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn a_plain_text_error_is_the_message_and_one_newline() {
        let r = text(StatusCode::NOT_FOUND, "station not found");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let h = headers(&r);
        assert_eq!(h[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(h[header::VARY], "Accept-Encoding");
        assert_eq!(body_of(r).await, "station not found\n");
    }

    /// Q-11: the absence of `Content-Type` is what three fixtures assert, so it is asserted
    /// here too -- `axum` labels a `String` body and would quietly satisfy nothing.
    #[tokio::test]
    async fn an_empty_reply_has_no_content_type_and_no_newline() {
        let r = empty(StatusCode::NOT_FOUND);
        let h = headers(&r);
        assert!(
            !h.contains_key(header::CONTENT_TYPE),
            "Q-11: three /forecast paths return zero bytes and no Content-Type"
        );
        assert_eq!(h[header::VARY], "Accept-Encoding");
        assert_eq!(body_of(r).await, "");
    }

    /// Q-10: a JSON body wearing a `text/plain` label.
    #[tokio::test]
    async fn a_forecast_period_error_is_json_labelled_as_text() {
        let r = json_as_text(
            StatusCode::BAD_REQUEST,
            "{\"error\":\"Invalid period parameter - must be a number\"}",
        );
        assert_eq!(
            headers(&r)[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            body_of(r).await,
            "{\"error\":\"Invalid period parameter - must be a number\"}\n"
        );
    }

    #[tokio::test]
    async fn a_json_reply_carries_cors_and_the_endpoints_cache_control() {
        let r = json(StatusCode::OK, Cache::NoStore, b"{}\n".to_vec());
        let h = headers(&r);
        assert_eq!(h[header::CONTENT_TYPE], "application/json");
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert_eq!(
            h[header::CACHE_CONTROL],
            "no-cache, no-store, must-revalidate"
        );
        assert_eq!(h[header::VARY], "Accept-Encoding");

        let r = json(StatusCode::OK, Cache::MaxAge(299), b"[]\n".to_vec());
        assert_eq!(headers(&r)[header::CACHE_CONTROL], "max-age=299");

        let r = json(StatusCode::OK, Cache::Unset, b"{}\n".to_vec());
        assert!(
            !headers(&r).contains_key(header::CACHE_CONTROL),
            "/stationinfo, /forecast and /api/stations set none"
        );
    }

    #[tokio::test]
    async fn the_no_website_reply_is_json_with_two_keys_in_order() {
        let r = no_website(
            "Weather data is not available until at least one weather website is configured",
        );
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers(&r)[header::CONTENT_TYPE], "application/json");
        let body = body_of(r).await;
        assert!(
            body.starts_with("{\"error\":\"No weather websites configured\",\"message\":\""),
            "{body}"
        );
        assert!(body.ends_with("}\n"), "{body}");
    }

    /// **DEV-5**: the two `X-MW-*` headers appear only when the snapshot is degraded, and
    /// the `ETag` always. A fresh snapshot carrying `X-MW-Degraded: ` would be a header
    /// with no signal in it.
    #[test]
    fn the_additive_headers_appear_only_where_they_mean_something() {
        let station = StationKey::new("davis-full");
        let etag = ETag::new(
            &station,
            chrono::Utc::now(),
            PayloadRev::of(1, mw_component::Generation::ZERO),
        );

        let fresh = additive(Response::builder(), &etag, 2.0, &Staleness::Fresh)
            .body(Body::empty())
            .expect("builds");
        assert_eq!(fresh.headers()[header::ETAG], etag.as_str());
        assert!(!fresh.headers().contains_key(SNAPSHOT_AGE));
        assert!(!fresh.headers().contains_key(DEGRADED));

        let degraded = Staleness::Degraded {
            missing: smallvec::smallvec![Capability::WindowedView, Capability::SnowCache],
            since: chrono::Utc::now(),
        };
        let r = additive(Response::builder(), &etag, 61.4, &degraded)
            .body(Body::empty())
            .expect("builds");
        assert_eq!(r.headers()[SNAPSHOT_AGE], "61");
        assert_eq!(r.headers()[DEGRADED], "WindowedView,SnowCache");
    }
}
