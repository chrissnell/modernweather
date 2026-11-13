//! What a read of the configuration returns — **with every secret omitted, not redacted**.
//!
//! W5 §1.10 narrows `GET /api/config` to `GET /api/v1/config` under `System → Advanced`
//! "with every secret omitted", and **D-67** drops the four plaintext secret columns the Go
//! schema carries. Omitted rather than redacted is the load-bearing word: a `"********"` in
//! a response is a value a client can round-trip back into a `PUT`, which is how a redacted
//! field becomes a stored literal eight asterisks long. So a credential is represented by a
//! *boolean* -- [`Credential`] -- and the form shows "set" or "not set" with a **Replace**
//! action beside it.
//!
//! **Every resource is a JSON object with named fields, never a bare array or a scalar**
//! (§4.4), so a field can be added without a version bump.
//!
//! # One read, many resources
//!
//! `GET /api/v1/stations`, `/websites`, `/storage`, `/services` and `/config` are all
//! projections of the same [`ConfigView`], which is one read of the configuration database
//! rather than five. That is not only an efficiency: W5 §2.2's generation has to be the same
//! integer on every resource a screen shows at once, and five independent reads could
//! straddle a write and report two.

use crate::display::Section;

/// Whether a credential is stored, which is the only thing any endpoint says about one.
///
/// A unit-like struct rather than a bare `bool` so that the schema -- and therefore the
/// generated TypeScript -- carries the reason, and so that a field of this type cannot be
/// confused with a feature toggle by a reader of either side.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
pub struct Credential {
    /// Whether something is stored. Never what.
    pub is_set: bool,
}

impl Credential {
    /// Whether the column held anything.
    pub fn of<T>(stored: Option<&T>) -> Self {
        Self {
            is_set: stored.is_some(),
        }
    }

    /// A credential that is definitely not stored.
    pub const NONE: Self = Self { is_set: false };
}

/// How a station is reached, as the operator configured it.
///
/// Flat rather than a tagged union over the seven `transport_kind`s, because the form is:
/// W5 §0.7 step 2 shows one "How is it connected?" picker and then the fields that kind
/// needs, and a discriminated union would make the client reconstruct the whole object to
/// change the picker. The schema's own `CHECK`s are what pair a kind with its columns.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TransportView {
    /// `serial`, `tcp_out`, `http_out`, `grpc_out`, `mqtt_sub`, `http_in` or `udp_in`.
    /// `None` is a station a peer forwards readings for -- inventory, not a component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The serial device path. "Serial port" keeps its name on screen (§0.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The baud rate. Keeps its name for §0.3's reason: the person holding the cable uses it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baud: Option<i64>,
    /// The host to dial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The port to dial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    /// The URL to poll or subscribe to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The HTTP method, where the transport takes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Whether this transport streams rather than polls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    /// The MQTT topic or the mount point, per kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The mount point an inbound transport listens on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    /// The consumer group, where the transport has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The CA bundle this transport verifies against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_ca_path: Option<String>,
    /// This end's certificate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    /// This end's key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,
    /// Whether a transport credential is stored. Never the credential.
    pub credential: Credential,
}

/// One weather station, as a read returns it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StationView {
    /// The domain identity, and the id every path segment uses.
    ///
    /// **Not renameable.** It is the stable suffix of this station's component ids and the
    /// key its history is stored under, so changing it would orphan the readings in the
    /// weather database -- a migration rather than an edit.
    pub key: String,
    /// The surrogate integer the **frozen** contract returns as `stations[].id`. Carried
    /// here because the UI deep-links the public site, which takes `?station_id=`.
    pub id: i64,
    /// What the operator calls it.
    pub name: String,
    /// The hardware type, in the frozen contract's spelling.
    pub kind: String,
    /// What the hardware type is called on screen (§0.2) -- "Snow depth sensor", not
    /// `snowgauge`.
    pub kind_display: String,
    /// The "Collecting readings" toggle.
    pub collecting: bool,
    /// How a frame's station identity is resolved: `fixed`, `from_path`, `from_query`,
    /// `from_payload` or `from_registration`.
    pub identity_kind: String,
    /// The identifier a non-`fixed` station presents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presented_key: Option<String>,
    /// Whether a protocol secret is stored for this station.
    pub protocol_secret: Credential,
    /// Degrees north.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// Degrees east.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    /// Metres. The column's name carries its unit so the conversion cannot be done twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
    /// An IANA time-zone name. `None` is a station whose day boundaries happen in UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The APRS callsign — public identity, printed on every packet, which is why it is on
    /// the station and not beside the passcode.
    ///
    /// **Returned**, unlike every credential on this type: a form that cannot prefill it
    /// writes `NULL` back on the next save, and `plan()` then refuses the whole
    /// configuration while APRS is paired — a station that cannot be edited at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_callsign: Option<String>,
    /// The vane's offset from north, in degrees.
    pub wind_dir_correction_degrees: i64,
    /// The distance a snow gauge reads with no snow under it, in millimetres.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snow_base_distance_mm: Option<f64>,
    /// How it is connected.
    pub transport: TransportView,
    /// "Which sensor is which", as the stored mapping (§1.6). Opaque here; the picker
    /// resolves templates client-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_mapping: Option<String>,
    /// How often a WeatherLink Live unit is polled, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_seconds: Option<i64>,
    /// Which services this station's readings are shared with, as the service keys.
    ///
    /// Read-only on the station page, which is **OQ-W5-4**'s resolution: the Go schema
    /// configures the same service in two places, and the UI's answer is one page per
    /// service plus this line, which links there.
    pub shared_with: Vec<String>,
}

/// One website, as a read returns it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct WebsiteView {
    /// The surrogate key.
    pub id: i64,
    /// What the operator calls it.
    pub name: String,
    /// The hostname it is served under. `""` is the **default** website, which an unmatched
    /// `Host` resolves to.
    pub hostname: String,
    /// The `<title>`.
    pub page_title: String,
    /// The "about this station" text. **HTML**, not plain text.
    pub about_station_html: String,
    /// The station the site shows when the request names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_station_key: Option<String>,
    /// Whether the snow panel is on.
    pub snow_enabled: bool,
    /// Which station the snow panel reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snow_station_key: Option<String>,
    /// Whether the air-quality panel is on.
    pub air_quality_enabled: bool,
    /// Which station the air-quality panel reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_quality_station_key: Option<String>,
    /// Whether this is the portal -- the one website `/api/stations` answers for.
    pub is_portal: bool,
    /// The App Store id for the `apple-itunes-app` tag. Empty removes the tag.
    pub apple_app_id: String,
    /// Whether the weather radar map is turned on.
    pub radar_enabled: bool,
    /// When the radar was registered.
    ///
    /// **The token itself is never returned** (§1.5): the screen shows "Registered on
    /// 4 March 2026" and a **Re-register** action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radar_registered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// This site's certificate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    /// This site's key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,
    /// Whether this site's certificate is the SNI fallback.
    pub is_default_tls: bool,
}

/// The weather database — "Data storage" on screen, never "TimescaleDB" (§0.2).
///
/// **Its absence is meaningful**, which is why this is an `Option` on [`ConfigView`] rather
/// than a row with empty fields: a node with no weather database has no `db:postgres` and
/// therefore no `writer:timescale` (**D-44**), and the screen says so.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StorageView {
    /// There is one weather database, so one id. Carried because the route is
    /// `PUT /api/v1/storage/{id}` -- §1.3's note that the Go surface has no by-id read for
    /// storage while stations and websites do is an inconsistency this surface removes by
    /// *having* the by-id route, not by pretending there can be two.
    pub id: i64,
    /// The host the URL names, for the screen. **Not the URL**: it carries a password.
    pub host: String,
    /// The database name the URL names.
    pub database: String,
    /// Whether a connection URL is stored. It is a credential, so that is all this says.
    pub connection: Credential,
}

/// One station this service is turned on for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PairingView {
    /// Which station.
    pub station_key: String,
    /// That station's identifier at the far end -- a Weather Underground or PWS Weather
    /// station id. A property of the pairing, not of the account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_station_id: Option<String>,
}

/// One upload service — a page under "Share my data".
///
/// There is no "add a controller" action and no list of controllers (§1.4): creating a
/// service is "turn on Weather Underground for this station", which is a `PUT` to this
/// resource.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ServiceView {
    /// `wunderground`, `pwsweather` or `aprs` -- the path segment.
    pub service: String,
    /// What it is called on screen: "Weather Underground", "PWS Weather", "APRS / CWOP".
    pub display: String,
    /// The account's name, which is also the stable suffix of its component ids.
    pub name: String,
    /// Whether it is on.
    pub enabled: bool,
    /// How often it uploads, in seconds. `None` is the kind's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<i64>,
    /// An endpoint override. `None` is the vendor's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    /// Whether an API key is stored.
    pub api_key: Credential,
    /// Whether an APRS passcode is stored. The **callsign** is on the station, because it is
    /// public identity printed on every packet.
    pub passcode: Credential,
    /// The APRS symbol table character.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_symbol_table: Option<String>,
    /// The APRS symbol code character.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_symbol_code: Option<String>,
    /// The APRS status comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_comment: Option<String>,
    /// The APRS-IS server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_server: Option<String>,
    /// `aprs-is`, `kiss-serial` or `kiss-tcp`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_transport: Option<String>,
    /// The KISS TNC's serial device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_kiss_serial_device: Option<String>,
    /// The KISS TNC's baud rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_kiss_serial_baud: Option<i64>,
    /// The KISS-over-TCP address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_kiss_tcp_address: Option<String>,
    /// The digipeater path. Keeps its name (§0.3): it is the word on the licence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_kiss_path: Option<String>,
    /// The KISS destination callsign.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aprs_kiss_destination: Option<String>,
    /// Which stations this service is on for.
    pub stations: Vec<PairingView>,
}

/// "Forecasts & alerts" — the Aeris account, named as a detail and not as the feature
/// (§0.2).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ForecastsView {
    /// Whether forecasts and alerts are being fetched.
    pub enabled: bool,
    /// The account's client id, which is not a secret on its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Whether the client secret is stored.
    pub client_secret: Credential,
    /// An endpoint override. `None` is the vendor's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    /// How often forecasts are refreshed, in seconds. `None` is the derived span/4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_interval_seconds: Option<i64>,
    /// Which stations get forecasts and alerts.
    pub stations: Vec<String>,
}

/// "Website & API" plus `System → Advanced`: where this node listens.
///
/// The `management` controller gets no page of its own (§1.4) — it is the thing the operator
/// is looking at. Its address, port and certificate are here, where changing the port shows
/// the warning and the new URL; its `auth_token` does not exist under §3.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ListenerView {
    /// The address the public site binds.
    pub listen_addr: String,
    /// The public HTTP port.
    pub http_port: u16,
    /// The public HTTPS port, when one is configured. `Some` makes the HTTP port a redirect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub https_port: Option<u16>,
    /// The "App & integration port" (§0.2) — where the phone app connects.
    pub grpc_port: u16,
    /// The address this API binds. Loopback by default (**A-W5-3**).
    pub management_listen_addr: String,
    /// The address it is **actually** bound to.
    ///
    /// Not always [`ListenerView::management_listen_addr`]: until implementation-plan.md
    /// **M7-2** there is no authentication, so a configured non-loopback address is narrowed
    /// to `127.0.0.1`. Reported rather than left to the log, because §1.4's "changing the
    /// port shows the new URL" is only honest if the URL is where the API is.
    pub management_bound_addr: String,
    /// The port this API binds.
    pub management_port: u16,
    /// This API's certificate.
    ///
    /// A **path**, not a credential, so it is returned — and it has to be, for
    /// [`StationView::aprs_callsign`]'s reason: a form that cannot prefill it writes `NULL`
    /// back, and an unrelated port change would silently turn off TLS on this API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_tls_cert_path: Option<String>,
    /// This API's key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_tls_key_path: Option<String>,
}

/// "This machine" — the node's own row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct NodeView {
    /// The node's name.
    pub name: String,
    /// The node's identity, as a UUID string.
    pub node_id: String,
    /// The deployment time zone, for components not scoped to a station.
    pub timezone: String,
    /// Where unsent readings are kept.
    pub spool_path: String,
    /// The spool's ceiling in bytes.
    pub spool_ceiling_bytes: i64,
    /// How far behind the spool is allowed to get, in seconds.
    pub spool_max_lag_seconds: i64,
}

/// A thing the configuration names, in the words a screen uses.
///
/// Returned beside a change so that the **server** renders the vocabulary once (§2.3) rather
/// than every client doing it: `display.section` and `display.href` are what let the UI link
/// a consequence to the page that owns it without knowing the component model.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Named {
    /// The internal id. Travels for keying, deep links and support; rendered raw only inside
    /// `System → Advanced` (**D-36**).
    pub id: String,
    /// What it is called on screen.
    pub display: String,
    /// Which part of the UI owns it.
    pub section: Section,
    /// Where to go to see it.
    pub href: String,
}

/// The whole configuration, with every secret omitted.
///
/// `GET /api/v1/config` under `System → Advanced` only (§1.10). No ordinary screen fetches
/// this; the per-resource routes are projections of it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ConfigView {
    /// The newest committed configuration generation (**D-53**). The integer every screen
    /// compares a component's `applied_generation` against.
    pub generation: u64,
    /// This node, if it has started before. `None` is a first start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeView>,
    /// Where this node listens.
    pub listener: ListenerView,
    /// Every weather station, including the disabled ones.
    pub stations: Vec<StationView>,
    /// Every website, in id order.
    pub websites: Vec<WebsiteView>,
    /// The weather database, if one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageView>,
    /// Every upload service that has an account row.
    pub services: Vec<ServiceView>,
    /// Forecasts and alerts, if an account is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forecasts: Option<ForecastsView>,
}

impl ConfigView {
    /// One station by key.
    pub fn station(&self, key: &str) -> Option<&StationView> {
        self.stations.iter().find(|s| s.key == key)
    }

    /// One website by id.
    pub fn website(&self, id: i64) -> Option<&WebsiteView> {
        self.websites.iter().find(|w| w.id == id)
    }

    /// One service by its path segment.
    pub fn service(&self, service: &str) -> Option<&ServiceView> {
        self.services.iter().find(|s| s.service == service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Omitted, not redacted.** A placeholder string is a value a client can round-trip
    /// into a `PUT`, which is how `"********"` becomes a stored password eight asterisks
    /// long. The only thing a response says about a credential is whether one is there.
    #[test]
    fn a_credential_serialises_as_whether_not_what() {
        let json = serde_json::to_string(&Credential::of(Some(&"hunter2".to_string())))
            .expect("the view serialises");
        assert_eq!(json, r#"{"is_set":true}"#);
        assert!(!json.contains("hunter2"));
        let none: Option<String> = None;
        assert_eq!(
            serde_json::to_string(&Credential::of(none.as_ref())).expect("serialises"),
            r#"{"is_set":false}"#
        );
    }

    /// `skip_serializing_if` on every optional field, so a client's `undefined` check is
    /// the same as its `null` check and a form does not have to distinguish them.
    #[test]
    fn an_absent_optional_field_is_absent_rather_than_null() {
        let t = TransportView::default();
        let json = serde_json::to_string(&t).expect("serialises");
        assert_eq!(json, r#"{"credential":{"is_set":false}}"#);
    }
}
