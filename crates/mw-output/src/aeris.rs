//! The four Aeris components — `docs/component-model.md` §4.3, **W2a D-33**.
//!
//! ```text
//!                       aeris_account (one row)        station_service (service = 'aeris')
//!                              │                                   │
//!            ┌─────────────────┴───────────────────┬───────────────┘
//!            ▼                                     ▼
//!  aeris:forecast:<station>:240   Every { 6h,  ≥1h }  ─┐
//!  aeris:forecast:<station>:24    Every { 1h,  ≥1h }  ─┤ ForecastStore H W   → /forecast
//!  aeris:alerts:<station>         Every { 15m }       ─┤ AlertStore    H W   → (no endpoint)
//!  aeris:alerts:gc                Calendar { 03:00 }  ─┘ AlertStore    H W
//! ```
//!
//! # Four components and not one controller, which is **D-33**'s own worked example
//!
//! > *One component, one trigger. Aeris becomes four components (two forecast spans,
//! > alerts, expired-alert sweep) and the snow cache becomes three, because they have
//! > independent periods, failures and rate budgets — and "alerts are failing, forecasts are
//! > fine" must be expressible.*
//!
//! Measured, the Go controller runs four independent goroutine families per process and
//! reports all four as one controller: `refreshForecastPeriodically(device, 10, 24)`,
//! `refreshForecastPeriodically(device, 24, 1)`, `refreshAlertsPeriodically(device)` and
//! `cleanupExpiredAlerts()`. Each has its own ticker, its own `log.Errorf` and no state
//! anything can read, so "the ten-day forecast has not refreshed since Tuesday" is not a
//! question the running system can answer. Four components make it four rows of
//! `mw_component_state` with four `last_run`s, four `last_error`s and four schedules.
//!
//! What that costs is in this file: four spec functions rather than one. What it buys is
//! that an alerts endpoint returning `403` for a credential that lost its alerts
//! entitlement leaves both forecasts running, which under one component is a single
//! `Failed` and no forecasts.
//!
//! # `aeris:alerts:gc` is a `Calendar` component, and that was a decision
//!
//! **C-W2a-15.** W2a's OQ-C offered two homes for the expired-alert sweep: W4 absorbs it as
//! a TimescaleDB retention policy *if* the alerts table becomes a hypertable, or it stays
//! the `Calendar` component W2a specified. W4 never addressed alerts at all, so the arm
//! that is operative is settled by elimination rather than by preference — and the alerts
//! table's own migration has why a hypertable would have been the wrong shape for an
//! upserted row anyway.
//!
//! It is also the one component here with **no station**: one sweep covers every row, so it
//! is `TzSource::Deployment` and `node.timezone`. A per-station sweep would be N components
//! deleting from one table at 3 a.m. for no reason.
//!
//! # `/alerts` is **not** built, and that is not an omission
//!
//! `docs/reconciliation.md` §6.3 retired the endpoint (no working in-repo client, not in
//! either client's endpoint list) and **A-M6-3-1** confirms it against this item's older
//! done-criterion. The alert *components* are in scope because D-33's "alerts are failing,
//! forecasts are fine" needs the fetch and the store; the endpoint is not. So
//! `mw_component::AlertStore` is write-only, the four `/alerts` fixtures stay in the
//! conformance runner's `retired` roster, and nothing in this crate renders an alert.
//!
//! # What this module does **not** have, and why each absence is deliberate
//!
//! - **No retry, no rate limiter, no `Retry-After` hold.** `crate::uploader` has all three
//!   because an upload is a *measurement* that is lost if it is not delivered in its
//!   window. A forecast is a *cache refresh*: the previous one is still being served, the
//!   period is the retry (W2a Decision 29), and a retry inside one occurrence would spend
//!   the API quota to be told the same thing. The quota is also the reason the floor is a
//!   plan-time constant rather than a limiter — `component-model.md` §1.5's note that "a
//!   restart loop is a billing event" is about exactly this API.
//! - **No `FirstRun::Immediate`.** §1.5 names this component as the example: *"everything
//!   else starts within one period, which [...] for a 6-hour Aeris refresh is the correct
//!   behaviour anyway (today Aeris calls the API immediately on every process start,
//!   because `time.Ticker` only fires after the interval elapses — a restart loop is a
//!   billing event)."* So the first fetch is at the next phase slot and a crash loop cannot
//!   bill.
//! - **No `Capability::ConfigStore`**, which §4.3's table declares. Nothing provides it:
//!   `config:sqlite` has no work item and the configuration rows reach every registrar
//!   through `main` as a root resource (**D-41**). A hard need on an unprovided capability
//!   would make `plan()` refuse the whole configuration, so it is **dropped rather than
//!   deviated from** — exactly what `docs/reconciliation.md` **A-M4-1-1** did with
//!   `snapshot:latest`'s `SnowCache S`, and for the same reason. It comes back with
//!   `config:sqlite`.

pub mod alerts;
pub mod forecast;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{
    AlertStore, BoxFuture, Builder, Capability, ComponentError, ComponentId, ComponentIdError,
    ComponentSpec, ConfigDigest, CronSpec, Cx, FirstRun, ForecastStore, Freshness, LocalTime,
    Misfire, Need, Phase, Tick, Trigger, TzSource, Zone,
};
use mw_types::StationKey;
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info, warn};

use crate::error::AerisError;
use crate::form::Form;
use crate::https::{self, Endpoint, EndpointError};
use crate::metrics as m;
use crate::uploader::ATTEMPT_TIMEOUT;

/// The vendor's default base URL, for an account with no `api_endpoint`.
///
/// `controller.go:402`'s own default, and the newer of the vendor's two host names: Aeris
/// renamed itself Xweather and `aerisapi.com` still answers, which is why the column exists
/// at all — a rename should not need a release.
pub const DEFAULT_ENDPOINT: &str = "https://data.api.xweather.com";

/// The prefix of an `aeris:forecast:<station>:<span>` component id.
pub const FORECAST_PREFIX: &str = "aeris:forecast";

/// The prefix of an `aeris:alerts:<station>` component id.
pub const ALERTS_PREFIX: &str = "aeris:alerts";

/// The expired-alert sweep's component id.
pub const GC_ID: &str = "aeris:alerts:gc";

/// The shortest forecast refresh period, enforced once by `plan()`.
///
/// `minAerisRefreshInterval` in the Go controller, with its own comment: *"Forecasts change
/// slowly, so hourly is plenty and keeps us well within API rate limits."* Enforced as
/// `Trigger::Every { floor }` rather than clamped per call, which is what `ResolveInterval`
/// does today — the difference is that a sub-floor configuration is raised **once**, at
/// plan time, with a line naming the component, instead of silently every tick.
pub const MIN_REFRESH: Duration = Duration::from_secs(3600);

/// The alerts refresh period.
///
/// Fifteen minutes, hard-coded in Go and deliberately not configurable here: an alert is
/// the one thing on this path where staleness is the whole cost, and the `aeris_account`
/// table has no column for it.
pub const ALERT_PERIOD: Duration = Duration::from_secs(900);

/// How long after expiry an alert is swept.
///
/// `cleanupExpiredAlerts`' own `time.Now().Add(-24 * time.Hour)`. A day of grace rather
/// than immediate deletion, because an expired warning is still the answer to "what
/// happened last night".
pub const ALERT_RETENTION: Duration = Duration::from_secs(24 * 3600);

/// When the sweep runs, in the deployment's zone.
///
/// W2a §4.3's `Calendar { daily 03:00, Deployment }`. Three in the morning because the
/// sweep's one effect is a `DELETE`, and the hour nothing else is doing anything is the
/// right hour for it.
pub const GC_AT: LocalTime = LocalTime::new(3, 0);

/// The sweep's per-occurrence deadline.
///
/// One indexed `DELETE` against a table whose size is bounded by how many alerts an
/// operator's counties issue. A minute is three orders of magnitude of slack, and a miss
/// means the database is not answering rather than that the sweep is slow.
pub const GC_DEADLINE: Duration = Duration::from_secs(60);

/// The most of an Aeris response this reads.
///
/// A 24-period hourly forecast is about 12 KiB and a county's full warning text is a few
/// KiB per alert, so this is roughly an order of magnitude of headroom. The cap exists for
/// `crate::https::MAX_RESPONSE_BYTES`' reason: a captive portal or a misrouted request can
/// answer with a page, and a bounded read is the difference between a surprising response
/// and an out-of-memory daemon.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The account this node fetches with.
///
/// One per node, which the `aeris_account` table's own comment argues for: the credential is
/// a property of the account, and the Go schema's per-device copies are three places to
/// rotate one secret.
#[derive(Clone)]
pub struct AerisAccount {
    /// `client_id`. Not a credential on its own, and it travels in the same query string as
    /// one — which is why [`Form`]'s `Debug` redacts the whole query rather than one field.
    pub client_id: String,
    /// `client_secret`. A [`SecretString`] from the moment it is read (**D-33**); it leaves
    /// `secrecy` in exactly one place, [`AerisAccount::credentials`].
    pub client_secret: SecretString,
    /// `api_endpoint`, or `None` for [`DEFAULT_ENDPOINT`].
    pub api_endpoint: Option<String>,
    /// `refresh_interval_seconds`, or `None` for the derived span/4.
    pub refresh_interval: Option<Duration>,
}

impl fmt::Debug for AerisAccount {
    /// Redacts (**D-33**). The client id is not a secret and is shown, because an operator
    /// debugging a `401` needs to know which account the daemon is using.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AerisAccount")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("api_endpoint", &self.api_endpoint)
            .field("refresh_interval", &self.refresh_interval)
            .finish()
    }
}

impl AerisAccount {
    /// The two authentication parameters, in `url.Values.Encode`'s order.
    ///
    /// The one place the secret leaves `secrecy`, and what it goes into is a [`Form`] whose
    /// own `Debug` redacts — the same single-exit arrangement
    /// `crate::encoder::account::Account::preamble` has.
    ///
    /// Alphabetical because Go's `v.Encode()` sorts by key, and a request this program
    /// makes being byte-comparable with one the Go daemon made is worth more here than the
    /// declaration order [`Form`] normally preserves: there is no declaration table on this
    /// path, so there is nothing for insertion order to reflect.
    fn credentials(&self) -> Form {
        let mut form = Form::new();
        form.set("client_id", &self.client_id);
        form.set("client_secret", self.client_secret.expose_secret());
        form
    }

    /// The base URL, defaulted.
    fn base(&self) -> &str {
        match self.api_endpoint.as_deref() {
            Some(url) if !url.trim().is_empty() => url.trim_end_matches('/'),
            _ => DEFAULT_ENDPOINT,
        }
    }
}

/// One station paired with the Aeris service.
#[derive(Debug, Clone, PartialEq)]
pub struct AerisStation {
    /// Whose.
    pub key: StationKey,
    /// `config.stations.latitude`.
    pub latitude: Option<f64>,
    /// `config.stations.longitude`.
    pub longitude: Option<f64>,
}

impl AerisStation {
    /// The pair, when the station has both.
    ///
    /// `zip` rather than two `0.0` defaults, which is the predicate
    /// `mw_component::StationMeta::coordinates` uses and the one
    /// `docs/reconciliation.md` DEV-17 records the Go daemon getting wrong in two
    /// directions.
    fn coordinates(&self) -> Option<(f64, f64)> {
        self.latitude.zip(self.longitude)
    }
}

/// Which forecast one `aeris:forecast:*` component fetches.
///
/// The two the Go controller starts per device, as a closed enum rather than a
/// `(periods, hours)` pair: the two are the only two, the span is derived from them, and a
/// third would be a configuration surface nothing asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// Ten daily periods — `refreshForecastPeriodically(device, 10, 24)`. The website's
    /// ten-day panel.
    TenDay,
    /// Twenty-four hourly periods — `refreshForecastPeriodically(device, 24, 1)`. The
    /// website's hourly chart.
    Hourly,
}

impl Span {
    /// Both, in the order the Go controller starts them.
    pub const ALL: [Span; 2] = [Span::TenDay, Span::Hourly];

    /// `limit=` — how many periods to ask for.
    const fn periods(self) -> u32 {
        match self {
            Span::TenDay => 10,
            Span::Hourly => 24,
        }
    }

    /// `filter=` — how long one period is, in hours.
    const fn period_hours(self) -> u32 {
        match self {
            Span::TenDay => 24,
            Span::Hourly => 1,
        }
    }

    /// `forecasts.span_hours` — the **total** span, which is the primary key's second
    /// column and `/forecast/{hours}`' path segment. 240 and 24.
    ///
    /// Not a duration: the endpoint's grammar is `^\d{1,4}$` and the value is a count of
    /// hours of total span.
    pub const fn span_hours(self) -> u32 {
        self.periods() * self.period_hours()
    }

    /// The derived refresh period: a quarter of one period, floored at [`MIN_REFRESH`].
    ///
    /// `docs/component-model.md` §4.3's `Every { span/4, ≥1h }`, which is the Go
    /// controller's `spanInterval/4` where `spanInterval` is **one period** and not the
    /// whole span. So the ten-day forecast refreshes every six hours and the hourly one
    /// would refresh every fifteen minutes — which is what the floor is for.
    fn derived_period(self) -> Duration {
        let quarter = Duration::from_secs(u64::from(self.period_hours()) * 3600 / 4);
        quarter.max(MIN_REFRESH)
    }

    /// The `filter=` parameter's value.
    fn filter(self) -> String {
        format!("{}h", self.period_hours())
    }
}

/// What this crate needs from the configuration to plan the Aeris components.
///
/// A trait for [`crate::plan::OutputConfig`]'s reason: `plan()` is generic over the
/// configuration and the binary chooses it (§1.3 rule 2), so a registrar takes `&C` and
/// each component crate names only the rows it plans from.
pub trait AerisConfig {
    /// The account, or `None` when there is no `aeris_account` row or it is switched off.
    ///
    /// The two cases are one answer here on purpose: both mean "this node does not fetch
    /// forecasts", and the only thing a component could do differently about them is log a
    /// different sentence at a level nobody reads. The management API (M7-1) is where the
    /// difference is an operator-facing fact.
    fn aeris_account(&self) -> Option<AerisAccount>;

    /// Every station with a `station_service` pairing for `service = 'aeris'`, in station
    /// order.
    fn aeris_stations(&self) -> Vec<AerisStation>;

    /// The deployment's own zone, for the sweep's `Calendar`.
    ///
    /// `None` when `node.timezone` does not resolve, which is a [`Trigger::Calendar`] with
    /// no zone and therefore a `PlanError::MissingTimeZone` naming the component —
    /// deliberately, for `mw_pipeline::rollup::register`'s reason: a daily sweep that
    /// silently never happens is the failure mode this programme keeps finding.
    fn deployment_zone(&self) -> Option<Zone>;
}

/// Declare the Aeris components: two forecasts and one alert fetch per paired station, and
/// one sweep.
///
/// # Nothing paired is nothing planned, and no account is nothing planned
///
/// Both are the ordinary case — most deployments have no Aeris account — and neither is an
/// error. They are logged at different levels because they mean different things: an account
/// with no pairings is an operator who configured a credential and no station, which is a
/// configuration that will never fetch anything and is worth a `warn`.
///
/// # A paired station with no coordinates is a **refusal**, and that is the point
///
/// Aeris is queried by `latitude,longitude`; a station without both cannot be asked about.
/// The Go controller skips such a device inside an `if` with no `else`, so an operator who
/// paired a station and left its coordinates NULL gets a website with
/// `aerisWeatherEnabled: true`, an empty forecast panel, and nothing anywhere saying why.
/// Here it is a `PlanError` naming the station, which `mw_pipeline::rollup::register`
/// establishes as this programme's answer to that class:
///
/// - **Skipping** is the silence above.
/// - **Defaulting** has no candidate. There is no sensible default latitude.
/// - The fix is one `UPDATE` and the refusal names the row.
pub fn register<C: AerisConfig + ?Sized>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), AerisPlanError> {
    let Some(account) = cfg.aeris_account() else {
        return Ok(());
    };
    let stations = cfg.aeris_stations();
    if stations.is_empty() {
        warn!(
            "an Aeris account is configured and no station is paired with it, so NO \
             forecast or alert component is planned and nothing will be fetched. Pair a \
             station (`station_service` with `service = 'aeris'`), or switch the account \
             off."
        );
        return Ok(());
    }
    for station in &stations {
        let Some((latitude, longitude)) = station.coordinates() else {
            return Err(AerisPlanError::NoCoordinates {
                station: station.key.to_string(),
            });
        };
        for span in Span::ALL {
            out.push(forecast_spec(
                &account,
                &station.key,
                latitude,
                longitude,
                span,
            )?);
        }
        out.push(alerts_spec(&account, &station.key, latitude, longitude)?);
    }
    // One sweep for every station's rows. Planned last so that a configuration with
    // pairings but a bad deployment zone still refuses on the zone rather than on the
    // first station.
    out.push(gc_spec(cfg.deployment_zone())?);
    info!(
        stations = stations.len(),
        components = stations.len() * 3 + 1,
        "planned the Aeris components: two forecast spans and one alert fetch per station, \
         and one expired-alert sweep"
    );
    Ok(())
}

/// Why a configuration does not produce an Aeris plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AerisPlanError {
    /// A paired station with no latitude or no longitude.
    #[error(
        "station `{station}` is paired with the Aeris service and has no latitude or no \
         longitude, and the forecast API is queried by coordinates -- so this station can \
         never be asked about. Set `config.stations.latitude` and `longitude`, or remove \
         the `station_service` row with `service = 'aeris'`."
    )]
    NoCoordinates {
        /// Which station.
        station: String,
    },

    /// A configured `api_endpoint` that is not a usable base URL.
    #[error("the configured Aeris `api_endpoint` is not usable")]
    Endpoint {
        /// Why.
        #[source]
        source: EndpointError,
    },

    /// A station key, or the sweep's name, that cannot name a component.
    #[error("`{what}` cannot name a component")]
    BadId {
        /// What was being named.
        what: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },
}

/// Declare `aeris:forecast:<station>:<span>`.
fn forecast_spec(
    account: &AerisAccount,
    station: &StationKey,
    latitude: f64,
    longitude: f64,
    span: Span,
) -> Result<ComponentSpec, AerisPlanError> {
    let name = format!("{FORECAST_PREFIX}:{station}:{}", span.span_hours());
    let id = component_id(&name)?;
    let endpoint = endpoint(account, "forecasts", latitude, longitude)?;
    // An override replaces the derived period and is still floored, which is
    // `ResolveInterval`'s behaviour with the clamp moved to plan time.
    let period = match account.refresh_interval {
        Some(configured) => configured.max(MIN_REFRESH),
        None => span.derived_period(),
    };
    if account.refresh_interval.is_some_and(|c| c < MIN_REFRESH) {
        warn!(
            component = %id,
            configured_seconds = account.refresh_interval.map(|d| d.as_secs()),
            floor_seconds = MIN_REFRESH.as_secs(),
            "the configured Aeris refresh interval is below this component's floor and has \
             been raised to it. Forecasts change slowly and the floor is what keeps this \
             inside the API's rate budget."
        );
    }
    let digest = ConfigDigest::of(&format!(
        "{};{};{};{:.6},{:.6};{}",
        account.client_id,
        account.base(),
        span.span_hours(),
        latitude,
        longitude,
        period.as_secs()
    ));
    let mut query = account.credentials();
    query.set("filter", &span.filter());
    query.set("limit", &span.periods().to_string());
    let station = station.clone();
    let id_for_body = id.clone();
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period,
            deadline: deadline(period),
            // See the module documentation: §1.5 names this component as the example of
            // why `AtPhase` is right and `Immediate` is a billing event.
            first: FirstRun::AtPhase,
            // Two spans for one station, and N stations, separated by a reproducible
            // amount. `Aligned` would put every forecast fetch on the same second, which
            // against one vendor with one rate budget is the one phase choice to avoid.
            phase: Phase::Spread,
            floor: MIN_REFRESH,
        },
        Builder::tick(move |cx| {
            let station = station.clone();
            let id = id_for_body.clone();
            let fetch = Https::new(endpoint.clone());
            let query = query.clone();
            async move {
                let store = cx.hard::<Arc<dyn ForecastStore>>()?;
                m::arm_fetch(&id, FORECAST_KIND);
                Ok(ForecastFetch {
                    id,
                    station,
                    span,
                    store,
                    fetch: Arc::new(fetch),
                    query,
                })
            }
        }),
    )
    // **`H W`**, which W2a §4.3's table writes and which is the access this component
    // actually takes: it never reads a forecast. `api:legacy` holds the same trait object
    // with `Strength::Soft` and `Access::Read`, and cannot write through it because the
    // only thing it has to pass is a `ForecastQuery`.
    .requires([Need::hard_write(Capability::ForecastStore)])
    // **W2a Decision 20's declaration, and the done-criterion of this work item.**
    // `/forecast` depending on Aeris having fetched is *data presence*, not capability
    // availability: modelling it as a startup edge would mean the website does not serve
    // until Aeris has succeeded, which is worse than today. So it is declared here, read by
    // W5 and by `/test/*`, and **never by the scheduler**.
    //
    // Twice the period, because one missed occurrence is not a stale forecast -- the
    // previous one is still inside its own validity -- and two is the first count that
    // cannot be explained by a single failed fetch.
    .freshness([Freshness {
        capability: Capability::ForecastStore,
        populated_by: component_id(&name)?,
        expected_max_age: period * 2,
    }])
    .config_digest(digest))
}

/// Declare `aeris:alerts:<station>`.
fn alerts_spec(
    account: &AerisAccount,
    station: &StationKey,
    latitude: f64,
    longitude: f64,
) -> Result<ComponentSpec, AerisPlanError> {
    let name = format!("{ALERTS_PREFIX}:{station}");
    let id = component_id(&name)?;
    let endpoint = endpoint(account, "alerts", latitude, longitude)?;
    let digest = ConfigDigest::of(&format!(
        "{};{};{:.6},{:.6}",
        account.client_id,
        account.base(),
        latitude,
        longitude
    ));
    // No `filter` and no `limit`: the alerts endpoint takes the credentials alone, which is
    // what `fetchAndStoreAlerts` sends.
    let query = account.credentials();
    let station = station.clone();
    let id_for_body = id.clone();
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period: ALERT_PERIOD,
            deadline: deadline(ALERT_PERIOD),
            first: FirstRun::AtPhase,
            phase: Phase::Spread,
            // The period *is* the floor: there is no column to configure it from, so a
            // value below it is unrepresentable rather than clamped.
            floor: ALERT_PERIOD,
        },
        Builder::tick(move |cx| {
            let station = station.clone();
            let id = id_for_body.clone();
            let fetch = Https::new(endpoint.clone());
            let query = query.clone();
            async move {
                let store = cx.hard::<Arc<dyn AlertStore>>()?;
                m::arm_fetch(&id, ALERTS_KIND);
                Ok(AlertFetch {
                    id,
                    station,
                    store,
                    fetch: Arc::new(fetch),
                    query,
                })
            }
        }),
    )
    .requires([Need::hard_write(Capability::AlertStore)])
    .freshness([Freshness {
        capability: Capability::AlertStore,
        populated_by: component_id(&name)?,
        expected_max_age: ALERT_PERIOD * 2,
    }])
    .config_digest(digest))
}

/// Declare `aeris:alerts:gc`.
fn gc_spec(zone: Option<Zone>) -> Result<ComponentSpec, AerisPlanError> {
    let id = component_id(GC_ID)?;
    let digest = ConfigDigest::of(&format!(
        "{};{}",
        zone.map_or("<none>", Zone::name),
        ALERT_RETENTION.as_secs()
    ));
    Ok(ComponentSpec::new(
        id.clone(),
        Trigger::Calendar {
            // One slot, so the occurrence key's slot index is always 0 and the key is
            // effectively the local date -- which is what makes a fall-back hour's second
            // pass recognisable as already completed (W2a Decision 13) rather than running
            // the sweep twice.
            spec: CronSpec::daily_at(GC_AT),
            // **No station.** One sweep covers every row, so the zone is the deployment's.
            tz: TzSource::Deployment { zone },
            // The default, and what makes this correct after a restart at noon: the
            // supervisor makes up the most recent missed occurrence, so a node that was off
            // at 03:00 sweeps on the way up instead of accumulating a day's expired alerts
            // per day it was down.
            misfire: Misfire::DEFAULT,
            deadline: GC_DEADLINE,
        },
        Builder::tick(move |cx| {
            let id = id.clone();
            async move {
                let store = cx.hard::<Arc<dyn AlertStore>>()?;
                m::arm_fetch(&id, GC_KIND);
                Ok(AlertSweep { id, store })
            }
        }),
    )
    .requires([Need::hard_write(Capability::AlertStore)])
    .config_digest(digest))
}

/// One occurrence's deadline: the attempt budget, capped at the period.
///
/// `deadline <= period` is a plan-time check (`PlanError::DeadlineExceedsPeriod`), and the
/// alerts component's period is fifteen minutes while the forecast's is an hour — so the
/// `min` is what keeps one constant correct for both. The budget itself is one attempt:
/// there is no retry on this path (see the module documentation), so the deadline only has
/// to cover a connect, a handshake, a request and a bounded read.
fn deadline(period: Duration) -> Duration {
    (ATTEMPT_TIMEOUT + Duration::from_secs(5)).min(period)
}

/// `<base>/<kind>/<lat>,<lon>` as an [`Endpoint`].
///
/// The coordinates are formatted to six decimal places, which is `fmt.Sprintf("%.6f,%.6f",
/// ...)` — the form the Go daemon has always sent and roughly a tenth of a metre, so the
/// vendor's own caching sees the same key for the same station.
fn endpoint(
    account: &AerisAccount,
    kind: &str,
    latitude: f64,
    longitude: f64,
) -> Result<Endpoint, AerisPlanError> {
    let url = format!(
        "{}/{kind}/{latitude:.6},{longitude:.6}",
        account.base(),
        latitude = latitude,
        longitude = longitude
    );
    Endpoint::parse(&url).map_err(|source| AerisPlanError::Endpoint { source })
}

/// A component id, with the refusal naming what could not be one.
fn component_id(name: &str) -> Result<ComponentId, AerisPlanError> {
    ComponentId::new(name).map_err(|source| AerisPlanError::BadId {
        what: name.to_string(),
        source,
    })
}

/// One `GET` against one Aeris endpoint.
///
/// The seam, for `crate::uploader::Transmit`'s reason: *"it does not talk to a third party
/// in a test."* Every parse, refusal and store path below is asserted against a recording
/// implementation of this trait, and the socket and the handshake are tested by neither —
/// which is the same boundary `crate::https`' module documentation draws, and necessary
/// for a stronger reason here: a test that fetched from Aeris would spend the owner's API
/// quota on every `cargo test`.
pub trait Fetch: Send + Sync + fmt::Debug + 'static {
    /// The decoded response body, or why there is none.
    fn get(&self, query: Form) -> BoxFuture<'static, Result<Vec<u8>, AerisError>>;
}

/// [`Fetch`] over [`crate::https`].
#[derive(Debug, Clone)]
struct Https {
    endpoint: Arc<Endpoint>,
}

impl Https {
    fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
        }
    }
}

impl Fetch for Https {
    fn get(&self, query: Form) -> BoxFuture<'static, Result<Vec<u8>, AerisError>> {
        let endpoint = Arc::clone(&self.endpoint);
        Box::pin(async move {
            let target = endpoint.target();
            // The attempt timeout, which `crate::https::get` does not own: `CONNECT_TIMEOUT`
            // bounds the connect alone, and a peer that accepts and then says nothing would
            // otherwise run to the supervisor's deadline -- reported as
            // `DeadlineExceeded` rather than as a timeout against a named target. The Go
            // client's own `Timeout: 5 * time.Second` is the same idea with a tighter
            // number; ten seconds is this crate's existing one, from `crate::uploader`.
            let exchange = https::get(&endpoint, query.as_str(), MAX_BODY_BYTES);
            let (head, body) = match tokio::time::timeout(ATTEMPT_TIMEOUT, exchange).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(AerisError::Transport(crate::error::UploadError::Timeout {
                        target,
                        after: ATTEMPT_TIMEOUT,
                    }))
                }
            };
            // The status, for the failures a status *does* describe -- a `429`, a `503`
            // with a `Retry-After`, a `404` on a mistyped endpoint. `success: false` in a
            // `200` body is the other half and belongs to whoever parses the payload, which
            // is why this is `classify` and not a verdict.
            https::classify(&head, &target, Utc::now())?;
            Ok(body)
        })
    }
}

/// The metric's `kind` label for a forecast fetch.
const FORECAST_KIND: &str = "forecast";
/// The metric's `kind` label for an alert fetch.
const ALERTS_KIND: &str = "alerts";
/// The metric's `kind` label for the sweep.
const GC_KIND: &str = "alerts_gc";

/// `aeris:forecast:<station>:<span>`'s body.
struct ForecastFetch {
    id: ComponentId,
    station: StationKey,
    span: Span,
    store: Arc<dyn ForecastStore>,
    fetch: Arc<dyn Fetch>,
    query: Form,
}

impl fmt::Debug for ForecastFetch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForecastFetch")
            .field("component", &self.id)
            .field("station", &self.station)
            .field("span_hours", &self.span.span_hours())
            .finish_non_exhaustive()
    }
}

impl Tick for ForecastFetch {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            // Cancellation observed at the await point (**D-24**): there is no stop method
            // anywhere in the abstraction, and a cancelled fetch is a stop rather than a
            // failure.
            let body = tokio::select! {
                biased;
                () = cx.cancelled() => {
                    m::fetch(&self.id, FORECAST_KIND, "cancelled");
                    return Err(ComponentError::Cancelled);
                }
                result = self.fetch.get(self.query.clone()) => match result {
                    Ok(body) => body,
                    Err(e) => return self.failed(e, "fetch a forecast from Aeris"),
                },
            };
            let data = match forecast::periods_json(&body) {
                Ok(data) => data,
                Err(e) => return self.failed(e, "read the Aeris forecast response"),
            };
            // **The store write is the occurrence's outcome**, which is why a failure here
            // is not reported as a fetch failure: the API answered, the quota was spent,
            // and what failed is the database. `StoreError`'s own classification is what
            // the supervisor's retry reads.
            if let Err(e) = self
                .store
                .store_forecast(&self.station, self.span.span_hours(), &data)
                .await
            {
                m::fetch(&self.id, FORECAST_KIND, "store");
                return Err(ComponentError::tick("store the Aeris forecast", e));
            }
            m::fetch(&self.id, FORECAST_KIND, m::FETCH_OK);
            debug!(
                component = %self.id,
                station = %self.station,
                span_hours = self.span.span_hours(),
                bytes = data.len(),
                "stored a refreshed forecast"
            );
            Ok(())
        })
    }
}

impl ForecastFetch {
    /// Count the failure and report it, with the reason as the metric's label.
    fn failed(&self, e: AerisError, what: &'static str) -> Result<(), ComponentError> {
        m::fetch(&self.id, FORECAST_KIND, e.reason());
        Err(ComponentError::tick(what, e))
    }
}

/// `aeris:alerts:<station>`'s body.
struct AlertFetch {
    id: ComponentId,
    station: StationKey,
    store: Arc<dyn AlertStore>,
    fetch: Arc<dyn Fetch>,
    query: Form,
}

impl fmt::Debug for AlertFetch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlertFetch")
            .field("component", &self.id)
            .field("station", &self.station)
            .finish_non_exhaustive()
    }
}

impl Tick for AlertFetch {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let body = tokio::select! {
                biased;
                () = cx.cancelled() => {
                    m::fetch(&self.id, ALERTS_KIND, "cancelled");
                    return Err(ComponentError::Cancelled);
                }
                result = self.fetch.get(self.query.clone()) => match result {
                    Ok(body) => body,
                    Err(e) => {
                        m::fetch(&self.id, ALERTS_KIND, e.reason());
                        return Err(ComponentError::tick("fetch alerts from Aeris", e));
                    }
                },
            };
            let rows = match alerts::alerts(&body) {
                Ok(rows) => rows,
                Err(e) => {
                    m::fetch(&self.id, ALERTS_KIND, e.reason());
                    return Err(ComponentError::tick("read the Aeris alerts response", e));
                }
            };
            // An empty fetch is a successful occurrence that writes nothing, and that is
            // the ordinary case: most stations are under no warning most of the time. The
            // store's own documentation has why it must not be a delete.
            let written = match self.store.store_alerts(&self.station, &rows).await {
                Ok(written) => written,
                Err(e) => {
                    m::fetch(&self.id, ALERTS_KIND, "store");
                    return Err(ComponentError::tick("store the Aeris alerts", e));
                }
            };
            m::fetch(&self.id, ALERTS_KIND, m::FETCH_OK);
            if written > 0 {
                info!(
                    component = %self.id,
                    station = %self.station,
                    alerts = written,
                    "stored {written} weather alert(s)"
                );
            } else {
                debug!(
                    component = %self.id,
                    station = %self.station,
                    "no active alerts for this station"
                );
            }
            Ok(())
        })
    }
}

/// `aeris:alerts:gc`'s body.
struct AlertSweep {
    id: ComponentId,
    store: Arc<dyn AlertStore>,
}

impl fmt::Debug for AlertSweep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlertSweep")
            .field("component", &self.id)
            .finish_non_exhaustive()
    }
}

impl Tick for AlertSweep {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            // The cutoff is computed here and not in the store, because twenty-four hours
            // past expiry is a retention decision and the store should not hold a copy of
            // it -- the same division `LatestQuery::since` makes for the ten-minute
            // recency rule.
            let cutoff = cutoff(Utc::now());
            let gone = match self.store.sweep_expired_alerts(cutoff).await {
                Ok(gone) => gone,
                Err(e) => {
                    m::fetch(&self.id, GC_KIND, "store");
                    return Err(ComponentError::tick("sweep expired weather alerts", e));
                }
            };
            m::fetch(&self.id, GC_KIND, m::FETCH_OK);
            if gone > 0 {
                info!(
                    component = %self.id,
                    alerts = gone,
                    retention_hours = ALERT_RETENTION.as_secs() / 3600,
                    "deleted {gone} alert(s) that expired more than 24 hours ago"
                );
            } else {
                debug!(component = %self.id, "no expired alerts to delete");
            }
            Ok(())
        })
    }
}

/// The instant an alert must have expired before to be swept.
///
/// Its own function so the one arithmetic fact of this component is unit-testable without
/// a database: `now - 24 h`, which is `cleanupExpiredAlerts`' own expression.
fn cutoff(now: DateTime<Utc>) -> DateTime<Utc> {
    now - chrono::Duration::from_std(ALERT_RETENTION).unwrap_or(chrono::Duration::zero())
}

#[cfg(test)]
mod tests;
