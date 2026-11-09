//! What `api:legacy` plans from: the website rows, the station rows and the listener.
//!
//! A trait rather than a type, for `mw_station::StationConfig`'s reason: `plan()` is
//! generic over the configuration and the binary chooses it (§1.3 rule 2), so a registrar
//! takes `&C` and each component crate names only the rows it plans from. The rows are
//! copied column-into-field by the binary, which is what buys the property that this crate
//! cannot grow a query and `mw-store` cannot grow a route.
//!
//! # Why the configuration arrives through the spec rather than through a capability
//!
//! `docs/component-model.md` §4.1b gives `api:legacy` five requirements and `ConfigStore`
//! is **not** one of them: `LatestSnapshot` hard, and `ReadingStore`, `ForecastStore`,
//! `AlertStore` and `SnowCache` soft. So the website and station rows are closed over at
//! plan time and the spec's [`mw_component::ConfigDigest`] is what restarts the component
//! when they change -- which is the same arrangement every other component has, and the
//! reason a configuration change is a re-plan rather than a watch inside a handler.
//!
//! The cost is stated rather than hidden: until `ConfigAdmin` exists (M7-1) nothing
//! re-plans, so a website added while the daemon runs is not served until it restarts.
//! That is true of every configuration change in this build (`mw_store::registry` says so
//! for the station registry) and is not specific to this crate.

use std::sync::Arc;

use mw_types::{StationKey, WebsiteId};

use crate::component::RouteState;
use crate::render::Renderer;

/// One `config.website` row, as the frozen surface reads it.
///
/// Not [`mw_component::WebsiteView`], which is the *snapshot's* four-column view: that one
/// exists so a renderer can be handed the per-website half of a `/latest` payload, and this
/// one is what `/stationinfo`, `/snow`, `/almanac` and `/api/stations` read. The two
/// overlap in four fields and the overlap is deliberate -- the binary builds both from one
/// `config_db::WebsiteRow`, so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WebsiteView {
    /// `id`.
    pub id: WebsiteId,
    /// `name` -- `/stationinfo`'s `website_name`.
    pub name: String,
    /// `hostname`, already lower-case and port-free by the schema's own `CHECK`s. `""` is
    /// the default website.
    pub hostname: String,
    /// `page_title` -- reaches the wire only through `/api/stations`.
    pub page_title: String,
    /// `about_station_html` -- `/stationinfo`'s `about_text`, HTML and not plain text.
    pub about_text: String,
    /// `primary_station_key`: which station `/latest` answers for when the request names
    /// none.
    pub primary_station: Option<StationKey>,
    /// `snow_enabled`. `false` is `/snow`'s `404 snow data not enabled for this website`.
    pub snow_enabled: bool,
    /// `snow_station_key`. `/snow`'s `stationname` is **always** this station's name, even
    /// when `?station=` named another one (Q-8).
    pub snow_station: Option<StationKey>,
    /// `air_quality_enabled`.
    pub air_quality_enabled: bool,
    /// `air_quality_station_key`. `/almanac`'s particulate extremes are read from **this**
    /// station rather than from the one the request named.
    pub air_quality_station: Option<StationKey>,
    /// `is_portal`. The only website `/api/stations` answers for; every other one gets
    /// `403`.
    pub is_portal: bool,
    /// `radar_enabled`.
    pub radar_enabled: bool,
    /// `radar_token`. Public on `/stationinfo` and in the rendered markup.
    pub radar_token: String,
}

/// One `config.stations` row, as the frozen surface reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct StationView {
    /// `id` -- the surrogate integer `?station_id=` names and `/stationinfo` emits.
    pub id: i64,
    /// `station_key` -- the domain identity, and the `station_id` the store is keyed by.
    pub key: StationKey,
    /// `name` -- the **device name** `?station=` names and the `stationname` the writer
    /// projects onto every row.
    pub name: String,
    /// `type` -- the CONFIGURED device type, which is the vocabulary `/stationinfo`
    /// returns: `campbellscientific`, not the driver's self-reported `campbell`.
    /// `docs/legacy-api-contract.md` §3.1 is explicit that the two sets must not be
    /// unified.
    pub kind: String,
    /// `enabled`. A disabled station is still listed by `/api/stations` and
    /// `/stationinfo`.
    pub enabled: bool,
    /// `latitude`, degrees north.
    pub latitude: Option<f64>,
    /// `longitude`, degrees east.
    pub longitude: Option<f64>,
    /// `timezone`, an IANA name. What `/latest`'s `sunrise` and `sunset` are formatted in;
    /// `None` renders them in UTC, which is the honest answer for a station whose zone
    /// nobody configured.
    pub timezone: Option<String>,
}

impl StationView {
    /// Whether `/api/stations` excludes this station.
    ///
    /// `latitude == 0 AND longitude == 0` is excluded, which is the Go predicate verbatim:
    /// the column is `NOT NULL DEFAULT 0` there, so "no location" and "the Gulf of Guinea"
    /// are the same value. Here the columns are nullable, so `None` is excluded too --
    /// which is the same station and the honest spelling of it.
    pub fn has_location(&self) -> bool {
        match (self.latitude, self.longitude) {
            (Some(lat), Some(lon)) => lat != 0.0 || lon != 0.0,
            _ => false,
        }
    }
}

/// The one listener, as `/api/stations`' nested `website` block reports it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listener {
    /// `listener.http_port`, or `https_port` when one is configured.
    pub port: u16,
    /// Whether a TLS port is configured, which is the whole of `protocol`.
    pub tls: bool,
}

impl Listener {
    /// `"http"` or `"https"`.
    pub fn protocol(self) -> &'static str {
        if self.tls {
            "https"
        } else {
            "http"
        }
    }
}

/// What this crate needs from the configuration to plan `api:legacy`.
pub trait LegacyConfig {
    /// Every `config.website` row, in id order.
    fn legacy_websites(&self) -> &[WebsiteView];

    /// Every `config.stations` row, in key order.
    fn legacy_stations(&self) -> &[StationView];

    /// Where the one listener binds.
    fn legacy_listener(&self) -> Listener;

    /// The state slot the router reads, built by `main` as a root resource.
    ///
    /// `None` is a bug in the boot sequence rather than a configuration error -- the
    /// listener and the component are assembled by the same function -- and
    /// [`crate::LegacyPlanError::NoRouteState`] says so.
    fn legacy_route_state(&self) -> Option<&Arc<RouteState>>;

    /// The frozen DTO's serialiser, built by `main` as a root resource.
    ///
    /// **One object, two consumers**, which is why it travels this way rather than being
    /// built here: `snapshot:latest` renders every payload through it and `/snow` folds its
    /// revision into an `ETag`, so a second instance would be a second answer to "what
    /// version are these bytes". The same reasoning `mw_pipeline::snapshot`'s
    /// `snapshot_windows` records for the rollup state.
    fn legacy_render(&self) -> Option<&Arc<Renderer>>;

    /// Whether this configuration will contain a `LatestSnapshot` provider.
    ///
    /// **`api:legacy` is planned only when one will be**, and the reason is `plan()`'s own
    /// rule rather than a preference: **D-48** requires the hard need set to be exactly
    /// `{LatestSnapshot}`, and a hard need nobody provides is a `PlanError` that refuses
    /// the whole configuration. A node with no station in it would otherwise be a daemon
    /// that will not start.
    ///
    /// Answered by the binary, which is the one place a consumer and a provider meet
    /// (§1.3 rule 1) -- and answered from `mw_pipeline::snapshot::provides_latest_snapshot`,
    /// so there is one copy of the conditions and that crate's own test is what keeps it
    /// honest.
    fn has_latest_snapshot(&self) -> bool;
}

/// The resolved configuration one request is answered from.
///
/// Built once per plan and shared by every handler. Immutable: a configuration change is a
/// re-plan, so nothing here is behind a lock.
#[derive(Debug)]
pub struct Directory {
    websites: Vec<WebsiteView>,
    stations: Vec<StationView>,
    listener: Listener,
    /// Which website an unmatched `Host` resolves to (Q-18). An index, so that an empty
    /// configuration is representable as `None` rather than as a sentinel id.
    default_website: Option<usize>,
}

impl Directory {
    /// Resolve the tables once.
    pub fn new(websites: Vec<WebsiteView>, stations: Vec<StationView>, listener: Listener) -> Self {
        // **Q-18, preserved exactly** (§6.3): an unmatched `Host` gets the first website,
        // *unless* one has an empty hostname, in which case that one wins. Nothing in the
        // response distinguishes "you asked for the right host" from "you asked for an
        // unknown host and got someone else's station", and that is the contract.
        //
        // "First" is `ORDER BY id` here, where the Go provider ordered by name. The
        // difference is observable only for an unmatched `Host` on a configuration with no
        // empty-hostname website, which no fixture covers and which is a response nobody
        // should be relying on -- it is recorded here rather than silently changed.
        let default_website = websites
            .iter()
            .position(|w| w.hostname.is_empty())
            .or(if websites.is_empty() { None } else { Some(0) });
        Self {
            websites,
            stations,
            listener,
            default_website,
        }
    }

    /// Which website a `Host` header resolves to.
    ///
    /// Three normalisation steps, and only the third is a deviation (**DEV-2**, §6.3):
    /// strip the port, strip a single trailing dot, and lower-case. The Go lookup is a
    /// case-sensitive map, so today `EXAMPLE.COM` silently serves the **default**
    /// website's data under someone else's hostname; no fixture covers mixed case, and
    /// preserving the behaviour would preserve a data-disclosure bug rather than a
    /// contract.
    ///
    /// `None` only when no website is configured at all, which is the `503` case.
    pub fn website_for_host(&self, host: &str) -> Option<&WebsiteView> {
        let normalised = normalise_host(host);
        self.websites
            .iter()
            .find(|w| w.hostname == normalised)
            .or_else(|| self.default_website.map(|i| &self.websites[i]))
    }

    /// Every website, in id order.
    pub fn websites(&self) -> &[WebsiteView] {
        &self.websites
    }

    /// Every station, in key order.
    pub fn stations(&self) -> &[StationView] {
        &self.stations
    }

    /// The listener, for `/api/stations`.
    pub fn listener(&self) -> Listener {
        self.listener
    }

    /// One station by the surrogate integer `?station_id=` names.
    pub fn station_by_id(&self, id: i64) -> Option<&StationView> {
        self.stations.iter().find(|s| s.id == id)
    }

    /// One station by the **device name** `?station=` names.
    ///
    /// By `name` and not by `station_key`, which is the contract: `?station=davis-full`
    /// names what the Go `devices.name` column holds, and the same string is what the
    /// writer projects onto `stationname`. The two are routinely equal in a configuration
    /// migrated from Go and are not the same field.
    pub fn station_by_name(&self, name: &str) -> Option<&StationView> {
        self.stations.iter().find(|s| s.name == name)
    }

    /// One station by its key, for the website bindings -- which are keys rather than
    /// names.
    pub fn station_by_key(&self, key: &StationKey) -> Option<&StationView> {
        self.stations.iter().find(|s| &s.key == key)
    }

    /// The website a station is bound to, for `/api/stations`' nested block.
    ///
    /// The Go schema has `devices.website_id`; this one reverses the reference, so a
    /// station's website is the one whose `primary_station_key` names it. The **first** by
    /// id when more than one does, which the management API's own uniqueness check would
    /// refuse to create and which `ci/conformance/config-seed.sql` nonetheless does for
    /// three websites that share one station -- so it is answered rather than left to
    /// `expect`.
    pub fn website_of_station(&self, station: &StationView) -> Option<&WebsiteView> {
        self.websites
            .iter()
            .find(|w| w.primary_station.as_ref() == Some(&station.key))
    }
}

/// `Host`, normalised the three ways §6.3 specifies.
///
/// An IPv6 literal keeps its brackets and loses its port: `[::1]:8080` is `[::1]`, which is
/// what the stored hostname would have to be to match it. Splitting on the **last** colon
/// is what makes that work, and splitting on the first would truncate the address.
fn normalise_host(host: &str) -> String {
    let mut h = host.trim();
    // The port, if there is one. A bracketed literal's port is after the `]`.
    if let Some(close) = h.rfind(']') {
        h = &h[..=close];
    } else if let Some(colon) = h.rfind(':') {
        // Only when what follows is a port rather than part of a bare IPv6 literal, which
        // is not legal in a `Host` header anyway.
        if h[colon + 1..].chars().all(|c| c.is_ascii_digit()) {
            h = &h[..colon];
        }
    }
    h.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(id: i64, hostname: &str) -> WebsiteView {
        WebsiteView {
            id: WebsiteId::new(id),
            name: format!("site {id}"),
            hostname: hostname.into(),
            page_title: String::new(),
            about_text: String::new(),
            primary_station: None,
            snow_enabled: false,
            snow_station: None,
            air_quality_enabled: false,
            air_quality_station: None,
            is_portal: false,
            radar_enabled: false,
            radar_token: String::new(),
        }
    }

    fn directory(sites: Vec<WebsiteView>) -> Directory {
        Directory::new(
            sites,
            Vec::new(),
            Listener {
                port: 8080,
                tls: false,
            },
        )
    }

    #[test]
    fn a_host_is_normalised_three_ways() {
        assert_eq!(normalise_host("localhost:18080"), "localhost");
        assert_eq!(normalise_host("Example.COM"), "example.com");
        assert_eq!(normalise_host("example.com."), "example.com");
        assert_eq!(normalise_host("EXAMPLE.COM.:443"), "example.com");
        assert_eq!(normalise_host("[::1]:8080"), "[::1]");
        assert_eq!(normalise_host("[::1]"), "[::1]");
        // Two trailing dots are not "a single trailing dot", and `trim_end_matches` takes
        // both -- which is more permissive than §6.3 says and cannot match any stored
        // hostname either way, because the schema forbids a stored trailing dot.
        assert_eq!(normalise_host("example.com.."), "example.com");
    }

    /// **DEV-2**: mixed case matches rather than falling through to somebody else's
    /// website, which is the defect this closes.
    #[test]
    fn a_mixed_case_host_matches_its_own_website() {
        let d = directory(vec![site(1, "first.test"), site(2, "second.test")]);
        let w = d
            .website_for_host("SECOND.test:18080")
            .expect("a website is configured");
        assert_eq!(w.id, WebsiteId::new(2), "and NOT the default website");
    }

    /// **Q-18**, in both of its arms.
    #[test]
    fn an_unmatched_host_gets_the_default_website() {
        let d = directory(vec![site(1, "first.test"), site(2, "second.test")]);
        assert_eq!(
            d.website_for_host("nobody.test").expect("a default").id,
            WebsiteId::new(1),
            "the first website by id"
        );
        let d = directory(vec![site(1, "first.test"), site(2, "")]);
        assert_eq!(
            d.website_for_host("nobody.test").expect("a default").id,
            WebsiteId::new(2),
            "an empty hostname wins over the first row"
        );
    }

    /// No website configured at all is the one case with no answer, which is the `503`
    /// the frozen contract gives.
    #[test]
    fn no_website_configured_resolves_to_nothing() {
        assert!(directory(Vec::new()).website_for_host("anything").is_none());
    }

    /// `/api/stations` excludes a station at the Go zero coordinates, and so does one with
    /// no coordinates at all.
    #[test]
    fn a_station_with_no_location_is_excluded() {
        let at = |lat, lon| StationView {
            id: 1,
            key: StationKey::new("k"),
            name: "k".into(),
            kind: "davis".into(),
            enabled: true,
            latitude: lat,
            longitude: lon,
            timezone: None,
        };
        assert!(at(Some(39.9), Some(-105.5)).has_location());
        assert!(!at(Some(0.0), Some(0.0)).has_location());
        assert!(!at(None, None).has_location());
        assert!(!at(Some(39.9), None).has_location());
        // A station on the equator at a non-zero longitude is a real location, which the
        // Go predicate also accepts -- it is the conjunction that excludes.
        assert!(at(Some(0.0), Some(-105.5)).has_location());
    }
}
