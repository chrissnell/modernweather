//! The plan, and the three bodies against a recording [`Fetch`] and a recording store.
//!
//! No socket and no database: the seams are [`Fetch`] and the two capability handle traits,
//! which is `crate::uploader`'s own boundary and necessary for a stronger reason here — a
//! test that fetched from Aeris would spend the owner's API quota on every `cargo test`.

use std::sync::{Arc, Mutex};

use mw_component::{Alert, BoxFuture, Capability, StoreError, Strength, Trigger};

use super::*;

// ---------------------------------------------------------------------------------------
// The configuration, the fetch and the stores, recorded.

/// A configuration with one account, N stations and a zone.
struct Cfg {
    account: Option<AerisAccount>,
    stations: Vec<AerisStation>,
    zone: Option<Zone>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            account: Some(account()),
            stations: vec![station("backyard", Some(39.9), Some(-105.5))],
            zone: Some(Zone::new("America/Denver").expect("a real zone")),
        }
    }
}

impl AerisConfig for Cfg {
    fn aeris_account(&self) -> Option<AerisAccount> {
        self.account.clone()
    }
    fn aeris_stations(&self) -> Vec<AerisStation> {
        self.stations.clone()
    }
    fn deployment_zone(&self) -> Option<Zone> {
        self.zone
    }
}

fn account() -> AerisAccount {
    AerisAccount {
        client_id: "cid".to_string(),
        client_secret: SecretString::from("sec".to_string()),
        api_endpoint: None,
        refresh_interval: None,
    }
}

fn station(key: &str, latitude: Option<f64>, longitude: Option<f64>) -> AerisStation {
    AerisStation {
        key: StationKey::new(key),
        latitude,
        longitude,
    }
}

/// A [`Fetch`] that answers with a canned body and records the query it was given.
#[derive(Debug)]
struct Canned {
    body: Result<Vec<u8>, &'static str>,
    queries: Mutex<Vec<String>>,
}

impl Canned {
    fn ok(body: &str) -> Arc<Self> {
        Arc::new(Self {
            body: Ok(body.as_bytes().to_vec()),
            queries: Mutex::new(Vec::new()),
        })
    }

    fn broken() -> Arc<Self> {
        Arc::new(Self {
            body: Err("no route to host"),
            queries: Mutex::new(Vec::new()),
        })
    }

    fn queries(&self) -> Vec<String> {
        self.queries.lock().expect("not poisoned").clone()
    }
}

impl Fetch for Canned {
    fn get(&self, query: Form) -> BoxFuture<'static, Result<Vec<u8>, AerisError>> {
        self.queries
            .lock()
            .expect("not poisoned")
            .push(query.as_str().to_string());
        let body = self.body.clone();
        Box::pin(async move {
            body.map_err(|why| {
                AerisError::Transport(crate::error::UploadError::Connect {
                    what: "could not connect to",
                    target: crate::error::Target::peer("test", 443),
                    source: std::io::Error::other(why),
                })
            })
        })
    }
}

/// A `ForecastStore` and an `AlertStore` that record what they were asked to write.
#[derive(Debug, Default)]
struct Recorder {
    forecasts: Mutex<Vec<(String, u32, String)>>,
    alerts: Mutex<Vec<(String, Vec<String>)>>,
    swept: Mutex<Vec<DateTime<Utc>>>,
    fail: bool,
}

impl Recorder {
    fn failing() -> Arc<Self> {
        Arc::new(Self {
            fail: true,
            ..Self::default()
        })
    }

    fn err<T>(&self) -> Result<T, StoreError> {
        Err(StoreError::unavailable(std::io::Error::other(
            "connection reset",
        )))
    }
}

impl mw_component::ForecastStore for Recorder {
    fn forecast<'a>(
        &'a self,
        _q: mw_component::ForecastQuery,
    ) -> BoxFuture<'a, Result<Option<mw_component::Forecast>, StoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn store_forecast<'a>(
        &'a self,
        station: &'a StationKey,
        span_hours: u32,
        data: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if self.fail {
                return self.err();
            }
            self.forecasts.lock().expect("not poisoned").push((
                station.to_string(),
                span_hours,
                data.to_string(),
            ));
            Ok(())
        })
    }
}

impl AlertStore for Recorder {
    fn store_alerts<'a>(
        &'a self,
        station: &'a StationKey,
        alerts: &'a [Alert],
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            if self.fail {
                return self.err();
            }
            self.alerts.lock().expect("not poisoned").push((
                station.to_string(),
                alerts.iter().map(|a| a.alert_id.clone()).collect(),
            ));
            Ok(alerts.len() as u64)
        })
    }

    fn sweep_expired_alerts<'a>(
        &'a self,
        cutoff: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            if self.fail {
                return self.err();
            }
            self.swept.lock().expect("not poisoned").push(cutoff);
            Ok(3)
        })
    }
}

/// The `Cx` a `Tick` body is handed.
fn cx() -> Cx {
    let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
    Cx::new(
        ComponentId::new("aeris:forecast:backyard:240").expect("a component id"),
        tokio_util::sync::CancellationToken::new(),
        mw_component::Generation::ZERO,
        rx,
    )
}

const FORECAST_BODY: &str = r#"{"success":true,"response":[{"periods":[
    {"dateTimeISO":"2026-11-12T00:00:00-07:00","maxTempF":44,"pop":10,"precipIN":0,
     "snowIN":0,"iceaccumIN":0,"windDir":"NNW","windDirDEG":338,
     "weather":"Partly Cloudy","weatherPrimaryCoded":"::SC"}]}]}"#;

const ALERTS_BODY: &str = r#"{"success":true,"response":[
    {"id":"one","details":{"name":"Winter Storm Warning"},
     "timestamps":{"issued":1762900000,"expires":1763000000}}]}"#;

// ---------------------------------------------------------------------------------------
// The plan.

/// Three components per paired station and one sweep: **two forecast spans, alerts, and the
/// expired-alert sweep**, which is W2a **D-33**'s four kinds.
#[test]
fn one_station_is_two_forecasts_an_alert_fetch_and_one_sweep() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "aeris:forecast:backyard:240",
            "aeris:forecast:backyard:24",
            "aeris:alerts:backyard",
            "aeris:alerts:gc",
        ]
    );
}

/// The sweep is **one** component however many stations are paired: it deletes from one
/// table, so a per-station version would be N components doing the same `DELETE` at 3 a.m.
#[test]
fn two_stations_share_one_sweep() {
    let cfg = Cfg {
        stations: vec![
            station("backyard", Some(39.9), Some(-105.5)),
            station("summit", Some(39.6), Some(-105.9)),
        ],
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("a plannable configuration");
    assert_eq!(out.len(), 7, "3 per station + 1 sweep");
    let sweeps = out.iter().filter(|s| s.id.as_str() == GC_ID).count();
    assert_eq!(sweeps, 1);
}

#[test]
fn no_account_plans_nothing() {
    let cfg = Cfg {
        account: None,
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("not an error");
    assert!(out.is_empty(), "the ordinary case: no Aeris account");
}

/// An account and no pairing plans nothing — including no sweep, because there is nothing
/// to sweep.
#[test]
fn an_account_with_no_paired_station_plans_nothing() {
    let cfg = Cfg {
        stations: Vec::new(),
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("not an error");
    assert!(out.is_empty());
}

/// The refusal this item adds, and the reason it is a refusal: the Go controller skips such
/// a device inside an `if` with no `else`, so the operator gets `aerisWeatherEnabled: true`,
/// an empty forecast panel and no diagnosis.
#[test]
fn a_paired_station_with_no_coordinates_is_refused_by_name() {
    for (lat, lon) in [(Some(39.9), None), (None, Some(-105.5)), (None, None)] {
        let cfg = Cfg {
            stations: vec![station("backyard", lat, lon)],
            ..Cfg::default()
        };
        let mut out = Vec::new();
        let e = register(&cfg, &mut out).expect_err("a station that cannot be asked about");
        let AerisPlanError::NoCoordinates { station } = &e else {
            panic!("expected a coordinate refusal, got {e:?}");
        };
        assert_eq!(station, "backyard");
        assert!(
            e.to_string().contains("latitude"),
            "the message names the fix: {e}"
        );
    }
}

/// `docs/component-model.md` §4.3's `Every { span/4, ≥1h }`, and the floor is what makes the
/// hourly forecast's derived fifteen minutes an hour.
#[test]
fn the_two_spans_refresh_at_a_quarter_of_a_period_floored_at_an_hour() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    let period = |id: &str| match &out.iter().find(|s| s.id.as_str() == id).expect(id).trigger {
        Trigger::Every { period, floor, .. } => (*period, *floor),
        other => panic!("{id} is {other:?}"),
    };
    assert_eq!(
        period("aeris:forecast:backyard:240"),
        (Duration::from_secs(6 * 3600), MIN_REFRESH),
        "ten 24-hour periods: a quarter of 24 h"
    );
    assert_eq!(
        period("aeris:forecast:backyard:24"),
        (MIN_REFRESH, MIN_REFRESH),
        "twenty-four 1-hour periods: a quarter of 1 h, raised to the floor"
    );
    assert_eq!(
        period("aeris:alerts:backyard"),
        (ALERT_PERIOD, ALERT_PERIOD),
        "fifteen minutes, and not configurable"
    );
}

/// `ResolveInterval`'s clamp, moved to plan time: a sub-floor override is **raised**, not
/// rejected — which is W2a OQ-A's upgrade path.
#[test]
fn a_sub_floor_override_is_raised_to_the_floor_rather_than_refused() {
    let cfg = Cfg {
        account: Some(AerisAccount {
            refresh_interval: Some(Duration::from_secs(60)),
            ..account()
        }),
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("raised, not refused");
    for id in ["aeris:forecast:backyard:240", "aeris:forecast:backyard:24"] {
        let spec = out.iter().find(|s| s.id.as_str() == id).expect(id);
        let Trigger::Every { period, .. } = &spec.trigger else {
            panic!("{id} is not Every");
        };
        assert_eq!(*period, MIN_REFRESH);
    }
}

/// An override above the floor replaces the derived period for **both** spans, which is
/// what one `aeris_refresh_interval` column means.
#[test]
fn an_override_above_the_floor_replaces_the_derived_period() {
    let cfg = Cfg {
        account: Some(AerisAccount {
            refresh_interval: Some(Duration::from_secs(4 * 3600)),
            ..account()
        }),
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("a plannable configuration");
    for id in ["aeris:forecast:backyard:240", "aeris:forecast:backyard:24"] {
        let spec = out.iter().find(|s| s.id.as_str() == id).expect(id);
        let Trigger::Every { period, .. } = &spec.trigger else {
            panic!("{id} is not Every");
        };
        assert_eq!(*period, Duration::from_secs(4 * 3600));
    }
}

/// **The done-criterion's declaration.** `/forecast` depending on Aeris having fetched is
/// data presence, declared as `Freshness` and read by W5 — never by the scheduler, which is
/// W2a Decision 20.
#[test]
fn the_forecast_components_declare_freshness_and_not_a_startup_edge() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    let spec = out
        .iter()
        .find(|s| s.id.as_str() == "aeris:forecast:backyard:240")
        .expect("the ten-day span");
    assert_eq!(spec.freshness.len(), 1);
    let f = &spec.freshness[0];
    assert_eq!(f.capability, Capability::ForecastStore);
    assert_eq!(f.populated_by.as_str(), "aeris:forecast:backyard:240");
    assert_eq!(
        f.expected_max_age,
        Duration::from_secs(12 * 3600),
        "twice the period: one missed occurrence is not a stale forecast"
    );
    // The thing Decision 20 forbids: the freshness declaration must not also be an edge.
    // `requires` is the ordering graph and it names the store, not the data.
    assert_eq!(spec.requires.len(), 1);
    assert_eq!(spec.requires[0].capability, Capability::ForecastStore);
    assert_eq!(spec.requires[0].strength, Strength::Hard);
    assert_eq!(spec.requires[0].access, mw_component::Access::Write);
}

/// `H W`, which is the access each component actually takes — and the reason `api:legacy`
/// holding the same trait object cannot write through it.
#[test]
fn every_component_requires_its_store_hard_and_for_writing() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    for spec in &out {
        assert_eq!(spec.requires.len(), 1, "{}", spec.id);
        let need = &spec.requires[0];
        assert_eq!(need.strength, Strength::Hard, "{}", spec.id);
        assert_eq!(need.access, mw_component::Access::Write, "{}", spec.id);
        let expected = if spec.id.as_str().starts_with(FORECAST_PREFIX) {
            Capability::ForecastStore
        } else {
            Capability::AlertStore
        };
        assert_eq!(need.capability, expected, "{}", spec.id);
        assert!(spec.provides.is_empty(), "{} provides nothing", spec.id);
    }
}

/// **C-W2a-15**: the sweep stays the `Calendar` component W2a specified, at 03:00 in the
/// **deployment's** zone, with `RunLatest` so a node that was off at three o'clock sweeps on
/// the way up.
#[test]
fn the_sweep_is_a_daily_calendar_in_the_deployment_zone() {
    let mut out = Vec::new();
    register(&Cfg::default(), &mut out).expect("a plannable configuration");
    let spec = out
        .iter()
        .find(|s| s.id.as_str() == GC_ID)
        .expect("the sweep");
    let Trigger::Calendar {
        spec: cron,
        tz,
        misfire,
        deadline,
    } = &spec.trigger
    else {
        panic!("the sweep is {:?}", spec.trigger);
    };
    assert_eq!(*cron, CronSpec::daily_at(GC_AT));
    assert_eq!(
        tz,
        &TzSource::Deployment {
            zone: Some(Zone::new("America/Denver").expect("a real zone"))
        }
    );
    assert_eq!(*misfire, Misfire::DEFAULT);
    assert_eq!(*deadline, GC_DEADLINE);
}

/// A deployment with no usable zone still **plans** the sweep, with no zone — so `plan()`
/// refuses with `PlanError::MissingTimeZone` naming it, rather than the sweep silently not
/// existing. The same shape `rain:rollover:<station>` takes, for the same reason.
#[test]
fn a_deployment_with_no_zone_plans_the_sweep_for_plan_to_refuse() {
    let cfg = Cfg {
        zone: None,
        ..Cfg::default()
    };
    let mut out = Vec::new();
    register(&cfg, &mut out).expect("the registrar does not refuse this");
    let spec = out
        .iter()
        .find(|s| s.id.as_str() == GC_ID)
        .expect("the sweep is planned anyway");
    let Trigger::Calendar { tz, .. } = &spec.trigger else {
        panic!("not a Calendar");
    };
    assert!(tz.zone().is_none(), "which plan() is what refuses");
}

/// A digest that did not move is a component the plan diff does not restart. The credential
/// and the coordinates are in it; the secret is not — [`AerisAccount::client_secret`] cannot
/// be hashed without exposing it, and the client id changing is what a rotated account looks
/// like.
#[test]
fn the_digest_follows_the_account_the_coordinates_and_the_period() {
    let digest_of = |cfg: &Cfg| {
        let mut out = Vec::new();
        register(cfg, &mut out).expect("a plannable configuration");
        out.iter()
            .find(|s| s.id.as_str() == "aeris:forecast:backyard:240")
            .expect("the ten-day span")
            .config_digest
    };
    let base = digest_of(&Cfg::default());
    assert_eq!(base, digest_of(&Cfg::default()), "pure");
    assert_ne!(
        base,
        digest_of(&Cfg {
            stations: vec![station("backyard", Some(40.0), Some(-105.5))],
            ..Cfg::default()
        }),
        "a station that moved is a different forecast"
    );
    assert_ne!(
        base,
        digest_of(&Cfg {
            account: Some(AerisAccount {
                client_id: "other".to_string(),
                ..account()
            }),
            ..Cfg::default()
        }),
        "a different account"
    );
    assert_ne!(
        base,
        digest_of(&Cfg {
            account: Some(AerisAccount {
                refresh_interval: Some(Duration::from_secs(2 * 3600)),
                ..account()
            }),
            ..Cfg::default()
        }),
        "a different period"
    );
}

// ---------------------------------------------------------------------------------------
// The URL and the query.

#[test]
fn the_url_is_the_vendors_path_with_the_coordinates_to_six_places() {
    let e = endpoint(&account(), "forecasts", 39.9, -105.5).expect("a usable endpoint");
    assert_eq!(
        e.to_string(),
        "https://data.api.xweather.com/forecasts/39.900000,-105.500000"
    );
    let e = endpoint(&account(), "alerts", 39.9, -105.5).expect("a usable endpoint");
    assert!(e.to_string().ends_with("/alerts/39.900000,-105.500000"));
}

#[test]
fn a_configured_endpoint_replaces_the_default_and_a_trailing_slash_is_not_doubled() {
    let configured = AerisAccount {
        api_endpoint: Some("https://data.api.aerisapi.com/".to_string()),
        ..account()
    };
    let e = endpoint(&configured, "forecasts", 1.0, 2.0).expect("a usable endpoint");
    assert_eq!(
        e.to_string(),
        "https://data.api.aerisapi.com/forecasts/1.000000,2.000000"
    );
}

#[test]
fn a_blank_endpoint_is_the_default_rather_than_a_refusal() {
    let blank = AerisAccount {
        api_endpoint: Some("   ".to_string()),
        ..account()
    };
    assert_eq!(blank.base(), DEFAULT_ENDPOINT);
}

/// **The credential is in the query and the query's `Debug` redacts** (**D-33**). The one
/// place the secret leaves `secrecy` is `credentials`, and this asserts both halves.
#[test]
fn the_query_carries_the_credential_and_its_debug_does_not() {
    let form = account().credentials();
    assert_eq!(form.as_str(), "client_id=cid&client_secret=sec");
    assert!(
        !format!("{form:?}").contains("sec"),
        "the Debug must not carry the secret: {form:?}"
    );
}

#[test]
fn the_forecast_query_asks_for_the_spans_periods() {
    assert_eq!(Span::TenDay.span_hours(), 240);
    assert_eq!(Span::Hourly.span_hours(), 24);
    assert_eq!(Span::TenDay.filter(), "24h");
    assert_eq!(Span::Hourly.filter(), "1h");
    assert_eq!(Span::TenDay.periods(), 10);
    assert_eq!(Span::Hourly.periods(), 24);
}

// ---------------------------------------------------------------------------------------
// The bodies.

#[tokio::test]
async fn a_forecast_tick_stores_the_span_it_fetched() {
    let store = Arc::new(Recorder::default());
    let fetch = Canned::ok(FORECAST_BODY);
    let mut body = ForecastFetch {
        id: ComponentId::new("aeris:forecast:backyard:240").expect("an id"),
        station: StationKey::new("backyard"),
        span: Span::TenDay,
        store: Arc::clone(&store) as Arc<dyn ForecastStore>,
        fetch: Arc::clone(&fetch) as Arc<dyn Fetch>,
        query: {
            let mut q = account().credentials();
            q.set("filter", "24h");
            q.set("limit", "10");
            q
        },
    };
    body.tick(&cx()).await.expect("a successful occurrence");
    let written = store.forecasts.lock().expect("not poisoned").clone();
    assert_eq!(written.len(), 1);
    let (station, span_hours, data) = &written[0];
    assert_eq!(station, "backyard");
    assert_eq!(*span_hours, 240);
    assert!(data.starts_with('['), "a JSON array: {data}");
    assert!(data.contains(r#""precipIN":0,"#), "Go's digits: {data}");
    assert_eq!(
        fetch.queries(),
        vec!["client_id=cid&client_secret=sec&filter=24h&limit=10"]
    );
}

/// A fetch that failed stores nothing: the previous forecast keeps being served, which is
/// the whole reason this component has no retry.
#[tokio::test]
async fn a_failed_fetch_leaves_the_stored_forecast_alone() {
    let store = Arc::new(Recorder::default());
    let mut body = ForecastFetch {
        id: ComponentId::new("aeris:forecast:backyard:240").expect("an id"),
        station: StationKey::new("backyard"),
        span: Span::TenDay,
        store: Arc::clone(&store) as Arc<dyn ForecastStore>,
        fetch: Canned::broken() as Arc<dyn Fetch>,
        query: Form::new(),
    };
    let e = body.tick(&cx()).await.expect_err("the fetch failed");
    assert!(matches!(e, ComponentError::Tick { .. }), "{e:?}");
    assert!(store.forecasts.lock().expect("not poisoned").is_empty());
}

/// A store failure is reported as a store failure and not as a fetch failure: the API
/// answered, the quota was spent, and what is broken is the database.
#[tokio::test]
async fn a_store_failure_is_reported_separately_from_a_fetch_failure() {
    let mut body = ForecastFetch {
        id: ComponentId::new("aeris:forecast:backyard:240").expect("an id"),
        station: StationKey::new("backyard"),
        span: Span::TenDay,
        store: Recorder::failing() as Arc<dyn ForecastStore>,
        fetch: Canned::ok(FORECAST_BODY) as Arc<dyn Fetch>,
        query: Form::new(),
    };
    let e = body.tick(&cx()).await.expect_err("the store failed");
    let ComponentError::Tick { what, .. } = &e else {
        panic!("expected a tick failure, got {e:?}");
    };
    assert_eq!(what, "store the Aeris forecast");
}

#[tokio::test]
async fn an_alert_tick_stores_what_it_fetched() {
    let store = Arc::new(Recorder::default());
    let mut body = AlertFetch {
        id: ComponentId::new("aeris:alerts:backyard").expect("an id"),
        station: StationKey::new("backyard"),
        store: Arc::clone(&store) as Arc<dyn AlertStore>,
        fetch: Canned::ok(ALERTS_BODY) as Arc<dyn Fetch>,
        query: account().credentials(),
    };
    body.tick(&cx()).await.expect("a successful occurrence");
    let written = store.alerts.lock().expect("not poisoned").clone();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].0, "backyard");
    assert_eq!(written[0].1, vec!["one".to_string()]);
}

/// No active alert is a successful occurrence that writes nothing — and, crucially, deletes
/// nothing.
#[tokio::test]
async fn an_alert_tick_with_no_alerts_succeeds_and_writes_an_empty_list() {
    let store = Arc::new(Recorder::default());
    let mut body = AlertFetch {
        id: ComponentId::new("aeris:alerts:backyard").expect("an id"),
        station: StationKey::new("backyard"),
        store: Arc::clone(&store) as Arc<dyn AlertStore>,
        fetch: Canned::ok(r#"{"success":true,"response":[]}"#) as Arc<dyn Fetch>,
        query: Form::new(),
    };
    body.tick(&cx()).await.expect("a successful occurrence");
    let written = store.alerts.lock().expect("not poisoned").clone();
    assert_eq!(written.len(), 1, "the store was called");
    assert!(written[0].1.is_empty(), "with nothing to write");
}

#[tokio::test]
async fn the_sweep_deletes_alerts_that_expired_a_day_ago() {
    let store = Arc::new(Recorder::default());
    let mut body = AlertSweep {
        id: ComponentId::new(GC_ID).expect("an id"),
        store: Arc::clone(&store) as Arc<dyn AlertStore>,
    };
    let before = Utc::now();
    body.tick(&cx()).await.expect("a successful occurrence");
    let after = Utc::now();
    let swept = store.swept.lock().expect("not poisoned").clone();
    assert_eq!(swept.len(), 1);
    // The cutoff was taken at some instant inside the tick, so it is a day before an
    // instant in `[before, after]` -- bounded rather than equal, because the only
    // alternative is injecting a clock into a component whose whole body is one `DELETE`.
    assert!(
        swept[0] >= before - chrono::Duration::hours(24)
            && swept[0] <= after - chrono::Duration::hours(24),
        "the cutoff is a day back, not now: {}",
        swept[0]
    );
}

/// The one arithmetic fact of the sweep, without a database.
#[test]
fn the_cutoff_is_twenty_four_hours_before_now() {
    let now = chrono::DateTime::from_timestamp(1_763_000_000, 0).expect("a valid instant");
    assert_eq!(cutoff(now), now - chrono::Duration::hours(24));
}

/// Cancellation is a stop and not a failure (**D-24**), which is `ComponentError::Cancelled`
/// and classifies `Ignorable` — so a restart does not look like a broken fetcher.
#[tokio::test]
async fn a_cancelled_fetch_is_a_stop_rather_than_a_failure() {
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let (_tx, rx) = tokio::sync::watch::channel(mw_component::Generation::ZERO);
    let cx = Cx::new(
        ComponentId::new("aeris:forecast:backyard:240").expect("an id"),
        token,
        mw_component::Generation::ZERO,
        rx,
    );
    let store = Arc::new(Recorder::default());
    let mut body = ForecastFetch {
        id: ComponentId::new("aeris:forecast:backyard:240").expect("an id"),
        station: StationKey::new("backyard"),
        span: Span::TenDay,
        store: Arc::clone(&store) as Arc<dyn ForecastStore>,
        fetch: Canned::ok(FORECAST_BODY) as Arc<dyn Fetch>,
        query: Form::new(),
    };
    let e = body.tick(&cx).await.expect_err("cancelled");
    assert!(matches!(e, ComponentError::Cancelled), "{e:?}");
    assert!(
        store.forecasts.lock().expect("not poisoned").is_empty(),
        "and nothing was written"
    );
}

/// Every label this module emits is armed, so "nothing has been fetched yet" and "these
/// metrics are broken" are different observations.
#[test]
fn every_emitted_fetch_label_is_armed() {
    use crate::error::{Target, UploadError};
    let every: Vec<AerisError> = vec![
        AerisError::Transport(UploadError::Connect {
            what: "x",
            target: Target::peer("t", 1),
            source: std::io::Error::other("x"),
        }),
        AerisError::Decode {
            source: serde_json::from_str::<serde_json::Value>("{").expect_err("not JSON"),
        },
        AerisError::Shape {
            what: "x",
            found: "x",
        },
        AerisError::Refused {
            detail: String::new(),
        },
        AerisError::Empty { what: "x" },
        AerisError::Coded {
            coded: String::new(),
        },
        AerisError::Encode {
            source: serde_json::from_str::<serde_json::Value>("{").expect_err("not JSON"),
        },
    ];
    for e in &every {
        assert!(
            m::FETCH_OUTCOMES.contains(&e.reason()),
            "`{}` is emitted and not armed",
            e.reason()
        );
    }
    for extra in ["store", "cancelled", m::FETCH_OK] {
        assert!(m::FETCH_OUTCOMES.contains(&extra), "`{extra}` is not armed");
    }
}
