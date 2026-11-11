//! The registrar: `aprs:<name>`, `pws:<name>` and `wu:<name>`, one component per uploader.
//!
//! > | `crates/mw-output/src/plan.rs` | +1 arm: `Every { ≥ floor }`,
//! > `requires: [WindowedView(Hard), StationRegistry(Hard)]` |
//! > — `docs/service-architecture.md` §9.3
//!
//! # `StationRegistry` is **not** declared, and A-M3-3-1 is why
//!
//! §9.3 gives all three uploaders `StationRegistry` as a hard need, and
//! `docs/component-model.md` §4.2's component table gives it to **`aprs:<device>` only** —
//! `pws:<device>` and `wu:<device>` are `WindowedView` H alone there. So two of the three
//! components match that table exactly and only the APRS row departs. Declaring it would
//! refuse every configuration this program can have: W2a §4.1b gives `StationRegistry` to a `config:sqlite` component and
//! **no work item in `docs/implementation-plan.md` creates one**, so a hard need on it is "a
//! requirement nobody provides" and `plan()` refuses the whole configuration (**D-44**).
//!
//! That is not a new discovery — `docs/reconciliation.md`'s **A-M3-3-1** reached it for
//! `writer:timescale`, which needs the same handle for the same kind of reason and takes it
//! *through the registrar* instead. The uploaders need less than that: what they want from
//! a station is its coordinates and its key, both of which are `config.stations` columns
//! this registrar already reads at plan time. So nothing is resolved at run time and the
//! only declared need is `WindowedView`.
//!
//! # Clamp-and-log for the two with a feedback channel; refuse for the radio
//!
//! §4b.5 asks for both and they are different mechanisms:
//!
//! - **HTTP and APRS-IS are clamped**, once, here, with a single warning naming the
//!   component. `ResolveUploadInterval` in the Go tree clamps and warns **per call**, and
//!   `aprs/controller.go`'s worker re-reads it every cycle — so an APRS device configured
//!   below its floor warns once per report, forever. (The two HTTP controllers call it once
//!   per device at start, so §4b.5's "a warning per minute forever" is true of the APRS
//!   path rather than of all three; the clamp leaving the configuration still saying the
//!   wrong thing is true of all of them.)
//! - **A KISS beacon is refused**, by `crate::RadioInterval`'s only constructor, because a
//!   radio has no `429` and flooding a shared band has no feedback mechanism at all
//!   (W2a Decision 60, and **O-8** answered 600 s).
//!
//! After the clamp the declared `Trigger::Every { floor }` equals the clamped period, so
//! `PlanError::IntervalBelowFloor` is a backstop that can only fire on a bug in this file.
//! Stated rather than left for a reader to wonder about.
//!
//! # What a row has to have before a component exists for it
//!
//! The `uploader` table's `enabled` column is the switch (W5 §1.4's "Stop sharing with
//! Weather Underground"); a disabled row is not planned, which under W2a §2.5 stops the
//! component. Today `RemoveController` deletes a map entry and logs "will stop on next app
//! restart".
//!
//! Beyond that, each kind has a credential and an identity it cannot work without, and a
//! row missing one is a **plan error naming the row** rather than a component that starts
//! and silently does nothing — which is what the Go readiness predicates produce
//! (`aprsDeviceReady` returns false and the worker is simply never started, with one `Debugf`
//! nobody reads).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mw_aprs::{Callsign, Passcode, Position, Symbol};
use mw_component::{
    Builder, Capability, ComponentId, ComponentIdError, ComponentSpec, ConfigDigest, FirstRun,
    Need, Phase, Trigger, WindowedView,
};
use mw_types::StationKey;
use secrecy::SecretString;
use tracing::warn;

use crate::encoder::account::Account;
use crate::encoder::{cwop, pws, wu};
use crate::error::Target;
use crate::frame::{FrameSink, SharedSink};
use crate::https::{Endpoint, EndpointError, HttpUpload, AGENT};
use crate::interval::{RadioInterval, TooFast};
use crate::retry::DEFAULT_BUDGET;
use crate::uploader::{self, Uploader, ATTEMPT_TIMEOUT, LIMIT_PERIOD};

/// Weather Underground's floor, and its default interval.
///
/// `MinWUUploadInterval` is 60 s in the Go tree and the **Go** schema's column default is
/// 300 s, both carried forward. The new schema deliberately has no `DEFAULT` on
/// `uploader.interval_seconds`: NULL means "the kind's default", which is this constant.
pub const WU_FLOOR: Duration = Duration::from_secs(60);
/// Weather Underground's default period.
pub const WU_DEFAULT: Duration = Duration::from_secs(300);

/// PWS Weather's floor and default, both 60 s — `MinPWSUploadInterval`.
pub const PWS_FLOOR: Duration = Duration::from_secs(60);
/// PWS Weather's default period.
pub const PWS_DEFAULT: Duration = Duration::from_secs(60);

/// APRS-IS's floor: CWOP asks for no more than one report every five minutes.
///
/// `minAPRSInterval` in the Go tree, carried forward. **Not** the RF floor, which is
/// `RadioInterval::FLOOR` and is 600 s — the asymmetry is the decision (§4b.5).
pub const APRS_IS_FLOOR: Duration = Duration::from_secs(300);
/// APRS's default period, which is the Go default.
pub const APRS_DEFAULT: Duration = Duration::from_secs(300);

/// Weather Underground's endpoint when `api_endpoint` is NULL.
pub const WU_ENDPOINT: &str =
    "https://weatherstation.wunderground.com/weatherstation/updateweatherstation.php";
/// PWS Weather's endpoint when `api_endpoint` is NULL.
pub const PWS_ENDPOINT: &str = "https://pwsupdate.pwsweather.com/api/v1/submitwx";

/// What this crate needs from the configuration to plan an uploader.
///
/// A trait rather than a type for the reason `mw_station::StationConfig` is one: `plan()` is
/// generic over the configuration and the binary chooses it (§1.3 rule 2), so a registrar
/// takes `&C` and each component crate names only the rows it plans from.
pub trait OutputConfig {
    /// Every enabled `uploader` row, with its `station_service` pairing resolved.
    fn uploaders(&self) -> Vec<UploaderRow>;

    /// The maintained window values, as the capability the uploaders declare.
    ///
    /// `None` is a node with no rollup, which is a node with no weather database — and
    /// therefore no windows to upload. The handle travels through the registrar rather than
    /// the capability bus for the reason `mw_pipeline::rollup`'s does: `main` owns the
    /// state as a root resource, two components write it, and the bus holds one handle type
    /// per provider.
    ///
    /// **A `Hard` need on `WindowedView` is still declared**, because that is what makes a
    /// restart leave the uploaders in `Waiting { on: WindowedView }` until the rollup has
    /// published rather than beaconing an empty view — A-M4-2-1's reasoning, from the other
    /// side.
    fn windowed_view(&self) -> Option<&Arc<dyn WindowedView>>;

    /// Whether this node maintains windows for `station`.
    ///
    /// **Not the same question as "is this station configured".** The windowed view holds
    /// the stations this node *collects* for, because a window is accumulated from frames
    /// this node received — so a station a peer forwards readings for, or one whose
    /// collection is switched off, is absent from it. `WindowedView::resolve` answers
    /// `WindowError::UnknownStation` for such a station, which classifies `Fatal`, so an
    /// uploader paired with one would fail **every occurrence, forever**, with the reason
    /// visible only in that component's `last_error`. That is precisely the shape this
    /// item exists to remove, so the registrar refuses the pairing instead.
    fn maintains_windows_for(&self, station: &StationKey) -> bool;

    /// Whether `station` exists and has collection switched off.
    ///
    /// Separated from [`OutputConfig::maintains_windows_for`] because the two have
    /// different right answers. A disabled station is a **reversible operator action**
    /// (W5 §1.2) and refusing to start over one would mean a configuration change could not
    /// be undone without editing the uploader too — so that case is skipped with a warning
    /// naming the row. A station this node simply does not collect for is a pairing that
    /// cannot work, and is refused.
    fn station_is_disabled(&self, station: &StationKey) -> bool;
}

/// One `uploader` row, joined to its `station_service` pairing.
///
/// The column names are the schema's, so a reader can check this against
/// `migrations/config/20261102230000_config_schema_initial.sql` without a join in their
/// head. `mw-store` owns the query and may not name this crate; the binary is where the two
/// meet (§1.3 rule 1).
#[derive(Debug, Clone)]
pub struct UploaderRow {
    /// `uploader.kind` — `wunderground`, `pwsweather` or `aprs`.
    pub kind: String,
    /// `uploader.name`, which is the component id's stable suffix.
    pub name: String,
    /// `uploader.interval_seconds`. `None` is the kind's default.
    pub interval_seconds: Option<i64>,
    /// `uploader.api_endpoint`. `None` is the kind's default endpoint.
    pub api_endpoint: Option<String>,
    /// `uploader.api_key` — Weather Underground's and PWS Weather's credential.
    pub api_key: Option<SecretString>,

    /// The paired station's `stations.station_key`.
    ///
    /// `None` is a row with no `station_service` pairing, which is an uploader configured
    /// to share nothing.
    pub station_key: Option<String>,
    /// `station_service.remote_station_id` — this station's identifier at the far end.
    pub remote_station_id: Option<String>,
    /// `stations.latitude`, degrees north. APRS only.
    pub latitude: Option<f64>,
    /// `stations.longitude`, degrees east. APRS only.
    pub longitude: Option<f64>,
    /// `stations.aprs_callsign` — public identity, on the station row.
    pub aprs_callsign: Option<String>,
    /// `stations.enabled`, for the paired station. `None` is no pairing.
    ///
    /// Read by the binary's `OutputConfig` rather than by this file, which asks the
    /// configuration the two questions rather than deriving them — see
    /// [`OutputConfig::maintains_windows_for`].
    pub station_enabled: Option<bool>,
    /// `stations.transport_kind`, for the paired station. `None` on an existing station is
    /// one a peer forwards readings for.
    pub station_transport_kind: Option<String>,

    /// `uploader.aprs_passcode`.
    pub aprs_passcode: Option<String>,
    /// `uploader.aprs_symbol_table`.
    pub aprs_symbol_table: Option<String>,
    /// `uploader.aprs_symbol_code`.
    pub aprs_symbol_code: Option<String>,
    /// `uploader.aprs_comment`.
    pub aprs_comment: Option<String>,
    /// `uploader.aprs_server`, as `host:port`.
    pub aprs_server: Option<String>,
    /// `uploader.aprs_transport` — `aprs-is`, `kiss-serial` or `kiss-tcp`.
    pub aprs_transport: Option<String>,
    /// `uploader.aprs_kiss_serial_device`.
    pub aprs_kiss_serial_device: Option<String>,
    /// `uploader.aprs_kiss_serial_baud`.
    pub aprs_kiss_serial_baud: Option<i64>,
    /// `uploader.aprs_kiss_tcp_address`, as `host:port`.
    pub aprs_kiss_tcp_address: Option<String>,
    /// `uploader.aprs_kiss_path`, comma-separated.
    pub aprs_kiss_path: Option<String>,
    /// `uploader.aprs_kiss_destination`.
    pub aprs_kiss_destination: Option<String>,
}

/// Declare one component per enabled uploader.
pub fn register<C: OutputConfig + ?Sized>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), OutputPlanError> {
    let rows = cfg.uploaders();
    if rows.is_empty() {
        return Ok(());
    }
    let Some(windows) = cfg.windowed_view() else {
        // A node with uploaders and no rollup is a node with no weather database, so there
        // is nothing to upload. Reported rather than refused: the `uploader` rows are
        // configuration a collector node would have, and a satellite reaching `plan()`
        // without a database is W7 §8.2's shape.
        // `warn`, not `info`: an operator who enabled sharing and gets nothing uploaded,
        // forever, with one info line is the Go failure mode this item exists to remove.
        warn!(
            uploaders = rows.len(),
            "this node has no window rollup, so NO rate-limited output is planned and \
             nothing will be uploaded -- there are no windowed values to upload. A node \
             that uploads has a weather database and collects for at least one station."
        );
        return Ok(());
    };
    // One pass, so that two rows naming the same component id are a named refusal rather
    // than `PlanError::DuplicateComponentId` -- which is the same answer with less in it.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        // The pairing has to name a station this node maintains windows for, or the
        // component would fail every occurrence. `OutputConfig::maintains_windows_for` has
        // the whole argument; the two outcomes differ because the two causes do.
        if let Some(station) = row.station_key.as_deref().map(StationKey::new) {
            if !cfg.maintains_windows_for(&station) {
                if cfg.station_is_disabled(&station) {
                    warn!(
                        uploader = %row.name,
                        kind = %row.kind,
                        %station,
                        "not planning this uploader: the station it shares is disabled, so \
                         this node maintains no windows for it. Re-enable the station, or \
                         remove the `station_service` pairing."
                    );
                    continue;
                }
                return Err(OutputPlanError::StationNotCollected {
                    name: row.name.clone(),
                    kind: row.kind.clone(),
                    station: station.to_string(),
                });
            }
        }
        let spec = declare(&row, windows)?;
        let station = row.station_key.clone().unwrap_or_default();
        if let Some(first) = seen.insert(spec.id.to_string(), station.clone()) {
            // `uploader.name` is UNIQUE, so two rows with the same name are two
            // `station_service` pairings on one account -- which is W7 D-38's arity, and
            // the refusal the `station_service` table's own comment predicts.
            return Err(OutputPlanError::Arity {
                name: row.name.clone(),
                kind: row.kind.clone(),
                first,
                second: station,
            });
        }
        out.push(spec);
    }
    Ok(())
}

/// Declare one uploader.
fn declare(
    row: &UploaderRow,
    windows: &Arc<dyn WindowedView>,
) -> Result<ComponentSpec, OutputPlanError> {
    let station = station_key(row)?;
    match row.kind.as_str() {
        "wunderground" => http_spec(row, windows, station, Kind::Wu),
        "pwsweather" => http_spec(row, windows, station, Kind::Pws),
        "aprs" => aprs_spec(row, windows, station),
        other => Err(OutputPlanError::UnknownKind {
            name: row.name.clone(),
            kind: other.to_string(),
        }),
    }
}

/// Which HTTP service a row is, so the two share one function.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Wu,
    Pws,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Wu => "wu",
            Kind::Pws => "pws",
        }
    }

    fn floor(self) -> Duration {
        match self {
            Kind::Wu => WU_FLOOR,
            Kind::Pws => PWS_FLOOR,
        }
    }

    fn default_interval(self) -> Duration {
        match self {
            Kind::Wu => WU_DEFAULT,
            Kind::Pws => PWS_DEFAULT,
        }
    }

    fn default_endpoint(self) -> &'static str {
        match self {
            Kind::Wu => WU_ENDPOINT,
            Kind::Pws => PWS_ENDPOINT,
        }
    }
}

/// `wu:<name>` or `pws:<name>`.
fn http_spec(
    row: &UploaderRow,
    windows: &Arc<dyn WindowedView>,
    station: StationKey,
    kind: Kind,
) -> Result<ComponentSpec, OutputPlanError> {
    let id = component_id(kind.prefix(), &row.name)?;
    let api_key = row
        .api_key
        .clone()
        .ok_or_else(|| OutputPlanError::MissingCredential {
            name: row.name.clone(),
            column: "api_key",
        })?;
    let remote_id = row
        .remote_station_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| OutputPlanError::MissingStationId {
            name: row.name.clone(),
            station: station.to_string(),
        })?;
    let endpoint = endpoint(row, kind.default_endpoint())?;
    // **A credential may not cross a plaintext connection.** Both services send `ID` and
    // `PASSWORD` as query parameters, so an `api_endpoint` of `http://…` puts the
    // credential on the wire in clear — and `uploader.api_endpoint` has no scheme `CHECK`,
    // unlike `weather_database.url`, so one `UPDATE` is all it takes.
    //
    // Refused at plan time rather than warned about, which is `mw_station::plan`'s
    // precedent for the inverse mismatch: a configuration error belongs in the pure
    // function that exists to catch one. The rule is about the **credential** and not about
    // the scheme, so a future credential-less output may use `http`.
    if !endpoint.is_tls() {
        return Err(OutputPlanError::PlaintextCredential {
            name: row.name.clone(),
            endpoint: endpoint.to_string(),
        });
    }
    let period = clamped(row, &id, kind.default_interval(), kind.floor());
    let account = Account::new(remote_id, api_key, AGENT);
    let target = endpoint.target();
    let transport = HttpUpload::new(endpoint);

    // Everything the builder closes over, so the plan diff restarts the component when any
    // of it changes (`ComponentSpec::fingerprint`). The credential is folded in as its
    // **exposed** value because a changed password must restart the component -- and it
    // reaches only `ConfigDigest::of`, which answers a `u64`.
    let digest = ConfigDigest::of(&(
        kind.prefix(),
        &row.name,
        station.as_str(),
        account.station_id(),
        secret_digest(row.api_key.as_ref()),
        row.api_endpoint.as_deref(),
        period.as_secs(),
    ));

    let windows = Arc::clone(windows);
    let builder = match kind {
        Kind::Wu => tick_builder(
            id.clone(),
            wu::WeatherUnderground::new(account),
            station,
            windows,
            transport,
            target,
            period,
        )?,
        Kind::Pws => tick_builder(
            id.clone(),
            pws::PwsWeather::new(account),
            station,
            windows,
            transport,
            target,
            period,
        )?,
    };
    Ok(spec(id, period, builder, digest))
}

/// `aprs:<name>`.
fn aprs_spec(
    row: &UploaderRow,
    windows: &Arc<dyn WindowedView>,
    station: StationKey,
) -> Result<ComponentSpec, OutputPlanError> {
    let id = component_id("aprs", &row.name)?;
    let callsign_text = row
        .aprs_callsign
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| OutputPlanError::MissingCallsign {
            name: row.name.clone(),
            station: station.to_string(),
        })?;
    let source = Callsign::parse(&callsign_text).map_err(|source| OutputPlanError::Callsign {
        name: row.name.clone(),
        source,
    })?;
    let position = position(row, &station)?;
    let symbol = symbol(row)?;
    let transport_kind = row.aprs_transport.as_deref().unwrap_or("aprs-is");

    // The interval, and the asymmetry: APRS-IS is clamped and a radio is refused.
    //
    // **The default is transport-dependent**, and it has to be. `uploader.interval_seconds`
    // being NULL means "the kind's default" (the column's own comment), and for a KISS row
    // that default cannot be the 300 s the Go tree used for APRS generally — `RadioInterval`
    // would refuse it and the whole configuration would fail to plan, naming an interval the
    // operator never wrote. What the refusal is for is a value somebody *configured* below
    // the floor, which is O-8's upgrade case and W2a Decision 60's "a config parse error".
    let (period, sink) = match transport_kind {
        "aprs-is" => {
            let configured = configured_interval(row, APRS_DEFAULT);
            let period = clamp_with_warning(&id, configured, APRS_DEFAULT, APRS_IS_FLOOR);
            let (host, port) = peer(row.aprs_server.as_deref(), FrameSink::DEFAULT_APRS_IS, row)?;
            let passcode =
                Passcode::resolve(&source, row.aprs_passcode.as_deref()).map_err(|source| {
                    OutputPlanError::Passcode {
                        name: row.name.clone(),
                        source,
                    }
                })?;
            (
                period,
                FrameSink::AprsIs {
                    host,
                    port,
                    source: source.clone(),
                    passcode,
                    software: AGENT.to_string(),
                },
            )
        }
        "kiss-serial" | "kiss-tcp" => {
            // **Refused, not clamped.** `RadioInterval` has the whole argument. The default
            // is the floor itself, so a NULL interval plans and only a configured sub-floor
            // value is refused.
            let configured = configured_interval(row, RadioInterval::FLOOR);
            let interval =
                RadioInterval::new(configured).map_err(|source| OutputPlanError::RadioTooFast {
                    name: row.name.clone(),
                    source,
                })?;
            let dest = kiss_dest(row)?;
            let path = kiss_path(row)?;
            let sink = if transport_kind == "kiss-serial" {
                let dev = row
                    .aprs_kiss_serial_device
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| OutputPlanError::MissingKiss {
                        name: row.name.clone(),
                        column: "aprs_kiss_serial_device",
                    })?;
                FrameSink::KissSerial {
                    dev: PathBuf::from(dev),
                    baud: row
                        .aprs_kiss_serial_baud
                        .and_then(|b| u32::try_from(b).ok())
                        .filter(|b| *b > 0)
                        .unwrap_or(FrameSink::DEFAULT_BAUD),
                    source: source.clone(),
                    dest,
                    path,
                    interval,
                }
            } else {
                let (host, port) = peer(row.aprs_kiss_tcp_address.as_deref(), ("", 0), row)
                    .map_err(|_| OutputPlanError::MissingKiss {
                        name: row.name.clone(),
                        column: "aprs_kiss_tcp_address",
                    })?;
                FrameSink::KissTcp {
                    host,
                    port,
                    source: source.clone(),
                    dest,
                    path,
                    interval,
                }
            };
            (interval.get(), sink)
        }
        // The `default:` arm the Go dispatch gets wrong, as a refusal. The schema's own
        // `CHECK` admits only these three, so reaching here means the database was edited
        // outside it.
        other => {
            return Err(OutputPlanError::UnknownTransport {
                name: row.name.clone(),
                transport: other.to_string(),
            })
        }
    };

    // One string rather than a tuple, because `Hash` is implemented for tuples up to
    // twelve and an APRS row has seventeen values the builder closes over. Written out
    // field by field so that adding a column to the row without adding it here is visible
    // as an omission rather than as a tuple that still compiles.
    let digest = ConfigDigest::of(&format!(
        "aprs|{}|{}|{}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        row.name,
        station.as_str(),
        source,
        secret_digest_str(row.aprs_passcode.as_deref()),
        sink.kind(),
        row.aprs_server.as_deref().unwrap_or_default(),
        row.aprs_kiss_serial_device.as_deref().unwrap_or_default(),
        row.aprs_kiss_serial_baud.unwrap_or_default(),
        row.aprs_kiss_tcp_address.as_deref().unwrap_or_default(),
        row.aprs_kiss_path.as_deref().unwrap_or_default(),
        row.aprs_kiss_destination.as_deref().unwrap_or_default(),
        row.aprs_symbol_table.as_deref().unwrap_or_default(),
        row.aprs_symbol_code.as_deref().unwrap_or_default(),
        row.aprs_comment.as_deref().unwrap_or_default(),
        position.latitude().to_bits(),
        position.longitude().to_bits(),
    ));
    // The period is folded in separately so the format string above stays one line per
    // column: it is not a column of the `uploader` row as the builder sees it, it is the
    // clamped or refused result of one.
    let digest = ConfigDigest::of(&(digest.get(), period.as_secs()));

    let target = sink.target();
    let encoder = cwop::Cwop::new(position, symbol, AGENT, row.aprs_comment.clone());
    let builder = tick_builder(
        id.clone(),
        encoder,
        station,
        Arc::clone(windows),
        SharedSink::new(sink),
        target,
        period,
    )?;
    Ok(spec(id, period, builder, digest))
}

/// The common spec shape: `Every`, one hard need, no provisions.
fn spec(id: ComponentId, period: Duration, build: Builder, digest: ConfigDigest) -> ComponentSpec {
    ComponentSpec::new(
        id,
        Trigger::Every {
            period,
            deadline: deadline(period),
            // **Not `Immediate`.** W2a Decision 10 replaces the Go version's hard-coded
            // 15-second initial upload: the first occurrence is at the next phase slot, so
            // a restart does not post to three services at once and a restart loop cannot
            // become a burst against a rate-limited endpoint.
            first: FirstRun::AtPhase,
            // Two uploaders for two stations are separated by a reproducible amount, and a
            // restart does not reshuffle the schedule (`Phase::Spread`'s own
            // documentation). `Aligned` would put every uploader on the same second.
            phase: Phase::Spread,
            floor: period,
        },
        build,
    )
    .requires([Need::hard(Capability::WindowedView)])
    .config_digest(digest)
}

/// One occurrence's deadline.
///
/// The retry budget has to fit inside it ([`budget_for`], §4b.5) and it has to be
/// `<= period` (`PlanError::DeadlineExceedsPeriod`), which is what the `min` is for. The
/// budget needs `(retries + 1) x (attempt_timeout + limit_period)` = 39 s at the defaults,
/// and the smallest period this program plans is 60 s — so the deadline is 45 s for every
/// plannable configuration and the budget fits with six seconds to spare.
fn deadline(period: Duration) -> Duration {
    let wanted = Duration::from_secs(45);
    wanted.min(period)
}

/// The retry budget, checked against `deadline` — §4b.5's own rule.
///
/// > *"`deadline >= retries × (attempt_timeout + rate_limit_period)` must hold, and it is
/// > checked in `plan()` alongside `deadline <= period`… A retry budget that cannot fit
/// > inside the deadline is a configuration error, not a mysterious stream of
/// > `deadline_exceeded`."*
///
/// **It fits for every period this program can plan**, so this is a backstop rather than a
/// branch an operator reaches: the smallest plannable period is 60 s (Weather Underground's
/// and PWS Weather's floor), [`deadline`] is 45 s there, and three attempts need 39 s. The
/// check is here because the three constants it relates live in two files — lowering
/// `WU_FLOOR`, raising [`ATTEMPT_TIMEOUT`] or raising [`DEFAULT_BUDGET`] would each break it
/// silently, and the symptom would be the stream of `deadline_exceeded` §4b.5 names.
///
/// An earlier version of this function *shrank* the budget to fit instead. That read better
/// as prose and was dead code: it could not fire for any representable configuration, and it
/// was a departure from a named mechanism with nothing to show for it.
fn budget_for(deadline: Duration) -> Result<u8, OutputPlanError> {
    let per_attempt = ATTEMPT_TIMEOUT.saturating_add(LIMIT_PERIOD);
    if uploader::budget_fits(deadline, DEFAULT_BUDGET, ATTEMPT_TIMEOUT, LIMIT_PERIOD) {
        return Ok(DEFAULT_BUDGET);
    }
    Err(OutputPlanError::BudgetExceedsDeadline {
        deadline_s: deadline.as_secs(),
        attempts: u32::from(DEFAULT_BUDGET) + 1,
        per_attempt_s: per_attempt.as_secs(),
    })
}

/// The builder closure, shared by all three kinds.
#[allow(clippy::too_many_arguments)]
fn tick_builder<E, T>(
    id: ComponentId,
    encoder: E,
    station: StationKey,
    windows: Arc<dyn WindowedView>,
    transport: T,
    target: Target,
    period: Duration,
) -> Result<Builder, OutputPlanError>
where
    E: crate::encoder::OutputEncoder + Clone,
    T: uploader::Transmit<Payload = E::Payload>,
{
    let budget = budget_for(deadline(period))?;
    let service = encoder.service();
    Ok(Builder::tick(move |_cx| {
        // **The `WindowedView` handle comes from the registrar, not from `BuildCx`.** It is
        // a root resource `main` owns (`OutputConfig::windowed_view`), the same arrangement
        // `mw_pipeline::rollup`'s two writers have -- and the `Hard` need is still declared,
        // so the component waits for the provider to publish before it is built at all.
        let id = id.clone();
        let encoder = encoder.clone();
        let station = station.clone();
        let windows = Arc::clone(&windows);
        let transport = transport.clone();
        let target = target.clone();
        async move {
            // Zero this component's series before the first occurrence, so "nothing
            // uploaded yet" is visible rather than being an absent series (§5.10).
            crate::metrics::arm(&id, service);
            Ok(Uploader::new(
                id,
                encoder,
                station,
                windows,
                uploader::stack(transport, target, ATTEMPT_TIMEOUT, LIMIT_PERIOD),
                budget,
            ))
        }
    }))
}

/// The paired station, or the reason this uploader has nothing to upload.
fn station_key(row: &UploaderRow) -> Result<StationKey, OutputPlanError> {
    let key = row
        .station_key
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| OutputPlanError::NoPairing {
            name: row.name.clone(),
            kind: row.kind.clone(),
        })?;
    Ok(StationKey::new(key))
}

/// `<prefix>:<name>`.
fn component_id(prefix: &str, name: &str) -> Result<ComponentId, OutputPlanError> {
    let id = format!("{prefix}:{name}");
    ComponentId::new(id.clone()).map_err(|source| OutputPlanError::BadId { id, source })
}

/// The configured interval, if there is a usable one.
fn configured_interval(row: &UploaderRow, default: Duration) -> Duration {
    row.interval_seconds
        .and_then(|s| u64::try_from(s).ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// The clamped period for an HTTP uploader, with the one warning.
fn clamped(row: &UploaderRow, id: &ComponentId, default: Duration, floor: Duration) -> Duration {
    clamp_with_warning(id, configured_interval(row, default), default, floor)
}

/// Clamp to the floor, warning **once**, at plan time, naming the component.
fn clamp_with_warning(
    id: &ComponentId,
    configured: Duration,
    default: Duration,
    floor: Duration,
) -> Duration {
    if configured >= floor {
        return configured;
    }
    warn!(
        component = %id,
        configured_s = configured.as_secs(),
        floor_s = floor.as_secs(),
        default_s = default.as_secs(),
        "the configured upload interval is below this service's minimum and has been \
         raised to it. Set `uploader.interval_seconds` to the floor or above, or leave it \
         NULL for the default -- this warning is emitted once, at startup, and the \
         configuration still says the lower value."
    );
    floor
}

/// The endpoint, or the default.
fn endpoint(row: &UploaderRow, default: &str) -> Result<Endpoint, OutputPlanError> {
    let text = row
        .api_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default);
    Endpoint::parse(text).map_err(|source| OutputPlanError::Endpoint {
        name: row.name.clone(),
        source,
    })
}

/// A `host:port`, or the default pair.
fn peer(
    configured: Option<&str>,
    default: (&str, u16),
    row: &UploaderRow,
) -> Result<(String, u16), OutputPlanError> {
    let Some(text) = configured.map(str::trim).filter(|s| !s.is_empty()) else {
        if default.0.is_empty() {
            return Err(OutputPlanError::MissingKiss {
                name: row.name.clone(),
                column: "aprs_kiss_tcp_address",
            });
        }
        return Ok((default.0.to_string(), default.1));
    };
    // `rsplit_once` so that a bracketed IPv6 literal's own colons are not the separator.
    let (host, port) = text
        .rsplit_once(':')
        .ok_or_else(|| OutputPlanError::BadPeer {
            name: row.name.clone(),
            peer: text.to_string(),
        })?;
    let port = port.parse::<u16>().map_err(|_| OutputPlanError::BadPeer {
        name: row.name.clone(),
        peer: text.to_string(),
    })?;
    if host.is_empty() || port == 0 {
        return Err(OutputPlanError::BadPeer {
            name: row.name.clone(),
            peer: text.to_string(),
        });
    }
    Ok((host.to_string(), port))
}

/// The station's position, which an APRS beacon cannot work without.
fn position(row: &UploaderRow, station: &StationKey) -> Result<Position, OutputPlanError> {
    let (Some(lat), Some(lon)) = (row.latitude, row.longitude) else {
        return Err(OutputPlanError::NoPosition {
            name: row.name.clone(),
            station: station.to_string(),
        });
    };
    Position::new(lat, lon).map_err(|source| OutputPlanError::Position {
        name: row.name.clone(),
        station: station.to_string(),
        source,
    })
}

/// The configured symbol, or the weather-station default.
fn symbol(row: &UploaderRow) -> Result<Symbol, OutputPlanError> {
    let one = |text: &Option<String>, fallback: char| -> Result<char, OutputPlanError> {
        match text.as_deref().filter(|s| !s.is_empty()) {
            None => Ok(fallback),
            Some(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Ok(c),
                    // The schema's `CHECK (length(...) = 1)` makes this unreachable through
                    // the configuration API; reaching it means the database was edited
                    // outside it, and the Go builder's answer is to use the first byte.
                    _ => Err(OutputPlanError::Symbol {
                        name: row.name.clone(),
                        value: s.to_string(),
                    }),
                }
            }
        }
    };
    let table = one(&row.aprs_symbol_table, char::from(Symbol::WEATHER.table()))?;
    let code = one(&row.aprs_symbol_code, char::from(Symbol::WEATHER.code()))?;
    Symbol::new(table, code).map_err(|_| OutputPlanError::Symbol {
        name: row.name.clone(),
        value: format!("{table}{code}"),
    })
}

/// The AX.25 destination, or the default.
fn kiss_dest(row: &UploaderRow) -> Result<Callsign, OutputPlanError> {
    let text = row
        .aprs_kiss_destination
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FrameSink::DEFAULT_DEST);
    Callsign::parse(text).map_err(|source| OutputPlanError::Callsign {
        name: row.name.clone(),
        source,
    })
}

/// The digipeater path, or the default.
fn kiss_path(row: &UploaderRow) -> Result<Vec<Callsign>, OutputPlanError> {
    let text = row
        .aprs_kiss_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FrameSink::DEFAULT_PATH);
    text.split(',')
        .map(str::trim)
        .filter(|hop| !hop.is_empty())
        .map(|hop| {
            Callsign::parse(hop).map_err(|source| OutputPlanError::Callsign {
                name: row.name.clone(),
                source,
            })
        })
        .collect()
}

/// A credential's contribution to the configuration digest.
///
/// The exposed value, hashed — because a changed password must restart the component, and
/// the digest is a `u64` nothing can read back. `None` and an empty string are
/// distinguished, so clearing a credential is a change.
fn secret_digest(key: Option<&SecretString>) -> Option<u64> {
    use secrecy::ExposeSecret;
    key.map(|k| ConfigDigest::of(k.expose_secret()).get())
}

/// The same, for a passcode that is configured as text.
fn secret_digest_str(value: Option<&str>) -> Option<u64> {
    value.map(|v| ConfigDigest::of(v).get())
}

/// Why a set of uploader rows does not produce a plan.
///
/// Every variant names `uploader.name`, because that is the column an operator edits and
/// the component id's suffix. **No variant carries a credential**, which is why the two
/// credential failures are "this column is NULL" rather than "this value is wrong".
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OutputPlanError {
    /// A `kind` the schema's `CHECK` does not admit.
    #[error(
        "uploader `{name}` has the kind `{kind}`, and this program uploads to \
         `wunderground`, `pwsweather` and `aprs`. The `uploader` table's own CHECK admits \
         only those three, so this row was written outside the configuration API."
    )]
    UnknownKind {
        /// `uploader.name`.
        name: String,
        /// What was configured.
        kind: String,
    },

    /// An `aprs_transport` the schema's `CHECK` does not admit.
    ///
    /// **The Go dispatch's `default:` arm, as a refusal.** There, an unrecognised transport
    /// string silently transmits over APRS-IS — so `aprs_transport = "kis"` puts the
    /// weather report on the internet instead of the radio, with no error anywhere.
    #[error(
        "uploader `{name}` has the APRS transport `{transport}`, and the three are \
         `aprs-is`, `kiss-serial` and `kiss-tcp`. This is refused rather than defaulted: \
         defaulting an unrecognised transport to APRS-IS would put a weather report on the \
         internet when the configuration asked for a radio."
    )]
    UnknownTransport {
        /// `uploader.name`.
        name: String,
        /// What was configured.
        transport: String,
    },

    /// A name that cannot be part of a component id.
    #[error("uploader `{id}` cannot name a component")]
    BadId {
        /// What the id would have been.
        id: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },

    /// One uploader paired with more than one station.
    ///
    /// **W7 D-38's arity**, which the `station_service` table's own comment predicts as
    /// `PlanError::UploaderArity`: the one-station arity of `wu`, `pws` and `aprs` is a
    /// property of the KIND rather than of the selector, so it is checked here and not as a
    /// schema constraint. One component is one uploader (W2a D-55) and an uploader sends
    /// one station's windows; two stations on one account need two accounts, because the
    /// far end identifies a station by the credential it arrived under.
    #[error(
        "uploader `{name}` ({kind}) is paired with more than one station -- `{first}` and \
         `{second}`. One component is one uploader and one uploader sends one station's \
         windows, so each station needs its own `uploader` row. Remove the extra \
         `station_service` pairing or add a second account."
    )]
    Arity {
        /// `uploader.name`.
        name: String,
        /// `uploader.kind`.
        kind: String,
        /// The first station.
        first: String,
        /// The second.
        second: String,
    },

    /// An uploader with no `station_service` pairing.
    #[error(
        "uploader `{name}` ({kind}) is enabled and no station is paired with it, so there \
         is nothing for it to upload. Add a `station_service` row naming the station, or \
         set `uploader.enabled = 0`."
    )]
    NoPairing {
        /// `uploader.name`.
        name: String,
        /// `uploader.kind`.
        kind: String,
    },

    /// A pairing whose station this node maintains no windows for.
    ///
    /// Reachable, and the one refusal here that is about another component's station set: a
    /// station with no `transport_kind` is one a peer forwards readings for, and
    /// `window:rollup` maintains windows only for the stations this node collects from. The
    /// configuration schema's **D-45**/**A-W7-2** comment endorses a collector uploading on
    /// behalf of a satellite, so this refusal is the honest report of a gap rather than a
    /// rule: making it work means widening the rollup's station set, which is M3-4's
    /// component. Refusing is still better than the alternative, which is a component in
    /// permanent tick failure with the reason in `last_error`.
    #[error(
        "uploader `{name}` ({kind}) shares station `{station}`, and this node maintains no \
         windows for it -- `window:rollup` accumulates windows only for the stations this \
         node collects from, so a station a peer forwards readings for has none. An \
         uploader sends windows, so this pairing cannot work: remove the `station_service` \
         row, or share a station this node collects."
    )]
    StationNotCollected {
        /// `uploader.name`.
        name: String,
        /// `uploader.kind`.
        kind: String,
        /// The station.
        station: String,
    },

    /// An endpoint that would send the credential in clear.
    #[error(
        "uploader `{name}` has the plaintext endpoint `{endpoint}`, and this service sends \
         `ID` and `PASSWORD` as query parameters -- so the credential would cross the \
         internet in clear. Use `https`, or leave `uploader.api_endpoint` NULL for the \
         service's own endpoint."
    )]
    PlaintextCredential {
        /// `uploader.name`.
        name: String,
        /// The configured endpoint, which carries no query (`crate::error::Target`).
        endpoint: String,
    },

    /// A retry budget that cannot fit inside an occurrence's deadline.
    #[error(
        "a {deadline_s} s occurrence deadline cannot hold {attempts} delivery attempt(s) of \
         up to {per_attempt_s} s each. This is an arithmetic error between this crate's own \
         constants rather than a configuration problem -- `docs/component-model.md` §4b.5 \
         requires `deadline >= attempts x (attempt_timeout + rate_limit_period)`."
    )]
    BudgetExceedsDeadline {
        /// The occurrence deadline.
        deadline_s: u64,
        /// How many attempts were asked for.
        attempts: u32,
        /// What one attempt may cost.
        per_attempt_s: u64,
    },

    /// A row with no credential.
    #[error(
        "uploader `{name}` has no `{column}`, and the service will not accept an upload \
         without one. The value is not quoted in this message on purpose."
    )]
    MissingCredential {
        /// `uploader.name`.
        name: String,
        /// Which column.
        column: &'static str,
    },

    /// A pairing with no identifier at the far end.
    #[error(
        "station `{station}` is paired with uploader `{name}` and has no \
         `station_service.remote_station_id`. That is this station's identifier at the \
         service -- the id it was registered under -- and the upload is addressed to it."
    )]
    MissingStationId {
        /// `uploader.name`.
        name: String,
        /// The station.
        station: String,
    },

    /// A KISS row missing the column its transport needs.
    #[error(
        "uploader `{name}` has a KISS transport and no `{column}`. The schema's CHECK \
         requires it for that transport, so this row was written outside the configuration \
         API."
    )]
    MissingKiss {
        /// `uploader.name`.
        name: String,
        /// Which column.
        column: &'static str,
    },

    /// An APRS row whose station has no callsign.
    #[error(
        "station `{station}` is paired with APRS uploader `{name}` and has no \
         `stations.aprs_callsign`. APRS transmits the callsign on every packet, so there \
         is no default that would be correct."
    )]
    MissingCallsign {
        /// `uploader.name`.
        name: String,
        /// The station.
        station: String,
    },

    /// A callsign that is not one.
    #[error("uploader `{name}` has a callsign that cannot be encoded")]
    Callsign {
        /// `uploader.name`.
        name: String,
        /// Why.
        #[source]
        source: mw_aprs::CallsignError,
    },

    /// A passcode that is not one.
    #[error("uploader `{name}` has an unusable APRS passcode")]
    Passcode {
        /// `uploader.name`.
        name: String,
        /// Why.
        #[source]
        source: mw_aprs::PasscodeError,
    },

    /// An APRS row whose station has no coordinates.
    ///
    /// The Go readiness predicate is `device.Latitude != 0 && device.Longitude != 0`, which
    /// refuses a station in the Gulf of Guinea and silently starts no worker for one whose
    /// coordinates were never set. A `NULL` column is the honest test and a named refusal
    /// is the honest answer.
    #[error(
        "station `{station}` is paired with APRS uploader `{name}` and has no coordinates. \
         An APRS weather report IS a position report, so set `config.stations.latitude` \
         and `longitude` for it."
    )]
    NoPosition {
        /// `uploader.name`.
        name: String,
        /// The station.
        station: String,
    },

    /// Coordinates that are not a position.
    #[error("station `{station}`, paired with APRS uploader `{name}`, is not on the globe")]
    Position {
        /// `uploader.name`.
        name: String,
        /// The station.
        station: String,
        /// Why.
        #[source]
        source: mw_aprs::PositionError,
    },

    /// A symbol that is not two printable bytes.
    #[error(
        "uploader `{name}` has the APRS symbol `{value}`, which is not a printable \
         table-and-code pair. The weather-station symbol is table `/` with code `_`; leave \
         both columns NULL to use it."
    )]
    Symbol {
        /// `uploader.name`.
        name: String,
        /// What was configured.
        value: String,
    },

    /// A `host:port` that is not one.
    #[error(
        "uploader `{name}` has the peer `{peer}`, which is not `host:port`. Both \
         `aprs_server` and `aprs_kiss_tcp_address` carry the port, because neither service \
         has a well-known one this program should assume."
    )]
    BadPeer {
        /// `uploader.name`.
        name: String,
        /// What was configured.
        peer: String,
    },

    /// An endpoint that is not a URL this can request.
    #[error("uploader `{name}` has an unusable `api_endpoint`")]
    Endpoint {
        /// `uploader.name`.
        name: String,
        /// Why.
        #[source]
        source: EndpointError,
    },

    /// A KISS beacon interval below the RF floor.
    #[error("uploader `{name}` would beacon over the air too often")]
    RadioTooFast {
        /// `uploader.name`.
        name: String,
        /// Why.
        #[source]
        source: TooFast,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::{Declaration, Resolved, WindowError};

    #[derive(Debug)]
    struct NoWindows;

    impl WindowedView for NoWindows {
        fn resolve(
            &self,
            station: &StationKey,
            _want: &[Declaration],
        ) -> Result<Resolved, WindowError> {
            Err(WindowError::UnknownStation {
                station: station.clone(),
            })
        }
    }

    fn view() -> Arc<dyn WindowedView> {
        Arc::new(NoWindows)
    }

    struct Cfg {
        rows: Vec<UploaderRow>,
        windows: Option<Arc<dyn WindowedView>>,
        /// Stations this node maintains no windows for and that are not merely disabled —
        /// a station a peer forwards readings for.
        not_collected: Vec<String>,
        /// Stations whose collection is switched off.
        disabled: Vec<String>,
    }

    impl OutputConfig for Cfg {
        fn uploaders(&self) -> Vec<UploaderRow> {
            self.rows.clone()
        }

        fn windowed_view(&self) -> Option<&Arc<dyn WindowedView>> {
            self.windows.as_ref()
        }

        fn maintains_windows_for(&self, station: &StationKey) -> bool {
            !self.not_collected.iter().any(|s| s == station.as_str())
                && !self.disabled.iter().any(|s| s == station.as_str())
        }

        fn station_is_disabled(&self, station: &StationKey) -> bool {
            self.disabled.iter().any(|s| s == station.as_str())
        }
    }

    fn row(kind: &str, name: &str) -> UploaderRow {
        UploaderRow {
            kind: kind.to_string(),
            name: name.to_string(),
            interval_seconds: None,
            api_endpoint: None,
            api_key: None,
            station_key: Some("backyard".to_string()),
            remote_station_id: None,
            latitude: None,
            longitude: None,
            aprs_callsign: None,
            station_enabled: Some(true),
            station_transport_kind: Some("serial".to_string()),
            aprs_passcode: None,
            aprs_symbol_table: None,
            aprs_symbol_code: None,
            aprs_comment: None,
            aprs_server: None,
            aprs_transport: None,
            aprs_kiss_serial_device: None,
            aprs_kiss_serial_baud: None,
            aprs_kiss_tcp_address: None,
            aprs_kiss_path: None,
            aprs_kiss_destination: None,
        }
    }

    fn wu_row() -> UploaderRow {
        UploaderRow {
            api_key: Some(SecretString::from("hunter2")),
            remote_station_id: Some("KWABACKYARD".to_string()),
            ..row("wunderground", "backyard")
        }
    }

    fn aprs_row() -> UploaderRow {
        UploaderRow {
            latitude: Some(45.1),
            longitude: Some(-122.05),
            aprs_callsign: Some("N0CALL".to_string()),
            ..row("aprs", "backyard")
        }
    }

    fn plan(rows: Vec<UploaderRow>) -> Result<Vec<ComponentSpec>, OutputPlanError> {
        let cfg = Cfg {
            rows,
            windows: Some(view()),
            not_collected: Vec::new(),
            disabled: Vec::new(),
        };
        let mut out = Vec::new();
        register(&cfg, &mut out)?;
        Ok(out)
    }

    #[test]
    fn each_kind_produces_one_component_with_one_hard_need() {
        let specs = plan(vec![
            wu_row(),
            UploaderRow {
                kind: "pwsweather".to_string(),
                ..wu_row()
            },
            aprs_row(),
        ])
        .expect("plans");
        let ids: Vec<String> = specs.iter().map(|s| s.id.to_string()).collect();
        assert_eq!(ids, vec!["wu:backyard", "pws:backyard", "aprs:backyard"]);
        for spec in &specs {
            // `WindowedView` and nothing else -- `StationRegistry` is deliberately absent
            // (A-M3-3-1; this module's documentation has why).
            let needs: Vec<Capability> = spec.requires.iter().map(|n| n.capability).collect();
            assert_eq!(needs, vec![Capability::WindowedView], "{}", spec.id);
            assert!(spec.provides.is_empty(), "{}", spec.id);
            assert!(spec.identity.is_none(), "{}", spec.id);
            // The deadline fits inside the period, which is `PlanError::DeadlineExceedsPeriod`
            // one layer up.
            let Trigger::Every {
                period,
                deadline,
                floor,
                first,
                phase,
            } = &spec.trigger
            else {
                panic!("{} is not an `Every`", spec.id);
            };
            assert!(deadline <= period, "{}", spec.id);
            assert_eq!(floor, period, "the clamp happened at plan time");
            assert_eq!(*first, FirstRun::AtPhase, "{}", spec.id);
            assert_eq!(*phase, Phase::Spread, "{}", spec.id);
        }
    }

    /// The defaults, which are the Go values.
    #[test]
    fn the_default_periods_are_the_services_own() {
        let specs = plan(vec![
            wu_row(),
            UploaderRow {
                kind: "pwsweather".to_string(),
                ..wu_row()
            },
            aprs_row(),
        ])
        .expect("plans");
        let periods: Vec<u64> = specs
            .iter()
            .map(|s| match &s.trigger {
                Trigger::Every { period, .. } => period.as_secs(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(periods, vec![300, 60, 300]);
    }

    /// **The asymmetry.** HTTP and APRS-IS are clamped to their floor; a KISS beacon below
    /// the RF floor is refused.
    #[test]
    fn an_http_interval_is_clamped_and_a_radio_interval_is_refused() {
        let specs = plan(vec![UploaderRow {
            interval_seconds: Some(5),
            ..wu_row()
        }])
        .expect("plans");
        match &specs[0].trigger {
            Trigger::Every { period, .. } => assert_eq!(*period, WU_FLOOR),
            other => panic!("{other:?}"),
        }

        // APRS-IS: clamped to five minutes.
        let specs = plan(vec![UploaderRow {
            interval_seconds: Some(30),
            aprs_transport: Some("aprs-is".to_string()),
            ..aprs_row()
        }])
        .expect("plans");
        match &specs[0].trigger {
            Trigger::Every { period, .. } => assert_eq!(*period, APRS_IS_FLOOR),
            other => panic!("{other:?}"),
        }

        // A radio: refused, naming the row and both numbers. This is the value today's
        // default would produce.
        let e = plan(vec![UploaderRow {
            interval_seconds: Some(300),
            aprs_transport: Some("kiss-tcp".to_string()),
            aprs_kiss_tcp_address: Some("tnc.lan:8001".to_string()),
            ..aprs_row()
        }])
        .expect_err("below the RF floor");
        assert!(matches!(e, OutputPlanError::RadioTooFast { .. }), "{e}");
        assert!(
            mw_types::DisplayChain(&e).to_string().contains("600 s"),
            "{e}"
        );

        // At the floor it plans, and the period is the interval.
        let specs = plan(vec![UploaderRow {
            interval_seconds: Some(600),
            aprs_transport: Some("kiss-tcp".to_string()),
            aprs_kiss_tcp_address: Some("tnc.lan:8001".to_string()),
            ..aprs_row()
        }])
        .expect("plans");
        match &specs[0].trigger {
            Trigger::Every { period, .. } => assert_eq!(*period, RadioInterval::FLOOR),
            other => panic!("{other:?}"),
        }
    }

    /// **The Go dispatch's `default:` arm**, as a refusal rather than a silent APRS-IS
    /// injection.
    #[test]
    fn an_unrecognised_transport_is_refused_and_not_defaulted_to_the_internet() {
        let e = plan(vec![UploaderRow {
            aprs_transport: Some("kis".to_string()),
            ..aprs_row()
        }])
        .expect_err("an unknown transport");
        assert!(matches!(e, OutputPlanError::UnknownTransport { .. }), "{e}");
        assert!(e.to_string().contains("kis"), "{e}");
        assert!(e.to_string().contains("on the internet"), "{e}");
    }

    /// Every missing-configuration refusal names the row and says what to set.
    #[test]
    fn a_row_that_cannot_work_is_refused_by_name() {
        let cases: Vec<(&str, OutputPlanError)> = vec![
            (
                "no credential",
                plan(vec![UploaderRow {
                    api_key: None,
                    ..wu_row()
                }])
                .expect_err("no credential"),
            ),
            (
                "no remote station id",
                plan(vec![UploaderRow {
                    remote_station_id: None,
                    ..wu_row()
                }])
                .expect_err("no remote id"),
            ),
            (
                "no pairing",
                plan(vec![UploaderRow {
                    station_key: None,
                    ..wu_row()
                }])
                .expect_err("no pairing"),
            ),
            (
                "no callsign",
                plan(vec![UploaderRow {
                    aprs_callsign: None,
                    ..aprs_row()
                }])
                .expect_err("no callsign"),
            ),
            (
                "no position",
                plan(vec![UploaderRow {
                    latitude: None,
                    ..aprs_row()
                }])
                .expect_err("no position"),
            ),
            (
                "unknown kind",
                plan(vec![row("windy", "backyard")]).expect_err("unknown kind"),
            ),
        ];
        for (what, e) in cases {
            let text = mw_types::DisplayChain(&e).to_string();
            assert!(text.contains("backyard"), "{what}: {text}");
            assert!(text.len() > 60, "{what}: {text}");
            // No message quotes a credential.
            assert!(!text.contains("hunter2"), "{what}: {text}");
        }
    }

    /// **A KISS row with no configured interval plans**, at the floor.
    ///
    /// `uploader.interval_seconds` being NULL means "the kind's default", and before the
    /// default was made transport-dependent that default was 300 s for every APRS row — so
    /// an operator who created a KISS uploader through the admin surface and left the
    /// interval blank got a daemon that refused to start, naming an interval they never
    /// configured. The refusal is for a value somebody *wrote* below the floor.
    #[test]
    fn a_kiss_row_with_no_configured_interval_plans_at_the_floor() {
        for transport in ["kiss-serial", "kiss-tcp"] {
            let specs = plan(vec![UploaderRow {
                interval_seconds: None,
                aprs_transport: Some(transport.to_string()),
                aprs_kiss_serial_device: Some("/dev/ttyUSB0".to_string()),
                aprs_kiss_tcp_address: Some("tnc.lan:8001".to_string()),
                ..aprs_row()
            }])
            .unwrap_or_else(|e| panic!("{transport}: {}", mw_types::DisplayChain(&e)));
            match &specs[0].trigger {
                Trigger::Every { period, .. } => {
                    assert_eq!(*period, RadioInterval::FLOOR, "{transport}");
                }
                other => panic!("{other:?}"),
            }
        }
        // An APRS-IS row with no interval still gets the Go default, not the RF floor.
        let specs = plan(vec![UploaderRow {
            interval_seconds: None,
            aprs_transport: Some("aprs-is".to_string()),
            ..aprs_row()
        }])
        .expect("plans");
        match &specs[0].trigger {
            Trigger::Every { period, .. } => assert_eq!(*period, APRS_DEFAULT),
            other => panic!("{other:?}"),
        }
    }

    /// **A pairing this node maintains no windows for is refused, and a disabled station is
    /// skipped.** Two causes, two answers.
    #[test]
    fn a_station_this_node_does_not_collect_cannot_be_shared() {
        // A station a peer forwards readings for: refused, naming the row and the station.
        let cfg = Cfg {
            rows: vec![wu_row()],
            windows: Some(view()),
            not_collected: vec!["backyard".to_string()],
            disabled: Vec::new(),
        };
        let mut out = Vec::new();
        let e = register(&cfg, &mut out).expect_err("the pairing cannot work");
        assert!(
            matches!(e, OutputPlanError::StationNotCollected { .. }),
            "{e}"
        );
        assert!(e.to_string().contains("backyard"), "{e}");
        assert!(e.to_string().contains("window:rollup"), "{e}");
        assert!(out.is_empty());

        // A disabled station: skipped with a warning, because disabling a station is a
        // reversible operator action and refusing to start over one would make it
        // irreversible without editing the uploader too.
        let cfg = Cfg {
            rows: vec![wu_row()],
            windows: Some(view()),
            not_collected: Vec::new(),
            disabled: vec!["backyard".to_string()],
        };
        let mut out = Vec::new();
        register(&cfg, &mut out).expect("a disabled station is not a refusal");
        assert!(out.is_empty(), "and no component is planned for it");
    }

    /// **A plaintext endpoint would put the credential on the wire in clear.**
    #[test]
    fn a_plaintext_endpoint_is_refused_for_a_credentialled_uploader() {
        let e = plan(vec![UploaderRow {
            api_endpoint: Some("http://wx.example/submit".to_string()),
            ..wu_row()
        }])
        .expect_err("plaintext");
        assert!(
            matches!(e, OutputPlanError::PlaintextCredential { .. }),
            "{e}"
        );
        assert!(e.to_string().contains("in clear"), "{e}");
        // The message names the endpoint and carries no query.
        assert!(e.to_string().contains("http://wx.example/submit"), "{e}");
        assert!(!e.to_string().contains('?'), "{e}");
        // `https` plans.
        assert!(plan(vec![UploaderRow {
            api_endpoint: Some("https://wx.example/submit".to_string()),
            ..wu_row()
        }])
        .is_ok());
    }

    /// **W7 D-38's arity**, which the `station_service` table's own comment predicts by
    /// name and nothing asserted.
    #[test]
    fn one_uploader_paired_with_two_stations_is_refused() {
        let first = UploaderRow {
            station_key: Some("backyard".to_string()),
            ..wu_row()
        };
        let second = UploaderRow {
            station_key: Some("roof".to_string()),
            ..wu_row()
        };
        let e = plan(vec![first, second]).expect_err("two pairings on one account");
        assert!(matches!(e, OutputPlanError::Arity { .. }), "{e}");
        let text = e.to_string();
        assert!(text.contains("backyard") && text.contains("roof"), "{text}");
        assert!(text.contains("more than one station"), "{text}");
    }

    /// **Every value the builder closes over changes the fingerprint.** The digest is the
    /// whole of the plan diff's accuracy (`ComponentSpec::fingerprint`), so a column that
    /// is read and not folded in is a configuration change that does not restart the
    /// component — with no symptom until somebody notices the old value still going out.
    #[test]
    fn every_folded_column_changes_the_fingerprint() {
        let base = plan(vec![aprs_row()]).expect("plans")[0].fingerprint();
        let mutations: Vec<(&str, UploaderRow)> = vec![
            (
                "interval_seconds",
                UploaderRow {
                    interval_seconds: Some(1800),
                    ..aprs_row()
                },
            ),
            (
                "aprs_passcode",
                UploaderRow {
                    aprs_passcode: Some("12345".to_string()),
                    ..aprs_row()
                },
            ),
            (
                "aprs_symbol_table",
                UploaderRow {
                    aprs_symbol_table: Some("\\".to_string()),
                    ..aprs_row()
                },
            ),
            (
                "aprs_comment",
                UploaderRow {
                    aprs_comment: Some("Suncrest WA".to_string()),
                    ..aprs_row()
                },
            ),
            (
                "aprs_server",
                UploaderRow {
                    aprs_server: Some("rotate.aprs2.net:14580".to_string()),
                    ..aprs_row()
                },
            ),
            (
                "aprs_transport",
                UploaderRow {
                    interval_seconds: Some(900),
                    aprs_transport: Some("kiss-tcp".to_string()),
                    aprs_kiss_tcp_address: Some("tnc.lan:8001".to_string()),
                    ..aprs_row()
                },
            ),
            (
                "latitude",
                UploaderRow {
                    latitude: Some(45.2),
                    ..aprs_row()
                },
            ),
            (
                "longitude",
                UploaderRow {
                    longitude: Some(-122.0),
                    ..aprs_row()
                },
            ),
            (
                "aprs_callsign",
                UploaderRow {
                    aprs_callsign: Some("W1AW".to_string()),
                    ..aprs_row()
                },
            ),
            (
                "station_key",
                UploaderRow {
                    station_key: Some("roof".to_string()),
                    ..aprs_row()
                },
            ),
        ];
        for (what, row) in mutations {
            let changed = plan(vec![row])
                .unwrap_or_else(|e| panic!("{what}: {}", mw_types::DisplayChain(&e)))[0]
                .fingerprint();
            assert_ne!(
                base, changed,
                "changing `{what}` must restart the component"
            );
        }

        // The KISS columns, which need a KISS row to be read at all.
        let kiss = |row: UploaderRow| plan(vec![row]).expect("plans")[0].fingerprint();
        let radio = UploaderRow {
            interval_seconds: Some(900),
            aprs_transport: Some("kiss-tcp".to_string()),
            aprs_kiss_tcp_address: Some("tnc.lan:8001".to_string()),
            ..aprs_row()
        };
        let base_kiss = kiss(radio.clone());
        for (what, row) in [
            (
                "aprs_kiss_tcp_address",
                UploaderRow {
                    aprs_kiss_tcp_address: Some("other.lan:8001".to_string()),
                    ..radio.clone()
                },
            ),
            (
                "aprs_kiss_path",
                UploaderRow {
                    aprs_kiss_path: Some("WIDE2-2".to_string()),
                    ..radio.clone()
                },
            ),
            (
                "aprs_kiss_destination",
                UploaderRow {
                    aprs_kiss_destination: Some("APZ001".to_string()),
                    ..radio.clone()
                },
            ),
        ] {
            assert_ne!(
                base_kiss,
                kiss(row),
                "changing `{what}` must restart the component"
            );
        }
        // And the serial pair, which needs a serial row.
        let serial = UploaderRow {
            interval_seconds: Some(900),
            aprs_transport: Some("kiss-serial".to_string()),
            aprs_kiss_serial_device: Some("/dev/ttyUSB0".to_string()),
            aprs_kiss_serial_baud: Some(9600),
            ..aprs_row()
        };
        assert_ne!(
            kiss(serial.clone()),
            kiss(UploaderRow {
                aprs_kiss_serial_device: Some("/dev/ttyUSB1".to_string()),
                ..serial.clone()
            })
        );
        assert_ne!(
            kiss(serial.clone()),
            kiss(UploaderRow {
                aprs_kiss_serial_baud: Some(19200),
                ..serial
            })
        );
    }

    /// A node with no rollup plans nothing, and says so rather than refusing: `uploader`
    /// rows are configuration a collector would have.
    #[test]
    fn a_node_with_no_windowed_view_plans_nothing() {
        let cfg = Cfg {
            rows: vec![wu_row()],
            windows: None,
            not_collected: Vec::new(),
            disabled: Vec::new(),
        };
        let mut out = Vec::new();
        register(&cfg, &mut out).expect("not an error");
        assert!(out.is_empty());

        // And no rows at all is also nothing, without the log line.
        let cfg = Cfg {
            rows: Vec::new(),
            windows: Some(view()),
            not_collected: Vec::new(),
            disabled: Vec::new(),
        };
        let mut out = Vec::new();
        register(&cfg, &mut out).expect("not an error");
        assert!(out.is_empty());
    }

    /// The digest follows every value the builder closes over, which is what makes the plan
    /// diff restart the component when one changes (`SpecFingerprint`).
    #[test]
    fn the_configuration_digest_follows_the_credential_and_the_callsign() {
        let base = plan(vec![wu_row()]).expect("plans")[0].fingerprint();
        let same = plan(vec![wu_row()]).expect("plans")[0].fingerprint();
        assert_eq!(base, same);

        // A changed password is a changed component, which an `APRS callsign change would
        // then compare equal` fingerprint would miss (`ComponentSpec`'s own note).
        let rotated = plan(vec![UploaderRow {
            api_key: Some(SecretString::from("hunter3")),
            ..wu_row()
        }])
        .expect("plans")[0]
            .fingerprint();
        assert_ne!(base, rotated);

        // So is a changed remote station id and a changed endpoint.
        for changed in [
            UploaderRow {
                remote_station_id: Some("KWAOTHER".to_string()),
                ..wu_row()
            },
            UploaderRow {
                api_endpoint: Some("https://wx.example/submit".to_string()),
                ..wu_row()
            },
        ] {
            assert_ne!(base, plan(vec![changed]).expect("plans")[0].fingerprint());
        }

        let aprs = plan(vec![aprs_row()]).expect("plans")[0].fingerprint();
        let moved = plan(vec![UploaderRow {
            latitude: Some(45.2),
            ..aprs_row()
        }])
        .expect("plans")[0]
            .fingerprint();
        assert_ne!(aprs, moved, "a moved station is a changed beacon");
        let recalled = plan(vec![UploaderRow {
            aprs_callsign: Some("W1AW".to_string()),
            ..aprs_row()
        }])
        .expect("plans")[0]
            .fingerprint();
        assert_ne!(aprs, recalled);
    }

    /// The retry budget shrinks to fit a short deadline rather than making a valid
    /// configuration unplannable.
    #[test]
    fn the_retry_budget_fits_inside_the_deadline() {
        // Every period this program can plan: the deadline is 45 s and the full budget
        // fits, with six seconds to spare at the defaults.
        for period in [60, 120, 300, 600, 900, 1800] {
            let d = deadline(Duration::from_secs(period));
            assert_eq!(d, Duration::from_secs(45), "period {period}");
            assert_eq!(budget_for(d).expect("fits"), DEFAULT_BUDGET);
            assert!(uploader::budget_fits(
                d,
                DEFAULT_BUDGET,
                ATTEMPT_TIMEOUT,
                LIMIT_PERIOD
            ));
        }
        // The backstop. A deadline this program cannot currently produce -- it would take a
        // floor below 39 s -- is a named refusal rather than a stream of
        // `deadline_exceeded`, which is §4b.5's own rule.
        let e = budget_for(Duration::from_secs(20)).expect_err("does not fit");
        assert!(
            matches!(e, OutputPlanError::BudgetExceedsDeadline { .. }),
            "{e}"
        );
        assert!(e.to_string().contains("4b.5"), "{e}");
    }

    /// The defaults for a KISS row: destination `APRS`, path `WIDE1-1,WIDE2-1`, 9600 baud.
    #[test]
    fn the_kiss_defaults_are_the_go_implementations() {
        let specs = plan(vec![UploaderRow {
            interval_seconds: Some(900),
            aprs_transport: Some("kiss-serial".to_string()),
            aprs_kiss_serial_device: Some("/dev/ttyUSB0".to_string()),
            ..aprs_row()
        }])
        .expect("plans");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id.to_string(), "aprs:backyard");

        // The path parse, which is the one default with structure in it.
        let row = UploaderRow {
            aprs_kiss_path: Some(" WIDE1-1 , , WIDE2-2 ".to_string()),
            ..aprs_row()
        };
        let path = kiss_path(&row).expect("parses");
        assert_eq!(
            path.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            vec!["WIDE1-1", "WIDE2-2"]
        );
        let default = kiss_path(&aprs_row()).expect("parses");
        assert_eq!(
            default.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            vec!["WIDE1-1", "WIDE2-1"]
        );
        // A path with an unencodable hop is a refusal rather than a dropped hop.
        let bad = UploaderRow {
            aprs_kiss_path: Some("WIDE1-1,TOOLONGHOP".to_string()),
            ..aprs_row()
        };
        assert!(kiss_path(&bad).is_err());
    }

    /// A peer is `host:port` and both APRS columns carry one.
    #[test]
    fn a_peer_is_a_host_and_a_port() {
        let row = aprs_row();
        assert_eq!(
            peer(None, FrameSink::DEFAULT_APRS_IS, &row).expect("the default"),
            ("noam.aprs2.net".to_string(), 14580)
        );
        assert_eq!(
            peer(
                Some("rotate.aprs2.net:14580"),
                FrameSink::DEFAULT_APRS_IS,
                &row
            )
            .expect("parses"),
            ("rotate.aprs2.net".to_string(), 14580)
        );
        for bad in ["noam.aprs2.net", "noam.aprs2.net:0", ":14580", "host:nope"] {
            assert!(
                peer(Some(bad), FrameSink::DEFAULT_APRS_IS, &row).is_err(),
                "{bad}"
            );
        }
    }
}
