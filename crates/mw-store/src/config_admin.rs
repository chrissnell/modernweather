//! The configuration store's **write** half: one statement set per thing an operator can
//! change, and nothing else.
//!
//! The read half is [`crate::config_db`]. This module is the other direction, and it exists
//! because `ConfigAdmin` is the only way configuration changes at run time (W2a §2.5: there
//! is no `reload`, a committed write bumps the generation and the supervisor reconciles the
//! plan diff).
//!
//! # What this module is not
//!
//! **It is not the management API's handler set, and it does not own the transaction.** The
//! API's types are `mw_api_mgmt`'s -- `ci/layers.toml` puts that crate one layer above this
//! one and forbids it from naming `mw-store` at all, so a [`Change`] here is the binary's
//! translation of a request body rather than the body itself. That copy is the same one
//! `crates/modernweather/src/config.rs` already makes column-into-field for every registrar,
//! and it buys the same property: this crate cannot grow a route and that crate cannot grow
//! a query.
//!
//! **It does not decide what a change means.** [`apply`] takes a connection, so the caller
//! chooses whether the surrounding transaction commits or rolls back -- which is exactly how
//! `POST /api/v1/config/preview` and a real save come from one code path (W5 §2.4). The
//! preview rolls back; the save commits. Nothing else differs, which is what makes
//! implementation-plan.md **M7-1**'s "a save's preview names exactly the components that then
//! restart" a structural property rather than an assertion two code paths have to keep
//! agreeing on.
//!
//! # Why the schema is the validator
//!
//! Every table in `migrations/config/` carries its constraints as `CHECK`s and foreign keys,
//! because **D-8** makes the schema the schema: `station_service`'s composite foreign key is
//! what stops a pairing naming one service while pointing at an account of another, and
//! `uploader`'s exhaustive per-kind exclusion lists are what stop a `pws_api_key` landing on
//! an APRS row. Re-deriving those rules in Rust would be a second copy that drifts, so this
//! module writes the columns and lets the constraints refuse -- [`ConfigAdminError::Rejected`]
//! carries the constraint's own text. The checks the *operator* needs stated in their own
//! words are separately in `mw_api_mgmt`, on the field, per W5 §7.2; this is the backstop
//! under them, and it is the one both write paths share.

use sqlx::sqlite::SqliteConnection;
use sqlx::Row;

/// Why a configuration write could not be applied.
///
/// Typed per module and preserving its cause chain (**D-34**).
#[derive(Debug, thiserror::Error)]
pub enum ConfigAdminError {
    /// The database refused the row: a `CHECK`, a `UNIQUE` or a foreign key.
    ///
    /// Separate from [`ConfigAdminError::Query`] because the two have different audiences. A
    /// rejection is about the *values* an operator supplied and is rendered as a `422` with
    /// the constraint named; a query failure is about the database and is a `500`.
    #[error("the configuration database refused the change to `{table}`: {detail}")]
    Rejected {
        /// Which table.
        table: &'static str,
        /// SQLite's own words, which name the constraint.
        detail: String,
    },

    /// The statement failed for a reason that is not the operator's.
    #[error("the configuration database could not be written: {what}")]
    Query {
        /// Which write, in words an operator can act on.
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// The change names a row that is not there.
    #[error("there is no {what} called `{id}`")]
    NoSuchRow {
        /// What kind of row.
        what: &'static str,
        /// What was asked for.
        id: String,
    },
}

/// Classify a `sqlx` error as the operator's fault or the database's.
///
/// SQLite reports a `CHECK`, a `UNIQUE` and a foreign key as `SQLITE_CONSTRAINT`, which
/// `sqlx` exposes through `DatabaseError::is_check_violation` and friends; anything else --
/// a closed connection, a disk error, a syntax mistake in this file -- is not something the
/// request can fix.
fn classify(table: &'static str, what: &'static str, source: sqlx::Error) -> ConfigAdminError {
    if let sqlx::Error::Database(db) = &source {
        if db.is_check_violation() || db.is_unique_violation() || db.is_foreign_key_violation() {
            return ConfigAdminError::Rejected {
                table,
                detail: db.message().to_string(),
            };
        }
    }
    ConfigAdminError::Query { what, source }
}

/// One `stations` row, as a write supplies it.
///
/// Every mutable column, including the ones a given `transport_kind` must leave NULL: the
/// table's `CHECK`s are what pair them up, and a partial struct here would mean a transport
/// change that left the previous kind's columns behind. `station_key` is the identity and is
/// not mutable -- renaming it would rename the station's component ids and orphan its
/// history in the weather database, which is a migration rather than an edit (W5 §1.2).
#[derive(Debug, Clone, Default)]
pub struct StationWrite {
    /// `station_key` -- which row this is.
    pub station_key: String,
    /// `id`, the surrogate integer, on an **insert only**.
    ///
    /// `None` is "let SQLite choose", which is every ordinary create. [`snapshot`] fills it
    /// and [`restore`] replays it, because the column reaches the **frozen** contract as
    /// `stations[].id` and `/api/stations`' `id` -- so a revert that renumbered the stations
    /// would change what a frozen response means (W1, **D-37**).
    pub id: Option<i64>,
    /// `name`.
    pub name: String,
    /// `type`.
    pub kind: String,
    /// `enabled` -- the "Collecting readings" toggle.
    pub enabled: bool,
    /// `owning_node_id`, as the 16 raw bytes. Only an insert supplies it; an update leaves
    /// the column alone, because a station does not change owner by being edited.
    pub owning_node_id: Option<Vec<u8>>,
    /// `identity_kind`.
    pub identity_kind: String,
    /// `presented_key`.
    pub presented_key: Option<String>,
    /// `ingest_secret_hash` -- already an Argon2id PHC string.
    ///
    /// Hashed by [`crate::registry::hash_ingest_secret`] before it reaches here, and
    /// `None` means "leave whatever is stored alone" rather than "clear it": a `PUT` of a
    /// station form that does not retype the secret must not erase it, because no endpoint
    /// returns one to retype (**D-67**). Clearing is [`Change::ClearIngestSecret`].
    pub ingest_secret_hash: Option<String>,
    /// `latitude`.
    pub latitude: Option<f64>,
    /// `longitude`.
    pub longitude: Option<f64>,
    /// `altitude_m`.
    pub altitude_m: Option<f64>,
    /// `timezone`.
    pub timezone: Option<String>,
    /// `aprs_callsign` -- public identity, so on the station and not beside the passcode.
    pub aprs_callsign: Option<String>,
    /// `wind_dir_correction_degrees`.
    pub wind_dir_correction_degrees: i64,
    /// `snow_base_distance_mm`.
    pub snow_base_distance_mm: Option<f64>,
    /// `transport_kind`.
    pub transport_kind: Option<String>,
    /// `transport_dev`.
    pub transport_dev: Option<String>,
    /// `transport_baud`.
    pub transport_baud: Option<i64>,
    /// `transport_host`.
    pub transport_host: Option<String>,
    /// `transport_port`.
    pub transport_port: Option<i64>,
    /// `transport_url`.
    pub transport_url: Option<String>,
    /// `transport_method`.
    pub transport_method: Option<String>,
    /// `transport_auth` -- a credential, with [`StationWrite::ingest_secret_hash`]'s
    /// leave-it-alone semantics for the same reason.
    pub transport_auth: Option<String>,
    /// `transport_streaming`.
    pub transport_streaming: Option<bool>,
    /// `transport_mount`.
    pub transport_mount: Option<String>,
    /// `transport_group`.
    pub transport_group: Option<String>,
    /// `transport_topic`.
    pub transport_topic: Option<String>,
    /// `transport_tls_ca_path`.
    pub transport_tls_ca_path: Option<String>,
    /// `transport_tls_cert_path`.
    pub transport_tls_cert_path: Option<String>,
    /// `transport_tls_key_path`.
    pub transport_tls_key_path: Option<String>,
    /// `wll_sensor_mapping` -- "which sensor is which", as the stored JSON.
    pub wll_sensor_mapping: Option<String>,
    /// `wll_poll_interval_seconds`.
    pub wll_poll_interval_seconds: Option<i64>,
}

/// One `website` row, as a write supplies it.
///
/// `radar_token` and `radar_registered_at` are **not** here: they are write-only and set by
/// registration alone (W5 §1.5), which is [`Change::RegisterRadar`].
#[derive(Debug, Clone, Default)]
pub struct WebsiteWrite {
    /// `id`, or `None` to let SQLite choose one.
    ///
    /// `Some` means **this row, by id, creating it with that id if it is not there** --
    /// which is what [`restore`] needs and what [`StationWrite::id`] means one table over.
    /// The *route* is what refuses a `PUT` to an id nothing has: `mw_api_mgmt` looks the id
    /// up and answers `404` before the change is built, because "edit this" and "put this
    /// back" are different intentions and only one of them may invent a row.
    pub id: Option<i64>,
    /// `name`.
    pub name: String,
    /// `hostname` -- lower-case, port-free, no trailing dot. `""` is the default website.
    pub hostname: String,
    /// `page_title`.
    pub page_title: String,
    /// `about_station_html`.
    pub about_station_html: String,
    /// `primary_station_key`.
    pub primary_station_key: Option<String>,
    /// `snow_enabled`.
    pub snow_enabled: bool,
    /// `snow_station_key`.
    pub snow_station_key: Option<String>,
    /// `air_quality_enabled`.
    pub air_quality_enabled: bool,
    /// `air_quality_station_key`.
    pub air_quality_station_key: Option<String>,
    /// `is_portal`.
    pub is_portal: bool,
    /// `apple_app_id`.
    pub apple_app_id: String,
    /// `tls_cert_path`.
    pub tls_cert_path: Option<String>,
    /// `tls_key_path`.
    pub tls_key_path: Option<String>,
    /// `is_default_tls`.
    pub is_default_tls: bool,
}

/// One `uploader` row and its one `station_service` pairing, as a write supplies it.
///
/// The pairing travels with the account because the screen does: W5 §1.4 presents **one page
/// per service** that edits both halves together, which is the whole point of **D-66**'s
/// split -- the Go schema configures the same service in two places and the UI resolves that
/// by never showing the operator the seam.
#[derive(Debug, Clone, Default)]
pub struct ServiceWrite {
    /// `uploader.kind`, which is also the path segment the route is keyed by.
    pub kind: String,
    /// `uploader.id`, on an **insert only**. [`StationWrite::id`]'s reason, one table over:
    /// `station_service`'s composite foreign key names it.
    pub id: Option<i64>,
    /// `uploader.name`.
    pub name: String,
    /// `uploader.enabled`.
    pub enabled: bool,
    /// `uploader.interval_seconds`.
    pub interval_seconds: Option<i64>,
    /// `uploader.api_endpoint`.
    pub api_endpoint: Option<String>,
    /// `uploader.api_key`, with [`StationWrite::ingest_secret_hash`]'s leave-it-alone
    /// semantics: no endpoint returns a credential, so a form that was not retyped must not
    /// erase one.
    pub api_key: Option<String>,
    /// `uploader.aprs_passcode`, same semantics.
    pub aprs_passcode: Option<String>,
    /// `uploader.aprs_symbol_table`.
    pub aprs_symbol_table: Option<String>,
    /// `uploader.aprs_symbol_code`.
    pub aprs_symbol_code: Option<String>,
    /// `uploader.aprs_comment`.
    pub aprs_comment: Option<String>,
    /// `uploader.aprs_server`.
    pub aprs_server: Option<String>,
    /// `uploader.aprs_transport`.
    pub aprs_transport: Option<String>,
    /// `uploader.aprs_kiss_serial_device`.
    pub aprs_kiss_serial_device: Option<String>,
    /// `uploader.aprs_kiss_serial_baud`.
    pub aprs_kiss_serial_baud: Option<i64>,
    /// `uploader.aprs_kiss_tcp_address`.
    pub aprs_kiss_tcp_address: Option<String>,
    /// `uploader.aprs_kiss_path`.
    pub aprs_kiss_path: Option<String>,
    /// `uploader.aprs_kiss_destination`.
    pub aprs_kiss_destination: Option<String>,
    /// The stations this service is turned on for, and each one's identifier at the far end.
    ///
    /// Replaces the service's whole pairing set, because the screen does: "share Backyard
    /// and not Front Garden" is one state, and applying it as an add plus a remove would
    /// leave an intermediate configuration the supervisor could reconcile against.
    pub pairings: Vec<Pairing>,
}

/// One `station_service` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// `station_key`.
    pub station_key: String,
    /// `remote_station_id` -- this station's id at the far end.
    pub remote_station_id: Option<String>,
}

/// The `aeris_account` row and its pairings, as a write supplies it.
#[derive(Debug, Clone, Default)]
pub struct AerisWrite {
    /// `enabled`.
    pub enabled: bool,
    /// `client_id`.
    pub client_id: Option<String>,
    /// `client_secret`, with the leave-it-alone semantics of every other credential here.
    pub client_secret: Option<String>,
    /// `api_endpoint`.
    pub api_endpoint: Option<String>,
    /// `refresh_interval_seconds`.
    pub refresh_interval_seconds: Option<i64>,
    /// Which stations get forecasts and alerts.
    pub stations: Vec<String>,
}

/// The `listener` row, as a write supplies it.
///
/// `management_listen_addr` and `management_port` are here and the management API's
/// certificate paths are too, which is W5 §1.4's narrowing made real: the Go management
/// controller is editable through its own API *including its `auth_token`*, which is how an
/// operator locks themselves out in one `PUT`. Under §3 the token does not exist, and what
/// is left -- the address, the port and the certificate -- lives on `System → Advanced`
/// with the port change warning and the new URL.
#[derive(Debug, Clone, Default)]
pub struct ListenerWrite {
    /// `listen_addr`.
    pub listen_addr: String,
    /// `http_port`.
    pub http_port: i64,
    /// `https_port`.
    pub https_port: Option<i64>,
    /// `grpc_port` -- "App & integration port" on screen (§0.2).
    pub grpc_port: i64,
    /// `management_listen_addr`.
    pub management_listen_addr: String,
    /// `management_port`.
    pub management_port: i64,
    /// `management_tls_cert_path`.
    pub management_tls_cert_path: Option<String>,
    /// `management_tls_key_path`.
    pub management_tls_key_path: Option<String>,
}

/// Everything an operator can change, as one closed enum.
///
/// Closed rather than a string plus a JSON blob, for `Capability`'s reason one level up: a
/// change is a compile-time fact about this program, and a reader of `apply` can see the
/// whole of what the management API is able to do to the configuration database.
// `StationWrite` is thirty-odd columns and `DeleteStorage` is a unit, so the variants
// differ by an order of magnitude -- which `clippy::large_enum_variant` is right to notice
// and wrong to want boxed. One of these is constructed per request, never in a collection,
// and the indirection would buy nothing but a second allocation on the one path where the
// transaction is already open.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Change {
    /// Insert or update a station by key.
    PutStation(StationWrite),
    /// Delete a station. `ON DELETE SET NULL` on the three `website` columns and
    /// `ON DELETE CASCADE` on `station_service` are what make the consequences W5 §8.7
    /// shows the operator real rather than dangling references.
    DeleteStation(String),
    /// Clear a station's stored protocol secret.
    ClearIngestSecret(String),
    /// Insert or update a website.
    PutWebsite(WebsiteWrite),
    /// Delete a website.
    DeleteWebsite(i64),
    /// Record a radar registration: the token and the moment.
    RegisterRadar {
        /// Which website.
        website: i64,
        /// The token the registration returned.
        token: String,
    },
    /// Withdraw a radar registration.
    UnregisterRadar(i64),
    /// Set the weather database's URL. One row, so this is both insert and update.
    PutWeatherDatabase(String),
    /// Remove the weather database row. **Meaningful**: a node with no row has no
    /// `db:postgres` and therefore no `writer:timescale` (**D-44**).
    DeleteWeatherDatabase,
    /// Insert or update one upload service and replace its pairings.
    PutService(ServiceWrite),
    /// Delete an upload service by kind -- "Stop sharing with Weather Underground".
    DeleteService(String),
    /// Insert or update the Aeris account and replace its pairings.
    PutAeris(AerisWrite),
    /// Delete the Aeris account -- "Stop fetching forecasts".
    DeleteAeris,
    /// Replace the forecast pairings **without** touching the account row.
    ///
    /// Not reachable from a route: [`snapshot`] emits it for the one state the schema admits
    /// and `PutAeris` cannot express -- stations paired with forecasts on a node that has no
    /// `aeris_account` row yet.
    PairForecasts(Vec<String>),
    /// Update the listener row.
    PutListener(ListenerWrite),
}

impl Change {
    /// The one-word name a log line and a metric label use.
    ///
    /// Internal: an operator never sees it (**D-36**), and the management API's own response
    /// carries the user-facing rendering instead.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Change::PutStation(_) => "put_station",
            Change::DeleteStation(_) => "delete_station",
            Change::ClearIngestSecret(_) => "clear_ingest_secret",
            Change::PutWebsite(_) => "put_website",
            Change::DeleteWebsite(_) => "delete_website",
            Change::RegisterRadar { .. } => "register_radar",
            Change::UnregisterRadar(_) => "unregister_radar",
            Change::PutWeatherDatabase(_) => "put_weather_database",
            Change::DeleteWeatherDatabase => "delete_weather_database",
            Change::PutService(_) => "put_service",
            Change::DeleteService(_) => "delete_service",
            Change::PutAeris(_) => "put_aeris",
            Change::DeleteAeris => "delete_aeris",
            Change::PairForecasts(_) => "pair_forecasts",
            Change::PutListener(_) => "put_listener",
        }
    }
}

/// Apply one change to the connection's current transaction.
///
/// **Does not commit.** The caller owns the transaction, which is what lets the preview and
/// the save be one code path: see the module documentation.
pub async fn apply(conn: &mut SqliteConnection, change: &Change) -> Result<(), ConfigAdminError> {
    match change {
        Change::PutStation(s) => put_station(conn, s).await,
        Change::DeleteStation(key) => {
            let n = exec(
                conn,
                sqlx::query("DELETE FROM stations WHERE station_key = ?").bind(key),
                "stations",
                "deleting a station",
            )
            .await?;
            missing(n, "weather station", key)
        }
        Change::ClearIngestSecret(key) => {
            let n = exec(
                conn,
                sqlx::query(
                    "UPDATE stations SET ingest_secret_hash = NULL, \
                     updated_at = unixepoch() WHERE station_key = ?",
                )
                .bind(key),
                "stations",
                "clearing a station's protocol secret",
            )
            .await?;
            missing(n, "weather station", key)
        }
        Change::PutWebsite(w) => put_website(conn, w).await,
        Change::DeleteWebsite(id) => {
            let n = exec(
                conn,
                sqlx::query("DELETE FROM website WHERE id = ?").bind(id),
                "website",
                "deleting a website",
            )
            .await?;
            missing(n, "website", &id.to_string())
        }
        Change::RegisterRadar { website, token } => {
            let n = exec(
                conn,
                sqlx::query(
                    "UPDATE website SET radar_enabled = 1, radar_token = ?, \
                     radar_registered_at = unixepoch(), updated_at = unixepoch() \
                     WHERE id = ?",
                )
                .bind(token)
                .bind(website),
                "website",
                "recording a radar registration",
            )
            .await?;
            missing(n, "website", &website.to_string())
        }
        Change::UnregisterRadar(id) => {
            let n = exec(
                conn,
                sqlx::query(
                    "UPDATE website SET radar_enabled = 0, radar_token = '', \
                     radar_registered_at = NULL, updated_at = unixepoch() WHERE id = ?",
                )
                .bind(id),
                "website",
                "withdrawing a radar registration",
            )
            .await?;
            missing(n, "website", &id.to_string())
        }
        Change::PutWeatherDatabase(url) => {
            exec(
                conn,
                sqlx::query(
                    "INSERT INTO weather_database (id, url) VALUES (1, ?) \
                     ON CONFLICT (id) DO UPDATE SET url = excluded.url, \
                     updated_at = unixepoch()",
                )
                .bind(url),
                "weather_database",
                "setting the weather database's URL",
            )
            .await?;
            Ok(())
        }
        Change::DeleteWeatherDatabase => {
            exec(
                conn,
                sqlx::query("DELETE FROM weather_database WHERE id = 1"),
                "weather_database",
                "removing the weather database",
            )
            .await?;
            Ok(())
        }
        Change::PutService(s) => put_service(conn, s).await,
        Change::DeleteService(kind) => {
            // `station_service`'s composite foreign key is `ON DELETE CASCADE`, so the
            // pairings go with the account: "stop sharing with Weather Underground" leaves
            // no pairing pointing at an account that is gone.
            let n = exec(
                conn,
                sqlx::query("DELETE FROM uploader WHERE kind = ?").bind(kind),
                "uploader",
                "deleting an upload service",
            )
            .await?;
            missing(n, "service", kind)
        }
        Change::PutAeris(a) => put_aeris(conn, a).await,
        Change::DeleteAeris => {
            exec(
                conn,
                sqlx::query("DELETE FROM station_service WHERE service = 'aeris'"),
                "station_service",
                "removing the Aeris pairings",
            )
            .await?;
            exec(
                conn,
                sqlx::query("DELETE FROM aeris_account WHERE id = 1"),
                "aeris_account",
                "removing the Aeris account",
            )
            .await?;
            Ok(())
        }
        Change::PairForecasts(stations) => {
            exec(
                conn,
                sqlx::query("DELETE FROM station_service WHERE service = 'aeris'"),
                "station_service",
                "replacing the forecast pairings",
            )
            .await?;
            for key in stations {
                exec(
                    conn,
                    sqlx::query(
                        "INSERT INTO station_service (station_key, service, uploader_id) \
                         VALUES (?, 'aeris', NULL)",
                    )
                    .bind(key),
                    "station_service",
                    "pairing a station with forecasts and alerts",
                )
                .await?;
            }
            Ok(())
        }
        Change::PutListener(l) => put_listener(conn, l).await,
    }
}

/// `Ok` for a statement that matched a row, [`ConfigAdminError::NoSuchRow`] for one that did
/// not.
///
/// Reported rather than accepted: a `200` for a `DELETE` that deleted nothing tells an
/// operator their change took effect when the row they named does not exist, which is the
/// class of lie W5 §2.1 is about.
fn missing(rows: u64, what: &'static str, id: &str) -> Result<(), ConfigAdminError> {
    if rows == 0 {
        return Err(ConfigAdminError::NoSuchRow {
            what,
            id: id.to_string(),
        });
    }
    Ok(())
}

/// Run one statement, classifying its failure, and report how many rows it touched.
async fn exec<'q>(
    conn: &mut SqliteConnection,
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    table: &'static str,
    what: &'static str,
) -> Result<u64, ConfigAdminError> {
    let done = query
        .execute(&mut *conn)
        .await
        .map_err(|e| classify(table, what, e))?;
    Ok(done.rows_affected())
}

async fn put_station(
    conn: &mut SqliteConnection,
    s: &StationWrite,
) -> Result<(), ConfigAdminError> {
    let existing: Option<(Option<String>, Option<String>)> = sqlx::query(
        "SELECT ingest_secret_hash, transport_auth FROM stations WHERE station_key = ?",
    )
    .bind(&s.station_key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| classify("stations", "reading the station being edited", e))?
    .map(|r| (r.get("ingest_secret_hash"), r.get("transport_auth")));

    // A credential the form did not retype keeps the stored value. `None` means "not
    // supplied", never "clear it": no endpoint returns a secret to retype (**D-67**), so
    // treating absence as a clear would erase a working credential on every save of the
    // page that holds it. Clearing is an action of its own.
    let (kept_secret, kept_auth) = existing.clone().unwrap_or((None, None));
    let secret = s.ingest_secret_hash.clone().or(kept_secret);
    let auth = s.transport_auth.clone().or(kept_auth);

    if existing.is_some() {
        exec(
            conn,
            sqlx::query(
                "UPDATE stations SET name = ?, type = ?, enabled = ?, identity_kind = ?, \
                 presented_key = ?, ingest_secret_hash = ?, latitude = ?, longitude = ?, \
                 altitude_m = ?, timezone = ?, aprs_callsign = ?, \
                 wind_dir_correction_degrees = ?, snow_base_distance_mm = ?, \
                 transport_kind = ?, transport_dev = ?, transport_baud = ?, \
                 transport_host = ?, transport_port = ?, transport_url = ?, \
                 transport_method = ?, transport_auth = ?, transport_streaming = ?, \
                 transport_mount = ?, transport_group = ?, transport_topic = ?, \
                 transport_tls_ca_path = ?, transport_tls_cert_path = ?, \
                 transport_tls_key_path = ?, wll_sensor_mapping = ?, \
                 wll_poll_interval_seconds = ?, updated_at = unixepoch() \
                 WHERE station_key = ?",
            )
            .bind(&s.name)
            .bind(&s.kind)
            .bind(s.enabled)
            .bind(&s.identity_kind)
            .bind(&s.presented_key)
            .bind(&secret)
            .bind(s.latitude)
            .bind(s.longitude)
            .bind(s.altitude_m)
            .bind(&s.timezone)
            .bind(&s.aprs_callsign)
            .bind(s.wind_dir_correction_degrees)
            .bind(s.snow_base_distance_mm)
            .bind(&s.transport_kind)
            .bind(&s.transport_dev)
            .bind(s.transport_baud)
            .bind(&s.transport_host)
            .bind(s.transport_port)
            .bind(&s.transport_url)
            .bind(&s.transport_method)
            .bind(&auth)
            .bind(s.transport_streaming)
            .bind(&s.transport_mount)
            .bind(&s.transport_group)
            .bind(&s.transport_topic)
            .bind(&s.transport_tls_ca_path)
            .bind(&s.transport_tls_cert_path)
            .bind(&s.transport_tls_key_path)
            .bind(&s.wll_sensor_mapping)
            .bind(s.wll_poll_interval_seconds)
            .bind(&s.station_key),
            "stations",
            "updating a station",
        )
        .await?;
        return Ok(());
    }

    // `owning_node_id` is `NOT NULL CHECK (length(...) = 16)` and has no default, so an
    // insert needs one. A node that has never started has no identity to put there, which
    // is a refusal naming the cause rather than a constraint violation naming a column.
    let owner = s
        .owning_node_id
        .as_ref()
        .ok_or_else(|| ConfigAdminError::NoSuchRow {
            what: "node identity, which a new station needs an owner from",
            id: s.station_key.clone(),
        })?;
    exec(
        conn,
        sqlx::query(
            // `id` is bound rather than omitted, and a NULL means "choose one": SQLite
            // assigns `rowid` for a NULL `INTEGER PRIMARY KEY`, which is what makes one
            // statement serve both an ordinary create and `restore`'s replay of a row whose
            // surrogate id reaches the frozen contract.
            "INSERT INTO stations (id, station_key, owning_node_id, name, type, enabled, \
             identity_kind, presented_key, ingest_secret_hash, latitude, longitude, \
             altitude_m, timezone, aprs_callsign, wind_dir_correction_degrees, \
             snow_base_distance_mm, transport_kind, transport_dev, transport_baud, \
             transport_host, transport_port, transport_url, transport_method, \
             transport_auth, transport_streaming, transport_mount, transport_group, \
             transport_topic, transport_tls_ca_path, transport_tls_cert_path, \
             transport_tls_key_path, wll_sensor_mapping, wll_poll_interval_seconds) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
             ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(s.id)
        .bind(&s.station_key)
        .bind(owner)
        .bind(&s.name)
        .bind(&s.kind)
        .bind(s.enabled)
        .bind(&s.identity_kind)
        .bind(&s.presented_key)
        .bind(&secret)
        .bind(s.latitude)
        .bind(s.longitude)
        .bind(s.altitude_m)
        .bind(&s.timezone)
        .bind(&s.aprs_callsign)
        .bind(s.wind_dir_correction_degrees)
        .bind(s.snow_base_distance_mm)
        .bind(&s.transport_kind)
        .bind(&s.transport_dev)
        .bind(s.transport_baud)
        .bind(&s.transport_host)
        .bind(s.transport_port)
        .bind(&s.transport_url)
        .bind(&s.transport_method)
        .bind(&auth)
        .bind(s.transport_streaming)
        .bind(&s.transport_mount)
        .bind(&s.transport_group)
        .bind(&s.transport_topic)
        .bind(&s.transport_tls_ca_path)
        .bind(&s.transport_tls_cert_path)
        .bind(&s.transport_tls_key_path)
        .bind(&s.wll_sensor_mapping)
        .bind(s.wll_poll_interval_seconds),
        "stations",
        "adding a station",
    )
    .await?;
    Ok(())
}

async fn put_website(
    conn: &mut SqliteConnection,
    w: &WebsiteWrite,
) -> Result<(), ConfigAdminError> {
    // An explicit id is an upsert, for the reason the field's own documentation gives. The
    // column list names `id` so the one statement serves both, and `ON CONFLICT (id)` is
    // what makes the replay idempotent against a row the delete did not reach.
    match w.id {
        Some(id) => {
            exec(
                conn,
                sqlx::query(
                    "INSERT INTO website (id, name, hostname, page_title, \
                     about_station_html, primary_station_key, snow_enabled, \
                     snow_station_key, air_quality_enabled, air_quality_station_key, \
                     is_portal, apple_app_id, tls_cert_path, tls_key_path, is_default_tls) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (id) DO UPDATE SET name = excluded.name, \
                     hostname = excluded.hostname, page_title = excluded.page_title, \
                     about_station_html = excluded.about_station_html, \
                     primary_station_key = excluded.primary_station_key, \
                     snow_enabled = excluded.snow_enabled, \
                     snow_station_key = excluded.snow_station_key, \
                     air_quality_enabled = excluded.air_quality_enabled, \
                     air_quality_station_key = excluded.air_quality_station_key, \
                     is_portal = excluded.is_portal, \
                     apple_app_id = excluded.apple_app_id, \
                     tls_cert_path = excluded.tls_cert_path, \
                     tls_key_path = excluded.tls_key_path, \
                     is_default_tls = excluded.is_default_tls, \
                     updated_at = unixepoch()",
                )
                .bind(id)
                .bind(&w.name)
                .bind(&w.hostname)
                .bind(&w.page_title)
                .bind(&w.about_station_html)
                .bind(&w.primary_station_key)
                .bind(w.snow_enabled)
                .bind(&w.snow_station_key)
                .bind(w.air_quality_enabled)
                .bind(&w.air_quality_station_key)
                .bind(w.is_portal)
                .bind(&w.apple_app_id)
                .bind(&w.tls_cert_path)
                .bind(&w.tls_key_path)
                .bind(w.is_default_tls),
                "website",
                "writing a website",
            )
            .await?;
            Ok(())
        }
        None => {
            exec(
                conn,
                sqlx::query(
                    "INSERT INTO website (name, hostname, page_title, about_station_html, \
                     primary_station_key, snow_enabled, snow_station_key, \
                     air_quality_enabled, air_quality_station_key, is_portal, \
                     apple_app_id, tls_cert_path, tls_key_path, is_default_tls) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&w.name)
                .bind(&w.hostname)
                .bind(&w.page_title)
                .bind(&w.about_station_html)
                .bind(&w.primary_station_key)
                .bind(w.snow_enabled)
                .bind(&w.snow_station_key)
                .bind(w.air_quality_enabled)
                .bind(&w.air_quality_station_key)
                .bind(w.is_portal)
                .bind(&w.apple_app_id)
                .bind(&w.tls_cert_path)
                .bind(&w.tls_key_path)
                .bind(w.is_default_tls),
                "website",
                "adding a website",
            )
            .await?;
            Ok(())
        }
    }
}

async fn put_service(
    conn: &mut SqliteConnection,
    s: &ServiceWrite,
) -> Result<(), ConfigAdminError> {
    let existing: Option<(i64, Option<String>, Option<String>)> =
        sqlx::query("SELECT id, api_key, aprs_passcode FROM uploader WHERE kind = ?")
            .bind(&s.kind)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| classify("uploader", "reading the service being edited", e))?
            .map(|r| (r.get("id"), r.get("api_key"), r.get("aprs_passcode")));

    let (kept_key, kept_passcode) = existing
        .as_ref()
        .map(|(_, k, p)| (k.clone(), p.clone()))
        .unwrap_or((None, None));
    let api_key = s.api_key.clone().or(kept_key);
    let passcode = s.aprs_passcode.clone().or(kept_passcode);

    let id = match existing.as_ref().map(|(id, _, _)| *id) {
        Some(id) => {
            exec(
                conn,
                sqlx::query(
                    "UPDATE uploader SET name = ?, enabled = ?, interval_seconds = ?, \
                     api_endpoint = ?, api_key = ?, aprs_passcode = ?, \
                     aprs_symbol_table = ?, aprs_symbol_code = ?, aprs_comment = ?, \
                     aprs_server = ?, aprs_transport = ?, aprs_kiss_serial_device = ?, \
                     aprs_kiss_serial_baud = ?, aprs_kiss_tcp_address = ?, \
                     aprs_kiss_path = ?, aprs_kiss_destination = ?, \
                     updated_at = unixepoch() WHERE id = ?",
                )
                .bind(&s.name)
                .bind(s.enabled)
                .bind(s.interval_seconds)
                .bind(&s.api_endpoint)
                .bind(&api_key)
                .bind(&passcode)
                .bind(&s.aprs_symbol_table)
                .bind(&s.aprs_symbol_code)
                .bind(&s.aprs_comment)
                .bind(&s.aprs_server)
                .bind(&s.aprs_transport)
                .bind(&s.aprs_kiss_serial_device)
                .bind(s.aprs_kiss_serial_baud)
                .bind(&s.aprs_kiss_tcp_address)
                .bind(&s.aprs_kiss_path)
                .bind(&s.aprs_kiss_destination)
                .bind(id),
                "uploader",
                "updating an upload service",
            )
            .await?;
            id
        }
        None => {
            let row = sqlx::query(
                "INSERT INTO uploader (id, kind, name, enabled, interval_seconds, \
                 api_endpoint, api_key, aprs_passcode, aprs_symbol_table, \
                 aprs_symbol_code, aprs_comment, aprs_server, aprs_transport, \
                 aprs_kiss_serial_device, aprs_kiss_serial_baud, aprs_kiss_tcp_address, \
                 aprs_kiss_path, aprs_kiss_destination) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 RETURNING id",
            )
            .bind(s.id)
            .bind(&s.kind)
            .bind(&s.name)
            .bind(s.enabled)
            .bind(s.interval_seconds)
            .bind(&s.api_endpoint)
            .bind(&api_key)
            .bind(&passcode)
            .bind(&s.aprs_symbol_table)
            .bind(&s.aprs_symbol_code)
            .bind(&s.aprs_comment)
            .bind(&s.aprs_server)
            .bind(&s.aprs_transport)
            .bind(&s.aprs_kiss_serial_device)
            .bind(s.aprs_kiss_serial_baud)
            .bind(&s.aprs_kiss_tcp_address)
            .bind(&s.aprs_kiss_path)
            .bind(&s.aprs_kiss_destination)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| classify("uploader", "adding an upload service", e))?;
            row.get("id")
        }
    };

    // The pairing set is replaced rather than merged, for the reason
    // `ServiceWrite::pairings` gives: a delete-then-insert inside the transaction is one
    // state change from the supervisor's point of view, because it never sees the middle.
    exec(
        conn,
        sqlx::query("DELETE FROM station_service WHERE service = ?").bind(&s.kind),
        "station_service",
        "replacing a service's stations",
    )
    .await?;
    for p in &s.pairings {
        exec(
            conn,
            sqlx::query(
                "INSERT INTO station_service (station_key, service, uploader_id, \
                 remote_station_id) VALUES (?, ?, ?, ?)",
            )
            .bind(&p.station_key)
            .bind(&s.kind)
            .bind(id)
            .bind(&p.remote_station_id),
            "station_service",
            "pairing a station with a service",
        )
        .await?;
    }
    Ok(())
}

async fn put_aeris(conn: &mut SqliteConnection, a: &AerisWrite) -> Result<(), ConfigAdminError> {
    let kept: Option<String> = sqlx::query("SELECT client_secret FROM aeris_account WHERE id = 1")
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| classify("aeris_account", "reading the Aeris account", e))?
        .and_then(|r| r.get("client_secret"));
    let secret = a.client_secret.clone().or(kept);

    exec(
        conn,
        sqlx::query(
            "INSERT INTO aeris_account (id, enabled, client_id, client_secret, \
             api_endpoint, refresh_interval_seconds) VALUES (1, ?, ?, ?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET enabled = excluded.enabled, \
             client_id = excluded.client_id, client_secret = excluded.client_secret, \
             api_endpoint = excluded.api_endpoint, \
             refresh_interval_seconds = excluded.refresh_interval_seconds, \
             updated_at = unixepoch()",
        )
        .bind(a.enabled)
        .bind(&a.client_id)
        .bind(&secret)
        .bind(&a.api_endpoint)
        .bind(a.refresh_interval_seconds),
        "aeris_account",
        "setting the Aeris account",
    )
    .await?;

    exec(
        conn,
        sqlx::query("DELETE FROM station_service WHERE service = 'aeris'"),
        "station_service",
        "replacing the Aeris pairings",
    )
    .await?;
    for key in &a.stations {
        exec(
            conn,
            sqlx::query(
                "INSERT INTO station_service (station_key, service, uploader_id) \
                 VALUES (?, 'aeris', NULL)",
            )
            .bind(key),
            "station_service",
            "pairing a station with forecasts and alerts",
        )
        .await?;
    }
    Ok(())
}

async fn put_listener(
    conn: &mut SqliteConnection,
    l: &ListenerWrite,
) -> Result<(), ConfigAdminError> {
    let n = exec(
        conn,
        sqlx::query(
            "UPDATE listener SET listen_addr = ?, http_port = ?, https_port = ?, \
             grpc_port = ?, management_listen_addr = ?, management_port = ?, \
             management_tls_cert_path = ?, management_tls_key_path = ?, \
             updated_at = unixepoch() WHERE id = 1",
        )
        .bind(&l.listen_addr)
        .bind(l.http_port)
        .bind(l.https_port)
        .bind(l.grpc_port)
        .bind(&l.management_listen_addr)
        .bind(l.management_port)
        .bind(&l.management_tls_cert_path)
        .bind(&l.management_tls_key_path),
        "listener",
        "updating the listener",
    )
    .await?;
    // The `listener` row is seeded by the first migration and `CHECK (id = 1)` makes a
    // second one unrepresentable, so zero rows means the row this whole process binds from
    // is missing -- which is worth a refusal rather than a `200` and a bumped generation.
    missing(n, "listener configuration", "1")
}

// ---------------------------------------------------------------------------------------
// One step back
//
// W5 §2.4's **Undo save** is "a real action, not a hope": because every write has a
// generation, the server keeps the previous committed configuration and restores it. The
// scope is deliberately small -- one step, offered only on the failure banner -- and full
// configuration history is OQ-W5-5.
//
// # Why the previous configuration is kept as a replayable snapshot rather than as an
// # inverse change
//
// The obvious implementation is to compute each change's inverse: the opposite of
// `PutStation` is `PutStation(the row as it was)`, and the opposite of `DeleteStation` is
// the row plus its pairings. It is also the one that quietly goes wrong, in two ways that
// no test of the forward direction can see.
//
// First, **a credential has no inverse through the write path.** `Change::PutStation`
// treats `None` as "keep what is stored", which is the right rule for a form -- no response
// returns a secret to retype (D-67) -- and it means the inverse of overwriting a secret is
// not expressible as a `Change` unless the OLD hash is captured. Second, **a delete
// cascades**: dropping a station takes its `station_service` rows with it and NULLs three
// `website` columns, so the inverse of one `DeleteStation` is a row, a set of pairings and
// up to three website updates. Each inverse is a separate thing to get right, and they are
// only exercised by the undo path.
//
// A snapshot has one shape for every change, and the forward path is what replays it. The
// surrogate ids travel with it, because `stations.id` and `website.id` reach the FROZEN
// contract -- a revert that renumbered them would change what `/stationinfo` means.
// ---------------------------------------------------------------------------------------

/// Every configurable row, as the changes that would recreate it.
///
/// Read inside the caller's transaction, so a snapshot taken immediately before a write is
/// the configuration that write replaces -- not a configuration some other connection
/// committed in between.
///
/// **`node` is not here**, and that is not an omission: the node's identity, time zone and
/// spool are not editable through the management API (W5 §1.4 moves what is editable to
/// `System → Advanced`, and the identity is minted once), so a revert has nothing to put
/// back. `listener` *is* here, because its ports are editable.
pub async fn snapshot(conn: &mut SqliteConnection) -> Result<Vec<Change>, ConfigAdminError> {
    let mut out: Vec<Change> = Vec::new();

    // Stations first: `website.primary_station_key`, `website.snow_station_key`,
    // `website.air_quality_station_key` and `station_service.station_key` all reference
    // them, so a replay in any other order fails a foreign key.
    let rows = sqlx::query(
        "SELECT id, station_key, owning_node_id, name, type, enabled, identity_kind, \
                presented_key, ingest_secret_hash, latitude, longitude, altitude_m, \
                timezone, aprs_callsign, wind_dir_correction_degrees, \
                snow_base_distance_mm, transport_kind, transport_dev, transport_baud, \
                transport_host, transport_port, transport_url, transport_method, \
                transport_auth, transport_streaming, transport_mount, transport_group, \
                transport_topic, transport_tls_ca_path, transport_tls_cert_path, \
                transport_tls_key_path, wll_sensor_mapping, wll_poll_interval_seconds \
         FROM stations ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| classify("stations", "snapshotting the stations", e))?;
    for r in rows {
        out.push(Change::PutStation(StationWrite {
            id: Some(r.get("id")),
            station_key: r.get("station_key"),
            owning_node_id: Some(r.get("owning_node_id")),
            name: r.get("name"),
            kind: r.get("type"),
            enabled: r.get("enabled"),
            identity_kind: r.get("identity_kind"),
            presented_key: r.get("presented_key"),
            ingest_secret_hash: r.get("ingest_secret_hash"),
            latitude: r.get("latitude"),
            longitude: r.get("longitude"),
            altitude_m: r.get("altitude_m"),
            timezone: r.get("timezone"),
            aprs_callsign: r.get("aprs_callsign"),
            wind_dir_correction_degrees: r.get("wind_dir_correction_degrees"),
            snow_base_distance_mm: r.get("snow_base_distance_mm"),
            transport_kind: r.get("transport_kind"),
            transport_dev: r.get("transport_dev"),
            transport_baud: r.get("transport_baud"),
            transport_host: r.get("transport_host"),
            transport_port: r.get("transport_port"),
            transport_url: r.get("transport_url"),
            transport_method: r.get("transport_method"),
            transport_auth: r.get("transport_auth"),
            transport_streaming: r.get("transport_streaming"),
            transport_mount: r.get("transport_mount"),
            transport_group: r.get("transport_group"),
            transport_topic: r.get("transport_topic"),
            transport_tls_ca_path: r.get("transport_tls_ca_path"),
            transport_tls_cert_path: r.get("transport_tls_cert_path"),
            transport_tls_key_path: r.get("transport_tls_key_path"),
            wll_sensor_mapping: r.get("wll_sensor_mapping"),
            wll_poll_interval_seconds: r.get("wll_poll_interval_seconds"),
        }));
    }

    let rows = sqlx::query(
        "SELECT id, name, hostname, page_title, about_station_html, primary_station_key, \
                snow_enabled, snow_station_key, air_quality_enabled, \
                air_quality_station_key, is_portal, apple_app_id, radar_enabled, \
                radar_token, tls_cert_path, tls_key_path, is_default_tls \
         FROM website ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| classify("website", "snapshotting the websites", e))?;
    for r in rows {
        let id: i64 = r.get("id");
        out.push(Change::PutWebsite(WebsiteWrite {
            id: Some(id),
            name: r.get("name"),
            hostname: r.get("hostname"),
            page_title: r.get("page_title"),
            about_station_html: r.get("about_station_html"),
            primary_station_key: r.get("primary_station_key"),
            snow_enabled: r.get("snow_enabled"),
            snow_station_key: r.get("snow_station_key"),
            air_quality_enabled: r.get("air_quality_enabled"),
            air_quality_station_key: r.get("air_quality_station_key"),
            is_portal: r.get("is_portal"),
            apple_app_id: r.get("apple_app_id"),
            tls_cert_path: r.get("tls_cert_path"),
            tls_key_path: r.get("tls_key_path"),
            is_default_tls: r.get("is_default_tls"),
        }));
        // The radar registration is write-only and is not a column `PutWebsite` carries
        // (W5 §1.5), so it is replayed as the action that set it. `radar_registered_at` is
        // re-stamped rather than preserved: the moment on the screen is "registered on",
        // and a revert *is* a registration as far as this node is concerned.
        let token: String = r.get("radar_token");
        if !token.is_empty() {
            out.push(Change::RegisterRadar { website: id, token });
        }
    }

    // `uploader` before `station_service`, which `PutService` carries: the pairing's
    // composite foreign key names both the id and the kind.
    let rows = sqlx::query(
        "SELECT id, kind, name, enabled, interval_seconds, api_endpoint, api_key, \
                aprs_passcode, aprs_symbol_table, aprs_symbol_code, aprs_comment, \
                aprs_server, aprs_transport, aprs_kiss_serial_device, \
                aprs_kiss_serial_baud, aprs_kiss_tcp_address, aprs_kiss_path, \
                aprs_kiss_destination \
         FROM uploader ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| classify("uploader", "snapshotting the upload services", e))?;
    for r in rows {
        let kind: String = r.get("kind");
        // **By `uploader_id` as well as by service.** `uploader.kind` is not unique -- only
        // `name` is -- so a configuration with two accounts of one kind (which `plan()`
        // refuses at boot, but whose rows exist) would otherwise give every account the
        // union of the pairings, and a snapshot-and-replay would collapse them.
        let id: i64 = r.get("id");
        let pairings = sqlx::query(
            "SELECT station_key, remote_station_id FROM station_service \
             WHERE service = ? AND uploader_id = ? ORDER BY station_key",
        )
        .bind(&kind)
        .bind(id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| classify("station_service", "snapshotting a service's stations", e))?;
        out.push(Change::PutService(ServiceWrite {
            id: Some(id),
            kind,
            name: r.get("name"),
            enabled: r.get("enabled"),
            interval_seconds: r.get("interval_seconds"),
            api_endpoint: r.get("api_endpoint"),
            api_key: r.get("api_key"),
            aprs_passcode: r.get("aprs_passcode"),
            aprs_symbol_table: r.get("aprs_symbol_table"),
            aprs_symbol_code: r.get("aprs_symbol_code"),
            aprs_comment: r.get("aprs_comment"),
            aprs_server: r.get("aprs_server"),
            aprs_transport: r.get("aprs_transport"),
            aprs_kiss_serial_device: r.get("aprs_kiss_serial_device"),
            aprs_kiss_serial_baud: r.get("aprs_kiss_serial_baud"),
            aprs_kiss_tcp_address: r.get("aprs_kiss_tcp_address"),
            aprs_kiss_path: r.get("aprs_kiss_path"),
            aprs_kiss_destination: r.get("aprs_kiss_destination"),
            pairings: pairings
                .into_iter()
                .map(|p| Pairing {
                    station_key: p.get("station_key"),
                    remote_station_id: p.get("remote_station_id"),
                })
                .collect(),
        }));
    }

    let aeris = sqlx::query(
        "SELECT enabled, client_id, client_secret, api_endpoint, refresh_interval_seconds \
         FROM aeris_account WHERE id = 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| classify("aeris_account", "snapshotting the forecast account", e))?;
    // The pairings are read **whether or not there is an account row**. `station_service`
    // has no foreign key to `aeris_account` -- the pairing carries no `uploader_id` by
    // design, because Aeris is not an uploader -- so rows with `service = 'aeris'` and no
    // account are representable. `restore` deletes them with everything else, so a snapshot
    // that skipped them would lose which stations were paired every time an operator
    // reverted on a node that had not finished configuring forecasts.
    let stations: Vec<String> = sqlx::query(
        "SELECT station_key FROM station_service WHERE service = 'aeris' \
         ORDER BY station_key",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| classify("station_service", "snapshotting the forecast stations", e))?
    .into_iter()
    .map(|s| s.get("station_key"))
    .collect();
    match aeris {
        Some(r) => out.push(Change::PutAeris(AerisWrite {
            enabled: r.get("enabled"),
            client_id: r.get("client_id"),
            client_secret: r.get("client_secret"),
            api_endpoint: r.get("api_endpoint"),
            refresh_interval_seconds: r.get("refresh_interval_seconds"),
            stations,
        })),
        // No account, but pairings: replayed as the pairings alone, which `PutAeris` would
        // turn into an account row that was never there.
        None if !stations.is_empty() => out.push(Change::PairForecasts(stations)),
        None => {}
    }

    let db = sqlx::query("SELECT url FROM weather_database WHERE id = 1")
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| classify("weather_database", "snapshotting the weather database", e))?;
    if let Some(r) = db {
        out.push(Change::PutWeatherDatabase(r.get("url")));
    }

    let l = sqlx::query(
        "SELECT listen_addr, http_port, https_port, grpc_port, management_listen_addr, \
                management_port, management_tls_cert_path, management_tls_key_path \
         FROM listener WHERE id = 1",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| classify("listener", "snapshotting the listener", e))?;
    out.push(Change::PutListener(ListenerWrite {
        listen_addr: l.get("listen_addr"),
        http_port: l.get("http_port"),
        https_port: l.get("https_port"),
        grpc_port: l.get("grpc_port"),
        management_listen_addr: l.get("management_listen_addr"),
        management_port: l.get("management_port"),
        management_tls_cert_path: l.get("management_tls_cert_path"),
        management_tls_key_path: l.get("management_tls_key_path"),
    }));

    Ok(out)
}

/// Put a [`snapshot`] back, in the caller's transaction.
///
/// Clears every table the snapshot covers and replays it through [`apply`], so the restore
/// path and the save path are the same statements. The clear is why this is not simply "run
/// the changes": a row that exists now and did not exist then has to go, and no forward
/// change says so.
///
/// **`DELETE FROM stations` cascades**, which is what makes one statement enough: the
/// `station_service` pairings go with it and the three `website` references are set to NULL.
/// The replay then puts all of them back, which is why the order below is stations, then
/// websites, then services.
pub async fn restore(
    conn: &mut SqliteConnection,
    snapshot: &[Change],
) -> Result<(), ConfigAdminError> {
    for (table, what) in [
        ("station_service", "clearing the service pairings"),
        ("uploader", "clearing the upload services"),
        ("aeris_account", "clearing the forecast account"),
        ("website", "clearing the websites"),
        ("stations", "clearing the stations"),
        ("weather_database", "clearing the weather database"),
    ] {
        exec(
            conn,
            sqlx::query(&format!("DELETE FROM {table}")),
            table,
            what,
        )
        .await?;
    }
    for change in snapshot {
        apply(conn, change).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory per test, removed on drop. The same twenty lines as
    /// `config_db`'s, for the reason its own comment gives: not needing `tempfile` is
    /// better than being permitted it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mw-config-admin-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }

        fn db(&self) -> PathBuf {
            self.0.join("config.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn a_station(correction: i64) -> StationWrite {
        StationWrite {
            station_key: "csi".into(),
            name: "CSI".into(),
            kind: "campbellscientific".into(),
            enabled: true,
            owning_node_id: Some(vec![7u8; 16]),
            identity_kind: "fixed".into(),
            // The schema's CHECK ties the two: a station that is not identified by its own
            // registration has to name a transport. `tcp_out` with a host and a port is the
            // Campbell station's own shape, over the ser2net bridge M5-1a measured.
            transport_kind: Some("tcp_out".into()),
            transport_host: Some("127.0.0.1".into()),
            transport_port: Some(7001),
            wind_dir_correction_degrees: correction,
            ..Default::default()
        }
    }

    /// The correction a round trip reports for `csi`.
    fn stored_correction(changes: &[Change]) -> i64 {
        changes
            .iter()
            .find_map(|c| match c {
                Change::PutStation(s) if s.station_key == "csi" => {
                    Some(s.wind_dir_correction_degrees)
                }
                _ => None,
            })
            .expect("the station this test wrote")
    }

    /// A non-zero wind-direction correction survives a write and a read.
    ///
    /// Every other test of this column uses **zero**, which is the value a dropped binding
    /// also produces -- so an `UPDATE` that stopped naming the column, or a `SELECT` that
    /// stopped reading it, would pass all of them. The calibration is not cosmetic: the
    /// vane is bolted to a mast that is not aligned to north, so a station with the wrong
    /// correction reports a wind direction that is wrong by a constant and no downstream
    /// check can tell. Measured on the Campbell station on 2026-11-13, where the Go
    /// configuration's `-94` against this side's `0` made `winddir` differ on 100% of
    /// readings and `windd` on 100% of `/span` buckets.
    #[tokio::test]
    async fn a_wind_direction_correction_survives_a_write_and_a_read() {
        let scratch = Scratch::new("wind-dir-correction");
        let pool = crate::config_db::open_and_migrate(&scratch.db())
            .await
            .expect("the configuration schema applies");
        let mut conn = pool.acquire().await.expect("a connection");

        apply(&mut conn, &Change::PutStation(a_station(-94)))
            .await
            .expect("a station with a correction is accepted");
        assert_eq!(
            stored_correction(&snapshot(&mut conn).await.expect("snapshot")),
            -94
        );

        // An edit changes it, rather than the insert being the only path that writes it:
        // 266 is the same rotation the other way round, and the column's CHECK admits both.
        apply(&mut conn, &Change::PutStation(a_station(266)))
            .await
            .expect("an edit is accepted");
        assert_eq!(
            stored_correction(&snapshot(&mut conn).await.expect("snapshot")),
            266
        );

        // And clearing it clears it. `0` is the column's default, so this is the one case an
        // update that silently did nothing would be indistinguishable from -- which is why it
        // is asserted after a non-zero value rather than from a fresh row.
        apply(&mut conn, &Change::PutStation(a_station(0)))
            .await
            .expect("clearing is accepted");
        assert_eq!(
            stored_correction(&snapshot(&mut conn).await.expect("snapshot")),
            0
        );
    }
}
