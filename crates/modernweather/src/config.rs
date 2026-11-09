//! `Config`, and the step 4 of W3 §3.1 that produces it.
//!
//! **Why the type lives in the binary.** §1.3 rule 2 writes the signature as
//! `plan(&Config, &Registrars)` and makes `plan()` generic over the configuration type,
//! because the supervisor must not know that a station or a database exists. The
//! configuration *tree* has the same problem one level down -- it describes stations and
//! websites -- so the crate that names the type is the crate that may name everything,
//! which is this one.
//!
//! **What it holds, and what it does not.** Four tables: the listener, the weather
//! database, the node row and `config.stations`. The first three are what the boot sequence
//! itself needs -- where to bind, whether **D-62** has a server to assert, and whether this
//! node has started before. The fourth arrived with the first registrar. Everything else in
//! `migrations/config/` is read by the component that consumes it, and a field here for a
//! table no consumer exists for yet would be guessing at a shape.
//!
//! **The first registrar is also where this type stopped being private to the binary.** A
//! registrar takes `&C` and `mw-station` may not depend on `mw-store`, so what it needs is a
//! *trait* this type implements -- one per component crate, naming only the rows that crate
//! plans from. [`mw_station::StationConfig`] is the first of them, and the station rows are
//! copied column-into-field below because both crates are in the `service` layer and
//! `ci/layers.toml` forbids either naming the other. That copy is what buys the property
//! that `mw-store` cannot grow a `Transport` and `mw-station` cannot grow a query.

use std::sync::Arc;

use mw_component::{Generation, Ingress, Zone};
use mw_store::config_db::{self, ConfigDbError, Listener, Node, WeatherDatabase, WebsiteRow};
use mw_types::StationKey;
use sqlx::SqlitePool;

/// Everything `plan()` and the boot sequence read from the configuration database.
pub struct Config {
    /// Where the one listener binds (W3 §6.1).
    pub listener: Listener,
    /// The weather database, if one is configured. **Its absence is meaningful**: a node
    /// with no row has no `db:postgres` and therefore no `writer:timescale` (**D-44**).
    pub weather_database: Option<WeatherDatabase>,
    /// This node's identity, if it has started before. `None` is a first start.
    pub node: Option<Node>,
    /// Every `config.stations` row, as the columns hold it. The registry's snapshot is
    /// built from these, because it needs `ingest_secret_hash` -- which is not a station
    /// *driver's* concern and is deliberately absent from the row below.
    pub stations: Vec<config_db::StationRow>,

    /// Every `config.website` row, in id order.
    ///
    /// The left half of the snapshot's `(website, station)` key (**D-31**). Only the five
    /// columns a payload depends on; the hostname router and the website renderer read the
    /// rest and both are M4-3's.
    pub websites: Vec<WebsiteRow>,

    /// The same rows in the shape `mw-station` plans from.
    ///
    /// `pub(crate)` rather than `pub`: the registrar reaches it through
    /// [`mw_station::StationConfig`], so nothing outside this crate needs the field, and
    /// this crate's own tests construct the type directly.
    pub(crate) declared_stations: Vec<mw_station::StationRow>,

    /// The write end of the reading pipeline, once the spool is open.
    ///
    /// `None` until `main` sets it, and legitimately `None` for a node with no `node` row:
    /// no identity means no spool, which means nothing to admit readings to (W7 §8.2).
    /// `mw_station::register` refuses to plan a station in that case rather than planning
    /// one that cannot store anything.
    pub(crate) ingress: Option<Arc<dyn Ingress>>,

    /// The published station registry, once the snapshot is built.
    ///
    /// Held here rather than only inside the ingress because **two** registrars need it:
    /// `mw-station`'s, through the `Ingress` above, and `db:postgres`, whose writer copies
    /// `config.stations` into the weather schema's read-side projection (**D-56**). One
    /// snapshot, two readers — a second `ConfigRegistry` would be a second answer to
    /// "where is this station", which is the one question W2a §4a.6 makes a single gate.
    pub(crate) registry: Option<Arc<dyn mw_component::StationRegistry>>,

    /// The maintained window values, once `main` has built them.
    ///
    /// A root resource `main` owns, like the SQLite pool, the spool and the registry — and
    /// here for the same reason the ingress is: **two** components write it,
    /// `window:rollup` and `rain:rollover:<station>`, and the capability bus holds one
    /// handle *type* per provider. `mw_pipeline::rollup`'s module documentation has the
    /// whole of that argument.
    pub(crate) rollup: Option<Arc<mw_pipeline::RollupState>>,

    /// The published snapshot table, once `main` has built it.
    ///
    /// A root resource for the reason the rollup state is one: **two** components write it,
    /// `snapshot:latest` and `snapshot:warm`, and the capability bus holds one handle
    /// *type* per provider. `mw_pipeline::snapshot::state`'s module documentation has the
    /// whole of that argument.
    pub(crate) snapshot: Option<Arc<mw_pipeline::SnapshotState>>,

    /// The same rollup state as [`Config::rollup`], upcast once to the capability the
    /// snapshot components declare.
    ///
    /// Two fields for one object because the two consumers want two types: the rollup's own
    /// registrar needs the concrete `RollupState` (both its writers call inherent methods),
    /// and the snapshot's needs `Arc<dyn WindowedView>`. Upcasting here rather than in the
    /// accessor keeps `snapshot_windows` a borrow — and keeps "these are the same object"
    /// a fact of `set_rollup` rather than of two call sites.
    pub(crate) snapshot_windows: Option<Arc<dyn mw_component::WindowedView>>,

    /// Every station and its **resolved** time zone, in station-key order.
    ///
    /// Taken from the registry snapshot rather than re-parsed, because the parse is the
    /// validation (W2a Decision 11) and it already happened there: a second parse would be
    /// a second answer to "which midnight does this station's day begin at".
    pub(crate) station_zones: Vec<(StationKey, Option<Zone>)>,

    /// The generation these rows were read at (**D-53**).
    ///
    /// `ZERO` until something can bump it: a generation is assigned by `ConfigAdmin` on
    /// every committed write, and `ConfigAdmin` arrives with the management API (M7-1).
    /// Until then every plan is derived from generation zero, which is the honest answer
    /// rather than a counter this crate invents.
    pub generation: Generation,
}

impl Config {
    /// Read the configuration. Step 4 of the boot sequence.
    pub async fn load(pool: &SqlitePool) -> Result<Self, ConfigDbError> {
        let stations = config_db::stations(pool).await?;
        Ok(Config {
            listener: config_db::listener(pool).await?,
            weather_database: config_db::weather_database(pool).await?,
            node: config_db::node(pool).await?,
            declared_stations: stations.iter().map(declare).collect(),
            stations,
            websites: config_db::websites(pool).await?,
            ingress: None,
            registry: None,
            rollup: None,
            snapshot: None,
            snapshot_windows: None,
            station_zones: Vec::new(),
            generation: Generation::ZERO,
        })
    }

    /// Hand the configuration the write end of the pipeline, once the spool is open.
    ///
    /// Step 4b produces it and step 5 reads it, so this is the one mutation between them.
    /// `mw_station::config`'s module documentation has the reasoning for why the handle
    /// travels this way rather than off the capability bus.
    pub fn set_ingress(&mut self, ingress: Arc<dyn Ingress>) {
        self.ingress = Some(ingress);
    }

    /// Hand the configuration the published station registry, and the resolved zones the
    /// same snapshot produced.
    ///
    /// One call rather than two, because the zones come out of the snapshot the registry is
    /// built from and splitting them would make it possible to publish one without the
    /// other — which is a plan with a `TzSource::Station` that disagrees with the gate about
    /// what the station's zone is.
    pub fn set_registry(
        &mut self,
        registry: Arc<dyn mw_component::StationRegistry>,
        zones: Vec<(StationKey, Option<Zone>)>,
    ) {
        self.registry = Some(registry);
        self.station_zones = zones;
    }

    /// Hand the configuration the maintained window values.
    ///
    /// Sets both views of the one object: the concrete handle the rollup's registrar plans
    /// from, and the `WindowedView` upcast the snapshot components are given. One call, so
    /// the two cannot disagree about which state is live.
    pub fn set_rollup(&mut self, rollup: Arc<mw_pipeline::RollupState>) {
        self.snapshot_windows = Some(Arc::clone(&rollup) as Arc<dyn mw_component::WindowedView>);
        self.rollup = Some(rollup);
    }

    /// Hand the configuration the published snapshot table.
    pub fn set_snapshot(&mut self, snapshot: Arc<mw_pipeline::SnapshotState>) {
        self.snapshot = Some(snapshot);
    }

    /// The address the one listener binds, and therefore where `/metrics` is served.
    ///
    /// §5.10 puts `/metrics` on **an additive route on the one listener** rather than on a
    /// port of its own, so this is the main port -- not the management port, which is
    /// loopback by default (A-W5-3) and would make the metrics unscrapable from the
    /// homelab's Prometheus.
    pub fn listen_addr(&self) -> Result<std::net::SocketAddr, AddressError> {
        let host = &self.listener.listen_addr;
        let port = self.listener.http_port;
        // A bare IPv6 literal has to be bracketed before it can be parsed with a port, and
        // the column holds whatever the operator typed.
        let text = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        text.parse().map_err(|_| AddressError {
            addr: host.clone(),
            port,
        })
    }
}

/// `listener.listen_addr` is not an address.
///
/// The schema cannot check this -- SQLite has no inet type -- so it is a startup failure
/// that names what was configured, rather than a bind that fails with "invalid argument".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{addr}` is not an address to listen on (with port {port})")]
pub struct AddressError {
    /// What `listener.listen_addr` held.
    pub addr: String,
    /// The port it would have been joined with.
    pub port: u16,
}

impl std::fmt::Debug for Config {
    /// `Ingress` is a capability handle and is not `Debug`; the station rows carry a
    /// `SecretString` each. What is useful in a log line is how much of each there is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listener", &self.listener)
            .field("weather_database", &self.weather_database.is_some())
            .field("node", &self.node)
            .field("stations", &self.stations.len())
            .field("ingress", &self.ingress.is_some())
            .field("registry", &self.registry.is_some())
            .field("websites", &self.websites.len())
            .field("rollup", &self.rollup.is_some())
            .field("snapshot", &self.snapshot.is_some())
            .field("generation", &self.generation)
            .finish()
    }
}

impl mw_station::StationConfig for Config {
    fn stations(&self) -> &[mw_station::StationRow] {
        &self.declared_stations
    }

    fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
        self.ingress.as_ref()
    }
}

impl mw_pipeline::RollupConfig for Config {
    fn has_weather_database(&self) -> bool {
        self.weather_database.is_some()
    }

    fn rollup_state(&self) -> Option<&Arc<mw_pipeline::RollupState>> {
        self.rollup.as_ref()
    }

    /// Only the stations this node **collects** for.
    ///
    /// A station with no `transport_kind` is one a peer forwards readings for — inventory
    /// on this node rather than a component (`mw_station::config`) — and a disabled one has
    /// collection switched off (W5 §1.2). Neither has windows to maintain, and emitting a
    /// `rain:rollover` for either would make a configuration refuse to plan over a station
    /// that is not reporting.
    fn rollup_stations(&self) -> Vec<(StationKey, Option<Zone>)> {
        let collecting: std::collections::BTreeSet<&str> = self
            .stations
            .iter()
            .filter(|r| r.enabled && r.transport_kind.is_some())
            .map(|r| r.station_key.as_str())
            .collect();
        self.station_zones
            .iter()
            .filter(|(key, _)| collecting.contains(key.as_str()))
            .map(|(key, zone)| (key.clone(), *zone))
            .collect()
    }
}

impl mw_pipeline::SnapshotConfig for Config {
    fn has_weather_database(&self) -> bool {
        self.weather_database.is_some()
    }

    /// `Trigger::OnReading` is validated against the root `ReadingStream`, and this node has
    /// one exactly when it has a spool — which is when it has a `node` row (W7 §8.2).
    fn has_reading_stream(&self) -> bool {
        self.ingress.is_some()
    }

    fn snapshot_state(&self) -> Option<&Arc<mw_pipeline::SnapshotState>> {
        self.snapshot.as_ref()
    }

    fn snapshot_registry(&self) -> Option<&Arc<dyn mw_component::StationRegistry>> {
        self.registry.as_ref()
    }

    /// The rollup state, upcast to the capability the snapshot declares.
    ///
    /// `mw_pipeline::SnapshotConfig::snapshot_windows` has why this travels through the
    /// registrar rather than off the bus. The field is stored pre-upcast so that this is a
    /// borrow rather than an allocation per call: the registrar asks once per `plan()`, but
    /// a trait method that had to build an `Arc` to answer would be the kind of accessor
    /// someone later calls in a loop.
    fn snapshot_windows(&self) -> Option<&Arc<dyn mw_component::WindowedView>> {
        self.snapshot_windows.as_ref()
    }

    /// **`None` until M4-2.** The payload is the frozen 133-key DTO and `mw-api-legacy`
    /// exclusively owns its serialiser (W3 D-9), so there is nothing here to hand over yet
    /// and `mw_pipeline::snapshot::register` plans neither component. When M4-2 lands it is
    /// a `mw_api_legacy::Renderer` built in [`crate::boot`] beside the registry, and this
    /// is the one line that changes.
    fn snapshot_render(&self) -> Option<&Arc<dyn mw_component::SnapshotRender>> {
        None
    }
}

/// One `config.stations` row, as the station crate declares components from.
///
/// Column into field, and nothing else: every validation is `mw_station`'s, because the
/// crate that owns what a transport *is* is the crate that should refuse one that is not.
fn declare(row: &config_db::StationRow) -> mw_station::StationRow {
    mw_station::StationRow {
        key: row.station_key.clone(),
        name: row.name.clone(),
        kind: row.kind.clone(),
        enabled: row.enabled,
        identity_kind: row.identity_kind.clone(),
        presented_key: row.presented_key.clone(),
        transport: row
            .transport_kind
            .as_ref()
            .map(|kind| mw_station::TransportColumns {
                kind: kind.clone(),
                dev: row.transport_dev.clone(),
                baud: row.transport_baud,
                host: row.transport_host.clone(),
                port: row.transport_port,
                url: row.transport_url.clone(),
                method: row.transport_method.clone(),
                auth: row.transport_auth.clone(),
                streaming: row.transport_streaming,
                mount: row.transport_mount.clone(),
                group: row.transport_group.clone(),
                topic: row.transport_topic.clone(),
                tls_ca: row.transport_tls_ca_path.clone(),
                tls_cert: row.transport_tls_cert_path.clone(),
                tls_key: row.transport_tls_key_path.clone(),
            }),
        wll_sensor_mapping: row.wll_sensor_mapping.clone(),
        wll_poll_interval_seconds: row.wll_poll_interval_seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(addr: &str, port: u16) -> Config {
        Config {
            stations: Vec::new(),
            declared_stations: Vec::new(),
            websites: Vec::new(),
            ingress: None,
            registry: None,
            rollup: None,
            snapshot: None,
            snapshot_windows: None,
            station_zones: Vec::new(),
            listener: Listener {
                listen_addr: addr.to_string(),
                http_port: port,
                https_port: None,
                grpc_port: 50051,
                management_listen_addr: "127.0.0.1".into(),
                management_port: 8081,
            },
            weather_database: None,
            node: None,
            generation: Generation::ZERO,
        }
    }

    #[test]
    fn the_schema_default_binds_every_interface_on_the_main_port() {
        let addr = config("0.0.0.0", 8080)
            .listen_addr()
            .expect("the seeded default is an address");
        assert_eq!(addr.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn an_ipv6_literal_is_bracketed_for_the_operator() {
        assert_eq!(
            config("::", 8080).listen_addr().expect("v6").to_string(),
            "[::]:8080"
        );
        assert_eq!(
            config("[::1]", 9090).listen_addr().expect("v6").to_string(),
            "[::1]:9090"
        );
    }

    #[test]
    fn a_hostname_is_rejected_with_what_was_configured() {
        // Deliberately not resolved: D-43's static musl linkage means musl's resolver,
        // which supports neither `nsswitch.conf` nor NSS modules (W8 OQ-4) -- so a
        // listen address that needs resolution is a configuration that behaves
        // differently in the release build than in a developer's.
        let e = config("localhost", 8080)
            .listen_addr()
            .expect_err("rejected");
        assert_eq!(
            e,
            AddressError {
                addr: "localhost".into(),
                port: 8080
            }
        );
        assert!(e.to_string().contains("localhost"), "{e}");
    }
}
