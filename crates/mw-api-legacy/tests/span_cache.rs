//! **D-54's cache, against a store that records what it was asked for.**
//!
//! `/span` answers from a 60-second cache that holds the **materialised prefix only**, and
//! before this file nothing executed that path. `tests/degraded.rs` builds a `SpanCache` and
//! then leaves the store slot empty, so `span::handle` returns before touching it; the unit
//! tests in `span.rs` drive `SpanCache::get`/`put` directly and never the handler; and the
//! conformance runner replays each fixture exactly once, so a hit never happens there.
//!
//! The two properties that were therefore unguarded:
//!
//! 1. **A hit asks for the tail alone.** The handler rewrites `query.since` to `tail_start`,
//!    which makes the prefix range empty by construction. Dropping that line would serve the
//!    prefix twice and no test would have said so.
//! 2. **An incomplete prefix is not cached.** The key does not move until the bucket does,
//!    so caching a prefix the continuous aggregate had not caught up to pinned the gap for
//!    the whole TTL -- a hole one element before the live point, on every chart, renewed
//!    every minute.
//!
//! `mw_component::Span::buckets()` is the footgun the second property is about: it
//! concatenates all three fields, and rendering the cached prefix from it would put the
//! trailing bucket in the cache.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use mw_api_legacy::config::{Directory, Listener, StationView, WebsiteView};
use mw_api_legacy::{Handlers, RouteState, SpanCache};
use mw_component::{
    BoxFuture, BuildCx, Capability, CapabilityAccess, CapabilityPublisher, CapabilitySlot,
    ComponentId, Cx, ForecastStore, Generation, LatestSnapshot, Need, Snapshot, SnapshotKey,
    SnapshotTable, SnowCache, Span, SpanBucket, SpanQuery, Store, StoreError,
};
use mw_types::{StationKey, WebsiteId};
use tower::ServiceExt as _;

/// A store that records every `SpanQuery` and answers from a script.
#[derive(Debug)]
struct Recording {
    seen: Mutex<Vec<SpanQuery>>,
    answer: Mutex<Span>,
}

impl Recording {
    fn new(answer: Span) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answer: Mutex::new(answer),
        })
    }

    fn queries(&self) -> Vec<SpanQuery> {
        self.seen.lock().expect("not poisoned").clone()
    }

    fn gone<T>() -> BoxFuture<'static, Result<T, StoreError>>
    where
        T: Send + 'static,
    {
        Box::pin(async {
            Err(StoreError::unavailable(std::io::Error::other(
                "this test only drives `span`",
            )))
        })
    }
}

impl mw_component::ReadingSink for Recording {
    fn store<'a>(
        &'a self,
        _batch: &'a [mw_types::Envelope],
    ) -> BoxFuture<'a, Result<mw_component::Stored, StoreError>> {
        Self::gone()
    }
}

impl mw_component::ReadingStore for Recording {
    fn buckets<'a>(
        &'a self,
        _q: mw_component::BucketQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::Bucket>, StoreError>> {
        Self::gone()
    }
    fn totals<'a>(
        &'a self,
        _q: mw_component::TotalsQuery,
    ) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
        Self::gone()
    }
    fn series<'a>(
        &'a self,
        _q: mw_component::SeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::Bucketed>, StoreError>> {
        Self::gone()
    }
    fn mean_series<'a>(
        &'a self,
        _q: mw_component::MeanSeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::MeanBucket>, StoreError>> {
        Self::gone()
    }
    fn latest<'a>(
        &'a self,
        _q: mw_component::LatestQuery,
    ) -> BoxFuture<'a, Result<Option<mw_types::Reading>, StoreError>> {
        Self::gone()
    }
    fn span<'a>(&'a self, q: SpanQuery) -> BoxFuture<'a, Result<Span, StoreError>> {
        self.seen.lock().expect("not poisoned").push(q);
        let answer = self.answer.lock().expect("not poisoned").clone();
        Box::pin(async move { Ok(answer) })
    }
    fn almanac<'a>(
        &'a self,
        _q: mw_component::AlmanacQuery,
    ) -> BoxFuture<'a, Result<Vec<mw_component::AlmanacRecord>, StoreError>> {
        Self::gone()
    }
}

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

#[derive(Debug)]
struct NoSnapshots(Arc<SnapshotTable>);

impl LatestSnapshot for NoSnapshots {
    fn get(&self, _key: &SnapshotKey) -> Option<Arc<Snapshot>> {
        None
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

fn published(handle: Arc<dyn std::any::Any + Send + Sync>) -> mw_component::Published {
    mw_component::Published {
        provider: ComponentId::new("db:postgres").expect("a valid id"),
        handle,
    }
}

fn website() -> WebsiteView {
    WebsiteView {
        id: WebsiteId::new(1),
        name: "Suncrest Live Weather".into(),
        hostname: "localhost".into(),
        page_title: "Suncrest Live Weather".into(),
        about_text: String::new(),
        primary_station: Some(StationKey::new("davis-full")),
        snow_enabled: false,
        snow_station: None,
        air_quality_enabled: false,
        air_quality_station: None,
        is_portal: false,
        radar_enabled: false,
        radar_token: String::new(),
    }
}

fn station() -> StationView {
    StationView {
        id: 1,
        key: StationKey::new("davis-full"),
        name: "davis-full".into(),
        kind: "davis".into(),
        enabled: true,
        latitude: Some(39.9),
        longitude: Some(-105.5),
        timezone: Some("America/Denver".into()),
    }
}

fn handlers(store: Arc<Recording>) -> Arc<Handlers> {
    let access = CapabilityAccess::new(
        Arc::from(vec![
            Need::hard(Capability::LatestSnapshot),
            Need::soft(Capability::ReadingStore),
            Need::soft(Capability::ForecastStore),
            Need::soft(Capability::SnowCache),
        ]),
        Arc::from(Vec::<Capability>::new()),
        vec![
            slot(
                Capability::LatestSnapshot,
                Some(published(Arc::new(
                    Arc::new(NoSnapshots(Arc::new(SnapshotTable::default())))
                        as Arc<dyn LatestSnapshot>,
                ))),
            ),
            slot(
                Capability::ReadingStore,
                Some(published(Arc::new(store as Arc<dyn Store>))),
            ),
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
            vec![station()],
            Listener {
                port: 18080,
                tls: false,
            },
        )),
        renderer: Arc::new(mw_api_legacy::Renderer::default()),
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

async fn get(handlers: &Arc<Handlers>, path: &str) -> (StatusCode, String) {
    let state = Arc::new(RouteState::new());
    state.install_for_test(Arc::clone(handlers));
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
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a small body");
    (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

/// A bucket whose `outtemp` is `temp`, in `SPAN_COLUMNS` order. Only the first column is
/// read by these assertions; the rest are absent, which the zero-fill handles.
fn bucket(minutes_ago: i64, temp: f64, width: i64) -> SpanBucket {
    let now = chrono::Utc::now().timestamp();
    let floored = now - now.rem_euclid(width);
    SpanBucket {
        bucket: chrono::DateTime::from_timestamp(floored - minutes_ago * width, 0)
            .expect("a real instant"),
        values: vec![Some(temp)],
    }
}

/// **A hit asks for the tail alone, and the prefix comes back byte-identical.**
#[tokio::test]
async fn a_cache_hit_asks_for_the_tail_and_reuses_the_prefix() {
    // A complete prefix: three closed buckets and nothing the aggregate has missed.
    let store = Recording::new(Span {
        prefix: vec![
            bucket(3, 10.0, 60),
            bucket(2, 11.0, 60),
            bucket(1, 12.0, 60),
        ],
        backfill: Vec::new(),
        tail: None,
    });
    let h = handlers(Arc::clone(&store));

    let (status, first) = get(&h, "/span/1h?station=davis-full").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (_, second) = get(&h, "/span/1h?station=davis-full").await;

    let queries = store.queries();
    assert_eq!(queries.len(), 2, "both requests reached the store");
    assert!(
        queries[0].since < queries[0].tail_start,
        "the miss asks for the prefix and the tail"
    );
    assert_eq!(
        queries[1].since, queries[1].tail_start,
        "**the hit asks for the tail alone** -- `since == tail_start` is what makes the \
         prefix range empty by construction (D-54). Dropping that rewrite would serve the \
         prefix twice and nothing else in the tree would have said so."
    );
    assert_eq!(
        first, second,
        "the cached prefix is reused byte for byte, which is the point of caching the text"
    );
    assert_eq!(first.matches("\"stationname\"").count(), 3, "{first}");
}

/// **An incomplete prefix is not cached.**
///
/// A non-empty `backfill` is exactly "the continuous aggregate has not caught up", and the
/// cache key does not move until the bucket does -- so caching it pinned the gap for the
/// whole 60-second TTL. Uncached, the next request sees the hole close as soon as the
/// refresh policy runs.
#[tokio::test]
async fn a_prefix_the_aggregate_has_not_caught_up_to_is_not_cached() {
    let store = Recording::new(Span {
        prefix: vec![bucket(3, 10.0, 60)],
        // One closed bucket the view does not hold yet.
        backfill: vec![bucket(2, 11.0, 60)],
        tail: Some(bucket(1, 12.0, 60)),
    });
    let h = handlers(Arc::clone(&store));

    let (status, body) = get(&h, "/span/1h?station=davis-full").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // All three are in the response: the hole the backfill closes is the whole reason it
    // exists.
    assert_eq!(body.matches("\"stationname\"").count(), 3, "{body}");

    let (_, again) = get(&h, "/span/1h?station=davis-full").await;
    assert_eq!(again.matches("\"stationname\"").count(), 3, "{again}");

    let queries = store.queries();
    assert_eq!(queries.len(), 2);
    assert!(
        queries[1].since < queries[1].tail_start,
        "the second request must MISS, because the first prefix was incomplete: caching it \
         would serve the same hole for the whole TTL"
    );
}

/// **A zero or negative span is an empty array, and the store is not asked.**
///
/// Go's predicate is `bucket > now - span` (`controller.go:890`), so a zero span asks for
/// buckets strictly after *now* and finds none. Here it made `since == tail_start`, which is
/// `SpanQuery`'s cache-hit sentinel -- the prefix was skipped and the still-filling bucket
/// appended anyway, so `/span/0` answered a one-element array. `parse_go_duration` maps a
/// negative Go duration to zero, so `/span/-1h` is the same request.
#[tokio::test]
async fn a_zero_span_is_an_empty_array() {
    let store = Recording::new(Span {
        prefix: vec![bucket(1, 10.0, 60)],
        backfill: Vec::new(),
        tail: Some(bucket(0, 11.0, 60)),
    });
    let h = handlers(Arc::clone(&store));

    for path in ["/span/0?station=davis-full", "/span/-1h?station=davis-full"] {
        let (status, body) = get(&h, path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body, "[]\n", "{path}");
    }
    assert!(
        store.queries().is_empty(),
        "an empty window needs no query, and asking for one is how the sentinel got \
         confused with a real range"
    );
}
