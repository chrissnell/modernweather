//! What a write sends, what it is checked against, and the one type a preview and a save
//! both take.
//!
//! # One change type, two verbs
//!
//! W5 §2.4 shows the preview's body as a partial configuration document
//! (`{ "stations": [ ... ] }`), and W5 §8.7 requires the **deletion** consequence list to
//! come from the same `POST /api/v1/config/preview` call. Those two cannot both be true of a
//! partial document: a document that omits a station is indistinguishable from a document
//! that was not asked about it, so "delete Backyard" has no representation.
//!
//! So the preview takes a [`Change`] — the same value the mutating route builds from its own
//! body — and the deviation is deliberate. What it buys is the property M7-1's
//! done-criterion is stated over: the preview and the save are **literally one code path**
//! that differs only in whether the surrounding transaction commits, so "a save's preview
//! names exactly the components that then restart" is not two implementations agreeing.
//! Recorded here rather than only in the pull request, because a reader of the schema will
//! notice the shape before they notice the argument.
//!
//! # Validation is layered, and each layer has an audience
//!
//! 1. **Here**, in the operator's words, on the field they typed into (§7.2, §8.5):
//!    "Enter a hostname or IP address." Blocks the save.
//! 2. **`plan()`**, which §7.2 makes a validation error too — `PlanError::RouteConflict`
//!    renders on the hostname field as "Another website already uses `weather.example.com`."
//!    Reached through the preview, which is the same call the Save button already makes.
//! 3. **The schema's own `CHECK`s**, which are the backstop both write paths share
//!    (`mw_store::config_admin`'s module documentation has that argument).
//!
//! A connectivity failure is **not** in that list and never becomes one (§7.2): saving a
//! database password the database rejects is a legitimate save, because the database may be
//! down or about to be reconfigured. The save succeeds and §2.4's banner ⑤ reports the
//! component failing.

use crate::error::FieldError;

/// Every `transport_kind` the schema admits, and the fields each one requires.
///
/// The list is the column's own `CHECK`. Duplicated here on purpose and only for the
/// *message*: the schema stays the authority on what is storable, and this is what turns
/// `CHECK constraint failed: stations_transport_serial` into "Choose a serial port and a
/// baud rate."
const TRANSPORTS: &[&str] = &[
    "serial", "tcp_out", "http_out", "grpc_out", "mqtt_sub", "http_in", "udp_in",
];

/// Every `stations.type` the schema admits.
const STATION_KINDS: &[&str] = &[
    "campbellscientific",
    "davis",
    "snowgauge",
    "ambient-customized",
    "airgradient",
    "weatherlink-live",
];

/// Every `identity_kind` the schema admits.
const IDENTITY_KINDS: &[&str] = &[
    "fixed",
    "from_path",
    "from_query",
    "from_payload",
    "from_registration",
];

/// Every upload service, as its path segment.
pub const SERVICES: &[&str] = &["wunderground", "pwsweather", "aprs"];

/// How a station is reached, as a form submits it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TransportInput {
    /// One of `serial`, `tcp_out`, `http_out`, `grpc_out`, `mqtt_sub`, `http_in`, `udp_in`.
    /// Omitted for a station a peer forwards readings for.
    #[serde(default)]
    pub kind: Option<String>,
    /// The serial port.
    #[serde(default)]
    pub device: Option<String>,
    /// The baud rate.
    #[serde(default)]
    pub baud: Option<i64>,
    /// The host to dial.
    #[serde(default)]
    pub host: Option<String>,
    /// The port to dial.
    #[serde(default)]
    pub port: Option<i64>,
    /// The URL to poll or subscribe to.
    #[serde(default)]
    pub url: Option<String>,
    /// The HTTP method.
    #[serde(default)]
    pub method: Option<String>,
    /// Whether this transport streams rather than polls.
    #[serde(default)]
    pub streaming: Option<bool>,
    /// The MQTT topic.
    #[serde(default)]
    pub topic: Option<String>,
    /// The mount point an inbound transport listens on.
    #[serde(default)]
    pub mount: Option<String>,
    /// The consumer group.
    #[serde(default)]
    pub group: Option<String>,
    /// The CA bundle to verify against.
    #[serde(default)]
    pub tls_ca_path: Option<String>,
    /// This end's certificate.
    #[serde(default)]
    pub tls_cert_path: Option<String>,
    /// This end's key.
    #[serde(default)]
    pub tls_key_path: Option<String>,
    /// A transport credential. **Omit to keep the stored one**; no response ever returns it,
    /// so treating absence as a clear would erase a working credential on every save.
    #[serde(default)]
    pub credential: Option<String>,
}

/// A weather station, as the "Add a station" and "Station settings" forms submit it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StationInput {
    /// The domain identity. Required on a create and ignored on an update — the path
    /// segment is what identifies the row, and the key is not renameable.
    #[serde(default)]
    pub key: Option<String>,
    /// What the operator calls it.
    pub name: String,
    /// The hardware type.
    pub kind: String,
    /// The "Collecting readings" toggle.
    #[serde(default)]
    pub collecting: bool,
    /// How a frame's station identity is resolved.
    pub identity_kind: String,
    /// The identifier a non-`fixed` station presents.
    #[serde(default)]
    pub presented_key: Option<String>,
    /// A new protocol secret. Omit to keep the stored one.
    #[serde(default)]
    pub protocol_secret: Option<String>,
    /// Degrees north.
    #[serde(default)]
    pub latitude: Option<f64>,
    /// Degrees east.
    #[serde(default)]
    pub longitude: Option<f64>,
    /// Metres.
    #[serde(default)]
    pub altitude_m: Option<f64>,
    /// An IANA time-zone name.
    #[serde(default)]
    pub timezone: Option<String>,
    /// The APRS callsign — public identity, printed on every packet.
    #[serde(default)]
    pub aprs_callsign: Option<String>,
    /// The vane's offset from north, in degrees.
    #[serde(default)]
    pub wind_dir_correction_degrees: i64,
    /// The distance a snow gauge reads with no snow under it, in millimetres.
    #[serde(default)]
    pub snow_base_distance_mm: Option<f64>,
    /// How it is connected.
    #[serde(default)]
    pub transport: TransportInput,
    /// "Which sensor is which", as the resolved mapping.
    #[serde(default)]
    pub sensor_mapping: Option<String>,
    /// How often a WeatherLink Live unit is polled, in seconds.
    #[serde(default)]
    pub poll_interval_seconds: Option<i64>,
}

/// A website, as its form submits it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct WebsiteInput {
    /// What the operator calls it.
    pub name: String,
    /// The hostname it is served under. `""` is the default website.
    #[serde(default)]
    pub hostname: String,
    /// The `<title>`.
    #[serde(default)]
    pub page_title: String,
    /// The "about this station" text, as HTML.
    #[serde(default)]
    pub about_station_html: String,
    /// The station the site shows when the request names none.
    #[serde(default)]
    pub primary_station_key: Option<String>,
    /// Whether the snow panel is on.
    #[serde(default)]
    pub snow_enabled: bool,
    /// Which station the snow panel reads.
    #[serde(default)]
    pub snow_station_key: Option<String>,
    /// Whether the air-quality panel is on.
    #[serde(default)]
    pub air_quality_enabled: bool,
    /// Which station the air-quality panel reads.
    #[serde(default)]
    pub air_quality_station_key: Option<String>,
    /// Whether this is the portal.
    #[serde(default)]
    pub is_portal: bool,
    /// The App Store id for the `apple-itunes-app` tag.
    #[serde(default)]
    pub apple_app_id: String,
    /// This site's certificate.
    #[serde(default)]
    pub tls_cert_path: Option<String>,
    /// This site's key.
    #[serde(default)]
    pub tls_key_path: Option<String>,
    /// Whether this site's certificate is the SNI fallback.
    #[serde(default)]
    pub is_default_tls: bool,
}

/// The weather database's connection, as its form submits it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StorageInput {
    /// The PostgreSQL URL. A credential, so it is write-only: no response returns it.
    pub url: String,
}

/// One station a service is turned on for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PairingInput {
    /// Which station.
    pub station_key: String,
    /// That station's identifier at the far end.
    #[serde(default)]
    pub remote_station_id: Option<String>,
}

/// One upload service, as its page submits it — the account and its stations together.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ServiceInput {
    /// The account's name, which is the stable suffix of its component ids.
    pub name: String,
    /// Whether it is on.
    #[serde(default)]
    pub enabled: bool,
    /// How often it uploads, in seconds. Omit for the service's default.
    #[serde(default)]
    pub interval_seconds: Option<i64>,
    /// An endpoint override.
    #[serde(default)]
    pub api_endpoint: Option<String>,
    /// A new API key. Omit to keep the stored one.
    #[serde(default)]
    pub api_key: Option<String>,
    /// A new APRS passcode. Omit to keep the stored one.
    #[serde(default)]
    pub passcode: Option<String>,
    /// The APRS symbol table character.
    #[serde(default)]
    pub aprs_symbol_table: Option<String>,
    /// The APRS symbol code character.
    #[serde(default)]
    pub aprs_symbol_code: Option<String>,
    /// The APRS status comment.
    #[serde(default)]
    pub aprs_comment: Option<String>,
    /// The APRS-IS server.
    #[serde(default)]
    pub aprs_server: Option<String>,
    /// `aprs-is`, `kiss-serial` or `kiss-tcp`.
    #[serde(default)]
    pub aprs_transport: Option<String>,
    /// The KISS TNC's serial device.
    #[serde(default)]
    pub aprs_kiss_serial_device: Option<String>,
    /// The KISS TNC's baud rate.
    #[serde(default)]
    pub aprs_kiss_serial_baud: Option<i64>,
    /// The KISS-over-TCP address.
    #[serde(default)]
    pub aprs_kiss_tcp_address: Option<String>,
    /// The digipeater path.
    #[serde(default)]
    pub aprs_kiss_path: Option<String>,
    /// The KISS destination callsign.
    #[serde(default)]
    pub aprs_kiss_destination: Option<String>,
    /// Which stations this service is on for. Replaces the whole set.
    #[serde(default)]
    pub stations: Vec<PairingInput>,
}

/// Forecasts and alerts, as their page submits them.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ForecastsInput {
    /// Whether to fetch forecasts and alerts.
    #[serde(default)]
    pub enabled: bool,
    /// The account's client id.
    #[serde(default)]
    pub client_id: Option<String>,
    /// A new client secret. Omit to keep the stored one.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// An endpoint override.
    #[serde(default)]
    pub api_endpoint: Option<String>,
    /// How often to refresh, in seconds.
    #[serde(default)]
    pub refresh_interval_seconds: Option<i64>,
    /// Which stations get forecasts and alerts.
    #[serde(default)]
    pub stations: Vec<String>,
}

/// Where this node listens, as `System → Advanced` submits it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ListenerInput {
    /// The address the public site binds.
    pub listen_addr: String,
    /// The public HTTP port.
    pub http_port: u16,
    /// The public HTTPS port.
    #[serde(default)]
    pub https_port: Option<u16>,
    /// The "App & integration port".
    pub grpc_port: u16,
    /// The address this API binds.
    pub management_listen_addr: String,
    /// The port this API binds.
    pub management_port: u16,
    /// This API's certificate.
    #[serde(default)]
    pub management_tls_cert_path: Option<String>,
    /// This API's key.
    #[serde(default)]
    pub management_tls_key_path: Option<String>,
}

/// A radar registration, as "Turn on the weather radar map" submits it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RadarInput {
    /// The token the registration returned. Write-only: no response returns it (§1.5).
    pub token: String,
}

/// Everything an operator can change, as one discriminated union.
///
/// Both `POST /api/v1/config/preview` and every mutating route construct one of these, which
/// is what makes the preview and the save one code path. The discriminant is `change` so
/// that `openapi-typescript` emits a union the client narrows on rather than an object with
/// every field optional.
// The station variant is an order of magnitude larger than `DeleteStorage`, which
// `clippy::large_enum_variant` is right to notice and wrong to want fixed here: boxing it
// would put an indirection on the one type `utoipa` derives the request body's schema from,
// and a `Box<StationInput>` in a `oneOf` is a worse generated client for a value that is
// constructed once per request and never in a collection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    /// Add or update a weather station.
    PutStation {
        /// Which station. Absent on a create, where [`StationInput::key`] carries it.
        #[serde(default)]
        key: Option<String>,
        /// The form's values.
        station: StationInput,
    },
    /// Delete a weather station.
    DeleteStation {
        /// Which station.
        key: String,
    },
    /// Clear a station's stored protocol secret.
    ClearStationSecret {
        /// Which station.
        key: String,
    },
    /// Add or update a website.
    PutWebsite {
        /// Which website. Absent on a create.
        #[serde(default)]
        id: Option<i64>,
        /// The form's values.
        website: WebsiteInput,
    },
    /// Delete a website.
    DeleteWebsite {
        /// Which website.
        id: i64,
    },
    /// Record a radar registration.
    RegisterRadar {
        /// Which website.
        id: i64,
        /// The registration.
        radar: RadarInput,
    },
    /// Withdraw a radar registration.
    UnregisterRadar {
        /// Which website.
        id: i64,
    },
    /// Set the weather database's connection.
    PutStorage {
        /// The connection.
        storage: StorageInput,
    },
    /// Remove the weather database.
    DeleteStorage,
    /// Turn an upload service on, off, or change its settings.
    PutService {
        /// Which service: `wunderground`, `pwsweather` or `aprs`.
        service: String,
        /// The page's values.
        settings: ServiceInput,
    },
    /// Stop sharing with an upload service, and forget its account.
    DeleteService {
        /// Which service.
        service: String,
    },
    /// Turn forecasts and alerts on, off, or change their settings.
    PutForecasts {
        /// The page's values.
        forecasts: ForecastsInput,
    },
    /// Stop fetching forecasts and forget the account.
    DeleteForecasts,
    /// Change where this node listens.
    PutListener {
        /// The form's values.
        listener: ListenerInput,
    },
}

impl Change {
    /// The one-word name a log line uses. Internal (**D-36**).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Change::PutStation { .. } => "put_station",
            Change::DeleteStation { .. } => "delete_station",
            Change::ClearStationSecret { .. } => "clear_station_secret",
            Change::PutWebsite { .. } => "put_website",
            Change::DeleteWebsite { .. } => "delete_website",
            Change::RegisterRadar { .. } => "register_radar",
            Change::UnregisterRadar { .. } => "unregister_radar",
            Change::PutStorage { .. } => "put_storage",
            Change::DeleteStorage => "delete_storage",
            Change::PutService { .. } => "put_service",
            Change::DeleteService { .. } => "delete_service",
            Change::PutForecasts { .. } => "put_forecasts",
            Change::DeleteForecasts => "delete_forecasts",
            Change::PutListener { .. } => "put_listener",
        }
    }

    /// Everything wrong with this change that can be seen without the configuration.
    ///
    /// Empty means "nothing a form can tell you" — not "valid": the two checks that need
    /// more than the body are `plan()`'s and the schema's, and both are reached by applying
    /// the change inside a transaction.
    pub fn problems(&self) -> Vec<FieldError> {
        match self {
            Change::PutStation { key, station } => {
                let mut v = station.problems();
                let key = key.as_deref().or(station.key.as_deref());
                match key {
                    None | Some("") => v.push(FieldError::new(
                        "key",
                        "Give this station a short identifier, like `backyard`.",
                    )),
                    Some(k) if !is_key(k) => v.push(FieldError::new(
                        "key",
                        "Use lower-case letters, digits, `-` and `_` only.",
                    )),
                    Some(_) => {}
                }
                v
            }
            Change::PutWebsite { website, .. } => website.problems(),
            Change::PutStorage { storage } => storage.problems(),
            Change::PutService { service, settings } => {
                let mut v = settings.problems(service);
                if !SERVICES.contains(&service.as_str()) {
                    v.push(FieldError::new(
                        "service",
                        "That is not a service this daemon can share with.",
                    ));
                }
                v
            }
            Change::DeleteService { service } if !SERVICES.contains(&service.as_str()) => {
                vec![FieldError::new(
                    "service",
                    "That is not a service this daemon can share with.",
                )]
            }
            Change::PutForecasts { forecasts } => forecasts.problems(),
            Change::PutListener { listener } => listener.problems(),
            Change::RegisterRadar { radar, .. } if radar.token.trim().is_empty() => {
                vec![FieldError::new(
                    "radar.token",
                    "Paste the token the radar registration gave you.",
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Consequences worth saying out loud that do **not** block the save (§8.5).
    ///
    /// A warning explains a consequence; an error says what to type. The operator may know
    /// something the validator does not, which is why these are separate — today's UI
    /// blocks on both and explains neither.
    pub fn warnings(&self) -> Vec<String> {
        match self {
            // §2.4's own example.
            Change::PutService { service, settings } => {
                let mut v = Vec::new();
                if let Some(s) = settings.interval_seconds {
                    if service == "wunderground" && s < 60 {
                        v.push(
                            "Weather Underground asks for no more than one upload a minute; \
                             shorter intervals may be rejected."
                                .to_string(),
                        );
                    }
                }
                if settings.enabled && settings.stations.is_empty() {
                    v.push(
                        "This service is on but no station is shared with it, so nothing \
                         will be uploaded."
                            .to_string(),
                    );
                }
                v
            }
            Change::DeleteStorage => vec![
                "Readings already saved in the weather database are kept; this only stops \
                 new ones being written."
                    .to_string(),
            ],
            Change::DeleteStation { .. } => {
                vec!["Readings already saved in the weather database are kept.".to_string()]
            }
            Change::PutForecasts { forecasts }
                if forecasts.enabled && forecasts.stations.is_empty() =>
            {
                vec![
                    "Forecasts are on but no station is selected, so no forecast will be \
                     fetched."
                        .to_string(),
                ]
            }
            _ => Vec::new(),
        }
    }
}

/// A usable component-id suffix: the characters `ComponentId` admits in a segment.
fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// `one of a, b, c` — for a message that has to name a closed set.
fn one_of(values: &[&str]) -> String {
    values.join(", ")
}

impl StationInput {
    fn problems(&self) -> Vec<FieldError> {
        let mut v = Vec::new();
        if self.name.trim().is_empty() {
            v.push(FieldError::new("name", "Give this station a name."));
        }
        if !STATION_KINDS.contains(&self.kind.as_str()) {
            v.push(FieldError::new(
                "kind",
                format!("Choose the kind of station: {}.", one_of(STATION_KINDS)),
            ));
        }
        if !IDENTITY_KINDS.contains(&self.identity_kind.as_str()) {
            v.push(FieldError::new(
                "identity_kind",
                format!(
                    "Choose how this station identifies itself: {}.",
                    one_of(IDENTITY_KINDS)
                ),
            ));
        }
        if self.identity_kind != "fixed"
            && self
                .presented_key
                .as_deref()
                .is_none_or(|k| k.trim().is_empty())
        {
            v.push(FieldError::new(
                "presented_key",
                "Enter the identifier this station sends.",
            ));
        }
        if let Some(lat) = self.latitude {
            if !(-90.0..=90.0).contains(&lat) {
                v.push(FieldError::new(
                    "latitude",
                    "Enter a latitude between -90 and 90.",
                ));
            }
        }
        if let Some(lon) = self.longitude {
            if !(-180.0..=180.0).contains(&lon) {
                v.push(FieldError::new(
                    "longitude",
                    "Enter a longitude between -180 and 180.",
                ));
            }
        }
        if !(-360..=360).contains(&self.wind_dir_correction_degrees) {
            v.push(FieldError::new(
                "wind_dir_correction_degrees",
                "Enter a correction between -360 and 360 degrees.",
            ));
        }
        if let Some(mm) = self.snow_base_distance_mm {
            if mm <= 0.0 {
                v.push(FieldError::new(
                    "snow_base_distance_mm",
                    "Enter how far the sensor is above bare ground, in millimetres.",
                ));
            }
        }
        v.extend(self.transport.problems());
        v
    }
}

impl TransportInput {
    fn problems(&self) -> Vec<FieldError> {
        let Some(kind) = self.kind.as_deref() else {
            return Vec::new();
        };
        if !TRANSPORTS.contains(&kind) {
            return vec![FieldError::new(
                "transport.kind",
                format!(
                    "Choose how this station is connected: {}.",
                    one_of(TRANSPORTS)
                ),
            )];
        }
        let mut v = Vec::new();
        // The schema's per-kind `CHECK`s, turned into the sentence the field needs. Only
        // the required-field half: the "and nothing else" half is the schema's, because a
        // second exclusion list here would be the defect that one exists to prevent.
        match kind {
            "serial" => {
                if self.device.as_deref().unwrap_or("").is_empty() {
                    v.push(FieldError::new(
                        "transport.device",
                        "Choose the serial port the station is plugged into.",
                    ));
                }
                if self.baud.is_none() {
                    v.push(FieldError::new(
                        "transport.baud",
                        "Choose the baud rate the station uses.",
                    ));
                }
            }
            "tcp_out" => {
                if self.host.as_deref().unwrap_or("").is_empty() {
                    v.push(FieldError::new(
                        "transport.host",
                        "Enter a hostname or IP address.",
                    ));
                }
                if !self.port.is_some_and(|p| (1..=65535).contains(&p)) {
                    v.push(FieldError::new(
                        "transport.port",
                        "Enter a port between 1 and 65535.",
                    ));
                }
            }
            // **`streaming` belongs to `grpc_out` alone**, and the two are separated here
            // rather than shared because the schema separates them: W2a §4a.3 is
            // `HttpOut { auth }` and `GrpcOut { streaming: bool }`, and the configuration
            // database carries that as `CHECK (transport_streaming IS NULL OR
            // COALESCE(transport_kind, '') = 'grpc_out')`.
            //
            // Requiring it for `http_out` as well made every `http_out` station
            // **unsatisfiable through this API**: omitting it was a 422 naming
            // `transport.streaming`, and supplying it was that `CHECK` failing with
            // SQLite's own wording. Two of the six station types the schema admits --
            // `weatherlink-live` and `snowgauge` -- are `http_out`, so neither could be
            // created or edited at all. Found by implementation-plan.md M7-3's second
            // Playwright flow, which is the first thing to create one.
            "http_out" => {
                if self.url.as_deref().unwrap_or("").is_empty() {
                    v.push(FieldError::new(
                        "transport.url",
                        "Enter the address to poll.",
                    ));
                }
                if self.streaming.is_some() {
                    // Refused here rather than left to the `CHECK`, so the operator reads
                    // a sentence about their form instead of a constraint name.
                    v.push(FieldError::new(
                        "transport.streaming",
                        "A connection over the web is always polled, so this does not apply.",
                    ));
                }
            }
            "grpc_out" => {
                if self.url.as_deref().unwrap_or("").is_empty() {
                    v.push(FieldError::new(
                        "transport.url",
                        "Enter the address to connect to.",
                    ));
                }
                if self.streaming.is_none() {
                    v.push(FieldError::new(
                        "transport.streaming",
                        "Say whether this connection stays open or is polled.",
                    ));
                }
            }
            "mqtt_sub" => {
                if self.url.as_deref().unwrap_or("").is_empty() {
                    v.push(FieldError::new(
                        "transport.url",
                        "Enter the address of the broker.",
                    ));
                }
                if self.topic.as_deref().unwrap_or("").is_empty() {
                    v.push(FieldError::new(
                        "transport.topic",
                        "Enter the topic the station publishes to.",
                    ));
                }
            }
            // `http_in` and `udp_in` are inbound: the station reaches us, so there is
            // nothing to dial and the listener is the one the node already has.
            _ => {}
        }
        v
    }
}

impl WebsiteInput {
    fn problems(&self) -> Vec<FieldError> {
        let mut v = Vec::new();
        if self.name.trim().is_empty() {
            v.push(FieldError::new("name", "Give this website a name."));
        }
        // The hostname's three `CHECK`s are the three steps W3 §6.3 normalises a `Host`
        // header by, so a stored value that has not had all three applied is a row that can
        // never match. Each one gets its own sentence, because each is a different typo.
        let h = &self.hostname;
        if h != &h.to_lowercase() {
            v.push(FieldError::new(
                "hostname",
                "Enter the hostname in lower case.",
            ));
        }
        if h.contains(':') {
            v.push(FieldError::new(
                "hostname",
                "Leave the port off the hostname — the ports are set under System.",
            ));
        }
        if h.ends_with('.') {
            v.push(FieldError::new(
                "hostname",
                "Leave the trailing dot off the hostname.",
            ));
        }
        if self.snow_enabled && self.snow_station_key.as_deref().is_none_or(str::is_empty) {
            v.push(FieldError::new(
                "snow_station_key",
                "Choose which station the snow panel reads.",
            ));
        }
        if self.air_quality_enabled
            && self
                .air_quality_station_key
                .as_deref()
                .is_none_or(str::is_empty)
        {
            v.push(FieldError::new(
                "air_quality_station_key",
                "Choose which station the air-quality panel reads.",
            ));
        }
        match (&self.tls_cert_path, &self.tls_key_path) {
            (Some(_), None) => v.push(FieldError::new(
                "tls_key_path",
                "A certificate needs its private key as well.",
            )),
            (None, Some(_)) => v.push(FieldError::new(
                "tls_cert_path",
                "A private key needs its certificate as well.",
            )),
            _ => {}
        }
        if self.is_default_tls && self.tls_cert_path.is_none() {
            v.push(FieldError::new(
                "is_default_tls",
                "Only a website that has its own certificate can be the fallback.",
            ));
        }
        v
    }
}

impl StorageInput {
    fn problems(&self) -> Vec<FieldError> {
        // The column's own `GLOB`. The message names both spellings because `libpq` accepts
        // both and an operator who pasted one from somewhere else should not have to guess.
        if !(self.url.starts_with("postgres://") || self.url.starts_with("postgresql://")) {
            return vec![FieldError::new(
                "url",
                "Enter a connection address starting `postgres://` or `postgresql://`.",
            )];
        }
        Vec::new()
    }
}

impl ServiceInput {
    fn problems(&self, service: &str) -> Vec<FieldError> {
        let mut v = Vec::new();
        if !is_key(&self.name) {
            v.push(FieldError::new(
                "name",
                "Use a short identifier: lower-case letters, digits, `-` and `_`.",
            ));
        }
        if let Some(s) = self.interval_seconds {
            if s <= 0 {
                v.push(FieldError::new(
                    "interval_seconds",
                    "Enter how many seconds to wait between uploads.",
                ));
            }
        }
        for (field, value) in [
            ("aprs_symbol_table", &self.aprs_symbol_table),
            ("aprs_symbol_code", &self.aprs_symbol_code),
        ] {
            if let Some(c) = value {
                if c.chars().count() != 1 {
                    v.push(FieldError::new(field, "Enter a single character."));
                }
            }
        }
        if service == "aprs" {
            match self.aprs_transport.as_deref() {
                None if self.enabled => v.push(FieldError::new(
                    "aprs_transport",
                    "Choose how to send: APRS-IS, a KISS TNC on a serial port, or KISS over TCP.",
                )),
                Some("kiss-serial") => {
                    if self.aprs_kiss_serial_device.is_none() {
                        v.push(FieldError::new(
                            "aprs_kiss_serial_device",
                            "Choose the serial port the TNC is on.",
                        ));
                    }
                    if self.aprs_kiss_serial_baud.is_none() {
                        v.push(FieldError::new(
                            "aprs_kiss_serial_baud",
                            "Choose the baud rate the TNC uses.",
                        ));
                    }
                }
                Some("kiss-tcp") => {
                    if self.aprs_kiss_tcp_address.is_none() {
                        v.push(FieldError::new(
                            "aprs_kiss_tcp_address",
                            "Enter the address of the KISS TNC.",
                        ));
                    }
                }
                Some("aprs-is") | None => {}
                Some(_) => v.push(FieldError::new(
                    "aprs_transport",
                    "Choose one of: aprs-is, kiss-serial, kiss-tcp.",
                )),
            }
        }
        // W7 D-38's arity, stated on the field rather than left to `PlanError::UploaderArity`:
        // the plan's refusal names a component the operator has never seen.
        if self.stations.len() > 1 {
            v.push(FieldError::new(
                "stations",
                "This service uploads for one station. Add a second account to share another.",
            ));
        }
        for (i, p) in self.stations.iter().enumerate() {
            if p.station_key.trim().is_empty() {
                v.push(FieldError::new(
                    format!("stations[{i}].station_key"),
                    "Choose a station.",
                ));
            }
            // Weather Underground and PWS Weather both key uploads by a remote station id,
            // and an upload with none is accepted by this daemon and rejected by theirs --
            // §8.5's own worked example of an error that blocks rather than warns.
            if matches!(service, "wunderground" | "pwsweather")
                && p.remote_station_id
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                v.push(FieldError::new(
                    format!("stations[{i}].remote_station_id"),
                    "Enter the station ID this service gave you.",
                ));
            }
        }
        v
    }
}

impl ForecastsInput {
    fn problems(&self) -> Vec<FieldError> {
        let mut v = Vec::new();
        // The `aeris_account` table's own `CHECK`, which its migration argues for: an
        // enabled account with no credential is a component that fails every tick forever
        // with a 401, and W2a Decision 30 forbids giving up on it.
        if self.enabled
            && self
                .client_id
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
        {
            v.push(FieldError::new(
                "client_id",
                "Enter the client ID from your forecast account.",
            ));
        }
        if let Some(s) = self.refresh_interval_seconds {
            if s <= 0 {
                v.push(FieldError::new(
                    "refresh_interval_seconds",
                    "Enter how many seconds to wait between refreshes.",
                ));
            }
        }
        v
    }
}

impl ListenerInput {
    fn problems(&self) -> Vec<FieldError> {
        let mut v = Vec::new();
        for (field, addr) in [
            ("listen_addr", &self.listen_addr),
            ("management_listen_addr", &self.management_listen_addr),
        ] {
            if addr.parse::<std::net::IpAddr>().is_err() {
                v.push(FieldError::new(
                    field,
                    "Enter an IP address to listen on, such as 0.0.0.0 or 127.0.0.1.",
                ));
            }
        }
        // The `listener` row's own cross-column `CHECK`: the four ports must differ.
        // Stated here because the constraint's message names none of them.
        let mut seen: Vec<(&str, u16)> = vec![
            ("http_port", self.http_port),
            ("grpc_port", self.grpc_port),
            ("management_port", self.management_port),
        ];
        if let Some(p) = self.https_port {
            seen.push(("https_port", p));
        }
        for (i, (field, port)) in seen.iter().enumerate() {
            if *port == 0 {
                v.push(FieldError::new(*field, "Enter a port between 1 and 65535."));
            }
            if seen[..i].iter().any(|(_, other)| other == port) {
                v.push(FieldError::new(
                    *field,
                    "Each port has to be different from the others.",
                ));
            }
        }
        match (
            &self.management_tls_cert_path,
            &self.management_tls_key_path,
        ) {
            (Some(_), None) => v.push(FieldError::new(
                "management_tls_key_path",
                "A certificate needs its private key as well.",
            )),
            (None, Some(_)) => v.push(FieldError::new(
                "management_tls_cert_path",
                "A private key needs its certificate as well.",
            )),
            _ => {}
        }
        v
    }
}

#[cfg(test)]
mod tests {
    /// The defect this test exists for made two of the six station types the schema
    /// admits -- `weatherlink-live` and `snowgauge`, both `http_out` -- impossible to
    /// create or edit through this API: omitting `streaming` was a `422` and supplying it
    /// was the configuration database's `CHECK (transport_streaming IS NULL OR
    /// COALESCE(transport_kind, '') = 'grpc_out')` failing with SQLite's own wording.
    ///
    /// W2a §4a.3 is the authority and is unambiguous: `HttpOut { auth }` has no streaming
    /// field and `GrpcOut { streaming: bool }` does.
    #[test]
    fn streaming_belongs_to_grpc_out_and_not_to_http_out() {
        let polled = TransportInput {
            kind: Some("http_out".to_string()),
            url: Some("http://192.168.1.51".to_string()),
            ..TransportInput::default()
        };
        assert!(
            polled.problems().is_empty(),
            "an http_out station with a url is storable: {:?}",
            polled.problems()
        );

        let with_streaming = TransportInput {
            streaming: Some(false),
            ..polled.clone()
        };
        let problems = with_streaming.problems();
        let fields: Vec<&str> = problems.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(
            fields,
            vec!["transport.streaming"],
            "an http_out station carrying `streaming` is refused HERE, with a sentence, \
             rather than by the CHECK with a constraint name"
        );

        // And the converse: `grpc_out` still requires it, which is what §4a.3's
        // trigger-implication table reads at plan time.
        let grpc = TransportInput {
            kind: Some("grpc_out".to_string()),
            url: Some("https://peer.example:443".to_string()),
            ..TransportInput::default()
        };
        let problems = grpc.problems();
        let fields: Vec<&str> = problems.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(fields, vec!["transport.streaming"]);
        let grpc = TransportInput {
            streaming: Some(true),
            ..grpc
        };
        assert!(grpc.problems().is_empty());
    }

    use super::*;

    fn station() -> StationInput {
        StationInput {
            key: Some("backyard".into()),
            name: "Backyard".into(),
            kind: "davis".into(),
            identity_kind: "fixed".into(),
            collecting: true,
            transport: TransportInput {
                kind: Some("serial".into()),
                device: Some("/dev/ttyS0".into()),
                baud: Some(19200),
                ..TransportInput::default()
            },
            ..StationInput::default()
        }
    }

    /// Nothing wrong with a complete form, which is the case every other assertion is a
    /// deviation from.
    #[test]
    fn a_complete_station_has_no_problems() {
        let c = Change::PutStation {
            key: None,
            station: station(),
        };
        assert_eq!(c.problems(), Vec::new());
    }

    /// §8.5: every message says **what to type**, and lands on the field. The assertion is
    /// on the field path, because that is the half a banner cannot do.
    #[test]
    fn a_serial_station_with_no_port_names_the_port_field() {
        let mut s = station();
        s.transport.device = None;
        s.transport.baud = None;
        let problems = Change::PutStation {
            key: None,
            station: s,
        }
        .problems();
        let fields: Vec<&str> = problems.iter().map(|p| p.field.as_str()).collect();
        assert_eq!(fields, ["transport.device", "transport.baud"]);
        assert!(problems[0].message.ends_with('.'), "{:?}", problems[0]);
    }

    /// The hostname's three stored-shape rules are three different typos and get three
    /// different sentences — a single "invalid hostname" would leave the operator guessing
    /// which of the three they made.
    #[test]
    fn each_hostname_rule_has_its_own_sentence() {
        let w = |hostname: &str| {
            Change::PutWebsite {
                id: Some(1),
                website: WebsiteInput {
                    name: "Home".into(),
                    hostname: hostname.into(),
                    ..WebsiteInput::default()
                },
            }
            .problems()
        };
        assert!(w("weather.example.com").is_empty());
        assert!(w("Weather.example.com")[0].message.contains("lower case"));
        assert!(w("weather.example.com:8080")[0].message.contains("port"));
        assert!(w("weather.example.com.")[0]
            .message
            .contains("trailing dot"));
    }

    /// A credential field is **omitted to keep**, never "sent empty to keep": `null` and a
    /// missing key both deserialise to `None`, and an empty string is a real new value the
    /// operator typed. Asserted because getting this backwards erases a working credential
    /// on every save of the page that holds it.
    #[test]
    fn an_omitted_credential_deserialises_as_absent() {
        let s: ServiceInput =
            serde_json::from_str(r#"{"name":"backyard"}"#).expect("the body parses");
        assert_eq!(s.api_key, None);
        let s: ServiceInput =
            serde_json::from_str(r#"{"name":"backyard","api_key":""}"#).expect("the body parses");
        assert_eq!(s.api_key.as_deref(), Some(""));
    }

    /// §2.4's own warning, and the thing that makes it a warning: it does not appear among
    /// the problems, so it cannot block the save.
    #[test]
    fn a_short_upload_interval_warns_and_does_not_block() {
        let c = Change::PutService {
            service: "wunderground".into(),
            settings: ServiceInput {
                name: "backyard".into(),
                enabled: true,
                interval_seconds: Some(30),
                stations: vec![PairingInput {
                    station_key: "backyard".into(),
                    remote_station_id: Some("KSTATION1".into()),
                }],
                ..ServiceInput::default()
            },
        };
        assert_eq!(c.problems(), Vec::new());
        assert_eq!(c.warnings().len(), 1);
        assert!(c.warnings()[0].contains("one upload a minute"));
    }

    /// The change union is discriminated on `change`, which is what lets the generated
    /// TypeScript be a narrowable union rather than an object with every field optional.
    #[test]
    fn the_union_round_trips_through_its_discriminant() {
        let body = r#"{"change":"delete_station","key":"backyard"}"#;
        let c: Change = serde_json::from_str(body).expect("the body parses");
        assert_eq!(c.as_str(), "delete_station");
        assert_eq!(serde_json::to_string(&c).expect("serialises"), body);
    }

    /// The four ports have to differ, and the refusal names the field rather than the
    /// constraint — `listener`'s cross-column `CHECK` mentions none of them.
    #[test]
    fn two_equal_ports_name_the_second_one() {
        let c = Change::PutListener {
            listener: ListenerInput {
                listen_addr: "0.0.0.0".into(),
                http_port: 8080,
                https_port: None,
                grpc_port: 50051,
                management_listen_addr: "127.0.0.1".into(),
                management_port: 8080,
                management_tls_cert_path: None,
                management_tls_key_path: None,
            },
        };
        let problems = c.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].field, "management_port");
    }
}
