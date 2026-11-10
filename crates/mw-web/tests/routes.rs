//! The five handlers, over the real routers.
//!
//! **Written because `Cargo.toml` already claimed they existed.** Its `tower` dev-dependency
//! said it was there for "`ServiceExt::oneshot`, which is how the route tests drive a router
//! without binding a socket" and its `tokio-util` one for "`Cx::new` takes a
//! `CancellationToken`"; M4-3's code review found neither symbol used anywhere in the crate
//! and no route tests at all. `SiteState::install_for_test` existed for exactly this and had
//! no caller.
//!
//! What was unasserted and now is not:
//!
//! | Response | Why it matters |
//! |---|---|
//! | `/` and `/portal` as `text/html` with **no charset** | the frozen quirk, against `text/html; charset=utf-8` on `/privacy` and `/support` |
//! | `Cache-Control: no-cache` on `/config.json` | additive and per-website, so a shared cache must not reuse one website's payload for another |
//! | `500 No device configured for this website\n` | `handlers.go:869` reproduced; covered nowhere else |
//! | the `503` from an unmounted table | W2a Decision 19, and covered nowhere else |
//!
//! The content types of `/` and `/privacy` are also covered end to end by
//! `cargo xtask gate conformance`; the `500` and the `503` are not reachable from any
//! fixture.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use mw_types::{StationKey, WebsiteId};
use mw_web::{SiteState, SiteView};
use tower::ServiceExt;

/// The `localhost` website of `ci/conformance/rust-seed/01-config.sql`.
fn suncrest() -> SiteView {
    SiteView {
        id: WebsiteId::new(1),
        hostname: "localhost".into(),
        name: "Suncrest Live Weather".into(),
        page_title: "Suncrest Live Weather".into(),
        apple_app_id: "6755874087".into(),
        is_portal: false,
        radar_enabled: true,
        radar_token: "contract-radar-token".into(),
        snow_enabled: true,
        snow_device: "snowgauge".into(),
        air_quality_enabled: true,
        air_quality_device: "aqmon".into(),
        air_quality_device_id: 4,
        primary_station: Some(StationKey::new("davis-full")),
        primary_station_name: "davis-full".into(),
        primary_station_id: 1,
        latitude: Some(39.900),
        longitude: Some(-105.500),
        aeris_enabled: true,
    }
}

fn portal() -> SiteView {
    SiteView {
        id: WebsiteId::new(3),
        hostname: "portal.test".into(),
        name: "Weather Portal".into(),
        is_portal: true,
        radar_enabled: false,
        radar_token: String::new(),
        snow_enabled: false,
        snow_device: String::new(),
        air_quality_enabled: false,
        air_quality_device: String::new(),
        air_quality_device_id: 0,
        primary_station: None,
        primary_station_name: String::new(),
        primary_station_id: 0,
        latitude: None,
        longitude: None,
        aeris_enabled: false,
        ..suncrest()
    }
}

/// One request against a website's real table, with the slot filled.
async fn get(site: &SiteView, path: &str) -> (StatusCode, Vec<(String, String)>, String) {
    let state = Arc::new(SiteState::new());
    state.install_for_test(site, "0.1.0");
    serve(site, state, path).await
}

/// The same, with the slot **empty** -- which is what a component that is not `Running`
/// looks like.
async fn get_unmounted(site: &SiteView, path: &str) -> (StatusCode, Vec<(String, String)>, String) {
    serve(site, Arc::new(SiteState::new()), path).await
}

async fn serve(
    site: &SiteView,
    state: Arc<SiteState>,
    path: &str,
) -> (StatusCode, Vec<(String, String)>, String) {
    let table = mw_web::site_table(site, state).expect("a legal component id");
    let router = mw_http::RouteTable::into_router_for_test(table);
    let resp = router
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a legal request"),
        )
        .await
        .expect("the router is infallible");
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("a readable body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn header_of(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// **`text/html` with no charset.** The frozen quirk: `ServeWeatherWebsiteTemplate` and
/// `ServePortal` set it, `ServePrivacy` and `ServeSupport` set `; charset=utf-8`, and
/// `html-weather-station` and `html-privacy` assert both.
#[tokio::test]
async fn a_rendered_page_is_text_html_with_no_charset() {
    let (status, headers, body) = get(&suncrest(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_of(&headers, "content-type").as_deref(),
        Some("text/html")
    );
    assert_eq!(
        header_of(&headers, "vary").as_deref(),
        Some("Accept-Encoding")
    );
    assert!(body.starts_with("<!DOCTYPE html>"), "{:?}", &body[..40]);

    // And a portal serves the same bytes from `/` and `/portal` (**D-75**).
    let portal = portal();
    let (root_status, _, root) = get(&portal, "/").await;
    let (portal_status, portal_headers, at_portal) = get(&portal, "/portal").await;
    assert_eq!(
        (root_status, portal_status),
        (StatusCode::OK, StatusCode::OK)
    );
    assert_eq!(root, at_portal);
    assert_eq!(
        header_of(&portal_headers, "content-type").as_deref(),
        Some("text/html")
    );
    assert!(at_portal.contains("Weather Management Portal"));
}

/// A non-portal website whose `/portal` is not declared falls through to the catch-all,
/// which is how `html-portal-on-non-portal` gets Go's `404 page not found\n` without a
/// branch in a handler.
#[tokio::test]
async fn a_non_portal_website_does_not_declare_portal() {
    let (status, _, _) = get(&suncrest(), "/portal").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the site table must not answer `/portal`, so the asset tree's 404 does"
    );
    // And the portal does not declare `/config.json`, which the weather page is the only
    // consumer of.
    assert_eq!(
        get(&portal(), "/config.json").await.0,
        StatusCode::NOT_FOUND
    );
}

/// **DEV-1/D-79's additive endpoint**, including the one header a shared cache reads.
#[tokio::test]
async fn config_json_is_the_nine_values_with_no_cache() {
    let site = suncrest();
    let (status, headers, body) = get(&site, "/config.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_of(&headers, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        header_of(&headers, "cache-control").as_deref(),
        Some("no-cache"),
        "per-website and additive, so a shared cache must not reuse one website's payload"
    );
    assert_eq!(body, mw_web::config_json(&site));
}

/// `handlers.go:869`: a non-portal website with no resolvable primary station is a `500`
/// with that text, not a blank page. Reachable from no fixture.
#[tokio::test]
async fn a_website_with_no_primary_station_is_gos_500() {
    let mut orphan = suncrest();
    orphan.primary_station = None;
    orphan.radar_enabled = false;
    let (status, headers, body) = get(&orphan, "/").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        header_of(&headers, "content-type").as_deref(),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(body, "No device configured for this website\n");
}

/// **W2a Decision 19's unmounted-declared-path `503`**, which is what a declared path
/// answers between the socket opening and its component reaching `Running`. Reachable from
/// no fixture either.
///
/// **None of these paths is frozen**, so **D-48** is not in play: the five frozen patterns
/// are `api:legacy`'s in `Scope::Any`, and `mw_http::MountError::FrozenPatternShadowed`
/// refuses a per-website table that claims one.
#[tokio::test]
async fn an_unmounted_website_answers_a_bodyless_503() {
    let site = suncrest();
    for path in ["/", "/config.json"] {
        let (status, headers, body) = get_unmounted(&site, path).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert!(body.is_empty(), "{path}: {body:?}");
        assert_eq!(
            header_of(&headers, "vary").as_deref(),
            Some("Accept-Encoding"),
            "{path}"
        );
        assert!(
            header_of(&headers, "content-type").is_none(),
            "{path}: the empty `503` carries no content type"
        );
    }
    for pattern in mw_http::FROZEN_PATTERNS {
        assert!(
            !pattern.starts_with("/config.json") && pattern != "/",
            "`{pattern}` is frozen and must not be a per-website path"
        );
    }
}

/// `/privacy` and `/support` come from the shared table, bypass the website check entirely
/// (§6.3), and carry the *other* content type.
#[tokio::test]
async fn the_two_static_pages_carry_a_charset_and_need_no_website() {
    let table = mw_web::static_table().expect("a legal component id");
    let router = mw_http::RouteTable::into_router_for_test(table);
    for path in ["/privacy", "/support"] {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("a legal request"),
            )
            .await
            .expect("infallible");
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "{path}"
        );
    }
}
