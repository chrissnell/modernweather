//! **D-48's byte-level half: with the database down, every frozen route answers its
//! contract response and never `503`.**
//!
//! This is the obligation M4-1 recorded on M4-2 and the one `cargo xtask gate conformance`
//! **cannot** establish: that harness replays against a running database, so a green
//! conformance run says nothing about this. M4-1 tested the precondition -- a consumer whose
//! only hard need is `LatestSnapshot` reaches `Running` with `db:postgres` planned and never
//! publishing (`crates/modernweather/tests/snapshot_degraded.rs`) -- and the responses
//! arrive with the routes, which is here.
//!
//! # How "the database is down" is expressed
//!
//! Exactly as the supervisor expresses it: the three soft capability slots are **empty**.
//! `BuildCx::deferred` hands the handlers a `Deferred` over each one and they load `None`
//! per request, which is the same state a pool that has not connected -- or one whose probe
//! loop has revoked it -- produces. Nothing here mocks a failing query; the handle is
//! absent, which is the stronger case.
//!
//! The snapshot is **populated**, because that is the whole of **D-36**: `snapshot:latest`
//! folds arrivals in memory, so a database outage leaves `/latest` serving its last good
//! window-derived values rather than nothing.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use mw_api_legacy::config::{Directory, Listener, StationView, WebsiteView};
use mw_api_legacy::{Handlers, RouteState, SpanCache};
use mw_component::{
    BuildCx, Capability, CapabilityAccess, CapabilityPublisher, CapabilitySlot, ComponentId, Cx,
    ForecastStore, Generation, LatestSnapshot, Need, PayloadRev, Snapshot, SnapshotKey,
    SnapshotRender, SnapshotSource, SnapshotTable, SnowCache, Staleness, StationMeta, Store,
};
use mw_types::{Reading, StationKey, WebsiteId};
use tower::ServiceExt as _;

/// A capability bus that routes nothing. `api:legacy` publishes `HttpRoutes` in `build`,
/// which this test does not reach -- it builds the handler set directly, because what is
/// under test is the handlers' behaviour and not the component's lifecycle.
#[derive(Debug)]
struct Nowhere;

impl CapabilityPublisher for Nowhere {
    fn publish(
        &self,
        _provider: &ComponentId,
        _capability: Capability,
        _handle: Arc<dyn std::any::Any + Send + Sync>,
    ) {
    }

    fn revoke(&self, _provider: &ComponentId, _capability: Capability) {}
}

/// The snapshot table, with one populated key.
#[derive(Debug)]
struct OneKey(Arc<SnapshotTable>);

impl LatestSnapshot for OneKey {
    fn get(&self, key: &SnapshotKey) -> Option<Arc<Snapshot>> {
        self.0.get(key).map(Arc::clone)
    }

    fn subscribe(
        &self,
        _key: &SnapshotKey,
    ) -> Option<tokio::sync::watch::Receiver<Option<Arc<Snapshot>>>> {
        None
    }

    fn table(&self) -> Arc<SnapshotTable> {
        Arc::clone(&self.0)
    }
}

fn website() -> WebsiteView {
    WebsiteView {
        id: WebsiteId::new(1),
        name: "Suncrest Live Weather".into(),
        hostname: "localhost".into(),
        page_title: "Suncrest Live Weather".into(),
        about_text: "<p>On a ridge.</p>".into(),
        primary_station: Some(StationKey::new("davis-full")),
        snow_enabled: true,
        snow_station: Some(StationKey::new("snowgauge")),
        air_quality_enabled: false,
        air_quality_station: None,
        is_portal: false,
        radar_enabled: true,
        radar_token: "contract-radar-token".into(),
    }
}

fn station(id: i64, key: &str, kind: &str) -> StationView {
    StationView {
        id,
        key: StationKey::new(key),
        name: key.into(),
        kind: kind.into(),
        enabled: true,
        latitude: Some(39.9),
        longitude: Some(-105.5),
        timezone: Some("America/Denver".into()),
    }
}

/// A capability slot with one provider, or none.
///
/// The sender is leaked on purpose: a dropped sender closes the channel, and a `Deferred`
/// over a closed channel is a different state from one over an empty slot -- which is the
/// state under test.
fn slot(
    capability: Capability,
    provider: Option<mw_component::Published>,
) -> (
    Capability,
    tokio::sync::watch::Receiver<Arc<CapabilitySlot>>,
) {
    let (tx, rx) = tokio::sync::watch::channel(Arc::new(CapabilitySlot {
        providers: provider.into_iter().collect(),
    }));
    std::mem::forget(tx);
    (capability, rx)
}

/// The handler set, with every soft capability slot **empty**.
fn handlers() -> Arc<Handlers> {
    let renderer = Arc::new(mw_api_legacy::Renderer::default());

    // One folded snapshot, which is what `snapshot:latest` leaves behind when the database
    // goes away mid-process.
    let key = SnapshotKey::new(WebsiteId::new(1), StationKey::new("davis-full"));
    let source = SnapshotSource {
        key,
        website: mw_component::WebsiteView {
            id: WebsiteId::new(1),
            name: "Suncrest Live Weather".into(),
            primary_station: Some(StationKey::new("davis-full")),
            snow_enabled: true,
            snow_station: Some(StationKey::new("snowgauge")),
        },
        reading: Reading {
            time: chrono::Utc::now(),
            station_id: "davis-full".into(),
            station_name: Some("davis-full".into()),
            station_type: Some("davis".into()),
            out_temp: Some(62.7),
            ..Reading::default()
        },
        meta: StationMeta {
            latitude: Some(39.9),
            longitude: Some(-105.5),
            altitude_m: Some(1800.0),
            snow_base_distance_mm: None,
        },
        // **Degraded**, which is the state a database outage leaves the windows in: the
        // provider's last answer, or none at all.
        windows: mw_component::Resolved::new(chrono::Utc::now(), []),
    };
    let snapshot = Snapshot::render(
        source,
        Staleness::Degraded {
            missing: smallvec::smallvec![Capability::WindowedView],
            since: chrono::Utc::now(),
        },
        chrono::Utc::now(),
        PayloadRev::of(renderer.revision(), Generation::ZERO),
        Arc::clone(&renderer) as Arc<dyn SnapshotRender>,
    )
    .expect("the DTO serialises");
    let mut table = SnapshotTable::new();
    table.insert(Arc::new(snapshot));
    let latest: Arc<dyn LatestSnapshot> = Arc::new(OneKey(Arc::new(table)));

    let access = CapabilityAccess::new(
        Arc::from(vec![
            Need::hard(Capability::LatestSnapshot),
            Need::soft(Capability::ReadingStore),
            Need::soft(Capability::ForecastStore),
            Need::soft(Capability::SnowCache),
        ]),
        Arc::from(Vec::<Capability>::new()),
        vec![
            // The one hard slot, populated: `LatestSnapshot` is never absent while the
            // component runs, which is what makes `/latest`'s zero queries structural.
            slot(
                Capability::LatestSnapshot,
                Some(mw_component::Published {
                    provider: ComponentId::new("snapshot:latest").expect("a valid id"),
                    handle: Arc::new(latest) as Arc<dyn std::any::Any + Send + Sync>,
                }),
            ),
            slot(Capability::ReadingStore, None),
            slot(Capability::ForecastStore, None),
            slot(Capability::SnowCache, None),
        ],
        Arc::new(Nowhere) as Arc<dyn CapabilityPublisher>,
    );
    let (tx, generations) = tokio::sync::watch::channel(Generation::ZERO);
    std::mem::forget(tx);
    let build = BuildCx::new(
        Cx::new(
            ComponentId::new("api:legacy").expect("a valid id"),
            tokio_util::sync::CancellationToken::new(),
            Generation::ZERO,
            generations,
        ),
        access,
    );

    Arc::new(Handlers {
        directory: Arc::new(Directory::new(
            vec![website()],
            vec![
                station(1, "davis-full", "davis"),
                station(3, "snowgauge", "snowgauge"),
            ],
            Listener {
                port: 18080,
                tls: false,
            },
        )),
        renderer,
        snapshot: build
            .hard::<Arc<dyn LatestSnapshot>>()
            .expect("the hard slot is populated"),
        store: build.deferred::<Arc<dyn Store>>().expect("declared soft"),
        forecasts: build
            .deferred::<Arc<dyn ForecastStore>>()
            .expect("declared soft"),
        snow: build
            .deferred::<Arc<dyn SnowCache>>()
            .expect("declared soft"),
        span_cache: SpanCache::new(),
    })
}

/// One request against the real router, over the real state slot.
async fn get(path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let state = Arc::new(RouteState::new());
    state.install_for_test(handlers());
    let router = mw_api_legacy::routes(state);
    let response = router
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .expect("a valid request"),
        )
        .await
        .expect("the router is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a small body");
    (
        status,
        headers,
        String::from_utf8(bytes.to_vec()).expect("utf-8"),
    )
}

/// **The invariant.** Every frozen route answers its contract response with the database
/// gone, and not one of them is a `503`.
#[tokio::test]
async fn with_the_database_down_every_frozen_route_answers_its_contract_response() {
    // `/latest` -- unaffected, because the snapshot is memory. The last good values are
    // served and the staleness goes out through the additive header, never the body
    // (**D-36**).
    let (status, headers, body) = get("/latest?station=davis-full").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "no-cache, no-store, must-revalidate"
    );
    assert!(headers.contains_key("x-mw-degraded"), "DEV-5: {headers:?}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(
        parsed.as_object().expect("an object").len(),
        78,
        "the full key set, degraded or not"
    );
    assert!(
        !body.contains("degraded") && !body.contains("stale"),
        "D-36: the staleness must not reach the frozen body: {body}"
    );

    // `/span` -- the store is gone, so the response is the empty array the contract already
    // has for a station with no data in the window, with the frozen `Cache-Control`.
    let (status, headers, body) = get("/span/24h?station=davis-full").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "[]\n");
    assert_eq!(headers[header::CACHE_CONTROL], "max-age=60");

    // `/snow` -- the cache is gone, so seven zeros under the website's gauge name, which is
    // byte-for-byte what a gauge that has never reported produces.
    let (status, _, body) = get("/snow").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "{\"stationname\":\"snowgauge\",\"snowdepth\":0,\"snowtoday\":0,\"snowlast24\":0,\
         \"snowlast72\":0,\"snowseason\":0,\"snowstorm\":0,\"snowfallrate\":0}\n"
    );

    // `/stationinfo` -- configuration only, so a database has nothing to do with it.
    let (status, _, body) = get("/stationinfo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("\"website_name\":\"Suncrest Live Weather\""),
        "{body}"
    );
    assert!(body.contains("\"about_text\":"), "DEV-10: {body}");
    assert!(body.contains("\"weather_device\":1"), "DEV-10: {body}");

    // `/forecast` -- the store is gone, so the no-record `404`: zero bytes and **no**
    // `Content-Type` (Q-11), which is the response the contract has for it.
    let (status, headers, body) = get("/forecast/24").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());
    assert!(!headers.contains_key(header::CONTENT_TYPE), "{headers:?}");
}

/// The coupled pair, for completeness: neither becomes a `503` either.
#[tokio::test]
async fn the_coupled_routes_degrade_rather_than_failing() {
    let (status, headers, body) = get("/almanac?station=davis-full").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{}\n", "the empty-cache response the contract has");
    assert_eq!(headers[header::CACHE_CONTROL], "max-age=3600");

    // `/api/stations` is a `403` on a non-portal website whatever the database is doing.
    let (status, _, body) = get("/api/stations").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "stations API not available for this website\n");
}

/// **Not one `503`, across every frozen and coupled path, with every soft capability
/// absent.** The blanket form of the assertion above: a route added later that answered
/// `503` on a database outage would fail this without anybody having to remember D-48.
#[tokio::test]
async fn no_frozen_or_coupled_route_can_answer_503_with_the_database_down() {
    for path in [
        "/latest",
        "/latest?station=davis-full",
        "/latest?station_id=1",
        "/latest?station=does-not-exist",
        "/latest?station_id=abc",
        "/span/24h?station=davis-full",
        "/span/1h?station=snowgauge",
        "/span/24h",
        "/span/30d?station=davis-full",
        "/snow",
        "/snow?station_id=3",
        "/snow?station=davis-full",
        "/stationinfo",
        "/forecast/24",
        "/forecast/240?period=3",
        "/forecast/99999",
        "/almanac?station=davis-full",
        "/almanac",
        "/api/stations",
    ] {
        let (status, _, _) = get(path).await;
        assert_ne!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "D-48: `{path}` answered 503 with the database down"
        );
        assert!(
            !status.is_server_error(),
            "`{path}` answered {status} with the database down, and a 5xx on a frozen path \
             is the family of outcome D-48 exists to make unobservable"
        );
    }
}

/// The one place a `503` does exist, which is the state D-48's structural half makes
/// unreachable: the route table is mounted and the component is **not running**.
///
/// Asserted so that the two halves are visibly different things -- the response exists, and
/// `api:legacy`'s declarations are what stop a healthy process reaching it.
#[tokio::test]
async fn an_unmounted_table_is_the_only_503() {
    let state = Arc::new(RouteState::new());
    let router = mw_api_legacy::routes(state);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/latest")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .expect("a valid request"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
