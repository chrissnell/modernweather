//! `Config`, and the step 4 of W3 §3.1 that produces it.
//!
//! **Why the type lives in the binary.** §1.3 rule 2 writes the signature as
//! `plan(&Config, &Registrars)` and makes `plan()` generic over the configuration type,
//! because the supervisor must not know that a station or a database exists. The
//! configuration *tree* has the same problem one level down -- it describes stations and
//! websites -- so the crate that names the type is the crate that may name everything,
//! which is this one.
//!
//! **What it holds, and what it does not.** Five tables: the listener, the weather
//! database, the node row, `config.stations` and `config.website`. The first three are what
//! the boot sequence itself needs -- where to bind, whether **D-62** has a server to assert,
//! and whether this node has started before. The fourth arrived with the first registrar
//! and the fifth with the frozen REST surface. Everything else in `migrations/config/` is
//! read by the component that consumes it, and a field here for a table no consumer exists
//! for yet would be guessing at a shape.
//!
//! **The first registrar is also where this type stopped being private to the binary.** A
//! registrar takes `&C` and `mw-station` may not depend on `mw-store`, so what it needs is a
//! *trait* this type implements -- one per component crate, naming only the rows that crate
//! plans from. [`mw_station::StationConfig`] is the first of them, and the station rows are
//! copied column-into-field below because both crates are in the `service` layer and
//! `ci/layers.toml` forbids either naming the other. That copy is what buys the property
//! that `mw-store` cannot grow a `Transport` and `mw-station` cannot grow a query.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{Generation, Ingress, Zone};
use mw_store::config_db::{self, ConfigDbError, Listener, Node, WeatherDatabase, WebsiteRow};
use mw_types::{StationKey, WebsiteId};
use secrecy::ExposeSecret as _;

/// Everything `plan()` and the boot sequence read from the configuration database.
pub struct Config {
    /// Where the one listener binds (W3 §6.1).
    pub listener: Listener,
    /// The weather database, if one is configured. **Its absence is meaningful**: a node
    /// with no row has no `db:postgres` and therefore no `writer:timescale` (**D-44**).
    pub weather_database: Option<WeatherDatabase>,
    /// This node's identity, if it has started before. `None` is a first start.
    pub node: Option<Node>,

    /// Every `config.ingest` row, as the columns hold it.
    ///
    /// **Its emptiness is meaningful**: a node with no row is a satellite and has no
    /// receiver, which is W7 **D-44**'s "no flag, no mode" (`mw_grpc::grpcrx`).
    pub ingest: Vec<config_db::IngestRow>,

    /// The same rows in the shape `mw-grpc` plans from.
    ///
    /// `pub(crate)` for [`Config::declared_stations`]' reason, and a second type for the
    /// same reason that one is: `mw-store` owns the query and may not name `mw-grpc`,
    /// `mw-grpc` owns what an ingest row is and may not name `mw-store`, and the binary is
    /// the one place they meet (§1.3 rule 1). The copy is four lines of nothing, and it is
    /// what buys the property that neither crate can grow the other's job.
    pub(crate) declared_ingest: Vec<mw_grpc::IngestRow>,
    /// Every `config.stations` row, as the columns hold it. The registry's snapshot is
    /// built from these, because it needs `ingest_secret_hash` -- which is not a station
    /// *driver's* concern and is deliberately absent from the row below.
    pub stations: Vec<config_db::StationRow>,

    /// Every `config.website` row, in id order.
    ///
    /// The left half of the snapshot's `(website, station)` key (**D-31**), and the whole of
    /// what the frozen REST surface reads a website for. The TLS paths, `is_default_tls` and
    /// `apple_app_id` arrived with M4-3, which built the SNI resolver and the rendered
    /// page -- the two consumers the earlier note said they were waiting for.
    pub websites: Vec<WebsiteRow>,

    /// The website rows in the shape `mw-api-legacy` plans from.
    ///
    /// `pub(crate)` for [`Config::declared_stations`]' reason: the registrar reaches them
    /// through [`mw_api_legacy::LegacyConfig`], so nothing outside this crate needs the
    /// field.
    pub(crate) legacy_websites: Vec<mw_api_legacy::WebsiteView>,

    /// The station rows in the shape `mw-api-legacy` plans from.
    pub(crate) legacy_stations: Vec<mw_api_legacy::StationView>,

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

    /// The frozen route table's state slot, once `main` has built it.
    ///
    /// A root resource for [`Config::snapshot`]'s reason and one more: the **listener** is
    /// built from it too, in the same function, so the slot has to exist before `plan()`
    /// runs. `api:legacy` installs its handler set into it while the component is
    /// `Running`, which is what makes "the table is mounted" and "the component is
    /// running" one fact (`mw_api_legacy::component`).
    pub(crate) legacy_routes: Option<Arc<mw_api_legacy::RouteState>>,

    /// The Ambient ingest endpoint's state slot, once `main` has built it.
    ///
    /// A root resource for [`Config::legacy_routes`]' reason exactly: the route is an
    /// `axum` one and `mw-station` may not name `axum`, so what travels is a closure over
    /// this slot and the binary is the only place the two crates meet (§1.3 rule 1).
    /// `station:ambient` installs its handler into it while the component is `Running`.
    pub(crate) ambient_upload: Option<Arc<mw_station::ambient::UploadState>>,

    /// The reading receiver's state slot, once `main` has built it.
    ///
    /// A root resource for [`Config::ambient_upload`]'s reason exactly: the service is a
    /// route on the one listener, built before `plan()` runs, and `station:grpcrx` installs
    /// its handlers into this slot while it is `Running`.
    pub(crate) grpcrx_service: Option<Arc<mw_grpc::grpcrx::ServiceState>>,

    /// The frozen DTO's serialiser, once `main` has built it.
    ///
    /// **This is the one line M4-1 said would change.** `snapshot:latest` and
    /// `snapshot:warm` are complete and were not planned, because the payload is the frozen
    /// 133-key DTO and `mw-api-legacy` exclusively owns its serialiser; this is that
    /// serialiser, and both components plan now.
    ///
    /// Held as the concrete type and upcast in the accessor, for [`Config::rollup`]'s
    /// reason: **two** consumers want two types. `api:legacy` needs the `Renderer` itself
    /// -- `/snow` folds its revision into an `ETag` without going through a snapshot -- and
    /// the snapshot components need `Arc<dyn SnapshotRender>`. One object, so the two
    /// cannot disagree about what version the bytes are.
    pub(crate) legacy_render: Option<Arc<mw_api_legacy::Renderer>>,

    /// The same renderer, upcast once to the capability the snapshot components declare.
    ///
    /// Two fields for one object, exactly as [`Config::snapshot_windows`] is: upcasting
    /// here rather than in the accessor keeps it a borrow, which is what the trait's
    /// signature asks for -- and keeps "these are the same object" a fact of
    /// [`Config::set_legacy`] rather than of two call sites.
    pub(crate) snapshot_render: Option<Arc<dyn mw_component::SnapshotRender>>,

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

    /// Every website in the shape `mw-web` renders from -- the row **joined to its primary
    /// station**, which is what the Go handler renders from.
    ///
    /// `pub(crate)` for [`Config::declared_stations`]' reason: the registrar reaches them
    /// through [`mw_web::WebConfig`].
    pub(crate) web_sites: Vec<mw_web::SiteView>,

    /// The per-website state slots, once `main` has built them.
    ///
    /// A root resource for [`Config::legacy_routes`]' reason: the **listener** is built
    /// from the same tables, in the same function, so the slots have to exist before
    /// `plan()` runs.
    pub(crate) web_states: Option<Vec<Arc<mw_web::SiteState>>>,

    /// The mounted router set, once `main` has built it.
    ///
    /// The fourth root resource of its kind, and the one every other route provider feeds:
    /// `mw-http` may name neither `mw-api-legacy` nor `mw-web` (`ci/layers.toml`), so the
    /// binary is where the tables and the listener meet (§1.3 rule 1).
    pub(crate) mount: Option<Arc<mw_http::Mount>>,

    /// The SNI resolver, for a node with a TLS port.
    ///
    /// `None` when `listener.https_port` is NULL, which is every node that has not
    /// configured one.
    pub(crate) sni: Option<Arc<mw_http::SniResolver>>,

    /// The management API's socket and router, once `main` has built them (**M7-1**).
    ///
    /// A root resource for [`Config::mount`]'s reason and the mirror of it: `mw-http` may
    /// not name `mw-api-mgmt` (`ci/layers.toml`), and `mw-api-mgmt` may not bind a socket,
    /// so the binary is where the router and the listener meet (§1.3 rule 1).
    ///
    /// `None` is a node whose management API has not been built, which is every
    /// `crate::boot` test that plans without one. `http:listener` plans either way --
    /// `/metrics` is on the one listener and every node has a `listener` row -- so the
    /// absence is a configuration rather than a wiring error.
    pub(crate) mgmt: Option<mw_http::Management>,

    /// Every enabled `uploader` row, joined to its `station_service` pairing, in the shape
    /// `mw-output` plans from.
    ///
    /// **Its emptiness is meaningful** and is the common case: a node with no row shares
    /// nothing with a third party, and `mw_output::register` plans nothing. Read here
    /// rather than by the component that consumes it for the reason every other row is:
    /// `mw-output` may not depend on `mw-store` (`ci/layers.toml`, W2a decision 50), so the
    /// query is this crate's and the shape the registrar plans from is the component
    /// crate's.
    ///
    /// Only the declared shape is kept, unlike `stations` and `ingest`, which keep both:
    /// nothing in the boot sequence reads an uploader, so a second copy in the store's own
    /// shape would be a field whose only purpose is to be converted.
    pub(crate) declared_uploaders: Vec<mw_output::UploaderRow>,

    /// The same `config.ingest` rows in the shape `mw-collector` plans from.
    ///
    /// A third copy of one table, for [`Config::declared_ingest`]'s reason exactly: three
    /// crates in two layers read it, none of them may name the others, and the binary is
    /// where they meet. The copy is four lines and it is what stops `mw-grpc` and
    /// `mw-collector` from growing each other's job.
    pub(crate) declared_collector_ingest: Vec<mw_collector::IngestRow>,

    /// Every `uplink` row in the shape `mw-collector` plans from.
    ///
    /// **Its emptiness is meaningful** and is the common case for a collector: a node with
    /// no row sends to nobody, which is the other half of W7 **D-44**. Joined to the
    /// `peers` rows here, because an uplink needs its peer's `NodeId` and `mw-collector`
    /// may not read a database.
    pub(crate) declared_uplinks: Vec<mw_collector::UplinkRow>,

    /// The `uplink` rows whose `peer_name` matched no `peers` row.
    ///
    /// **Carried rather than dropped**, because dropping them was a silent no-op on the
    /// documented satellite: §8.2's `[[uplink]]` has no `[[peer]]` beside it, so the join
    /// found nothing, `declared_uplinks` was empty, and `register` planned no uplink and
    /// said nothing. The node admitted and spooled its readings, forwarded none of them,
    /// and `[spool].max_lag` eventually abandoned the cursor. `mw_collector`'s
    /// `UplinkPlanError::UnknownPeer` is what these become.
    pub(crate) unjoinable_uplinks: Vec<String>,

    /// The peer registry and the registration path (W7 §5.1), once `main` has built it.
    ///
    /// A root resource for [`Config::registry`]'s reason and the same object's other half:
    /// `PeerRegistry` is `Capability::StationRegistry` seen from the collector's side, so
    /// there is one published snapshot and two accessor sets.
    pub(crate) peer_registry: Option<Arc<dyn mw_component::PeerRegistry>>,

    /// The collector's state slot, once `main` has built it.
    ///
    /// A root resource for [`Config::grpcrx_service`]'s reason exactly.
    pub(crate) ingest_service: Option<Arc<mw_collector::ServiceState>>,

    /// What an uplink's `Hello` announces about this node (§5.2), once the registry is
    /// published.
    pub(crate) node_identity: Option<mw_collector::Identity>,

    /// The ingest socket's own TLS, with a client-certificate verifier (W7 §5.4 layer 1).
    ///
    /// `None` leaves the gRPC port plaintext, which is every node with no `config.ingest`
    /// row and every node that has one but no certificate to present. Assembled in the
    /// binary because the verifier's choice is per `auth_kind` and the pinned case reads
    /// the `peers` snapshot: `mw-http` binds the socket and may not name a peer,
    /// `mw-collector` decides who may be on it and may not bind.
    pub(crate) ingest_tls: Option<Arc<rustls::ServerConfig>>,

    /// The open spool, once step 4b has opened it.
    ///
    /// Held as well as handed to the ingress because the collector reads two numbers off it
    /// that no capability carries: the per-origin high water for `HelloAck.known_through`,
    /// and the cursor lag for `IngestBatch.sender_depth`.
    pub(crate) spool: Option<Arc<mw_pipeline::Spool>>,

    /// Every `uploader` row, enabled or not, joined to its pairing.
    ///
    /// **A second read of the same table**, and the reason is which question each answers:
    /// [`Config::declared_uploaders`] is what the registrar plans from and holds only the
    /// enabled rows, because a disabled uploader is not a component. The management API has
    /// to show a service the operator has just switched off, or the toggle is write-only and
    /// the page 404s the moment it is used (M7-1). One `WHERE` apart, so a second query is
    /// cheaper than a second shape.
    pub services: Vec<config_db::UploaderRow>,

    /// The `aeris_account` row, if this node has one.
    ///
    /// Read here rather than by the component that consumes it for the reason every other
    /// row is: `mw-output` may not depend on `mw-store` (`ci/layers.toml`), so the query is
    /// this crate's and the shape the registrar plans from is the component crate's. **Its
    /// absence is the ordinary case** -- most deployments have no Aeris account.
    pub aeris_account: Option<config_db::AerisAccountRow>,

    /// Every station paired with the Aeris service, in the shape `mw-output` plans from.
    ///
    /// `pub(crate)` for [`Config::declared_stations`]' reason. The pairing itself is a
    /// `station_service` row with `service = 'aeris'`; the coordinates come off the station
    /// row, because the forecast API is queried by `latitude,longitude`.
    pub(crate) declared_aeris: Vec<mw_output::AerisStation>,

    /// Whether an administrator account exists — W5 §3.4's claimed/unclaimed distinction.
    ///
    /// Read here rather than asked of the `AuthStore` because it is an input to
    /// [`Config::management_addr`], and that is called from inside the save transaction:
    /// `crate::admin`'s re-plan reads the whole configuration out of an open transaction, so
    /// a second read through a pool handle would be a different snapshot. **Not a secret and
    /// not a credential** — it is one boolean, and the unauthenticated
    /// `GET /api/v1/auth/session` already reports it, because a UI with no way to ask would
    /// have no way to show a setup screen.
    pub claimed: bool,

    /// `--setup-listen <address>`, W5 §3.4's explicit opt-out for a headless install.
    ///
    /// `None` is the normal case. When set **and the instance is unclaimed**, it replaces
    /// loopback as the address the management API binds; once claimed it is ignored, because
    /// the configured `management_listen_addr` is the answer from then on.
    ///
    /// A field set by a setter rather than a column, because it is a property of **this
    /// invocation** and not of the installation: a headless first run passes it once and a
    /// stored one would be a permanently widened unclaimed window that nobody chose twice.
    pub(crate) setup_listen: Option<std::net::IpAddr>,

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
    ///
    /// **A connection rather than the pool**, which is what lets `ConfigAdmin`'s save
    /// preview read the configuration *it is proposing*: the preview applies its change in a
    /// transaction, re-reads through this function, runs `plan()` and rolls back, so the
    /// preview and the save are one code path (implementation-plan.md **M7-1**). Every call
    /// on a pool takes its own connection and would see the pre-change rows.
    pub async fn load(conn: &mut sqlx::SqliteConnection) -> Result<Self, ConfigDbError> {
        let stations = config_db::stations(&mut *conn).await?;
        let ingest = config_db::ingest(&mut *conn).await?;
        // The `uplink` rows joined to the `peers` rows, which is the one join this type
        // does: `mw_collector::UplinkRow` needs the peer's `NodeId` for the loop filter and
        // the `uplink` table keys on the peer's *name*. A row whose peer has no `peers` row
        // is dropped here and the warning is `mw_collector::register`'s, which is where
        // the plan error that names it lives.
        let peers = config_db::peers(&mut *conn).await?;
        let uplinks = config_db::uplinks(&mut *conn).await?;
        let websites = config_db::websites(&mut *conn).await?;
        // `aerisWeatherEnabled` is a `station_service` row with `service = 'aeris'` rather
        // than a column -- the `website` table's own comment says so -- and it is folded
        // into the website views below rather than kept, because nothing else reads it.
        let aeris_stations = config_db::aeris_stations(&mut *conn).await?;
        let uploaders = config_db::uploaders(&mut *conn).await?;
        Ok(Config {
            declared_collector_ingest: ingest.iter().map(declare_collector_ingest).collect(),
            declared_uplinks: uplinks
                .iter()
                .filter_map(|row| declare_uplink(row, &peers))
                .collect(),
            unjoinable_uplinks: uplinks
                .iter()
                .filter(|row| declare_uplink(row, &peers).is_none())
                .map(|row| row.peer_name.clone())
                .collect(),
            peer_registry: None,
            ingest_service: None,
            node_identity: None,
            ingest_tls: None,
            spool: None,
            declared_uploaders: uploaders.iter().map(declare_uploader).collect(),
            services: config_db::all_uploaders(&mut *conn).await?,
            aeris_account: config_db::aeris_account(&mut *conn).await?,
            declared_aeris: declare_aeris(&stations, &aeris_stations),
            web_sites: websites
                .iter()
                .map(|w| declare_site(w, &stations, &aeris_stations))
                .collect(),
            listener: config_db::listener(&mut *conn).await?,
            weather_database: config_db::weather_database(&mut *conn).await?,
            node: config_db::node(&mut *conn).await?,
            declared_ingest: ingest.iter().map(declare_ingest).collect(),
            ingest,
            declared_stations: stations.iter().map(declare).collect(),
            legacy_websites: websites.iter().map(declare_website).collect(),
            legacy_stations: stations.iter().map(declare_station).collect(),
            stations,
            websites,
            ingress: None,
            registry: None,
            rollup: None,
            snapshot: None,
            snapshot_windows: None,
            legacy_routes: None,
            ambient_upload: None,
            grpcrx_service: None,
            legacy_render: None,
            snapshot_render: None,
            station_zones: Vec::new(),
            web_states: None,
            mount: None,
            sni: None,
            mgmt: None,
            claimed: mw_store::admin_auth::is_claimed(&mut *conn)
                .await
                .map_err(|source| ConfigDbError::Query {
                    what: "whether an administrator account exists",
                    source,
                })?,
            setup_listen: None,
            generation: Generation::ZERO,
        })
    }

    /// Hand the configuration `--setup-listen`, for an unclaimed headless install.
    ///
    /// Set by the boot sequence from the command line, and re-set on every re-plan by
    /// `crate::admin::Shared`, so that a claim taken in this process plans against the same
    /// invocation's flags.
    pub fn set_setup_listen(&mut self, addr: Option<std::net::IpAddr>) {
        self.setup_listen = addr;
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

    /// Hand the configuration the frozen route table's state slot and the DTO's
    /// serialiser.
    ///
    /// One call, because the two are the same item's and a configuration with one and not
    /// the other is a plan that either refuses `api:legacy` or silently drops the snapshot
    /// components -- both of which are a wiring mistake rather than a state worth
    /// representing.
    pub fn set_legacy(
        &mut self,
        routes: Arc<mw_api_legacy::RouteState>,
        render: Arc<mw_api_legacy::Renderer>,
    ) {
        self.legacy_routes = Some(routes);
        self.snapshot_render = Some(Arc::clone(&render) as Arc<dyn mw_component::SnapshotRender>);
        self.legacy_render = Some(render);
    }

    /// Hand the configuration the peer registry, the spool and what a `Hello` announces.
    ///
    /// One call rather than three, because all three come out of step 4b/4c together and a
    /// configuration with one and not the others is a boot sequence that stopped halfway
    /// rather than a shape worth planning: the collector needs the registry **and** the
    /// spool, and the uplink needs the spool **and** the identity.
    pub fn set_collector(
        &mut self,
        peers: Arc<dyn mw_component::PeerRegistry>,
        spool: Arc<mw_pipeline::Spool>,
        identity: mw_collector::Identity,
    ) {
        self.peer_registry = Some(peers);
        self.spool = Some(spool);
        self.node_identity = Some(identity);
    }

    /// Hand the configuration the collector's state slot and the ingest socket's TLS.
    ///
    /// One call, because `ingest:grpc`'s plan check reads the second to decide whether
    /// **D-35**'s opt-out is required, and a slot with no answer about TLS would make that
    /// check depend on the order two setters were called in.
    pub fn set_ingest(
        &mut self,
        state: Arc<mw_collector::ServiceState>,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) {
        self.ingest_service = Some(state);
        self.ingest_tls = tls;
    }

    /// Hand the configuration the reading receiver's state slot.
    ///
    /// Built beside the listener's route tables, for [`Config::set_ambient_upload`]'s
    /// reason.
    pub fn set_grpcrx_service(&mut self, state: Arc<mw_grpc::grpcrx::ServiceState>) {
        self.grpcrx_service = Some(state);
    }

    /// Hand the configuration the Ambient ingest endpoint's state slot.
    ///
    /// Built beside the listener's route tables, because the route is one of them: the slot
    /// has to exist before `plan()` runs, for [`Config::set_legacy`]'s reason.
    pub fn set_ambient_upload(&mut self, state: Arc<mw_station::ambient::UploadState>) {
        self.ambient_upload = Some(state);
    }

    /// Hand the configuration the per-website state slots.
    ///
    /// One per website in [`Config::web_sites`]' order, which `mw_web::register` asserts:
    /// a mismatch would mount one website's page under another's hostname.
    pub fn set_web_states(&mut self, states: Vec<Arc<mw_web::SiteState>>) {
        self.web_states = Some(states);
    }

    /// Hand the configuration the mounted router set and, where there is a TLS port, the
    /// certificate resolver.
    ///
    /// One call for the same reason [`Config::set_legacy`] is one: a mount with no resolver
    /// on a node with a TLS port is a listener that accepts connections and fails every
    /// handshake, and `mw_http::bind` refuses it -- so the two travel together rather than
    /// being two states a caller can get half right.
    pub fn set_listener(
        &mut self,
        mount: Arc<mw_http::Mount>,
        sni: Option<Arc<mw_http::SniResolver>>,
    ) {
        self.mount = Some(mount);
        self.sni = sni;
    }

    /// Hand the configuration the management API's socket and router (**M7-1**).
    ///
    /// Separate from [`Config::set_listener`] rather than folded into it, because the two
    /// have different preconditions and the order is forced: the management router holds a
    /// `ConfigAdmin`, which holds the plan sink, which the supervisor produces -- and the
    /// supervisor is assembled after the route tables are mounted. One call would mean
    /// mounting the public tables after the supervisor existed, which is the ordering
    /// `assert_route_tables_match_the_plan` depends on not changing.
    pub fn set_management(&mut self, mgmt: mw_http::Management) {
        self.mgmt = Some(mgmt);
    }

    /// Where the management API binds, from `listener.management_listen_addr` and
    /// `management_port`.
    ///
    /// **Loopback while the instance is unclaimed** — W5 §3.4, answered as **O-10 /
    /// OQ-W5-1 (a)**. The hazard is stated there and is worth restating here, because this
    /// function is where it is closed: an unclaimed instance reachable from the network means
    /// whoever opens the setup page first owns the install, and the window is "from first
    /// start until the owner gets round to it", which on a headless Pi is days. The setup
    /// code gates that window; binding loopback makes it empty.
    ///
    /// Three cases, in this order:
    ///
    /// | | |
    /// |---|---|
    /// | claimed | the configured address, which is what `A-W5-3` defaults to `127.0.0.1` anyway |
    /// | unclaimed, `--setup-listen` given | that address, with a `WARN` naming what it widens |
    /// | unclaimed | loopback, with an `INFO` naming the configured address it is narrowing |
    ///
    /// **The order of the last two is load-bearing.** `A-W5-3` makes the configured address
    /// default to `127.0.0.1`, so the flag has to be consulted *before* the loopback case --
    /// otherwise it does nothing at all on a fresh install, which is the only kind of install
    /// it exists for.
    ///
    /// **M7-1 shipped the middle and bottom rows unconditionally** and warned that the
    /// management API had no authentication. That is no longer true, so the narrowing is now
    /// conditional on the one fact that makes it necessary.
    ///
    /// Narrowed rather than refused outright, for the reason M7-1 gave and which still
    /// holds: the alternative is a daemon that will not start on the owner's existing
    /// configuration.
    pub fn management_addr(&self) -> Result<std::net::SocketAddr, AddressError> {
        let configured = &self.listener.management_listen_addr;
        let port = self.listener.management_port;
        let bare = configured
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(configured);
        let addr: std::net::IpAddr = bare.parse().map_err(|_| AddressError {
            addr: configured.clone(),
            port,
        })?;
        if self.claimed {
            return Ok(std::net::SocketAddr::new(addr, port));
        }
        // **Before the loopback check, not after.** `A-W5-3` makes the column's default
        // `127.0.0.1`, so on a fresh install the configured address is *already* loopback --
        // and a `--setup-listen` consulted only after an `is_loopback()` early return would
        // be a silent no-op on exactly the headless install the flag, and the first-run
        // banner that advertises it, exist for.
        if let Some(opted) = self.setup_listen {
            if opted.is_loopback() {
                // Asked for, and narrower than or equal to the configured address: there is
                // nothing to warn about.
                tracing::info!(
                    configured = %configured,
                    setup_listen = %opted,
                    port,
                    "--setup-listen binds the management API to a loopback address, which \
                     widens nothing"
                );
            } else {
                tracing::warn!(
                    configured = %configured,
                    setup_listen = %opted,
                    port,
                    "--setup-listen widens the management API before this daemon has an \
                     administrator account; anyone who reaches it and has the setup code \
                     printed on this machine's console can create that account"
                );
            }
            return Ok(std::net::SocketAddr::new(opted, port));
        }
        if addr.is_loopback() {
            return Ok(std::net::SocketAddr::new(addr, port));
        }
        tracing::info!(
            configured = %configured,
            port,
            "this daemon has no administrator account yet, so the management API is bound to \
             127.0.0.1 rather than the configured address; reach it over an SSH tunnel, or \
             start with --setup-listen, and it will bind the configured address once the \
             account exists"
        );
        Ok(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port,
        ))
    }

    /// The management API's certificate paths, when one is configured.
    ///
    /// Keyed by an empty hostname and marked the default, because there is one certificate
    /// and no SNI to select on: an operator reaches this API by address, not by a name the
    /// certificate has to match.
    pub fn management_cert(&self) -> Option<mw_http::CertPaths> {
        let cert = self.listener.management_tls_cert_path.as_ref()?;
        let key = self.listener.management_tls_key_path.as_ref()?;
        Some(mw_http::CertPaths {
            hostname: String::new(),
            cert: cert.into(),
            key: key.into(),
            is_default: true,
        })
    }

    /// Every website's certificate paths, for the SNI resolver (§6.4).
    ///
    /// Only the rows that have one: the schema's `CHECK` makes a certificate without its
    /// key unrepresentable, so a `Some` cert path is a complete pair.
    pub fn cert_paths(&self) -> Vec<mw_http::CertPaths> {
        self.websites
            .iter()
            .filter_map(|w| {
                let (cert, key) = (w.tls_cert_path.as_ref()?, w.tls_key_path.as_ref()?);
                Some(mw_http::CertPaths {
                    hostname: w.hostname.clone(),
                    cert: cert.into(),
                    key: key.into(),
                    is_default: w.is_default_tls,
                })
            })
            .collect()
    }

    /// The sockets the configuration names (§6.1), with `listen_addr` already parsed.
    pub fn listen(&self) -> Result<mw_http::Listen, AddressError> {
        Ok(mw_http::Listen {
            addr: self.listen_ip()?,
            http_port: self.listener.http_port,
            https_port: self.listener.https_port,
            grpc_port: self.listener.grpc_port,
        })
    }

    /// `listener.listen_addr`, parsed.
    ///
    /// The schema cannot check this -- SQLite has no inet type -- so it is a startup
    /// failure that names what was configured, rather than a bind that fails with "invalid
    /// argument".
    fn listen_ip(&self) -> Result<std::net::IpAddr, AddressError> {
        let host = &self.listener.listen_addr;
        // A bracketed IPv6 literal is what an operator may have typed into a column that
        // also accepts a bare one.
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        bare.parse().map_err(|_| AddressError {
            addr: host.clone(),
            port: self.listener.http_port,
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
            .field("legacy_routes", &self.legacy_routes.is_some())
            .field("ambient_upload", &self.ambient_upload.is_some())
            .field("grpcrx_service", &self.grpcrx_service.is_some())
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

    fn ambient_upload_state(&self) -> Option<&Arc<mw_station::ambient::UploadState>> {
        self.ambient_upload.as_ref()
    }
}

impl mw_grpc::SnowGaugeConfig for Config {
    fn stations(&self) -> &[mw_station::StationRow] {
        // The same rows `mw_station::register` dispatches over, because the two registrars
        // dispatch on the same `type` column (`mw_grpc::config`).
        &self.declared_stations
    }

    fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
        self.ingress.as_ref()
    }
}

impl mw_grpc::GrpcIngestConfig for Config {
    fn ingest(&self) -> &[mw_grpc::IngestRow] {
        &self.declared_ingest
    }

    fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
        self.ingress.as_ref()
    }

    /// The **same** answer `mw_collector::CollectorConfig::ingest_tls_available` gives,
    /// read from the same field: the two components are mounted on one socket, so a
    /// configuration one of them refuses and the other accepts is a daemon that will not
    /// start for no reason an operator can act on.
    fn ingest_tls_available(&self) -> bool {
        self.ingest_tls.is_some()
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

/// What the three rate-limited outputs plan from.
///
/// Column into field, like every other registrar trait here. The handle the uploaders need
/// is the **same** `Arc<dyn WindowedView>` the snapshot components are given -- one object,
/// so an uploader and `/latest` cannot disagree about what the five-minute wind average is.
impl mw_pipeline::snow::SnowConfig for Config {
    fn has_weather_database(&self) -> bool {
        self.weather_database.is_some()
    }

    /// Every enabled snow gauge, with its calibration and its **resolved** zone.
    ///
    /// Three predicates, each with a reason the registrar cannot supply:
    ///
    /// - `kind == "snowgauge"`, which is the configured device type `/stationinfo` returns
    ///   (`docs/legacy-api-contract.md` §3.1 is explicit that it and the driver's
    ///   self-reported `stationtype` "are not the same set and must not be unified").
    /// - `enabled`, which is W5 §1.2's "Collecting readings" toggle: a gauge with
    ///   collection switched off has no buckets to read, so planning four components over
    ///   it would mean four ticks a minute finding nothing, forever.
    /// - a `transport_kind`, which is what distinguishes a station this node **collects
    ///   from** from one a peer forwards readings for (`mw_station::config`). A gauge
    ///   received from a peer is inventory on this node; its snow totals are computed on
    ///   the node that owns it, and computing them here as well would be two answers to one
    ///   question.
    ///
    /// The zone comes from [`Config::station_zones`] -- the registry snapshot's resolved
    /// parse, for `rollup_stations`' reason: the parse **is** the validation (W2a Decision
    /// 11) and it already happened there, so a second parse would be a second answer to
    /// which midnight the station's day begins at.
    fn snow_stations(&self) -> Vec<mw_pipeline::snow::SnowStation> {
        let zones: std::collections::BTreeMap<&str, Option<Zone>> = self
            .station_zones
            .iter()
            .map(|(key, zone)| (key.as_str(), *zone))
            .collect();
        self.stations
            .iter()
            .filter(|r| r.kind == "snowgauge" && r.enabled && r.transport_kind.is_some())
            .map(|r| mw_pipeline::snow::SnowStation {
                key: StationKey::new(r.station_key.as_str()),
                base_distance_mm: r.snow_base_distance_mm,
                zone: zones
                    .get(r.station_key.as_str())
                    .copied()
                    .unwrap_or_default(),
            })
            .collect()
    }
}

impl mw_output::AerisConfig for Config {
    /// The account, or `None` when there is no row **or it is switched off**.
    ///
    /// The `enabled` flag is read here rather than in the registrar because this is the
    /// layer that holds the row: the trait's contract is "the account this node fetches
    /// with", and a disabled account is not one. An enabled row with no credential is
    /// unrepresentable -- the table's own `CHECK` rejects it -- and is answered as absent
    /// anyway rather than unwrapped, because a frozen boot path may not panic over a
    /// constraint.
    fn aeris_account(&self) -> Option<mw_output::AerisAccount> {
        let row = self.aeris_account.as_ref()?;
        if !row.enabled {
            return None;
        }
        Some(mw_output::AerisAccount {
            client_id: row.client_id.clone()?,
            client_secret: row.client_secret.clone()?,
            api_endpoint: row.api_endpoint.clone(),
            refresh_interval: row
                .refresh_interval_seconds
                .and_then(|seconds| u64::try_from(seconds).ok())
                .map(Duration::from_secs),
        })
    }

    fn aeris_stations(&self) -> Vec<mw_output::AerisStation> {
        self.declared_aeris.clone()
    }

    /// The deployment's own zone, for `aeris:alerts:gc`'s `Calendar`.
    ///
    /// `node.timezone` is `NOT NULL DEFAULT 'UTC'`, so the `None` here is a *malformed*
    /// value rather than an absent one -- and it is a `PlanError::MissingTimeZone` naming
    /// the sweep rather than a silent fallback, which is `TzSource`'s own rule. A node with
    /// no `node` row at all has not started before and has no components either.
    fn deployment_zone(&self) -> Option<Zone> {
        Zone::new(&self.node.as_ref()?.timezone).ok()
    }
}

impl mw_output::OutputConfig for Config {
    fn uploaders(&self) -> Vec<mw_output::UploaderRow> {
        self.declared_uploaders.clone()
    }

    /// The rollup state, upcast to the capability the uploaders declare — **and only when
    /// `window:rollup` is actually planned.**
    ///
    /// Set by `set_rollup` beside the snapshot's view, which is what makes "these are the
    /// same object" a fact of that one call rather than of two call sites.
    ///
    /// The condition is the load-bearing part and it is not the same question as "has the
    /// state been built". The uploaders declare `WindowedView` as a **`Hard`** need —
    /// deliberately, so that a restart leaves them `Waiting { on: WindowedView }` until the
    /// rollup has published rather than resolving against an empty view — and a hard need
    /// nobody provides makes `plan()` refuse the **whole** configuration (**D-44**). The
    /// state is a root resource `attach_registry` builds unconditionally, so answering
    /// `Some` from its existence alone would turn "this node has an uploader and collects
    /// for nothing" into a daemon that will not start.
    ///
    /// So the answer is gated on the predicate `mw_pipeline::rollup::register` plans the
    /// provider on, read through the **same accessors** it reads — a weather database, and
    /// at least one collecting station. The rule is duplicated here and the inputs are not,
    /// which is the half that could actually drift; `crates/modernweather/src/boot.rs` has
    /// a test for both directions.
    fn windowed_view(&self) -> Option<&Arc<dyn mw_component::WindowedView>> {
        use mw_pipeline::RollupConfig;
        if !self.has_weather_database() || self.rollup_stations().is_empty() {
            return None;
        }
        self.snapshot_windows.as_ref()
    }

    /// Whether `window:rollup` maintains windows for this station.
    ///
    /// Read from the **same accessor the rollup's registrar plans its station set from**
    /// (`mw_pipeline::RollupConfig::rollup_stations`), so the answer cannot disagree with
    /// the set `RollupState` was actually built over: `crate::boot::attach_registry` builds
    /// it from that call and nothing else.
    fn maintains_windows_for(&self, station: &StationKey) -> bool {
        use mw_pipeline::RollupConfig;
        self.rollup_stations().iter().any(|(key, _)| key == station)
    }

    /// Whether this station exists and has collection switched off.
    ///
    /// From the rows rather than from the rollup's set, because the point is to tell the two
    /// reasons a station is absent from that set apart. A station that is not in
    /// `config.stations` at all answers `false`, which is the right answer: the pairing's
    /// foreign key makes that unrepresentable, and if it somehow happened a refusal is the
    /// honest outcome rather than a skip.
    fn station_is_disabled(&self, station: &StationKey) -> bool {
        self.stations
            .iter()
            .any(|row| row.station_key == station.as_str() && !row.enabled)
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

    /// The `mw_api_legacy::Renderer`, built in [`crate::boot`] beside the registry.
    ///
    /// `None` only before `set_legacy` has run, which is a boot sequence that has not
    /// reached step 4c -- `mw_pipeline::snapshot::register` then plans neither component
    /// and says so at `info`, which is the state every build before M4-2 was in.
    fn snapshot_render(&self) -> Option<&Arc<dyn mw_component::SnapshotRender>> {
        self.snapshot_render.as_ref()
    }
}

/// What the frozen REST surface plans from.
///
/// Column into field, like [`mw_station::StationConfig`] above it and for the same reason:
/// `mw-api-legacy` may not depend on `mw-store`, so what it gets is a trait this crate
/// implements over rows it has already read. The copy is what buys the property that
/// `mw-store` cannot grow a route and `mw-api-legacy` cannot grow a query.
impl mw_api_legacy::LegacyConfig for Config {
    fn legacy_websites(&self) -> &[mw_api_legacy::WebsiteView] {
        &self.legacy_websites
    }

    fn legacy_stations(&self) -> &[mw_api_legacy::StationView] {
        &self.legacy_stations
    }

    /// Where `/api/stations` reports this server answers.
    ///
    /// `https` when a TLS port is configured, and **that port** rather than the plaintext
    /// one: the block is a URL the website builds, so it has to name the port a browser
    /// should use. A node with both serves the frozen surface on both; the website links
    /// to the secure one, which is what the Go handler's `protocol` field was for.
    fn legacy_listener(&self) -> mw_api_legacy::Listener {
        match self.listener.https_port {
            Some(port) => mw_api_legacy::Listener { port, tls: true },
            None => mw_api_legacy::Listener {
                port: self.listener.http_port,
                tls: false,
            },
        }
    }

    fn legacy_route_state(&self) -> Option<&Arc<mw_api_legacy::RouteState>> {
        self.legacy_routes.as_ref()
    }

    fn legacy_render(&self) -> Option<&Arc<mw_api_legacy::Renderer>> {
        self.legacy_render.as_ref()
    }

    /// Answered by the crate that plans the provider, so there is one copy of the
    /// conditions.
    fn has_latest_snapshot(&self) -> bool {
        mw_pipeline::snapshot::provides_latest_snapshot(self)
    }
}

/// What the public website plans and renders from.
///
/// Column into field, like every registrar trait above it: `mw-web` may not depend on
/// `mw-store`, and the row it needs is the website **joined to its primary station** --
/// which is the join the Go handler does per request (`handlers.go:869`).
impl mw_web::WebConfig for Config {
    fn web_sites(&self) -> &[mw_web::SiteView] {
        &self.web_sites
    }

    /// The daemon's version, which the footer renders.
    ///
    /// `CARGO_PKG_VERSION` of the **binary**, which is what `mw_build_info` reports and
    /// what `--version` prints. The Go page rendered `constants.Version` for the same
    /// reason.
    fn web_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// The same question `api:legacy` asks, with the same one answer behind it -- see
    /// `mw_web::component`'s module documentation for why a website component has to ask.
    fn web_has_latest_snapshot(&self) -> bool {
        mw_pipeline::snapshot::provides_latest_snapshot(self)
    }

    fn web_states(&self) -> Option<&[Arc<mw_web::SiteState>]> {
        self.web_states.as_deref()
    }
}

/// What the one listener plans from.
impl mw_http::ListenerConfig for Config {
    /// **`expect` rather than a fallible accessor**, and the reason is where the parse
    /// already happened: [`crate::boot`] calls [`Config::listen`] before `plan()` -- it has
    /// to, because the sockets' addresses are what the mount is built for -- so a bad
    /// `listen_addr` is already a named startup failure by the time a registrar runs. A
    /// second fallible path here would be a second place that decides what an unparseable
    /// address means.
    fn listen(&self) -> mw_http::Listen {
        Config::listen(self).expect(
            "the boot sequence parses `listener.listen_addr` before `plan()` runs, so an \
             unparseable one has already been reported",
        )
    }

    fn mount(&self) -> Option<&Arc<mw_http::Mount>> {
        self.mount.as_ref()
    }

    fn sni_resolver(&self) -> Option<&Arc<mw_http::SniResolver>> {
        self.sni.as_ref()
    }

    fn ingest_tls(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.ingest_tls.clone()
    }

    /// The management API's socket and router (**M7-1**).
    ///
    /// Cloned per call rather than borrowed, because [`mw_http::Management`] owns an
    /// `axum::Router` that the component's builder closure moves into a task -- and a
    /// `Backoff` retry builds again, so the router has to survive being taken.
    fn management(&self) -> Option<mw_http::Management> {
        self.mgmt.as_ref().map(|m| mw_http::Management {
            addr: m.addr,
            router: m.router.clone(),
            tls: m.tls.clone(),
        })
    }
}

impl mw_collector::CollectorConfig for Config {
    fn ingest(&self) -> &[mw_collector::IngestRow] {
        &self.declared_collector_ingest
    }

    fn ingress(&self) -> Option<&Arc<dyn Ingress>> {
        self.ingress.as_ref()
    }

    fn spool(&self) -> Option<&Arc<mw_pipeline::Spool>> {
        self.spool.as_ref()
    }

    fn station_registry(&self) -> Option<&Arc<dyn mw_component::StationRegistry>> {
        self.registry.as_ref()
    }

    fn peer_registry(&self) -> Option<&Arc<dyn mw_component::PeerRegistry>> {
        self.peer_registry.as_ref()
    }

    fn node_id(&self) -> Option<mw_types::OriginId> {
        self.node
            .as_ref()
            .and_then(|node| <[u8; 16]>::try_from(node.node_id.as_slice()).ok())
            .map(mw_types::OriginId::new)
    }

    fn ingest_service(&self) -> Option<&Arc<mw_collector::ServiceState>> {
        self.ingest_service.as_ref()
    }

    fn ingest_tls_available(&self) -> bool {
        self.ingest_tls.is_some()
    }
}

impl mw_collector::UplinkConfig for Config {
    fn uplinks(&self) -> &[mw_collector::UplinkRow] {
        &self.declared_uplinks
    }

    fn unjoinable_peers(&self) -> &[String] {
        &self.unjoinable_uplinks
    }

    fn identity(&self) -> Option<&mw_collector::Identity> {
        self.node_identity.as_ref()
    }

    fn spool(&self) -> Option<&Arc<mw_pipeline::Spool>> {
        self.spool.as_ref()
    }
}

/// One `config.ingest` row in the shape `mw-collector` plans from.
fn declare_collector_ingest(row: &config_db::IngestRow) -> mw_collector::IngestRow {
    mw_collector::IngestRow {
        listen: row.listen.clone(),
        peer_auth: row.peer_auth.clone(),
        ca_path: row.ca_path.clone(),
        allow_insecure: row.allow_insecure,
    }
}

/// One `uplink` row joined to the peer it names, or `None` when there is no such peer.
///
/// A `None` is **not** dropped: it is carried to `mw_collector::uplink::register` through
/// [`Config::unjoinable_uplinks`] and becomes `UplinkPlanError::UnknownPeer`. The uplink
/// needs the peer's `NodeId` for the loop filter and for `forward` and there is nowhere
/// else to get one, so a row whose peer is unknown cannot be planned — and the difference
/// between saying that and planning nothing is the difference between a daemon that will
/// not start and a satellite that silently forwards nothing.
fn declare_uplink(
    row: &config_db::UplinkRow,
    peers: &[config_db::PeerRow],
) -> Option<mw_collector::UplinkRow> {
    let peer = peers.iter().find(|p| p.name == row.peer_name)?;
    let node = <[u8; 16]>::try_from(peer.node_id.as_slice()).ok()?;
    Some(mw_collector::UplinkRow {
        peer_name: row.peer_name.clone(),
        peer_node: mw_types::OriginId::new(node),
        endpoint: row.endpoint.clone(),
        auth_kind: row.auth_kind.clone(),
        tls_cert_path: row.tls_cert_path.clone(),
        tls_key_path: row.tls_key_path.clone(),
        tls_ca_path: row.tls_ca_path.clone(),
        // Re-read from the row rather than cloned: `SecretString` is deliberately not
        // `Clone`, and `Arc` is how the one copy is shared with a builder that runs per
        // start attempt (`mw_collector::UplinkRow`).
        psk: row
            .psk
            .as_ref()
            .map(|psk| Arc::new(secrecy::SecretString::from(psk.expose_secret().to_string()))),
        // §9.3's `[[uplink]].require_ack` has no column yet -- the `uplink` table predates
        // that section -- so the transition default is what travels, and adding the column
        // is a migration rather than a redesign.
        require_ack: false,
    })
}

/// One `config.website` row joined to its primary station, as the website renders it.
///
/// Three of the nine values the inline configuration block carries are **not** on the
/// website row, and each one is resolved here: `stationID` and `airQualityDeviceID` are the
/// `stations.id` surrogates, and `aerisWeatherEnabled` is a `station_service` row with
/// `service = 'aeris'` (the schema's own comment says so).
fn declare_site(
    row: &WebsiteRow,
    stations: &[config_db::StationRow],
    aeris: &[String],
) -> mw_web::SiteView {
    let by_key = |key: &Option<String>| -> Option<&config_db::StationRow> {
        let key = key.as_deref()?;
        stations.iter().find(|s| s.station_key == key)
    };
    let primary = by_key(&row.primary_station_key);
    let air_quality = by_key(&row.air_quality_station_key);
    let snow = by_key(&row.snow_station_key);
    mw_web::SiteView {
        id: WebsiteId::new(row.id),
        hostname: row.hostname.clone(),
        name: row.name.clone(),
        page_title: row.page_title.clone(),
        apple_app_id: row.apple_app_id.clone(),
        is_portal: row.is_portal,
        radar_enabled: row.radar_enabled,
        radar_token: row.radar_token.clone(),
        snow_enabled: row.snow_enabled,
        // The station **name**, which is what the JavaScript queries by -- not the key.
        // `?station=` names `devices.name` in the frozen contract, and the writer projects
        // the same string onto `stationname` (`mw_api_legacy::config`).
        snow_device: snow.map(|s| s.name.clone()).unwrap_or_default(),
        air_quality_enabled: row.air_quality_enabled,
        air_quality_device: air_quality.map(|s| s.name.clone()).unwrap_or_default(),
        // `0` for a website with no air-quality station, which is what
        // `getAirQualityDeviceID` returns.
        air_quality_device_id: air_quality.map(|s| s.id).unwrap_or(0),
        // `None` when the column names a station that is not in `stations` -- which
        // `ON DELETE SET NULL` makes unreachable from a consistent database, and which a
        // hand-edited one can still produce. A portal has no primary station by design.
        primary_station: primary.map(|s| StationKey::new(s.station_key.clone())),
        primary_station_name: primary.map(|s| s.name.clone()).unwrap_or_default(),
        primary_station_id: primary.map(|s| s.id).unwrap_or(0),
        latitude: primary.and_then(|s| s.latitude),
        longitude: primary.and_then(|s| s.longitude),
        aeris_enabled: primary.is_some_and(|s| aeris.contains(&s.station_key)),
    }
}

/// One `config.website` row, as the frozen surface reads it.
fn declare_website(row: &WebsiteRow) -> mw_api_legacy::WebsiteView {
    mw_api_legacy::WebsiteView {
        id: WebsiteId::new(row.id),
        name: row.name.clone(),
        hostname: row.hostname.clone(),
        page_title: row.page_title.clone(),
        about_text: row.about_station_html.clone(),
        primary_station: row.primary_station_key.clone().map(StationKey::new),
        snow_enabled: row.snow_enabled,
        snow_station: row.snow_station_key.clone().map(StationKey::new),
        air_quality_enabled: row.air_quality_enabled,
        air_quality_station: row.air_quality_station_key.clone().map(StationKey::new),
        is_portal: row.is_portal,
        radar_enabled: row.radar_enabled,
        radar_token: row.radar_token.clone(),
    }
}

/// One `config.stations` row, as the frozen surface reads it.
fn declare_station(row: &config_db::StationRow) -> mw_api_legacy::StationView {
    mw_api_legacy::StationView {
        id: row.id,
        key: StationKey::new(row.station_key.clone()),
        name: row.name.clone(),
        kind: row.kind.clone(),
        enabled: row.enabled,
        latitude: row.latitude,
        longitude: row.longitude,
        timezone: row.timezone.clone(),
    }
}

/// Every Aeris-paired station, joined to its coordinates.
///
/// The pairing is a `station_service` row with `service = 'aeris'` and the coordinates are
/// the station's, which is the join `mw-output` cannot do: the two tables are `mw-store`'s
/// and `ci/layers.toml` forbids that crate to it.
///
/// **A disabled station keeps its pairing and loses its components**, which is the same
/// predicate `snow_stations` applies and for the same reason: an operator who switched a
/// station off did not ask to keep paying for its forecasts. A station received from a
/// peer, by contrast, **does** get its forecasts fetched here -- a forecast is a property
/// of a *location*, not of who collects the readings, and the node that serves a website
/// for a remote station is the node that needs its forecast.
///
/// # The disabled case is logged, because the website does not agree with it
///
/// `declare_site` derives the rendered page's `aerisWeatherEnabled` from the **unfiltered**
/// pairing list, so a disabled, paired station that is a website's primary still advertises
/// forecasts -- while this function drops it and no component is planned. That is the exact
/// state `mw_output::aeris::register` refuses the NULL-coordinates route to ("a website with
/// `aerisWeatherEnabled: true`, an empty forecast panel, and nothing anywhere saying why"),
/// reached by a different route.
///
/// It is **logged rather than refused or re-enabled**, and each of the three was considered:
/// refusing would make switching a station off a configuration the daemon will not start on;
/// planning the components anyway would fetch -- and bill -- for a station the operator
/// turned off; and changing what the website renders would change a frozen page's bytes for
/// a reason that has nothing to do with this item. So it is a `warn` naming the row and both
/// fixes, which is what `mw_output::plan`'s `station_is_disabled` branch does for the same
/// situation one table over.
fn declare_aeris(
    stations: &[config_db::StationRow],
    paired: &[String],
) -> Vec<mw_output::AerisStation> {
    for row in stations {
        if !row.enabled && paired.contains(&row.station_key) {
            tracing::warn!(
                station = %row.station_key,
                "this station is paired with the Aeris service and has collecting switched                  off, so NO forecast or alert component is planned for it -- while a website                  whose primary station it is still advertises forecasts. Re-enable the                  station, or remove its `station_service` row with `service = 'aeris'`."
            );
        }
    }
    stations
        .iter()
        .filter(|r| r.enabled && paired.contains(&r.station_key))
        .map(|r| mw_output::AerisStation {
            key: StationKey::new(r.station_key.as_str()),
            latitude: r.latitude,
            longitude: r.longitude,
        })
        .collect()
}

/// One `uploader` row, column into field.
///
/// Field for field with no interpretation, which is the point: every default, every clamp
/// and every refusal is `mw_output::plan`'s, so a reader comparing the two structs is
/// comparing names rather than behaviour. A column added to the schema that is not added
/// here is an uploader that silently ignores it, which is why both structs document each
/// field with its column name.
fn declare_uploader(row: &config_db::UploaderRow) -> mw_output::UploaderRow {
    mw_output::UploaderRow {
        kind: row.kind.clone(),
        name: row.name.clone(),
        interval_seconds: row.interval_seconds,
        api_endpoint: row.api_endpoint.clone(),
        api_key: row.api_key.clone(),
        station_key: row.station_key.clone(),
        remote_station_id: row.remote_station_id.clone(),
        latitude: row.latitude,
        longitude: row.longitude,
        aprs_callsign: row.aprs_callsign.clone(),
        station_enabled: row.station_enabled,
        station_transport_kind: row.station_transport_kind.clone(),
        aprs_passcode: row.aprs_passcode.clone(),
        aprs_symbol_table: row.aprs_symbol_table.clone(),
        aprs_symbol_code: row.aprs_symbol_code.clone(),
        aprs_comment: row.aprs_comment.clone(),
        aprs_server: row.aprs_server.clone(),
        aprs_transport: row.aprs_transport.clone(),
        aprs_kiss_serial_device: row.aprs_kiss_serial_device.clone(),
        aprs_kiss_serial_baud: row.aprs_kiss_serial_baud,
        aprs_kiss_tcp_address: row.aprs_kiss_tcp_address.clone(),
        aprs_kiss_path: row.aprs_kiss_path.clone(),
        aprs_kiss_destination: row.aprs_kiss_destination.clone(),
    }
}

/// One `config.ingest` row, as the gRPC crate declares its receiver from.
///
/// Column into field, and nothing else: every decision is `mw_grpc::grpcrx::register`'s,
/// because the crate that owns what a receiver is is the crate that should decide whether
/// to plan one.
fn declare_ingest(row: &config_db::IngestRow) -> mw_grpc::IngestRow {
    mw_grpc::IngestRow {
        listen: row.listen.clone(),
        peer_auth: row.peer_auth.clone(),
        ca_path: row.ca_path.clone(),
        allow_insecure: row.allow_insecure,
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
        wind_dir_correction_degrees: row.wind_dir_correction_degrees,
        snow_base_distance_mm: row.snow_base_distance_mm,
        // The bit, not the hash: `mw-station` plans and never verifies, so what it needs is
        // whether this station *can* be authenticated (`mw_station::StationRow`).
        //
        // `trim()` because `mw_store::RegistrySnapshot` uses `trim()`, and the two
        // predicates have to be the same one. They were not: an `ingest_secret_hash` of
        // `' '` -- the column has no `CHECK` -- was non-empty here and absent there, so
        // `plan`'s `NoSecret` guard passed and the station then failed closed on every
        // upload with nothing naming it. `SecretProblem::Missing` reports the case that
        // still reaches the registry; this makes `plan` refuse the row that produces it.
        has_ingest_secret: row
            .ingest_secret_hash
            .as_deref()
            .is_some_and(|hash| !hash.trim().is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(addr: &str, port: u16) -> Config {
        Config {
            declared_uploaders: Vec::new(),
            services: Vec::new(),
            aeris_account: None,
            declared_aeris: Vec::new(),
            stations: Vec::new(),
            declared_stations: Vec::new(),
            websites: Vec::new(),
            legacy_websites: Vec::new(),
            legacy_stations: Vec::new(),
            ingest: Vec::new(),
            declared_ingest: Vec::new(),
            declared_collector_ingest: Vec::new(),
            declared_uplinks: Vec::new(),
            unjoinable_uplinks: Vec::new(),
            peer_registry: None,
            ingest_service: None,
            node_identity: None,
            ingest_tls: None,
            spool: None,
            legacy_routes: None,
            ambient_upload: None,
            grpcrx_service: None,
            legacy_render: None,
            snapshot_render: None,
            ingress: None,
            registry: None,
            rollup: None,
            snapshot: None,
            snapshot_windows: None,
            station_zones: Vec::new(),
            web_sites: Vec::new(),
            web_states: None,
            mount: None,
            sni: None,
            mgmt: None,
            listener: Listener {
                listen_addr: addr.to_string(),
                http_port: port,
                https_port: None,
                grpc_port: 50051,
                management_listen_addr: "127.0.0.1".into(),
                management_port: 8081,
                management_tls_cert_path: None,
                management_tls_key_path: None,
            },
            weather_database: None,
            node: None,
            claimed: false,
            setup_listen: None,
            generation: Generation::ZERO,
        }
    }

    /// **W5 §3.4 / O-10's OQ-W5-1 (a)**, as the three cases the function has.
    ///
    /// The one worth asserting is the middle row: `--setup-listen` is the only thing that
    /// widens an *unclaimed* instance, and it is **ignored once claimed**, because from then
    /// on `listener.management_listen_addr` is the answer and a flag that outlived the window
    /// it exists for would be a permanently widened bind nobody chose twice.
    #[test]
    fn the_management_api_binds_loopback_until_the_instance_is_claimed() {
        let widened = std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0));

        // Unclaimed, no flag: narrowed to loopback, whatever was configured.
        let mut cfg = config("127.0.0.1", 8080);
        cfg.listener.management_listen_addr = "0.0.0.0".into();
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "127.0.0.1:8081"
        );

        // Unclaimed, with the flag: the flag's address.
        cfg.set_setup_listen(Some(widened));
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "0.0.0.0:8081"
        );

        // **And with the flag against the schema's own default**, which is the case the flag
        // exists for and the one an `is_loopback()` check placed first would silently ignore:
        // a fresh install has `management_listen_addr = '127.0.0.1'`, so a headless operator
        // passing `--setup-listen` has to get the widened address rather than loopback.
        let mut fresh = config("127.0.0.1", 8080);
        assert_eq!(fresh.listener.management_listen_addr, "127.0.0.1");
        fresh.set_setup_listen(Some(widened));
        assert_eq!(
            fresh.management_addr().expect("an address").to_string(),
            "0.0.0.0:8081"
        );
        // A loopback `--setup-listen` is honoured too, and widens nothing.
        fresh.set_setup_listen(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
            127, 0, 0, 2,
        ))));
        assert_eq!(
            fresh.management_addr().expect("an address").to_string(),
            "127.0.0.2:8081"
        );

        // Claimed: the configured address, and the flag no longer applies.
        cfg.claimed = true;
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "0.0.0.0:8081"
        );
        cfg.set_setup_listen(None);
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "0.0.0.0:8081"
        );

        // A configured loopback address is returned as-is in both states, so the narrowing
        // is invisible on the schema's own default (**A-W5-3**).
        let mut cfg = config("127.0.0.1", 8080);
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "127.0.0.1:8081"
        );
        cfg.claimed = true;
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "127.0.0.1:8081"
        );

        // A bracketed IPv6 address survives the strip in both states.
        let mut cfg = config("127.0.0.1", 8080);
        cfg.listener.management_listen_addr = "[::]".into();
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "127.0.0.1:8081",
            "an unclaimed instance narrows an IPv6 any-address too"
        );
        cfg.claimed = true;
        assert_eq!(
            cfg.management_addr().expect("an address").to_string(),
            "[::]:8081"
        );

        // And something that is not an address is still a refusal naming what was typed.
        let mut cfg = config("127.0.0.1", 8080);
        cfg.listener.management_listen_addr = "weatherbox".into();
        cfg.claimed = true;
        let e = cfg.management_addr().expect_err("not an address");
        assert!(e.to_string().contains("weatherbox"), "{e}");
    }

    #[test]
    fn the_schema_default_binds_every_interface_on_the_main_port() {
        let listen = config("0.0.0.0", 8080)
            .listen()
            .expect("the seeded default is an address");
        assert_eq!(listen.main().to_string(), "0.0.0.0:8080");
        assert_eq!(listen.grpc().to_string(), "0.0.0.0:50051");
        assert_eq!(listen.redirect(), None, "no TLS port, no redirector");
    }

    /// An IPv6 literal, bracketed or bare, which is what the column accepts.
    #[test]
    fn an_ipv6_literal_is_accepted_either_way() {
        assert_eq!(
            config("::", 8080).listen().expect("v6").main().to_string(),
            "[::]:8080"
        );
        assert_eq!(
            config("[::1]", 9090)
                .listen()
                .expect("v6")
                .main()
                .to_string(),
            "[::1]:9090"
        );
    }

    /// With a TLS port configured the main socket is **that** port and the HTTP port
    /// becomes the redirector (§6.1).
    #[test]
    fn a_tls_port_moves_the_main_socket_and_leaves_a_redirector() {
        let mut cfg = config("127.0.0.1", 8080);
        cfg.listener.https_port = Some(8443);
        let listen = cfg.listen().expect("an address");
        assert_eq!(listen.main().to_string(), "127.0.0.1:8443");
        assert_eq!(
            listen.redirect().expect("a redirector").to_string(),
            "127.0.0.1:8080"
        );
        assert!(listen.tls());
    }

    #[test]
    fn a_hostname_is_rejected_with_what_was_configured() {
        // Deliberately not resolved: D-43's static musl linkage means musl's resolver,
        // which supports neither `nsswitch.conf` nor NSS modules (W8 OQ-4) -- so a
        // listen address that needs resolution is a configuration that behaves
        // differently in the release build than in a developer's.
        let e = config("localhost", 8080).listen().expect_err("rejected");
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
