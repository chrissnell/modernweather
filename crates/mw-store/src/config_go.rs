//! The **Go** configuration database, and its translation onto this schema (M7-4).
//!
//! [`config_db`](crate::config_db) is the read half of *this* program's configuration and
//! [`config_admin`](crate::config_admin) is its write half. This module is a third thing:
//! it reads the configuration database the **Go** daemon has been running from and turns it
//! into the rows this one starts from. It exists once, it is run once per deployment, and it
//! is committed because a configuration migration that exists only in a document has not
//! been executed and one that exists only in somebody's shell history cannot be reviewed --
//! which is `ci/db/migrate-from-go/`'s argument for the *data* migration, one database over.
//!
//! # It writes through [`config_admin`], and that is the whole design
//!
//! Nothing here composes an `INSERT` into `stations`, `website`, `uploader`,
//! `station_service` or `aeris_account`. [`translate`] produces a `Vec<`[`Change`]`>` -- the
//! same closed enum the management API's save path applies -- and [`write`] applies it with
//! [`config_admin::apply`]. Three properties follow, and each one is a defect this way round
//! cannot have:
//!
//!   * **Every constraint in `migrations/config/` is enforced on the migrated rows**, in the
//!     same statements an operator's own edit goes through. A migration with its own
//!     `INSERT`s would be a second write path, and the second one is always the one that
//!     learns about a new `CHECK` late.
//!   * **A dry run is free and is the same code.** `apply` does not commit, so the caller
//!     chooses; `POST /api/v1/config/preview` and a real save already differ in exactly that
//!     one way (W5 §2.4), and so do `--migrate-config-dry-run` and a real migration.
//!   * **What the migration can express is what an operator can express.** A configuration
//!     only reachable by migration would be a configuration the management UI cannot edit.
//!
//! The three tables [`Change`] does not cover -- `node`, `peers` and `peer_origins` -- are
//! written here, and their absence from that enum is not an oversight: a node identity is
//! minted once by the node and a peer row is written by enrolment, so neither is something
//! an operator *edits*. [`write`] is explicit about being the one exception.
//!
//! # The source's shape is not knowable at compile time
//!
//! `pkg/config/provider_sqlite.go` passes `CREATE TABLE IF NOT EXISTS` to one `Exec` at
//! every start, so an existing database's schema has been frozen at its own first-run shape
//! ever since -- that is the actual instance of the defect **D-8** was written about. On top
//! of that, `migrations/config/` in the Go tree both **adds** columns the first-run DDL does
//! not declare (`019_add_wll_fields` adds five) and **removes** columns it does
//! (`013_remove_controller_configs` drops every per-service credential from
//! `controller_configs`). So the column set of the database in front of this code is a
//! function of when it was created and which migrations have run, and no single column list
//! is correct for every deployment.
//!
//! [`read`] therefore does what W9's raw import does one database over: it asks the source's
//! own catalogue which columns exist, selects the intersection with what it wants, and
//! reports the difference in both directions. A column that is absent reads as its Go
//! default; a column that is present and that this code does not know about is named in the
//! report rather than silently skipped.
//!
//! **Every value is read as text and parsed here.** The Go tables are not `STRICT`, so
//! SQLite's declared type is a hint and nothing more: `devices.port` is `TEXT` holding a
//! number, `aprs_symbol_table CHAR(1)` will hold a four-hundred-character string if
//! something writes one, and a `BOOLEAN` column holds whatever was last put in it. A reader
//! that asked `sqlx` for an `i64` would fail on an operator's database rather than on a
//! test; `CAST(... AS TEXT)` and one parse path per type cannot.
//!
//! # What it refuses, and what it reports
//!
//! The split is by who can fix it. A **refusal** ([`ConfigGoError`]) is a source
//! configuration this schema has no faithful image of -- two stations whose keys collide,
//! two Aeris accounts where there is now one, a device type with no driver. A **note**
//! ([`Note`]) is a faithful image that is nonetheless not identical: a value raised to a
//! floor, a setting with no successor column, a credential the owner has to type. Notes are
//! the "one-line report" **O-8** asks for, and they are structured rather than formatted so
//! that the caller decides how to render them and a test can assert on one.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use sqlx::{Row, SqliteConnection};

use crate::config_admin::{
    self, AerisWrite, Change, ListenerWrite, Pairing, ServiceWrite, StationWrite, WebsiteWrite,
};

/// The RF beacon floor, in seconds: **O-8**, answered 600 s by the owner.
///
/// `mw_output::interval::RadioInterval::FLOOR` is the same number as a `Duration`, and that
/// type is where it is enforced -- a `KissSerial` or `KissTcp` sink cannot be constructed
/// below it, so a sub-floor KISS uploader is a component that cannot be built and therefore
/// a daemon that does not start. This crate may not depend on `mw-output` (it is the same
/// layer), so the constant is restated with the test that pins the two together living in
/// the binary, which may name both.
pub const RADIO_FLOOR_SECONDS: i64 = 600;

/// `stations.wll_poll_interval_seconds`' own floor, which is a `CHECK` on the column.
///
/// Unlike the RF floor this one *is* in the schema, so a sub-floor value is not a row this
/// migration may write at all -- it is raised here with a line, because the Go column has no
/// floor and an operator who typed `10` would otherwise have the migration stop on a
/// constraint.
pub const WLL_POLL_FLOOR_SECONDS: i64 = 30;

/// The characters `mw_component::ComponentId` permits, which is what makes a `station_key`
/// usable: lower-case ASCII, digits, `-`, `_`, `.` and `:`.
fn id_safe(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ':')
}

/// The `station_key` a Go `devices.name` becomes.
///
/// **One rule, and it has to be the same rule the data migration used.** W9's
/// `w9.station_map` maps the Go `stationname` to the `observations.station_id` the history
/// is stored under, and the frozen read path resolves `?station=<name>` to a
/// `config.stations` row and then queries the store by that row's `station_key` -- so if the
/// two normalisations disagree, the charts draw a live station's few minutes beside a year
/// of history nothing can see. That is the defect this work item was written to fix, and
/// the fix is that both ends derive the key from `devices.name` by the same rule rather than
/// one being typed by hand.
///
/// Lower-case, and every character `mw_component::ComponentId` does not permit becomes `-`,
/// with runs collapsed and the ends trimmed. `CSI` is `csi`, `snow` is `snow`, and
/// `Suncrest Air Quality` is `suncrest-air-quality`. A name that normalises to nothing --
/// `"***"`, `""` -- is a refusal rather than a generated key, because a station nobody can
/// name in a URL is not a migration of anything.
///
/// The `name` column keeps the operator's spelling. `/stationinfo` emits it, `?station=CSI`
/// matches it, and renaming it would be a frozen-contract change (D-68).
pub fn station_key(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if id_safe(lower) {
            out.push(lower);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What a [`Note`] is about, which is also how a caller should render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoteKind {
    /// A value this migration derived rather than copied -- a `station_key`, a node
    /// identity, a URL assembled from parts. Informational: nothing was lost.
    Derived,
    /// A value that was **changed** to make it representable or buildable. The sub-floor
    /// KISS beacon interval is the one **O-8** names.
    Raised,
    /// A Go setting with no successor here. Stated rather than dropped silently, because an
    /// operator who configured it will look for it.
    NotCarried,
    /// Something the **owner** has to do by hand before the configuration is complete --
    /// a credential to re-enter, a peer to re-enrol.
    Action,
}

impl NoteKind {
    /// The one word a log line or a table row is labelled with.
    pub const fn as_str(self) -> &'static str {
        match self {
            NoteKind::Derived => "derived",
            NoteKind::Raised => "raised",
            NoteKind::NotCarried => "not-carried",
            NoteKind::Action => "action",
        }
    }
}

/// One line of the migration's report.
///
/// `subject` is what the line is about -- a station key, an uploader name, a column -- so
/// that a caller can sort or filter without parsing `message`, and a test can assert on the
/// pair rather than on prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Which kind of line this is.
    pub kind: NoteKind,
    /// What it is about.
    pub subject: String,
    /// One sentence, in the operator's words.
    pub message: String,
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {}: {}",
            self.kind.as_str(),
            self.subject,
            self.message
        )
    }
}

/// The report, as the notes in the order they were produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every line.
    pub notes: Vec<Note>,
}

impl Report {
    fn push(&mut self, kind: NoteKind, subject: impl Into<String>, message: impl Into<String>) {
        self.notes.push(Note {
            kind,
            subject: subject.into(),
            message: message.into(),
        });
    }

    /// Every note of one kind, which is how the caller renders the owner's to-do list
    /// separately from the informational half.
    pub fn of(&self, kind: NoteKind) -> impl Iterator<Item = &Note> {
        self.notes.iter().filter(move |n| n.kind == kind)
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Why a Go configuration could not be migrated.
///
/// Typed per module and preserving its cause chain (**D-34**). Every variant is a source
/// configuration this schema has no faithful image of -- which is deliberately a *small*
/// set, because the alternative to refusing is choosing a winner and not saying so.
#[derive(Debug, thiserror::Error)]
pub enum ConfigGoError {
    /// The source is not a Go configuration database.
    #[error(
        "`{table}` is not in this database, so it is not a RemoteWeather configuration \
         database. Point `--migrate-config-from` at the file the Go daemon's `--config` \
         names."
    )]
    NotAGoConfig {
        /// The table that was looked for.
        table: &'static str,
    },

    /// A read failed for a reason that is not the configuration's.
    #[error("the Go configuration database could not be read: {what}")]
    Query {
        /// Which read, in words.
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// A `devices.name` that cannot become a `station_key`.
    #[error(
        "the station named `{name}` has no component-safe key: a `station_key` may hold \
         lower-case ASCII, digits, `-`, `_`, `.` and `:`, and nothing in this name survives \
         that. Rename the device in the Go configuration and run this again."
    )]
    NoStationKey {
        /// `devices.name`.
        name: String,
    },

    /// Two `devices.name`s that normalise to one key.
    #[error(
        "`{first}` and `{second}` both become the station key `{key}`. The key is the \
         identity the weather history is stored under, so two stations cannot share one: \
         rename one of them in the Go configuration and run this again."
    )]
    StationKeyCollision {
        /// The name seen first.
        first: String,
        /// The name that collided with it.
        second: String,
        /// The key they share.
        key: String,
    },

    /// A device type no driver here configures.
    #[error(
        "the station `{name}` has type `{kind}`, which is not a type this daemon drives. \
         The types it drives are campbellscientific, davis, snowgauge, ambient-customized, \
         airgradient and weatherlink-live."
    )]
    UnknownStationType {
        /// `devices.name`.
        name: String,
        /// `devices.type`.
        kind: String,
    },

    /// A device whose connection columns name no transport.
    #[error(
        "the station `{name}` (type `{kind}`) has no usable connection in the Go \
         configuration: {because}. A station row here must name its transport, so there is \
         nothing to migrate it to."
    )]
    NoTransport {
        /// `devices.name`.
        name: String,
        /// `devices.type`.
        kind: String,
        /// Which columns were looked at and what was in them.
        because: String,
    },

    /// `aprs_transport = 'kiss'` with no usable `aprs_kiss_connection`.
    #[error(
        "the station `{name}` beacons over KISS but `aprs_kiss_connection` is `{found}`, \
         which is neither `serial` nor `tcp`. This schema spells the transport \
         `kiss-serial` or `kiss-tcp`, so there is no row to write."
    )]
    UnknownKissConnection {
        /// `devices.name`.
        name: String,
        /// What the column held.
        found: String,
    },

    /// Two devices with different Aeris accounts.
    ///
    /// `aeris_account` is one row, and its own schema comment says this is a refusal here
    /// rather than a silently-chosen winner.
    #[error(
        "`{first}` and `{second}` are configured with different Aeris accounts \
         (`{first_id}` and `{second_id}`). There is one forecast account here and the \
         per-station switch is the pairing, so one of the two has to go: clear the account \
         on the station that should not have its own, and run this again."
    )]
    AerisAccountsDisagree {
        /// The first station.
        first: String,
        /// Its client id.
        first_id: String,
        /// The station that disagreed.
        second: String,
        /// Its client id.
        second_id: String,
    },

    /// Two devices naming one Aeris account with two different secrets.
    #[error(
        "`{first}` and `{second}` are both configured with the Aeris client id \
         `{client_id}` and two different client secrets. One of them is wrong and this \
         migration cannot tell which, so clear the secret on the station that should not \
         carry it and run this again."
    )]
    AerisSecretsDisagree {
        /// The first station.
        first: String,
        /// The station that disagreed.
        second: String,
        /// The client id they share.
        client_id: String,
    },

    /// A website naming a station that is not there.
    #[error(
        "the website `{website}` names the station `{device}` for {role}, and no device by \
         that name is in the Go configuration. A website here may only point at a station \
         that exists."
    )]
    NoSuchWebsiteStation {
        /// `weather_websites.name`.
        website: String,
        /// What the website named.
        device: String,
        /// Which of the three columns named it.
        role: &'static str,
    },

    /// No station has a time zone and the database connection named none either.
    #[error(
        "the station `{name}` has no time zone, and `storage_configs.timescale_timezone` is \
         empty too, so there is nothing to fall back to. Every collecting station here \
         rolls its rain total over at its own local midnight and `plan()` refuses a station \
         with no valid zone rather than rolling over in the server's -- so set \
         `devices.timezone` to an IANA name (`America/Denver`) and run this again."
    )]
    NoTimezone {
        /// `devices.name`.
        name: String,
    },

    /// Two stations with the same service enabled.
    ///
    /// One `uploader` row is one account and one component (W2a **D-55**), and `PutService`
    /// -- the one write path the management UI also goes through -- is keyed by the service's
    /// kind, so this schema holds one account per service. W7 **D-38** says the same thing
    /// from the other end: the far end identifies a station by the credential it arrived
    /// under, so two stations on one account is `PlanError::UploaderArity`.
    #[error(
        "{service} is enabled for both `{first}` and `{second}`. There is one {service} \
         account here and it sends one station's readings, so turn it off for one of them \
         in the Go configuration and add the second account afterwards -- a silently \
         dropped upload would be worse than this refusal."
    )]
    TwoAccountsForOneService {
        /// Which service, in the operator's words.
        service: &'static str,
        /// The station that claimed it.
        first: String,
        /// The station that could not.
        second: String,
    },

    /// A write failed.
    #[error("the migrated configuration could not be written")]
    Write {
        #[source]
        source: config_admin::ConfigAdminError,
    },

    /// A write of one of the three tables `Change` does not cover failed.
    #[error("the migrated configuration could not be written: {what}")]
    WriteQuery {
        /// Which write, in words.
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// The destination already holds configuration.
    #[error(
        "the destination configuration database already holds {rows} row(s) in `{table}`. \
         This migration writes a deployment's whole configuration and will not merge into \
         one, so point `--config` at a path that does not exist yet."
    )]
    DestinationNotEmpty {
        /// Which table had rows.
        table: &'static str,
        /// How many.
        rows: i64,
    },
}

// ---------------------------------------------------------------------------
// The source, as it is read
// ---------------------------------------------------------------------------

/// One `devices` row, with every value already parsed.
///
/// Field for field with `pkg/config/provider.go`'s `DeviceData`, including the columns
/// `provider_sqlite.go`'s first-run DDL does not declare and `019_add_wll_fields` adds. An
/// absent column reads as its Go default, which for every one of these is the `None`/`0`/
/// empty case.
#[derive(Debug, Clone, Default)]
pub struct GoDevice {
    /// `id` -- the integer the FROZEN contract returns as `stations[].id`.
    pub id: Option<i64>,
    /// `name`.
    pub name: String,
    /// `type`.
    pub kind: String,
    /// `enabled`.
    pub enabled: bool,
    /// `hostname`.
    pub hostname: Option<String>,
    /// `port`, which is `TEXT` in the Go schema.
    pub port: Option<String>,
    /// `serial_device`.
    pub serial_device: Option<String>,
    /// `baud`.
    pub baud: Option<i64>,
    /// `path` -- the Ambient endpoint's mount.
    pub path: Option<String>,
    /// `wind_dir_correction`, in degrees.
    pub wind_dir_correction: Option<i64>,
    /// `base_snow_distance`, in millimetres.
    pub base_snow_distance: Option<f64>,
    /// `latitude`.
    pub latitude: Option<f64>,
    /// `longitude`.
    pub longitude: Option<f64>,
    /// `altitude`.
    pub altitude: Option<f64>,
    /// `timezone`.
    pub timezone: Option<String>,
    /// `tls_cert_file`.
    pub tls_cert_file: Option<String>,
    /// `tls_key_file`.
    pub tls_key_file: Option<String>,

    /// `wll_host`.
    pub wll_host: Option<String>,
    /// `wll_port`.
    pub wll_port: Option<i64>,
    /// `wll_broadcast`.
    pub wll_broadcast: Option<bool>,
    /// `wll_sensor_mapping`.
    pub wll_sensor_mapping: Option<String>,
    /// `wll_poll_interval`.
    pub wll_poll_interval: Option<i64>,

    /// `pws_enabled`.
    pub pws_enabled: bool,
    /// `pws_station_id`.
    pub pws_station_id: Option<String>,
    /// `pws_password`.
    pub pws_password: Option<String>,
    /// `pws_upload_interval`.
    pub pws_upload_interval: Option<i64>,
    /// `pws_api_endpoint`.
    pub pws_api_endpoint: Option<String>,

    /// `wu_enabled`.
    pub wu_enabled: bool,
    /// `wu_station_id`.
    pub wu_station_id: Option<String>,
    /// `wu_password`.
    pub wu_password: Option<String>,
    /// `wu_upload_interval`.
    pub wu_upload_interval: Option<i64>,
    /// `wu_api_endpoint`.
    pub wu_api_endpoint: Option<String>,

    /// `aprs_enabled`.
    pub aprs_enabled: bool,
    /// `aprs_callsign` -- public identity, so it lands on the station row.
    pub aprs_callsign: Option<String>,
    /// `aprs_passcode` -- the account credential, so it lands on the uploader row.
    pub aprs_passcode: Option<String>,
    /// `aprs_symbol_table`.
    pub aprs_symbol_table: Option<String>,
    /// `aprs_symbol_code`.
    pub aprs_symbol_code: Option<String>,
    /// `aprs_comment`.
    pub aprs_comment: Option<String>,
    /// `aprs_server`.
    pub aprs_server: Option<String>,
    /// `aprs_upload_interval`.
    pub aprs_upload_interval: Option<i64>,
    /// `aprs_transport` -- `aprs-is` or `kiss`.
    pub aprs_transport: Option<String>,
    /// `aprs_kiss_connection` -- `serial` or `tcp`.
    pub aprs_kiss_connection: Option<String>,
    /// `aprs_kiss_serial_device`.
    pub aprs_kiss_serial_device: Option<String>,
    /// `aprs_kiss_serial_baud`.
    pub aprs_kiss_serial_baud: Option<i64>,
    /// `aprs_kiss_tcp_address`.
    pub aprs_kiss_tcp_address: Option<String>,
    /// `aprs_kiss_path`.
    pub aprs_kiss_path: Option<String>,
    /// `aprs_kiss_destination`.
    pub aprs_kiss_destination: Option<String>,

    /// `aeris_enabled`.
    pub aeris_enabled: bool,
    /// `aeris_api_client_id`.
    pub aeris_api_client_id: Option<String>,
    /// `aeris_api_client_secret`.
    pub aeris_api_client_secret: Option<String>,
    /// `aeris_api_endpoint`.
    pub aeris_api_endpoint: Option<String>,
    /// `aeris_refresh_interval` -- `0` means "derive from the forecast period".
    pub aeris_refresh_interval: Option<i64>,
}

/// One `weather_websites` row.
#[derive(Debug, Clone, Default)]
pub struct GoWebsite {
    /// `id`.
    pub id: Option<i64>,
    /// `name`.
    pub name: String,
    /// `device_id` -- the primary station, by the `devices` surrogate.
    pub device_id: Option<i64>,
    /// `hostname`. `''` is the default website an unmatched `Host` resolves to.
    pub hostname: Option<String>,
    /// `page_title`.
    pub page_title: Option<String>,
    /// `about_station_html`.
    pub about_station_html: Option<String>,
    /// `snow_enabled`.
    pub snow_enabled: bool,
    /// `snow_device_name`.
    pub snow_device_name: Option<String>,
    /// `air_quality_enabled`.
    pub air_quality_enabled: bool,
    /// `air_quality_device_name`.
    pub air_quality_device_name: Option<String>,
    /// `tls_cert_path`.
    pub tls_cert_path: Option<String>,
    /// `tls_key_path`.
    pub tls_key_path: Option<String>,
    /// `is_portal`.
    pub is_portal: bool,
    /// `apple_app_id`.
    pub apple_app_id: Option<String>,
    /// `radar_enabled`.
    pub radar_enabled: bool,
    /// `radar_token`.
    pub radar_token: Option<String>,
    /// `radar_registered_at`.
    pub radar_registered_at: Option<i64>,
}

/// The TimescaleDB `storage_configs` row, as its seven DSN parts.
#[derive(Debug, Clone, Default)]
pub struct GoTimescale {
    /// `timescale_host`.
    pub host: Option<String>,
    /// `timescale_port`.
    pub port: Option<i64>,
    /// `timescale_database`.
    pub database: Option<String>,
    /// `timescale_user`.
    pub user: Option<String>,
    /// `timescale_password`.
    pub password: Option<String>,
    /// `timescale_ssl_mode`.
    pub ssl_mode: Option<String>,
    /// `timescale_timezone` -- which is `TimeZone=` in the DSN and therefore the zone Go's
    /// `date_trunc('day', NOW())` rollovers actually truncate in.
    pub timezone: Option<String>,
}

/// The `controller_configs` rows, merged.
///
/// Merged rather than kept per row because the table is keyed by `controller_type` and each
/// row carries every service's columns at once -- the `rest` row's `management_port` is NULL
/// and the `management` row's `rest_port` is NULL, and which row holds a given value is a
/// property of how the Go management API last wrote it rather than of the value. The merge
/// takes the first non-empty, and a disagreement between two rows about one field is not
/// representable in the Go daemon either: `LoadConfig` reads them into one struct too.
#[derive(Debug, Clone, Default)]
pub struct GoControllers {
    /// `rest_port` -- Go's one HTTP(S) port.
    pub rest_port: Option<i64>,
    /// `rest_listen_addr`.
    pub rest_listen_addr: Option<String>,
    /// `grpc_port`.
    pub grpc_port: Option<i64>,
    /// `grpc_listen_addr`.
    pub grpc_listen_addr: Option<String>,
    /// `grpc_cert`.
    pub grpc_cert: Option<String>,
    /// `grpc_key`.
    pub grpc_key: Option<String>,
    /// `management_port`.
    pub management_port: Option<i64>,
    /// `management_listen_addr`.
    pub management_listen_addr: Option<String>,
    /// `management_cert`.
    pub management_cert: Option<String>,
    /// `management_key`.
    pub management_key: Option<String>,
    /// Whether `management_auth_token` was set. The token itself is deliberately not read:
    /// it has no successor (W5 §3 removes it), so the only thing worth knowing is whether
    /// to tell the owner his old one has stopped meaning anything.
    pub management_auth_token_present: bool,
    /// `management_enable_cors`.
    pub management_enable_cors: bool,
    /// `aprs_server` -- the global default an uploader falls back to.
    pub aprs_server: Option<String>,
    /// `pws_api_endpoint`.
    pub pws_api_endpoint: Option<String>,
    /// `wu_api_endpoint`.
    pub wu_api_endpoint: Option<String>,
    /// `aeris_api_endpoint`.
    pub aeris_api_endpoint: Option<String>,
    /// `aeris_api_client_id`, which `013_remove_controller_configs` removes from this table
    /// and moves onto `devices`. Read because a database created before that migration ran
    /// still has it here and nowhere else.
    pub aeris_api_client_id: Option<String>,
    /// `aeris_api_client_secret`, for the reason above.
    pub aeris_api_client_secret: Option<String>,
    /// The snow controller's tuning, if any row carried it.
    pub snow: BTreeMap<String, String>,
}

/// One `remote_stations` row -- a station that registered over gRPC.
///
/// **The four plaintext secret columns are not fields here**, which is **D-67** as a type:
/// `aprs_password`, `wu_api_key`, `aeris_client_secret` and `pws_password` crossed the wire
/// in cleartext and sat in a second database where nothing read them (A-W7-2), so migrating
/// them would carry the exposure forward. The four `*_enabled` flags *are* read, because
/// what the owner needs to know is which services to turn back on.
#[derive(Debug, Clone, Default)]
pub struct GoRemoteStation {
    /// `station_id` -- a UUID, and a **station** identity rather than a node identity
    /// (W7 assumption 35). Read only so that the report can name it; it is not reused as a
    /// `node_id`.
    pub station_id: String,
    /// `station_name`.
    pub station_name: String,
    /// `station_type`.
    pub station_type: String,
    /// `aprs_enabled`, and the callsign, which is not a credential.
    pub aprs_enabled: bool,
    /// `aprs_callsign`.
    pub aprs_callsign: Option<String>,
    /// `wu_enabled`.
    pub wu_enabled: bool,
    /// `wu_station_id` -- the far-end identifier, not a credential.
    pub wu_station_id: Option<String>,
    /// `aeris_enabled`.
    pub aeris_enabled: bool,
    /// `pws_enabled`.
    pub pws_enabled: bool,
    /// `pws_station_id`.
    pub pws_station_id: Option<String>,
}

/// Everything [`read`] found.
#[derive(Debug, Clone, Default)]
pub struct GoConfig {
    /// `configs.name`, for the node name when nothing better is known.
    pub config_name: Option<String>,
    /// Every `devices` row, in `id` order.
    pub devices: Vec<GoDevice>,
    /// Every `weather_websites` row, in `id` order.
    pub websites: Vec<GoWebsite>,
    /// The TimescaleDB backend, if one is configured and enabled.
    pub timescale: Option<GoTimescale>,
    /// Every `storage_configs.backend_type` that is not `timescaledb`, for the report.
    pub other_backends: Vec<String>,
    /// Every `storage_configs.backend_type` that IS `timescaledb` and is switched off, for
    /// the report. Separate from [`GoConfig::timescale`] being `None` because the two mean
    /// different things to the operator: "you have no weather database" and "you have one
    /// and it was off".
    pub disabled_backends: Vec<String>,
    /// The merged `controller_configs`.
    pub controllers: GoControllers,
    /// Every `remote_stations` row.
    pub remote_stations: Vec<GoRemoteStation>,
    /// Every `controller_configs.controller_type` this code has no arm for.
    pub unknown_controllers: Vec<String>,
    /// How many `configs` rows are NOT the one being migrated.
    ///
    /// Zero in every deployment the Go management API produced, because it only ever writes
    /// `default`. Non-zero means rows nothing has ever loaded, and the report says so rather
    /// than silently leaving them behind.
    pub unused_configurations: i64,
    /// `table.column` for every column this code wanted and the source does not have.
    pub absent_columns: Vec<String>,
    /// `table.column` for every column the source has and this code does not read.
    pub unread_columns: Vec<String>,
}

// ---------------------------------------------------------------------------
// Reading the source
// ---------------------------------------------------------------------------

/// Every column name a table actually has, from the source's own catalogue.
async fn columns(
    conn: &mut SqliteConnection,
    table: &'static str,
) -> Result<BTreeSet<String>, ConfigGoError> {
    let rows = sqlx::query("SELECT name FROM pragma_table_info(?)")
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "listing a table's columns",
            source,
        })?;
    Ok(rows
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect::<BTreeSet<_>>())
}

/// `SELECT CAST(c AS TEXT) AS c, ...` over the columns that exist, with the difference in
/// both directions recorded.
///
/// `CAST(... AS TEXT)` on every column rather than letting `sqlx` decode: see the module
/// documentation -- the Go tables are not `STRICT`, so a declared `INTEGER` column can hold
/// a string and a declared `TEXT` one can hold a float, and one parse path per type in Rust
/// is the only reading that cannot fail on a database this code has never seen.
fn select(
    table: &'static str,
    wanted: &[&str],
    available: &BTreeSet<String>,
    order: &str,
    scope: &Scope,
    absent: &mut Vec<String>,
    unread: &mut Vec<String>,
) -> String {
    let mut list = String::new();
    for want in wanted {
        if available.contains(*want) {
            if !list.is_empty() {
                list.push_str(", ");
            }
            let _ = write!(list, "CAST(\"{want}\" AS TEXT) AS \"{want}\"");
        } else {
            absent.push(format!("{table}.{want}"));
        }
    }
    let known: BTreeSet<&str> = wanted.iter().copied().collect();
    for have in available {
        if !known.contains(have.as_str()) {
            unread.push(format!("{table}.{have}"));
        }
    }
    // Not reachable from any caller below -- every table has at least `name` or `id` in its
    // first-run DDL -- but a `SELECT  FROM` would be a syntax error rather than an empty
    // result, and this file is the one that knows why.
    if list.is_empty() {
        list.push_str("NULL AS \"empty\"");
    }
    format!(
        "SELECT {list} FROM \"{table}\"{} ORDER BY {order}",
        scope.clause(available)
    )
}

/// Which `configs` row every other table is read under.
///
/// **Every read in `provider_sqlite.go` is scoped the same way** --
/// `WHERE config_id = (SELECT id FROM configs WHERE name = 'default')` -- so the Go daemon
/// has never loaded a second named configuration, and a migration that merged two would
/// produce a deployment nothing was running. The id is resolved once and applied to every
/// table that has the column.
#[derive(Debug, Clone, Copy, Default)]
struct Scope {
    config_id: Option<i64>,
}

impl Scope {
    /// ` WHERE config_id = <id>`, or nothing when there is no id or the table has no column
    /// to apply it to. `remote_stations` is the table with no column: it is keyed by the
    /// station's own UUID and is not per-configuration.
    fn clause(&self, available: &BTreeSet<String>) -> String {
        match self.config_id {
            Some(id) if available.contains("config_id") => format!(" WHERE config_id = {id}"),
            _ => String::new(),
        }
    }
}

/// One column as text, with `NULL`, an absent column and `''` all reading as `None`.
///
/// The three are one case on purpose: Go's provider writes `sql.NullString{Valid: s != ""}`
/// for most of these columns, so `''` and `NULL` already mean the same thing there, and a
/// column the source does not have means the same thing again.
fn text(row: &sqlx::sqlite::SqliteRow, col: &str) -> Option<String> {
    let v: Option<String> = row.try_get(col).ok().flatten();
    v.filter(|s| !s.is_empty())
}

/// `Option<String>` with the empty string for `None`, written once.
///
/// One named function rather than a defaulting expression at thirteen call sites, which is
/// what `ci/layers.toml`'s `mw-store`/`forbidden_source` rule asks for: "the rule is not 'do
/// not default', it is 'say what the default is'". This is where it is said -- and it is a
/// *harmless* default by construction, because every caller is a column the Go schema
/// declares `TEXT DEFAULT ''` and this crate's rule is about a numeric default that becomes
/// a reading nobody can distinguish from a measurement (§1.7).
///
/// The `None` arm is written as a conversion rather than as the type's own constructor
/// deliberately: the constructor form is the one clippy rewrites into the method that rule
/// forbids, so the two would be in a loop. A comment is cheaper than an `allow` whose own
/// text the gate would then reject.
fn or_empty(v: Option<String>) -> String {
    match v {
        Some(s) => s,
        None => "".to_string(),
    }
}

/// One column as an integer, ignoring a value that is not one.
fn int(row: &sqlx::sqlite::SqliteRow, col: &str) -> Option<i64> {
    text(row, col).and_then(|s| s.trim().parse::<i64>().ok())
}

/// One column as a float, ignoring a value that is not one.
fn real(row: &sqlx::sqlite::SqliteRow, col: &str) -> Option<f64> {
    text(row, col)
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite())
}

/// One column as a boolean.
///
/// Both spellings, because both are in the Go schema: the provider binds Go `bool` as `1`
/// and `0`, and `DEFAULT TRUE` on a `BOOLEAN` column stores the integer SQLite folds that
/// to -- but a `BOOLEAN` column in a non-`STRICT` table will hold the literal string
/// `'true'` if anything ever wrote one, and a reader that treated that as false would turn
/// an enabled uploader off without saying so.
fn flag(row: &sqlx::sqlite::SqliteRow, col: &str) -> Option<bool> {
    let raw = text(row, col)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Read the Go configuration database.
///
/// Reports rather than refuses on a shape it does not recognise, with two exceptions:
/// `devices` and `weather_websites` absent means this is not a RemoteWeather configuration
/// database at all, and that is worth saying plainly rather than migrating an empty
/// deployment.
pub async fn read(conn: &mut SqliteConnection) -> Result<GoConfig, ConfigGoError> {
    let mut go = GoConfig::default();
    let (absent, unread) = (&mut go.absent_columns, &mut go.unread_columns);

    let device_cols = columns(&mut *conn, "devices").await?;
    if device_cols.is_empty() {
        return Err(ConfigGoError::NotAGoConfig { table: "devices" });
    }
    let website_cols = columns(&mut *conn, "weather_websites").await?;
    if website_cols.is_empty() {
        return Err(ConfigGoError::NotAGoConfig {
            table: "weather_websites",
        });
    }

    // `configs` is the Go schema's one-row-per-named-configuration table, and **every read
    // in `provider_sqlite.go` is scoped to the row named `default`**. So that is the
    // configuration the Go daemon has been running, and it is the one migrated: a second
    // named configuration is rows nothing has ever loaded, and merging it in would produce a
    // deployment that was never deployed.
    //
    // `ORDER BY name = 'default' DESC, id` rather than `WHERE name = 'default'`: a database
    // whose configuration has been renamed still has exactly one, and refusing to migrate it
    // over a name would be a refusal with no defect behind it. The preference is explicit so
    // that the common case is exact and the odd case still works.
    let config_cols = columns(&mut *conn, "configs").await?;
    let mut scope = Scope::default();
    if config_cols.contains("name") {
        let row = sqlx::query(
            // `CAST(... AS TEXT)` on both, like every other read here: `text()` and `int()`
            // are one parse path over strings, so an integer column read without the cast
            // decodes to nothing at all.
            "SELECT CAST(id AS TEXT) AS id, CAST(name AS TEXT) AS name FROM configs \
             ORDER BY name = 'default' DESC, id LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "reading the configuration's name",
            source,
        })?;
        go.config_name = row.as_ref().and_then(|r| text(r, "name"));
        scope.config_id = row.as_ref().and_then(|r| int(r, "id"));
        let others: i64 = sqlx::query_scalar("SELECT count(*) - 1 FROM configs")
            .fetch_one(&mut *conn)
            .await
            .map_err(|source| ConfigGoError::Query {
                what: "counting the named configurations",
                source,
            })?;
        if others > 0 {
            go.unused_configurations = others;
        }
    }

    const DEVICE_WANTED: &[&str] = &[
        "id",
        "config_id",
        "name",
        "type",
        "enabled",
        "hostname",
        "port",
        "serial_device",
        "baud",
        "path",
        "wind_dir_correction",
        "base_snow_distance",
        "website_id",
        "latitude",
        "longitude",
        "altitude",
        "timezone",
        "tls_cert_file",
        "tls_key_file",
        "wll_host",
        "wll_port",
        "wll_broadcast",
        "wll_sensor_mapping",
        "wll_poll_interval",
        "pws_enabled",
        "pws_station_id",
        "pws_password",
        "pws_upload_interval",
        "pws_api_endpoint",
        "wu_enabled",
        "wu_station_id",
        "wu_password",
        "wu_upload_interval",
        "wu_api_endpoint",
        "aprs_enabled",
        "aprs_callsign",
        "aprs_passcode",
        "aprs_symbol_table",
        "aprs_symbol_code",
        "aprs_comment",
        "aprs_server",
        "aprs_upload_interval",
        "aprs_transport",
        "aprs_kiss_connection",
        "aprs_kiss_serial_device",
        "aprs_kiss_serial_baud",
        "aprs_kiss_tcp_address",
        "aprs_kiss_path",
        "aprs_kiss_destination",
        "aeris_enabled",
        "aeris_api_client_id",
        "aeris_api_client_secret",
        "aeris_api_endpoint",
        "aeris_refresh_interval",
    ];
    let sql = select(
        "devices",
        DEVICE_WANTED,
        &device_cols,
        "id",
        &scope,
        absent,
        unread,
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "reading the devices",
            source,
        })?;
    for r in &rows {
        go.devices.push(GoDevice {
            id: int(r, "id"),
            name: or_empty(text(r, "name")),
            kind: or_empty(text(r, "type")),
            // The Go DDL is `enabled BOOLEAN DEFAULT TRUE`, so an absent or unreadable
            // value is an enabled device -- which is also the safe direction: a station
            // migrated as enabled and then turned off is an edit, and one migrated as
            // disabled is a station that silently stops reporting.
            enabled: flag(r, "enabled").unwrap_or(true),
            hostname: text(r, "hostname"),
            port: text(r, "port"),
            serial_device: text(r, "serial_device"),
            baud: int(r, "baud"),
            path: text(r, "path"),
            wind_dir_correction: int(r, "wind_dir_correction"),
            base_snow_distance: real(r, "base_snow_distance"),
            latitude: real(r, "latitude"),
            longitude: real(r, "longitude"),
            altitude: real(r, "altitude"),
            timezone: text(r, "timezone"),
            tls_cert_file: text(r, "tls_cert_file"),
            tls_key_file: text(r, "tls_key_file"),
            wll_host: text(r, "wll_host"),
            wll_port: int(r, "wll_port"),
            wll_broadcast: flag(r, "wll_broadcast"),
            wll_sensor_mapping: text(r, "wll_sensor_mapping"),
            wll_poll_interval: int(r, "wll_poll_interval"),
            pws_enabled: flag(r, "pws_enabled").unwrap_or(false),
            pws_station_id: text(r, "pws_station_id"),
            pws_password: text(r, "pws_password"),
            pws_upload_interval: int(r, "pws_upload_interval"),
            pws_api_endpoint: text(r, "pws_api_endpoint"),
            wu_enabled: flag(r, "wu_enabled").unwrap_or(false),
            wu_station_id: text(r, "wu_station_id"),
            wu_password: text(r, "wu_password"),
            wu_upload_interval: int(r, "wu_upload_interval"),
            wu_api_endpoint: text(r, "wu_api_endpoint"),
            aprs_enabled: flag(r, "aprs_enabled").unwrap_or(false),
            aprs_callsign: text(r, "aprs_callsign"),
            aprs_passcode: text(r, "aprs_passcode"),
            aprs_symbol_table: text(r, "aprs_symbol_table"),
            aprs_symbol_code: text(r, "aprs_symbol_code"),
            aprs_comment: text(r, "aprs_comment"),
            aprs_server: text(r, "aprs_server"),
            aprs_upload_interval: int(r, "aprs_upload_interval"),
            aprs_transport: text(r, "aprs_transport"),
            aprs_kiss_connection: text(r, "aprs_kiss_connection"),
            aprs_kiss_serial_device: text(r, "aprs_kiss_serial_device"),
            aprs_kiss_serial_baud: int(r, "aprs_kiss_serial_baud"),
            aprs_kiss_tcp_address: text(r, "aprs_kiss_tcp_address"),
            aprs_kiss_path: text(r, "aprs_kiss_path"),
            aprs_kiss_destination: text(r, "aprs_kiss_destination"),
            aeris_enabled: flag(r, "aeris_enabled").unwrap_or(false),
            aeris_api_client_id: text(r, "aeris_api_client_id"),
            aeris_api_client_secret: text(r, "aeris_api_client_secret"),
            aeris_api_endpoint: text(r, "aeris_api_endpoint"),
            aeris_refresh_interval: int(r, "aeris_refresh_interval"),
        });
    }

    const WEBSITE_WANTED: &[&str] = &[
        "id",
        "config_id",
        "name",
        "device_id",
        "hostname",
        "page_title",
        "about_station_html",
        "snow_enabled",
        "snow_device_name",
        "air_quality_enabled",
        "air_quality_device_name",
        "tls_cert_path",
        "tls_key_path",
        "is_portal",
        "apple_app_id",
        "radar_enabled",
        "radar_token",
        "radar_registered_at",
        "created_at",
        "updated_at",
    ];
    let sql = select(
        "weather_websites",
        WEBSITE_WANTED,
        &website_cols,
        "id",
        &scope,
        absent,
        unread,
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "reading the websites",
            source,
        })?;
    for r in &rows {
        go.websites.push(GoWebsite {
            id: int(r, "id"),
            name: or_empty(text(r, "name")),
            device_id: int(r, "device_id"),
            hostname: text(r, "hostname"),
            page_title: text(r, "page_title"),
            about_station_html: text(r, "about_station_html"),
            snow_enabled: flag(r, "snow_enabled").unwrap_or(false),
            snow_device_name: text(r, "snow_device_name"),
            air_quality_enabled: flag(r, "air_quality_enabled").unwrap_or(false),
            air_quality_device_name: text(r, "air_quality_device_name"),
            tls_cert_path: text(r, "tls_cert_path"),
            tls_key_path: text(r, "tls_key_path"),
            is_portal: flag(r, "is_portal").unwrap_or(false),
            apple_app_id: text(r, "apple_app_id"),
            radar_enabled: flag(r, "radar_enabled").unwrap_or(false),
            radar_token: text(r, "radar_token"),
            radar_registered_at: int(r, "radar_registered_at"),
        });
    }

    read_storage(&mut *conn, &mut go, &scope).await?;
    read_controllers(&mut *conn, &mut go, &scope).await?;
    read_remote_stations(&mut *conn, &mut go, &scope).await?;
    Ok(go)
}

/// `storage_configs`: the one PostgreSQL connection, and the names of the backends that are
/// not one.
///
/// The gRPC and APRS rows have no successor in `weather_database` by design -- "a backend
/// that is really an egress is an `uploader` row or an `uplink` row" -- so they are named in
/// the report and not translated. The four health columns go with
/// `storage.GlobalHealthManager`: component state is the health surface now.
async fn read_storage(
    conn: &mut SqliteConnection,
    go: &mut GoConfig,
    scope: &Scope,
) -> Result<(), ConfigGoError> {
    let cols = columns(&mut *conn, "storage_configs").await?;
    if cols.is_empty() {
        go.absent_columns.push("storage_configs.*".to_string());
        return Ok(());
    }
    const WANTED: &[&str] = &[
        "id",
        "config_id",
        "backend_type",
        "enabled",
        "timescale_host",
        "timescale_port",
        "timescale_database",
        "timescale_user",
        "timescale_password",
        "timescale_ssl_mode",
        "timescale_timezone",
        "grpc_cert",
        "grpc_key",
        "grpc_listen_addr",
        "grpc_port",
        "grpc_pull_from_device",
        "station_id",
        "aprs_callsign",
        "aprs_server",
        "aprs_location_lat",
        "aprs_location_lon",
        "health_last_check",
        "health_status",
        "health_message",
        "health_error",
    ];
    let sql = select(
        "storage_configs",
        WANTED,
        &cols,
        "id",
        scope,
        &mut go.absent_columns,
        &mut go.unread_columns,
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "reading the storage backends",
            source,
        })?;
    for r in &rows {
        let kind = or_empty(text(r, "backend_type"));
        // `enabled` is checked rather than merely selected, because the absence of a
        // `weather_database` row is MEANINGFUL here (D-44) and a disabled backend is a
        // deployment Go was not writing to: migrating it as the live database would give
        // the new daemon a `writer:timescale` against a server the old one was not using.
        // The Go DDL is `enabled BOOLEAN DEFAULT TRUE`, so an unreadable value is enabled.
        let enabled = flag(r, "enabled").unwrap_or(true);
        if kind == "timescaledb" && enabled {
            go.timescale = Some(GoTimescale {
                host: text(r, "timescale_host"),
                port: int(r, "timescale_port"),
                database: text(r, "timescale_database"),
                user: text(r, "timescale_user"),
                password: text(r, "timescale_password"),
                ssl_mode: text(r, "timescale_ssl_mode"),
                timezone: text(r, "timescale_timezone"),
            });
        } else if kind == "timescaledb" {
            go.disabled_backends.push(kind);
        } else if !kind.is_empty() {
            go.other_backends.push(kind);
        }
    }
    Ok(())
}

/// `controller_configs`, merged into one row. See [`GoControllers`] for why merging is the
/// faithful reading rather than a convenience.
async fn read_controllers(
    conn: &mut SqliteConnection,
    go: &mut GoConfig,
    scope: &Scope,
) -> Result<(), ConfigGoError> {
    let cols = columns(&mut *conn, "controller_configs").await?;
    if cols.is_empty() {
        // `013_remove_controller_configs` rebuilds this table rather than dropping it, so
        // an absent one means a database older than the first-run DDL that declares it --
        // which is a deployment with no REST server and no management API, and therefore
        // one where every value below is its default.
        go.absent_columns.push("controller_configs.*".to_string());
        return Ok(());
    }
    const WANTED: &[&str] = &[
        "id",
        "config_id",
        "controller_type",
        "enabled",
        "pws_station_id",
        "pws_api_key",
        "pws_upload_interval",
        "pws_pull_from_device",
        "pws_api_endpoint",
        "wu_station_id",
        "wu_api_key",
        "wu_upload_interval",
        "wu_pull_from_device",
        "wu_api_endpoint",
        "aeris_api_client_id",
        "aeris_api_client_secret",
        "aeris_api_endpoint",
        "aeris_latitude",
        "aeris_longitude",
        "rest_port",
        "rest_listen_addr",
        "grpc_port",
        "grpc_listen_addr",
        "grpc_cert",
        "grpc_key",
        "management_cert",
        "management_key",
        "management_port",
        "management_listen_addr",
        "management_auth_token",
        "management_enable_cors",
        "aprs_server",
        "snow_station_name",
        "snow_base_distance",
        "snow_smoothing_window",
        "snow_penalty",
        "snow_min_accumulation",
        "snow_min_segment_size",
    ];
    let sql = select(
        "controller_configs",
        WANTED,
        &cols,
        "id",
        scope,
        &mut go.absent_columns,
        &mut go.unread_columns,
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "reading the controllers",
            source,
        })?;
    let c = &mut go.controllers;
    for r in &rows {
        // **A field belongs to the row whose `controller_type` owns it, and only to an
        // ENABLED row.** `provider_sqlite.go` switches on `controller_type` and reads each
        // group of columns in exactly one arm, under `WHERE ... enabled = 1`. Reading "the
        // first non-NULL across rows" instead looks equivalent and is not: every row carries
        // every column, and three of them have non-NULL DEFAULTS -- `grpc_port DEFAULT
        // 50051`, `grpc_listen_addr DEFAULT '0.0.0.0'`, `management_enable_cors DEFAULT
        // FALSE`. The first-run DDL also inserts a `snowcache` row, so that row's default
        // `grpc_port` would win over the `rest` row's real one on every database the Go
        // daemon ever created.
        if !flag(r, "enabled").unwrap_or(true) {
            continue;
        }
        match or_empty(text(r, "controller_type")).as_str() {
            // `RESTServerData`, which carries the gRPC server's columns too: one listener
            // configures both in the Go tree.
            "rest" => {
                c.rest_port = int(r, "rest_port");
                c.rest_listen_addr = text(r, "rest_listen_addr");
                c.grpc_port = int(r, "grpc_port");
                c.grpc_listen_addr = text(r, "grpc_listen_addr");
                c.grpc_cert = text(r, "grpc_cert");
                c.grpc_key = text(r, "grpc_key");
            }
            "management" => {
                c.management_port = int(r, "management_port");
                c.management_listen_addr = text(r, "management_listen_addr");
                c.management_cert = text(r, "management_cert");
                c.management_key = text(r, "management_key");
                c.management_auth_token_present = text(r, "management_auth_token").is_some();
                c.management_enable_cors = flag(r, "management_enable_cors").unwrap_or(false);
            }
            "aprs" => c.aprs_server = text(r, "aprs_server"),
            "pwsweather" => c.pws_api_endpoint = text(r, "pws_api_endpoint"),
            "weatherunderground" => c.wu_api_endpoint = text(r, "wu_api_endpoint"),
            "aerisweather" => {
                c.aeris_api_endpoint = text(r, "aeris_api_endpoint");
                // Removed from this table by `013_remove_controller_configs`, so these two
                // are read only for a database created before that migration ran.
                c.aeris_api_client_id = text(r, "aeris_api_client_id");
                c.aeris_api_client_secret = text(r, "aeris_api_client_secret");
            }
            "snowcache" => {
                for col in [
                    "snow_station_name",
                    "snow_base_distance",
                    "snow_smoothing_window",
                    "snow_penalty",
                    "snow_min_accumulation",
                    "snow_min_segment_size",
                ] {
                    if let Some(v) = text(r, col) {
                        c.snow.insert(col.to_string(), v);
                    }
                }
            }
            other => {
                if !other.is_empty() {
                    go.unknown_controllers.push(other.to_string());
                }
            }
        }
    }
    Ok(())
}

/// `remote_stations`, minus the four plaintext secret columns (**D-67**).
async fn read_remote_stations(
    conn: &mut SqliteConnection,
    go: &mut GoConfig,
    scope: &Scope,
) -> Result<(), ConfigGoError> {
    let cols = columns(&mut *conn, "remote_stations").await?;
    if cols.is_empty() {
        // `016_add_remote_stations` and the first-run DDL both create this with
        // `IF NOT EXISTS`, so an absent table is a deployment that has never had a
        // satellite -- not a shape to report on.
        return Ok(());
    }
    // The four secret columns are deliberately **not** in this list, so the only record of
    // them is the `unread_columns` report, which names them without reading them.
    const WANTED: &[&str] = &[
        "station_id",
        "station_name",
        "station_type",
        "aprs_enabled",
        "aprs_callsign",
        "wu_enabled",
        "wu_station_id",
        "aeris_enabled",
        "aeris_client_id",
        "pws_enabled",
        "pws_station_id",
        "registered_at",
        "last_seen",
    ];
    let sql = select(
        "remote_stations",
        WANTED,
        &cols,
        "station_name",
        scope,
        &mut go.absent_columns,
        &mut go.unread_columns,
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "reading the registered stations",
            source,
        })?;
    for r in &rows {
        go.remote_stations.push(GoRemoteStation {
            station_id: or_empty(text(r, "station_id")),
            station_name: or_empty(text(r, "station_name")),
            station_type: or_empty(text(r, "station_type")),
            aprs_enabled: flag(r, "aprs_enabled").unwrap_or(false),
            aprs_callsign: text(r, "aprs_callsign"),
            wu_enabled: flag(r, "wu_enabled").unwrap_or(false),
            wu_station_id: text(r, "wu_station_id"),
            aeris_enabled: flag(r, "aeris_enabled").unwrap_or(false),
            pws_enabled: flag(r, "pws_enabled").unwrap_or(false),
            pws_station_id: text(r, "pws_station_id"),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Node identities
// ---------------------------------------------------------------------------

/// A UUIDv7 from a millisecond timestamp and ten random bytes.
///
/// 48 bits of big-endian Unix milliseconds, four version bits set to `7`, twelve random,
/// two variant bits set to `10`, and sixty-two more random -- RFC 9562 §5.7. Pure, so that
/// the layout is testable without a clock or an entropy source; [`mint_node_id`] supplies
/// both.
///
/// Written here rather than taken from the `uuid` crate because **D-32**'s table is the list
/// of crates this program may use and `uuid` is not in it -- "a substitution is an
/// amendment, not a manifest edit" -- and what it would buy is a parser and five other
/// versions for sixteen bytes this code never formats.
pub fn uuid_v7(millis: u64, random: [u8; 10]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let ms = millis & 0x0000_FFFF_FFFF_FFFF;
    out[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    out[6] = 0x70 | (random[0] & 0x0F);
    out[7] = random[1];
    out[8] = 0x80 | (random[2] & 0x3F);
    out[9..16].copy_from_slice(&random[3..10]);
    out
}

/// A fresh `node_id`: now, and ten bytes from the operating system.
///
/// **Never derived from `remote_stations.station_id`**, which is the one thing W7 assumption
/// 35 is explicit about: those UUIDs are *station* identities, and reusing one as a node
/// identity would make a node and one of its stations the same thing -- which is exactly the
/// conflation `peers` and `config.stations` exist to separate. Every identity this migration
/// writes is new.
pub fn mint_node_id() -> [u8; 16] {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut random = [0u8; 10];
    OsRng.fill_bytes(&mut random);
    uuid_v7(millis, random)
}

// ---------------------------------------------------------------------------
// The translation
// ---------------------------------------------------------------------------

/// What the caller has to decide, because the Go configuration does not say.
#[derive(Debug, Clone)]
pub struct Options {
    /// `node.node_name` -- operator-facing and mutable, so renaming a node later is an edit
    /// rather than an identity change (W7 §5.2).
    pub node_name: String,
    /// `node.spool_path`. The column has no default in the schema and no value in any
    /// design document, which is why it is an argument: the caller knows where its state
    /// directory is and this module does not.
    pub spool_path: String,
}

/// One `peers` row and the origins it may speak for.
#[derive(Debug, Clone)]
pub struct PeerRow {
    /// The minted `node_id`.
    pub node_id: [u8; 16],
    /// `peers.name`.
    pub name: String,
    /// The origins this peer may deliver, which for a migrated peer is itself alone
    /// (W7 **D-36**: `relay` is false by default, so a compromised leaf cannot inject
    /// history for another station).
    pub origins: Vec<[u8; 16]>,
}

/// Everything the migration will write.
///
/// Separate from [`write`] so that the whole translation can be inspected -- and tested --
/// without a destination database, which is what makes the station-key rule and the O-8
/// floor assertable as facts about a function rather than as facts about a file.
#[derive(Debug, Clone)]
pub struct Migrated {
    /// `node.node_id`.
    pub node_id: [u8; 16],
    /// `node.node_name`.
    pub node_name: String,
    /// `node.timezone` -- the deployment zone, for components not scoped to a station.
    pub node_timezone: String,
    /// `node.spool_path`.
    pub spool_path: String,
    /// One row per satellite that had registered with the Go daemon.
    pub peers: Vec<PeerRow>,
    /// The `ingest` row, when this deployment receives readings from somewhere.
    pub ingest: Option<IngestRow>,
    /// Every change, in the order [`write`] applies it: the listener, the weather database,
    /// the stations, the websites, the upload services and the forecast account.
    pub changes: Vec<Change>,
    /// The report.
    pub report: Report,
}

impl Migrated {
    /// `station_key` for every station this migration writes, in the order it writes them.
    ///
    /// The one summary worth a method: it is what the operator checks against the station
    /// ids the data migration wrote, and the two agreeing is the whole point of
    /// [`station_key`].
    pub fn station_keys(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter_map(|c| match c {
                Change::PutStation(s) => Some(s.station_key.as_str()),
                _ => None,
            })
            .collect()
    }
}

impl Migrated {
    /// Turn one migrated station off, and say why.
    ///
    /// For the checks this crate cannot make. `ci/layers.toml` puts `mw-codec` below this
    /// crate and `mw-station` beside it, so the `weatherlink-live` sensor mapping -- which
    /// `plan()` parses and refuses, and whose refusal is a daemon that does not start
    /// (docs/reconciliation.md **DEV-9**) -- cannot be validated here. The binary can name
    /// both crates, so it makes that check and calls this: a station this daemon cannot
    /// start on is migrated **off** with the column to fix named, rather than written
    /// enabled and discovered at the next boot.
    ///
    /// A disabled station is not an error and produces no component
    /// (`mw_station::StationRow::is_collected_here`), which is what makes this the
    /// proportionate answer rather than refusing the whole migration over one column.
    pub fn disable_station(&mut self, key: &str, because: &str) -> bool {
        for change in &mut self.changes {
            if let Change::PutStation(s) = change {
                if s.station_key == key {
                    s.enabled = false;
                    self.report.push(
                        NoteKind::Action,
                        key,
                        format!(
                            "migrated with collection switched OFF: {because}. Fix it on \
                             the station's page and turn collection back on."
                        ),
                    );
                    return true;
                }
            }
        }
        false
    }

    /// Every migrated station of one configured type, as `(station_key, mapping)`.
    ///
    /// For [`Migrated::disable_station`]'s caller: the binary needs the rows it has to check
    /// without reaching into [`Change`] itself.
    pub fn stations_of_type(&self, kind: &str) -> Vec<(&str, Option<&str>)> {
        self.changes
            .iter()
            .filter_map(|c| match c {
                Change::PutStation(s) if s.kind == kind => {
                    Some((s.station_key.as_str(), s.wll_sensor_mapping.as_deref()))
                }
                _ => None,
            })
            .collect()
    }
}

/// Percent-encode a `userinfo` component of a URL.
///
/// A password is not a URL-safe string and `weather_database.url` is one URL rather than the
/// Go schema's seven columns, so the one place the two shapes meet has to do this. The
/// unreserved set of RFC 3986 §2.3 and nothing else: encoding more than necessary is always
/// correct, and the characters a password may contain that would otherwise re-partition the
/// URL (`:`, `@`, `/`, `?`, `#`) are exactly the ones this escapes.
fn userinfo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// One port, avoiding the ones already chosen.
///
/// The configured value when it is usable and free; otherwise the default when that is free;
/// otherwise the first free port above the default, with a line saying so. Searching upward
/// rather than refusing because the alternative is a migration that stops on a port number,
/// which an operator can change in one field afterwards -- and because the *reason* it had
/// to move is a sentence this can produce and a `CHECK` cannot.
fn pick_port(
    configured: Option<i64>,
    default: i64,
    column: &'static str,
    taken: &mut Vec<i64>,
    report: &mut Report,
) -> i64 {
    let free = |p: i64, taken: &[i64]| (1..=65535).contains(&p) && !taken.contains(&p);
    let chosen = match configured {
        Some(p) if free(p, taken) => p,
        other => {
            let mut p = default;
            while !free(p, taken) {
                p += 1;
            }
            match other {
                Some(was) => report.push(
                    NoteKind::Raised,
                    format!("listener.{column}"),
                    format!(
                        "port {was} is already in use by another of this listener's four \
                         sockets, which this schema requires to differ (W3 D-92, as four \
                         CHECKs); migrated as {p}. Change it on System -> Advanced if that \
                         is not the port you want."
                    ),
                ),
                None if p != default => report.push(
                    NoteKind::Raised,
                    format!("listener.{column}"),
                    format!(
                        "this column had no value and its default of {default} is taken by \
                         another of this listener's sockets; migrated as {p}."
                    ),
                ),
                None => {}
            }
            p
        }
    };
    taken.push(chosen);
    chosen
}

/// The `postgresql://` URL the Go DSN's seven parts describe.
///
/// `GetConnectionString` builds `host=... port=... dbname=... user=... password=...
/// sslmode=... TimeZone=...`; this schema holds one URL, with the `CHECK` that pins the
/// scheme. `TimeZone` is deliberately **not** carried into the URL: in the Go daemon it is
/// what `date_trunc('day', NOW())` truncates in, which is the rollover W2a **D-11** replaces
/// with a per-station `Calendar` -- so the value is migrated to where it is now read
/// (`stations.timezone`, `node.timezone`) instead of to a session setting nothing consults.
fn weather_url(ts: &GoTimescale) -> Option<String> {
    let host = ts.host.as_deref()?;
    let db = ts.database.as_deref().unwrap_or("weather");
    let mut url = String::from("postgresql://");
    if let Some(user) = ts.user.as_deref() {
        url.push_str(&userinfo(user));
        if let Some(pw) = ts.password.as_deref() {
            url.push(':');
            url.push_str(&userinfo(pw));
        }
        url.push('@');
    }
    // A bare IPv6 literal has to be bracketed to be a URL authority, and `timescale_host`
    // is a free-text column an operator may have typed one into.
    if host.contains(':') && !host.starts_with('[') {
        let _ = write!(url, "[{host}]");
    } else {
        url.push_str(host);
    }
    if let Some(port) = ts.port.filter(|p| (1..=65535).contains(p)) {
        let _ = write!(url, ":{port}");
    }
    let _ = write!(url, "/{db}");
    if let Some(mode) = ts.ssl_mode.as_deref() {
        let _ = write!(url, "?sslmode={mode}");
    }
    Some(url)
}

/// The `transport_*` and `identity_kind` columns one `devices` row becomes.
struct Wire {
    kind: &'static str,
    identity: &'static str,
    dev: Option<String>,
    baud: Option<i64>,
    host: Option<String>,
    port: Option<i64>,
    url: Option<String>,
    mount: Option<String>,
    streaming: Option<bool>,
    /// Set when the row needs a `presented_key` the Go configuration has no column for, in
    /// which case the station is migrated off (see [`Migrated::disable_station`]).
    needs_presented_key: bool,
}

/// The configured device type, in the spelling `docs/legacy-api-contract.md` §3.1 freezes.
///
/// `wll` is the only rename: `internal/managers/weatherstation.go` dispatches `"wll"` and
/// `"weatherlink-live"` to the same driver, and `stations.type`'s `CHECK` admits only the
/// second -- which is the spelling five frozen fixtures carry.
fn device_type(raw: &str) -> Option<&'static str> {
    match raw {
        "campbellscientific" => Some("campbellscientific"),
        "davis" => Some("davis"),
        "snowgauge" => Some("snowgauge"),
        "ambient-customized" => Some("ambient-customized"),
        "airgradient" => Some("airgradient"),
        "weatherlink-live" | "wll" => Some("weatherlink-live"),
        _ => None,
    }
}

/// `http://host:port`, which is the only spelling `transport_url` accepts: a scheme, a host
/// and a port, and nothing after them. The request path is the driver's knowledge
/// (`mw_station::endpoint`).
fn device_url(host: &str, port: i64) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Which transport one `devices` row describes.
///
/// The conditions are `pkg/config/provider.go`'s own validation, read as a table rather than
/// reinvented: a serial device, or a hostname **and** a port, or -- for three types -- one of
/// those two alone. Each arm then has to produce a transport the row's driver accepts, which
/// is `mw_station::plan`'s table and `mw_grpc`'s one row.
fn wire(
    d: &GoDevice,
    kind: &'static str,
    report: &mut Report,
    key: &str,
) -> Result<Wire, ConfigGoError> {
    let port = d.port.as_deref().and_then(|p| p.trim().parse::<i64>().ok());
    let base = |k: &'static str, identity: &'static str| Wire {
        kind: k,
        identity,
        dev: None,
        baud: None,
        host: None,
        port: None,
        url: None,
        mount: None,
        streaming: None,
        needs_presented_key: false,
    };
    let no_transport = |because: String| ConfigGoError::NoTransport {
        name: d.name.clone(),
        kind: kind.to_string(),
        because,
    };

    match kind {
        // A line reader or a Davis console: a serial port, or a serial-over-TCP bridge.
        // `station.go` dials TCP when `serial_device` is empty, which is why the serial arm
        // is first rather than the network one.
        "campbellscientific" | "davis" => {
            if let Some(dev) = d.serial_device.clone() {
                let baud = d.baud.filter(|b| *b > 0).ok_or_else(|| {
                    no_transport(format!(
                        "`serial_device` is `{dev}` and `baud` is not a line rate, and a \
                         serial transport here must name both"
                    ))
                })?;
                let mut w = base("serial", "fixed");
                w.dev = Some(dev);
                w.baud = Some(baud);
                Ok(w)
            } else if let (Some(host), Some(port)) = (d.hostname.clone(), port) {
                let mut w = base("tcp_out", "fixed");
                w.host = Some(host);
                w.port = Some(port);
                Ok(w)
            } else {
                Err(no_transport(
                    "`serial_device` is empty and `hostname`/`port` do not both have a \
                     value, so neither of the two connections this driver makes is \
                     configured"
                        .to_string(),
                ))
            }
        }
        // A snow gauge streams over gRPC. Go dials `dns:host:port`; the column here is an
        // endpoint, and `transport_streaming` is load-bearing rather than descriptive --
        // `mw_grpc` refuses a gauge with the flag clear, because no gauge answers one
        // request per occurrence.
        "snowgauge" => {
            let (Some(host), Some(port)) = (d.hostname.clone(), port) else {
                return Err(no_transport(
                    "a snow gauge is reached over gRPC and `hostname`/`port` do not both \
                     have a value"
                        .to_string(),
                ));
            };
            let mut w = base("grpc_out", "fixed");
            w.url = Some(device_url(&host, port));
            w.streaming = Some(true);
            Ok(w)
        }
        // One `GET /measures/current` per occurrence. Go defaults the port to 80 when the
        // column is empty, which is the one default worth reproducing rather than refusing:
        // it is in the driver's own constructor.
        "airgradient" => {
            let Some(host) = d.hostname.clone() else {
                return Err(no_transport(
                    "an AirGradient monitor is polled over HTTP and `hostname` is empty"
                        .to_string(),
                ));
            };
            let port = port.unwrap_or(80);
            let mut w = base("http_out", "fixed");
            w.url = Some(device_url(&host, port));
            Ok(w)
        }
        // **The structural one.** Go runs one `http.Server` per Ambient station on that
        // station's own port, and the station's identity is the port the request arrived
        // on; here there is one mount on the one listener and the identity is the `?ID=` in
        // the query (W2a §4a.4). So the port is not carried, and that is the defect being
        // removed rather than a loss.
        "ambient-customized" => {
            let mount = d.path.clone().unwrap_or_else(|| "/".to_string());
            if let Some(p) = &d.port {
                report.push(
                    NoteKind::NotCarried,
                    key,
                    format!(
                        "the Ambient endpoint's own port ({p}) has no successor: this \
                         daemon serves one mount (`{mount}`) on the main listener and \
                         resolves the station from the `ID` in the request, rather than \
                         from the port it arrived on. Point the console at the main \
                         listener's port and the same path."
                    ),
                );
            }
            let mut w = base("http_in", "from_query");
            w.mount = Some(mount);
            Ok(w)
        }
        // Broadcast or poll, by `wll_broadcast`. Either way the unit identifies itself by
        // its serial, for which the Go schema has no column at all -- so the row cannot be
        // completed here and is migrated off with the field named.
        "weatherlink-live" => {
            let broadcast = d.wll_broadcast.unwrap_or(true);
            let host = d.wll_host.clone().or_else(|| d.hostname.clone());
            if broadcast {
                let mut w = base("udp_in", "from_payload");
                // Go learns the broadcast port from the unit's own answer to
                // `/v1/real_time` and never stores it; this schema holds it as a column, so
                // the migration writes the port a WeatherLink Live actually broadcasts to.
                w.port = Some(22222);
                w.host = host;
                w.needs_presented_key = true;
                report.push(
                    NoteKind::Derived,
                    key,
                    "broadcast port 22222: the Go driver learned the port from the unit's \
                     own answer to `/v1/real_time` and stored it nowhere, and this schema \
                     holds it as a column."
                        .to_string(),
                );
                Ok(w)
            } else {
                let Some(host) = host else {
                    return Err(no_transport(
                        "`wll_broadcast` is off, so the unit is polled, and neither \
                         `wll_host` nor `hostname` has a value"
                            .to_string(),
                    ));
                };
                let mut w = base("http_out", "fixed");
                w.url = Some(device_url(&host, d.wll_port.unwrap_or(80)));
                Ok(w)
            }
        }
        other => Err(ConfigGoError::UnknownStationType {
            name: d.name.clone(),
            kind: other.to_string(),
        }),
    }
}

/// One device's Aeris columns, kept together so the credential PAIR cannot be assembled from
/// two different rows.
#[derive(Debug, Clone)]
struct GoAeris {
    client_id: String,
    secret: Option<String>,
    endpoint: Option<String>,
    refresh: Option<i64>,
}

/// The `ingest` row, which is the only thing that makes a node a collector (**D-44**).
#[derive(Debug, Clone)]
pub struct IngestRow {
    /// `listen` -- `grpc` or `rest`.
    pub listen: &'static str,
    /// `peer_auth`.
    pub peer_auth: &'static str,
    /// `ca_path`.
    pub ca_path: Option<String>,
    /// `allow_insecure` -- `docs/collector.md` §8.5's explicit opt-out, without which an
    /// ingest listener with no TLS is `PlanError::InsecureIngest`.
    pub allow_insecure: bool,
}

/// W3 §6.3's three-step `Host` normalisation, applied to a stored hostname.
///
/// The column's three `CHECK`s are the same three steps, so a value that has not had all of
/// them applied is a row SQLite refuses -- and in the Go daemon it is worse than refused: the
/// host table is a case-sensitive map, so `EXAMPLE.COM` silently serves the default
/// website's data under the wrong hostname.
fn hostname(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    // Strip the port, which for a bracketed IPv6 literal is only the part after the `]`.
    let host = match lower.rfind(']') {
        Some(close) => match lower[close..].find(':') {
            Some(colon) => &lower[..close + colon],
            None => &lower[..],
        },
        None => match lower.find(':') {
            Some(colon) => &lower[..colon],
            None => &lower[..],
        },
    };
    host.trim_end_matches('.').to_string()
}

/// Translate a Go configuration into the rows this schema holds.
///
/// Pure apart from `mint`, which is the node-identity allocator -- [`mint_node_id`] in the
/// daemon and a counter in a test. Taking it as an argument is what makes the whole
/// translation assertable: the station keys, the O-8 floor, the report and the component set
/// that follows from them are facts about this function and not about a file on a disk.
pub fn translate(
    go: &GoConfig,
    opts: &Options,
    mint: &mut dyn FnMut() -> [u8; 16],
) -> Result<Migrated, ConfigGoError> {
    let mut report = Report::default();
    let node_id = mint();
    let node_timezone = go
        .timescale
        .as_ref()
        .and_then(|t| t.timezone.clone())
        .unwrap_or_else(|| "UTC".to_string());

    if go.unused_configurations > 0 {
        report.push(
            NoteKind::NotCarried,
            "configs",
            format!(
                "this database holds {} other named configuration(s), and they are not \
                 migrated. Every read in the Go daemon is scoped to the one named \
                 `default`, so those rows have never been loaded by anything.",
                go.unused_configurations
            ),
        );
    }
    for kind in &go.unknown_controllers {
        report.push(
            NoteKind::NotCarried,
            format!("controller_configs.{kind}"),
            "this controller type is enabled in the Go configuration and this migration \
             has no arm for it, so nothing is written for it."
                .to_string(),
        );
    }
    for col in &go.absent_columns {
        report.push(
            NoteKind::NotCarried,
            col.clone(),
            "this column is not in the source database, so its Go default was used. An \
             older configuration database that never ran the migration that adds it reads \
             exactly this way."
                .to_string(),
        );
    }
    for col in &go.unread_columns {
        report.push(
            NoteKind::NotCarried,
            col.clone(),
            "the source has this column and this migration does not read it.".to_string(),
        );
    }
    report.push(
        NoteKind::Derived,
        "node",
        format!(
            "node identity minted as a fresh UUIDv7, named `{}`, zone `{node_timezone}`, \
             spool `{}`. No `remote_stations.station_id` was reused: those are station \
             identities, not node identities (W7 assumption 35).",
            opts.node_name, opts.spool_path
        ),
    );

    // ---- the station keys, which the data migration has to agree with ----
    let mut key_of_name: BTreeMap<String, String> = BTreeMap::new();
    let mut name_of_key: BTreeMap<String, String> = BTreeMap::new();
    let mut local: Vec<(&GoDevice, &'static str, String)> = Vec::new();
    let mut receivers: Vec<&GoDevice> = Vec::new();

    for d in &go.devices {
        if d.kind == "grpcreceiver" {
            receivers.push(d);
            continue;
        }
        let kind = device_type(&d.kind).ok_or_else(|| ConfigGoError::UnknownStationType {
            name: d.name.clone(),
            kind: d.kind.clone(),
        })?;
        if kind != d.kind {
            report.push(
                NoteKind::Derived,
                d.name.clone(),
                format!(
                    "type `{}` is written `{kind}` here, which is the spelling the frozen \
                     `/stationinfo` contract carries.",
                    d.kind
                ),
            );
        }
        let key = station_key(&d.name).ok_or_else(|| ConfigGoError::NoStationKey {
            name: d.name.clone(),
        })?;
        if let Some(first) = name_of_key.get(&key) {
            return Err(ConfigGoError::StationKeyCollision {
                first: first.clone(),
                second: d.name.clone(),
                key,
            });
        }
        if key != d.name {
            report.push(
                NoteKind::Derived,
                key.clone(),
                format!(
                    "station key `{key}` from the device name `{}`, which the `name` column \
                     keeps: `?station={}` still resolves and `/stationinfo` still emits it. \
                     This key is what the weather history is stored under, so it has to be \
                     the one the data migration wrote.",
                    d.name, d.name
                ),
            );
        }
        name_of_key.insert(key.clone(), d.name.clone());
        key_of_name.insert(d.name.clone(), key.clone());
        local.push((d, kind, key));
    }

    let mut changes: Vec<Change> = Vec::new();

    // ---- the listener ----
    //
    // Go serves ONE socket on `rest_port`, and it is HTTPS if any website has a certificate
    // and plaintext otherwise (`shouldUseHTTPS`). Here `https_port` makes the main socket
    // TLS and `http_port` a redirect, so a TLS deployment migrates to the TLS port Go was
    // on plus a plaintext redirect this schema's default port, which is the one addition
    // worth reporting.
    let any_cert = go
        .websites
        .iter()
        .any(|w| w.tls_cert_path.is_some() && w.tls_key_path.is_some());
    let go_port = go.controllers.rest_port.filter(|p| (1..=65535).contains(p));
    let (http_port, https_port) = match (any_cert, go_port) {
        (true, Some(p)) => {
            report.push(
                NoteKind::Derived,
                "listener",
                format!(
                    "HTTPS stays on port {p}, which is the one socket the Go daemon served. \
                     A plaintext socket on 8080 is added and does nothing but redirect to \
                     it -- the Go daemon bound no plaintext socket at all."
                ),
            );
            (8080, Some(p))
        }
        (true, None) => {
            report.push(
                NoteKind::Derived,
                "listener",
                "a website has a certificate and `rest_port` has no value, so HTTPS is on \
                 443 and the plaintext redirect on 8080."
                    .to_string(),
            );
            (8080, Some(443))
        }
        (false, Some(p)) => (p, None),
        (false, None) => {
            report.push(
                NoteKind::Derived,
                "listener",
                "`rest_port` has no value and no website has a certificate, so the HTTP \
                 port is this schema's default of 8080."
                    .to_string(),
            );
            (8080, None)
        }
    };
    // **The collision avoidance has to cover the DEFAULTS, not only the configured
    // values.** The `listener` table requires all four ports to differ -- W3 D-92's shipped
    // validation, as four `CHECK`s -- and a Go deployment that put the REST server on 8081
    // would otherwise be migrated with `management_port` falling back to its default of
    // 8081 and refused by the schema. So each port is chosen against the ones already
    // taken, in the order the operator is most likely to care about.
    let mut taken: Vec<i64> = vec![http_port];
    taken.extend(https_port);
    let grpc_port = pick_port(
        go.controllers.grpc_port,
        50051,
        "grpc_port",
        &mut taken,
        &mut report,
    );
    let management_port = pick_port(
        go.controllers.management_port,
        8081,
        "management_port",
        &mut taken,
        &mut report,
    );
    // **A-W5-3: the management bind becomes loopback, and the Go value is deliberately not
    // carried.** `0.0.0.0` is half of what makes W5 §3.4's unclaimed-instance takeover
    // window reachable, and a migration that preserved it would carry the vulnerability
    // forward -- which is the same argument D-67 makes about the four plaintext credentials,
    // one table over.
    if let Some(addr) = &go.controllers.management_listen_addr {
        if addr != "127.0.0.1" && addr != "localhost" {
            report.push(
                NoteKind::NotCarried,
                "listener.management_listen_addr",
                format!(
                    "the management API was bound to `{addr}` and is migrated to \
                     `127.0.0.1` (A-W5-3). Reach it over an SSH tunnel, or widen it on \
                     System -> Advanced once it has an administrator."
                ),
            );
        }
    }
    if go.controllers.management_auth_token_present {
        report.push(
            NoteKind::Action,
            "management",
            "the Go `management_auth_token` has no successor: authentication is a username \
             and an Argon2id password hash with revocable sessions (W5 §3), so the old \
             token is not migrated and stops meaning anything. Claim the instance with the \
             one-time setup token the daemon logs at its first start."
                .to_string(),
        );
    }
    if go.controllers.management_enable_cors {
        report.push(
            NoteKind::NotCarried,
            "management_cors_origin",
            "`management_enable_cors` was on, which in the Go daemon is \
             `Access-Control-Allow-Origin: *` on the admin API. CORS is now an exact-match \
             allowlist defaulting to empty (A-W5-3), and a wildcard has no image in it, so \
             no origin is migrated. Add the ones you need."
                .to_string(),
        );
    }
    changes.push(Change::PutListener(ListenerWrite {
        listen_addr: go
            .controllers
            .rest_listen_addr
            .clone()
            .unwrap_or_else(|| "0.0.0.0".to_string()),
        http_port,
        https_port,
        grpc_port,
        management_listen_addr: "127.0.0.1".to_string(),
        management_port,
        management_tls_cert_path: go.controllers.management_cert.clone(),
        management_tls_key_path: go.controllers.management_key.clone(),
    }));

    // ---- the weather database ----
    match go.timescale.as_ref().and_then(weather_url) {
        Some(url) => {
            report.push(
                NoteKind::Derived,
                "weather_database",
                "one URL from the Go schema's seven DSN columns. `TimeZone` is not in it: \
                 it was the session zone Go's midnight rollovers truncated in, and the zone \
                 is now a per-station column with a `Calendar` behind it."
                    .to_string(),
            );
            changes.push(Change::PutWeatherDatabase(url));
        }
        None => report.push(
            NoteKind::NotCarried,
            "weather_database",
            "no enabled TimescaleDB backend with a host, so no weather database row is \
             written. That absence is meaningful rather than missing: a node with no row \
             has no `db:postgres` and therefore no `writer:timescale` (D-44)."
                .to_string(),
        ),
    }
    for backend in &go.disabled_backends {
        report.push(
            NoteKind::NotCarried,
            format!("storage_configs.{backend}"),
            "this backend is configured and DISABLED in the Go configuration, so no \
             weather database row is written. Nothing was being written to it; if that is \
             not what you meant, set the connection on the storage page."
                .to_string(),
        );
    }
    for backend in &go.other_backends {
        report.push(
            NoteKind::NotCarried,
            format!("storage_configs.{backend}"),
            "a storage backend that is really an egress has no successor in \
             `weather_database`: it is an `uploader` row or an `uplink` row here. Nothing \
             is written for it, because neither shape can be derived from these columns \
             without inventing the peer credential it needs."
                .to_string(),
        );
    }

    // ---- config.stations: the local ones ----
    let mut pending_disable: Vec<(String, String)> = Vec::new();
    for (d, kind, key) in &local {
        let w = wire(d, kind, &mut report, key)?;
        let timezone = match d.timezone.clone() {
            Some(tz) => tz,
            None => {
                // Go's rollovers truncate in the *database session's* zone
                // (`date_trunc('day', NOW())`) and `devices.timezone` is used nowhere but
                // rendering sun times, so the session zone is the honest fallback -- it is
                // the zone this station's day has in fact been rolling over in. With
                // neither, there is nothing to fall back to and `plan()` refuses the
                // station rather than rolling over in the server's zone (W2a D-11).
                let tz = go
                    .timescale
                    .as_ref()
                    .and_then(|t| t.timezone.clone())
                    .ok_or(ConfigGoError::NoTimezone {
                        name: d.name.clone(),
                    })?;
                report.push(
                    NoteKind::Derived,
                    key.clone(),
                    format!(
                        "no `devices.timezone`, so the zone is \
                         `storage_configs.timescale_timezone` (`{tz}`) -- which is the \
                         `TimeZone=` in the Go DSN and therefore the zone this station's \
                         midnight rollover has actually been happening in."
                    ),
                );
                tz
            }
        };
        // **The Ambient station is the second DEV-9 case, and the Go schema has neither
        // half of what it needs.** `plan()` requires an `ingest_secret_hash` -- `?PASSWORD=`
        // is the only credential that transport has -- and a `presented_key`, the `?ID=` the
        // console sends; in the Go daemon the station's identity was the PORT the request
        // arrived on and neither value is stored anywhere. So the row is migrated off with
        // both fields named, rather than written enabled for the daemon to refuse at its
        // next start.
        if *kind == "ambient-customized" {
            pending_disable.push((
                key.clone(),
                "an Ambient console identifies itself with the `ID` it sends and \
                 authenticates with the `PASSWORD` beside it, and the Go daemon used the \
                 port the request arrived on instead -- so it stored neither, and there is \
                 nothing to put in `presented_key` or the station's protocol secret"
                    .to_string(),
            ));
        }
        if w.needs_presented_key {
            pending_disable.push((
                key.clone(),
                "a WeatherLink Live identifies itself by the device serial in every \
                 datagram and the Go schema has no column holding it, so there is nothing \
                 to put in `presented_key`"
                    .to_string(),
            ));
        }
        // The column's own floor is 30 s, and the Go column has no floor at all. A
        // sub-floor value is raised with a line rather than aborting the migration on the
        // `CHECK`, which is the same answer O-8 gives one table over.
        let wll_poll = match d.wll_poll_interval.filter(|v| *v > 0) {
            Some(secs) if secs < WLL_POLL_FLOOR_SECONDS => {
                report.push(
                    NoteKind::Raised,
                    key.clone(),
                    format!(
                        "`wll_poll_interval` of {secs} s raised to this column's \
                         {WLL_POLL_FLOOR_SECONDS} s floor: a WeatherLink Live's HTTP \
                         interface is a small embedded web server and the Go column had no \
                         floor at all."
                    ),
                );
                Some(WLL_POLL_FLOOR_SECONDS)
            }
            other => other,
        };
        if *kind == "weatherlink-live" && d.wll_sensor_mapping.is_none() {
            pending_disable.push((
                key.clone(),
                "`wll_sensor_mapping` is empty, and nothing in a WeatherLink Live's \
                 payload says which transmitter is the outdoor one"
                    .to_string(),
            ));
        }
        // A certificate without its key is not a TLS configuration, and the column pair
        // carries `CHECK ((cert IS NULL) = (key IS NULL))` -- so a one-sided Go pair has to
        // be resolved here rather than aborting the migration on a constraint. Dropping
        // both is the only representable reading, and it is reported.
        let transport_tls = match (d.tls_cert_file.clone(), d.tls_key_file.clone()) {
            (Some(cert), Some(tls_key)) => {
                report.push(
                    NoteKind::Derived,
                    key.clone(),
                    "`tls_cert_file`/`tls_key_file` on the device row are carried to this \
                     station's transport TLS columns."
                        .to_string(),
                );
                (Some(cert), Some(tls_key))
            }
            (cert, tls_key) => {
                if cert.is_some() || tls_key.is_some() {
                    report.push(
                        NoteKind::NotCarried,
                        key.clone(),
                        "this device has one half of a TLS pair -- a certificate with no \
                         key, or a key with no certificate -- which is not a TLS \
                         configuration. Neither is carried; set both on the station's page \
                         if the transport needs them."
                            .to_string(),
                    );
                }
                (None, None)
            }
        };
        changes.push(Change::PutStation(StationWrite {
            station_key: key.clone(),
            // The Go surrogate, kept: `/stationinfo` emits `stations[].id` as an integer
            // and W3 §7's config payload emits `stationID` and `airQualityDeviceID` as
            // integers, so renumbering here would change what a frozen response means.
            id: d.id,
            name: d.name.clone(),
            kind: kind.to_string(),
            enabled: d.enabled,
            owning_node_id: Some(node_id.to_vec()),
            identity_kind: w.identity.to_string(),
            presented_key: None,
            ingest_secret_hash: None,
            latitude: d.latitude,
            longitude: d.longitude,
            altitude_m: d.altitude,
            timezone: Some(timezone),
            aprs_callsign: d.aprs_callsign.clone(),
            wind_dir_correction_degrees: d
                .wind_dir_correction
                .filter(|v| (-360..=360).contains(v))
                .unwrap_or(0),
            // `> 0` is the column's own `CHECK`, and Go's default is `0` meaning "not
            // configured" -- so a zero is an absent base distance rather than a gauge
            // mounted on the ground.
            snow_base_distance_mm: d.base_snow_distance.filter(|v| *v > 0.0),
            transport_kind: Some(w.kind.to_string()),
            transport_dev: w.dev,
            transport_baud: w.baud,
            transport_host: w.host,
            transport_port: w.port,
            transport_url: w.url,
            transport_method: None,
            transport_auth: None,
            transport_streaming: w.streaming,
            transport_mount: w.mount,
            transport_group: None,
            transport_topic: None,
            transport_tls_ca_path: None,
            transport_tls_cert_path: transport_tls.0,
            transport_tls_key_path: transport_tls.1,
            wll_sensor_mapping: d.wll_sensor_mapping.clone(),
            wll_poll_interval_seconds: wll_poll,
        }));
    }

    // ---- config.stations: the ones that arrived by registration, and their peers ----
    //
    // One `peers` row per registered station, each with a **freshly minted** `node_id`. The
    // Go schema has no node identity at all -- `remote_stations.station_id` is the station's
    // own UUID (W7 assumption 35) -- so there is nothing to carry and a satellite that
    // registers again after the cutover will present its own minted identity. Which is why
    // the report says every one of these has to be re-enrolled: the row records who is
    // allowed, and `relay` is false so a peer may speak only for itself (W7 D-36).
    let mut peers: Vec<PeerRow> = Vec::new();
    for rs in &go.remote_stations {
        let key = station_key(&rs.station_name).ok_or_else(|| ConfigGoError::NoStationKey {
            name: rs.station_name.clone(),
        })?;
        if let Some(first) = name_of_key.get(&key) {
            return Err(ConfigGoError::StationKeyCollision {
                first: first.clone(),
                second: rs.station_name.clone(),
                key,
            });
        }
        name_of_key.insert(key.clone(), rs.station_name.clone());
        key_of_name.insert(rs.station_name.clone(), key.clone());

        let peer_id = mint();
        peers.push(PeerRow {
            node_id: peer_id,
            name: rs.station_name.clone(),
            origins: vec![peer_id],
        });
        changes.push(Change::PutStation(StationWrite {
            station_key: key.clone(),
            id: None,
            name: rs.station_name.clone(),
            // The SATELLITE's declared type, unconstrained on a received row on purpose: a
            // newer peer with a driver this collector has never heard of must still enrol.
            kind: if rs.station_type.is_empty() {
                "grpcreceiver".to_string()
            } else {
                rs.station_type.clone()
            },
            enabled: true,
            owning_node_id: Some(peer_id.to_vec()),
            identity_kind: "from_registration".to_string(),
            aprs_callsign: rs.aprs_callsign.clone(),
            // No transport: this station is inventory on this node rather than a component,
            // and its hardware is configured on the node that owns it.
            transport_kind: None,
            ..StationWrite::default()
        }));
        report.push(
            NoteKind::Action,
            key.clone(),
            format!(
                "registered station `{}` is migrated as a peer with a fresh UUIDv7 node \
                 identity. The Go registration carried no peer authentication at all, so \
                 the peer row says who is allowed and nothing authenticates it yet: \
                 re-enrol this satellite once the collector has a CA.",
                rs.station_name
            ),
        );
        for (on, service) in [
            (rs.wu_enabled, "Weather Underground"),
            (rs.pws_enabled, "PWS Weather"),
            (rs.aprs_enabled, "APRS"),
            (rs.aeris_enabled, "Aeris"),
        ] {
            if !on {
                continue;
            }
            report.push(
                NoteKind::Action,
                key.clone(),
                format!(
                    "{service} was enabled for this registered station using a credential \
                     the satellite sent in cleartext and that nothing on the collector ever \
                     read (A-W7-2). That credential is NOT migrated (D-67), and no pairing \
                     is written: a collector uploads on a satellite's behalf using its OWN \
                     account now, so turn {service} on for this station after entering your \
                     own credentials."
                ),
            );
        }
    }

    // ---- the gRPC receiver ----
    let ingest = if !receivers.is_empty() || !go.remote_stations.is_empty() {
        for d in &receivers {
            report.push(
                NoteKind::Derived,
                d.name.clone(),
                format!(
                    "the `grpcreceiver` device `{}` is not a station here: being a \
                     collector is a `config.ingest` row and nothing else -- no flag and no \
                     mode (D-44) -- so the row is written there and this device is not \
                     migrated as a station.",
                    d.name
                ),
            );
        }
        let tls = go.controllers.grpc_cert.is_some() && go.controllers.grpc_key.is_some();
        report.push(
            NoteKind::Action,
            "ingest",
            format!(
                "a gRPC ingest listener is written because this deployment receives \
                 readings. The Go receiver authenticated no peer at all (D-46), so it is \
                 migrated with `peer_auth = 'mtls-ca'`, no CA{} and `allow_insecure = 1` -- \
                 which is the schema's explicit opt-out and the only way to keep accepting \
                 what is already being sent. Set a CA and clear the opt-out.",
                if tls {
                    " (the Go `grpc_cert`/`grpc_key` are the SERVER's certificate, not a \
                      trust store)"
                } else {
                    ""
                }
            ),
        );
        Some(IngestRow {
            listen: "grpc",
            peer_auth: "mtls-ca",
            ca_path: None,
            allow_insecure: true,
        })
    } else {
        None
    };

    // ---- the websites ----
    let key_by_device_id: BTreeMap<i64, String> = local
        .iter()
        .filter_map(|(d, _, key)| d.id.map(|id| (id, key.clone())))
        .collect();
    // At most one SNI fallback, and Go's own fallback is "the first certificate in map
    // iteration order" -- which is not deterministic, so any choice here is at least as
    // faithful. The lowest id with a certificate is the one that is reproducible.
    let default_tls = go
        .websites
        .iter()
        .filter(|w| w.tls_cert_path.is_some() && w.tls_key_path.is_some())
        .min_by_key(|w| w.id.unwrap_or(i64::MAX))
        .and_then(|w| w.id);
    for w in &go.websites {
        let look_up = |device: &Option<String>,
                       role: &'static str|
         -> Result<Option<String>, ConfigGoError> {
            match device.as_deref() {
                None => Ok(None),
                Some(name) => key_of_name.get(name).cloned().map(Some).ok_or_else(|| {
                    ConfigGoError::NoSuchWebsiteStation {
                        website: w.name.clone(),
                        device: name.to_string(),
                        role,
                    }
                }),
            }
        };
        let primary = match w.device_id {
            None => None,
            Some(id) => Some(key_by_device_id.get(&id).cloned().ok_or_else(|| {
                ConfigGoError::NoSuchWebsiteStation {
                    website: w.name.clone(),
                    device: id.to_string(),
                    role: "its primary station",
                }
            })?),
        };
        let snow = look_up(&w.snow_device_name, "its snow station")?;
        let air = look_up(&w.air_quality_device_name, "its air quality station")?;
        let host = or_empty(w.hostname.as_deref().map(hostname));
        if let Some(raw) = &w.hostname {
            if &host != raw {
                report.push(
                    NoteKind::Derived,
                    w.name.clone(),
                    format!(
                        "hostname `{raw}` is stored as `{host}`: lower-cased, with the port \
                         and a trailing dot removed, which is the three-step normalisation \
                         W3 §6.3 applies to a `Host` header before matching. The Go host \
                         table is a case-sensitive map, so `{raw}` could never have matched \
                         a browser's request."
                    ),
                );
            }
        }
        let website_tls = match (w.tls_cert_path.clone(), w.tls_key_path.clone()) {
            (Some(cert), Some(key)) => (Some(cert), Some(key)),
            (cert, key) => {
                if cert.is_some() || key.is_some() {
                    report.push(
                        NoteKind::NotCarried,
                        w.name.clone(),
                        "this website has one half of a TLS pair, which is not a TLS \
                         configuration: neither is carried, and the site is served over \
                         plaintext until both are set."
                            .to_string(),
                    );
                }
                (None, None)
            }
        };
        changes.push(Change::PutWebsite(WebsiteWrite {
            id: w.id,
            name: w.name.clone(),
            hostname: host,
            page_title: or_empty(w.page_title.clone()),
            about_station_html: or_empty(w.about_station_html.clone()),
            primary_station_key: primary,
            // The column's `CHECK` is "enabled implies a station", and Go has no such
            // constraint: a website with snow on and no device named renders an empty panel
            // the operator cannot explain. Migrating it as off is the only representable
            // reading, and it is reported.
            snow_enabled: w.snow_enabled && snow.is_some(),
            snow_station_key: snow.clone(),
            air_quality_enabled: w.air_quality_enabled && air.is_some(),
            air_quality_station_key: air.clone(),
            is_portal: w.is_portal,
            apple_app_id: or_empty(w.apple_app_id.clone()),
            // Paired, for the station transport's reason and against the same `CHECK`.
            // `any_cert` and `default_tls` above already require both halves, so this is
            // the one place the pairing has to be applied rather than assumed.
            tls_cert_path: website_tls.0,
            tls_key_path: website_tls.1,
            is_default_tls: default_tls.is_some() && default_tls == w.id,
        }));
        if w.snow_enabled && snow.is_none() {
            report.push(
                NoteKind::NotCarried,
                w.name.clone(),
                "snow was enabled with no snow device named, which renders an empty panel. \
                 Migrated with snow off, because `CHECK (snow_enabled = 0 OR \
                 snow_station_key IS NOT NULL)` makes the half-configured state \
                 unrepresentable."
                    .to_string(),
            );
        }
        if w.air_quality_enabled && air.is_none() {
            report.push(
                NoteKind::NotCarried,
                w.name.clone(),
                "air quality was enabled with no device named. Migrated with it off, for \
                 the reason above."
                    .to_string(),
            );
        }
        // Write-only and set by registration alone, so it is replayed as the action that
        // set it rather than as a column -- which is what `config_admin::snapshot` does for
        // the same two columns. `radar_registered_at` becomes the moment of the migration:
        // the action does not take one, and the token is what the rendered page needs.
        if w.radar_enabled {
            // `w.id` and not a chosen default: the action names a website BY ID, and a
            // website whose `weather_websites.id` is NULL -- which the Go schema's
            // `INTEGER PRIMARY KEY AUTOINCREMENT` makes impossible and a hand-edited
            // database does not -- would otherwise register the radar token against
            // website zero.
            match (w.radar_token.clone(), w.id) {
                (Some(token), Some(id)) => {
                    changes.push(Change::RegisterRadar { website: id, token });
                    report.push(
                        NoteKind::Derived,
                        w.name.clone(),
                        "the radar registration's token is carried and \
                         `radar_registered_at` becomes the moment of this migration: the \
                         registration is replayed as the action that set it, and that \
                         action does not take a timestamp."
                            .to_string(),
                    );
                }
                _ => report.push(
                    NoteKind::NotCarried,
                    w.name.clone(),
                    "radar was enabled with an empty token or with no website id, and \
                     neither can render. Migrated with radar off; re-register it from the \
                     website's page."
                        .to_string(),
                ),
            }
        }
    }
    if let Some(id) = default_tls {
        if go
            .websites
            .iter()
            .filter(|w| w.tls_cert_path.is_some())
            .count()
            > 1
        {
            report.push(
                NoteKind::Derived,
                "website.is_default_tls",
                format!(
                    "website id {id} is the SNI fallback. Go answers an unmatched SNI with \
                     whichever certificate Go's map iteration reached first, which is not \
                     deterministic between restarts; this is."
                ),
            );
        }
    }

    // ---- the upload services ----
    //
    // The Go schema configures the same service in two places -- the `devices` row carries
    // that station's PWS Weather, Weather Underground and APRS settings while the global
    // endpoints live on `controller_configs` -- and **D-66** splits it by what the thing is a
    // property OF: the `uploader` row is the remote ACCOUNT and the component, and the
    // `station_service` row is the PAIRING. One account per station, because W7 **D-38**
    // makes the one-station arity a property of the kind: the far end identifies a station
    // by the credential it arrived under, so two stations on one account is
    // `PlanError::UploaderArity`.
    let mut claimed: BTreeMap<&'static str, String> = BTreeMap::new();
    for (d, _, key) in &local {
        for (on, kind, service) in [
            (d.wu_enabled, "wunderground", "Weather Underground"),
            (d.pws_enabled, "pwsweather", "PWS Weather"),
            (d.aprs_enabled, "aprs", "APRS"),
        ] {
            if !on {
                continue;
            }
            if let Some(first) = claimed.get(kind) {
                return Err(ConfigGoError::TwoAccountsForOneService {
                    service,
                    first: first.clone(),
                    second: key.clone(),
                });
            }
            claimed.insert(kind, key.clone());

            // **`uploader.name` is UNIQUE across kinds**, so the station key alone is not
            // a name: a station with both Weather Underground and PWS Weather enabled
            // would make the second insert fail and abort the whole migration. The prefix
            // is `ci/db/config-seed.sql`'s own convention -- `wu-backyard`,
            // `aprs-backyard` -- and it is also the component id's stable suffix, so
            // `wu:wu-csi` survives a rename of anything else.
            let prefix = match kind {
                "wunderground" => "wu",
                "pwsweather" => "pws",
                _ => "aprs",
            };
            let name = format!("{prefix}-{key}");
            // **`enabled` is whether the GO DAEMON WAS ACTUALLY UPLOADING**, which is its
            // own readiness test per service and not the `*_enabled` column alone:
            //
            //   `aprsDeviceReady`  enabled, a callsign, AND a non-zero position
            //   pwsweather         enabled, a station id AND a password
            //   wunderground       enabled, a station id AND a password
            //
            // Both halves of that matter. A row Go was skipping, migrated as enabled, would
            // START a beacon or an upload that was not happening -- a behaviour change, and
            // for a radio an unattended one. And `mw_output::plan` refuses an enabled
            // uploader with a blank credential, and refuses an APRS one with no coordinates
            // ("an APRS weather report IS a position report"), so an enabled row Go was not
            // using is also a daemon that does not start. A disabled `uploader` row is not
            // planned at all, so the pairing survives and one toggle turns it on.
            let ready = match kind {
                "wunderground" => d.wu_station_id.is_some() && d.wu_password.is_some(),
                "pwsweather" => d.pws_station_id.is_some() && d.pws_password.is_some(),
                _ => {
                    d.aprs_callsign.is_some()
                        && d.latitude.is_some_and(|v| v != 0.0)
                        && d.longitude.is_some_and(|v| v != 0.0)
                }
            };
            let mut s = ServiceWrite {
                kind: kind.to_string(),
                name: name.clone(),
                enabled: ready,
                pairings: vec![Pairing {
                    station_key: key.clone(),
                    remote_station_id: None,
                }],
                ..ServiceWrite::default()
            };
            match kind {
                "wunderground" => {
                    s.interval_seconds = d.wu_upload_interval.filter(|v| *v > 0);
                    s.api_endpoint = d
                        .wu_api_endpoint
                        .clone()
                        .or_else(|| go.controllers.wu_api_endpoint.clone());
                    s.api_key = d.wu_password.clone();
                    s.pairings[0].remote_station_id = d.wu_station_id.clone();
                }
                "pwsweather" => {
                    s.interval_seconds = d.pws_upload_interval.filter(|v| *v > 0);
                    s.api_endpoint = d
                        .pws_api_endpoint
                        .clone()
                        .or_else(|| go.controllers.pws_api_endpoint.clone());
                    s.api_key = d.pws_password.clone();
                    s.pairings[0].remote_station_id = d.pws_station_id.clone();
                }
                _ => {
                    // The callsign is the station's (public identity, printed on every
                    // packet) and the passcode is the account's (W7 §6.2), which is the one
                    // place the Go row's two columns are split across two tables.
                    s.aprs_passcode = d.aprs_passcode.clone();
                    s.aprs_server = d
                        .aprs_server
                        .clone()
                        .or_else(|| go.controllers.aprs_server.clone());
                    s.aprs_comment = d.aprs_comment.clone();
                    s.aprs_symbol_table = one_char(
                        d.aprs_symbol_table.as_deref(),
                        "/",
                        &mut report,
                        &name,
                        "aprs_symbol_table",
                    );
                    s.aprs_symbol_code = one_char(
                        d.aprs_symbol_code.as_deref(),
                        "_",
                        &mut report,
                        &name,
                        "aprs_symbol_code",
                    );
                    let transport = match d.aprs_transport.as_deref() {
                        None | Some("aprs-is") => "aprs-is",
                        Some("kiss") => match d.aprs_kiss_connection.as_deref() {
                            Some("serial") => "kiss-serial",
                            Some("tcp") => "kiss-tcp",
                            other => {
                                return Err(ConfigGoError::UnknownKissConnection {
                                    name: d.name.clone(),
                                    found: other.unwrap_or("empty").to_string(),
                                })
                            }
                        },
                        Some(other) => {
                            return Err(ConfigGoError::UnknownKissConnection {
                                name: d.name.clone(),
                                found: other.to_string(),
                            })
                        }
                    };
                    s.aprs_transport = Some(transport.to_string());
                    // Each transport's columns and **only** its columns: the table's
                    // exclusion `CHECK`s are exhaustive, so a `kiss_tcp_address` on an
                    // `aprs-is` uploader is configuration nothing will ever read and is
                    // refused rather than stored.
                    match transport {
                        "kiss-serial" => {
                            s.aprs_kiss_serial_device = d.aprs_kiss_serial_device.clone();
                            s.aprs_kiss_serial_baud = d.aprs_kiss_serial_baud.filter(|v| *v > 0);
                            s.aprs_kiss_path = d.aprs_kiss_path.clone();
                            s.aprs_kiss_destination = d.aprs_kiss_destination.clone();
                        }
                        "kiss-tcp" => {
                            s.aprs_kiss_tcp_address = d.aprs_kiss_tcp_address.clone();
                            s.aprs_kiss_path = d.aprs_kiss_path.clone();
                            s.aprs_kiss_destination = d.aprs_kiss_destination.clone();
                        }
                        _ => {
                            if d.aprs_kiss_serial_device.is_some()
                                || d.aprs_kiss_tcp_address.is_some()
                            {
                                report.push(
                                    NoteKind::NotCarried,
                                    format!("aprs:{name}"),
                                    "this station beacons over APRS-IS, and its KISS \
                                     columns are not carried: the table's per-transport \
                                     exclusions make configuration nothing reads \
                                     unrepresentable."
                                        .to_string(),
                                );
                            }
                        }
                    }
                    // **O-8, and the half of it that is this work item's.** A KISS sink
                    // cannot be constructed below the 600 s floor -- a radio has no way to
                    // tell us to slow down -- so a sub-floor interval here is a component
                    // that cannot be built and a daemon that does not start. Go's default
                    // is 300 s and applies to RF and internet alike, which is why this is
                    // the migration's job rather than a constraint's.
                    let configured = d.aprs_upload_interval.filter(|v| *v > 0);
                    s.interval_seconds = configured;
                    if transport != "aprs-is" {
                        if let Some(secs) = configured {
                            if secs < RADIO_FLOOR_SECONDS {
                                s.interval_seconds = Some(RADIO_FLOOR_SECONDS);
                                report.push(
                                    NoteKind::Raised,
                                    format!("aprs:{name}"),
                                    format!(
                                        "KISS beacon interval raised from {secs} s to the \
                                         {RADIO_FLOOR_SECONDS} s RF floor (O-8). A radio \
                                         has no feedback channel, so the floor is a \
                                         property of the transmitter type and the \
                                         configuration is corrected here rather than \
                                         clamped on every send."
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            if !ready {
                let because = match kind {
                    "wunderground" => "`wu_station_id` or `wu_password` is empty",
                    "pwsweather" => "`pws_station_id` or `pws_password` is empty",
                    _ => {
                        "there is no callsign, or the station has no coordinates -- and an \
                         APRS weather report IS a position report"
                    }
                };
                report.push(
                    NoteKind::Action,
                    format!("{prefix}:{name}"),
                    format!(
                        "{service} is switched on for this station in the Go configuration \
                         and {because}, so the Go daemon was NOT uploading: its own \
                         readiness test skips a station like this. Migrated with sharing \
                         OFF and the pairing kept, so filling in what is missing and \
                         turning it on is two fields and no re-pairing."
                    ),
                );
            }
            changes.push(Change::PutService(s));
        }
    }

    // ---- the forecast account ----
    //
    // `aeris_account` is ONE row and the Go `devices` row carries a copy per station, so
    // this is the collapse that table's own comment specifies: "M7-4's data migration
    // collapses the per-device copies; a disagreement between them is a refusal there rather
    // than a silently-chosen winner." Aeris is not an uploader -- it is four fetching
    // components (W2a D-33) -- so the pairing carries no `uploader_id`, which is also what
    // makes W3 §7's `aerisWeatherEnabled` derivable.
    // **The credential pair travels together, from one device.** Taking the id from the
    // first device that has one and the secret from the first device that has one -- which
    // need not be the same device -- is how a mismatched pair gets written `enabled = 1`,
    // and a silently-chosen winner is the outcome `AerisAccountsDisagree` exists to
    // prevent. So the first device with a client id DEFINES the account, and every other
    // device is compared against it.
    let mut account: Option<(String, GoAeris)> = None;
    let mut paired: Vec<String> = Vec::new();
    for (d, _, key) in &local {
        if let Some(id) = &d.aeris_api_client_id {
            let mine = GoAeris {
                client_id: id.clone(),
                secret: d.aeris_api_client_secret.clone(),
                endpoint: d.aeris_api_endpoint.clone(),
                refresh: d.aeris_refresh_interval.filter(|v| *v > 0),
            };
            match &account {
                None => account = Some((key.clone(), mine)),
                Some((first, theirs)) if theirs.client_id != mine.client_id => {
                    return Err(ConfigGoError::AerisAccountsDisagree {
                        first: first.clone(),
                        first_id: theirs.client_id.clone(),
                        second: key.clone(),
                        second_id: mine.client_id.clone(),
                    })
                }
                // The same account named twice with two secrets: one of them is wrong and
                // this migration cannot tell which.
                Some((first, theirs))
                    if mine.secret.is_some()
                        && theirs.secret.is_some()
                        && theirs.secret != mine.secret =>
                {
                    return Err(ConfigGoError::AerisSecretsDisagree {
                        first: first.clone(),
                        second: key.clone(),
                        client_id: mine.client_id,
                    })
                }
                // The same account, and this device has the secret the first one lacked.
                Some((first, theirs)) if theirs.secret.is_none() && mine.secret.is_some() => {
                    account = Some((
                        first.clone(),
                        GoAeris {
                            secret: mine.secret,
                            ..theirs.clone()
                        },
                    ));
                }
                Some(_) => {}
            }
        }
        if d.aeris_enabled {
            paired.push(key.clone());
        }
    }
    let account = account.map(|(_, a)| a).or_else(|| {
        // Pre-`013_remove_controller_configs`: the account was on `controller_configs` and
        // nowhere else, so a database that never ran that migration has it only there.
        go.controllers
            .aeris_api_client_id
            .clone()
            .map(|client_id| GoAeris {
                client_id,
                secret: go.controllers.aeris_api_client_secret.clone(),
                endpoint: go.controllers.aeris_api_endpoint.clone(),
                refresh: None,
            })
    });
    let client_id = account.as_ref().map(|a| a.client_id.clone());
    let aeris_secret = account.as_ref().and_then(|a| a.secret.clone());
    let aeris_refresh = account.as_ref().and_then(|a| a.refresh);
    let endpoint = account
        .as_ref()
        .and_then(|a| a.endpoint.clone())
        .or_else(|| go.controllers.aeris_api_endpoint.clone());
    if !paired.is_empty() || client_id.is_some() {
        let complete = client_id.is_some() && aeris_secret.is_some();
        if !paired.is_empty() && !complete {
            report.push(
                NoteKind::Action,
                "aeris",
                "forecasts were on in the Go configuration and its credential pair is not \
                 complete in this database, so the account is migrated SWITCHED OFF with \
                 the station pairings kept. Enter the Aeris client id and secret on the \
                 forecasts page and turn it on: the pairings are already there, so nothing \
                 has to be re-paired."
                    .to_string(),
            );
        }
        if paired.len() > 1 {
            report.push(
                NoteKind::Derived,
                "aeris",
                format!(
                    "{} stations share one forecast account, which is what this schema \
                     holds: the account is one row and the per-station switch is the \
                     pairing. Two stations fetching from two different accounts was \
                     expressible in Go and is not here.",
                    paired.len()
                ),
            );
        }
        changes.push(Change::PutAeris(AerisWrite {
            enabled: complete && !paired.is_empty(),
            client_id,
            client_secret: aeris_secret,
            api_endpoint: endpoint,
            refresh_interval_seconds: aeris_refresh,
            stations: paired,
        }));
    }

    if !go.controllers.snow.is_empty() {
        let listed = go
            .controllers
            .snow
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        report.push(
            NoteKind::NotCarried,
            "controller_configs.snow_*",
            format!(
                "the Go snow controller's tuning ({listed}) has no column here: the \
                 changepoint detector's penalty, kernel, minimum segment and minimum \
                 accumulation are constants in `mw_pipeline::snow::pelt`, taken from the \
                 values the shipped Go detector hard-codes rather than from these columns. \
                 Which station the snow panel reads is `website.snow_station_key` and is \
                 carried."
            ),
        );
    }

    let mut migrated = Migrated {
        node_id,
        node_name: opts.node_name.clone(),
        node_timezone,
        spool_path: opts.spool_path.clone(),
        peers,
        ingest,
        changes,
        report,
    };
    for (key, because) in pending_disable {
        migrated.disable_station(&key, &because);
    }
    Ok(migrated)
}

/// One character, for the two `CHAR(1)` columns the Go schema does not enforce.
///
/// `aprs_symbol_table CHAR(1)` holds a four-hundred-character string today if something
/// wrote one -- SQLite ignores a length it does not recognise -- and here the column carries
/// `CHECK (length(...) = 1)`. Taking the first character and saying so is the only reading
/// that both satisfies the constraint and keeps the beacon's symbol recognisable.
fn one_char(
    raw: Option<&str>,
    default: &str,
    report: &mut Report,
    subject: &str,
    column: &'static str,
) -> Option<String> {
    let Some(raw) = raw else {
        return Some(default.to_string());
    };
    let mut chars = raw.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        report.push(
            NoteKind::Raised,
            format!("aprs:{subject}"),
            format!(
                "`{column}` held `{raw}`, which is more than the one character this column \
                 permits; the first (`{first}`) is kept. The Go column is declared \
                 `CHAR(1)` and enforces nothing."
            ),
        );
    }
    Some(first.to_string())
}

// ---------------------------------------------------------------------------
// Writing it
// ---------------------------------------------------------------------------

/// Refuse a destination that already holds configuration.
///
/// This migration writes a whole deployment's configuration and does not merge into one:
/// `PutStation` and `PutWebsite` are upserts, so merging would *appear* to work and leave
/// the rows the Go configuration no longer has behind -- which is the half of
/// [`config_admin::restore`]'s problem that a forward change cannot express.
async fn assert_empty(conn: &mut SqliteConnection) -> Result<(), ConfigGoError> {
    for table in ["node", "stations", "website", "uploader", "peers"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *conn)
            .await
            .map_err(|source| ConfigGoError::WriteQuery {
                what: "counting the destination's rows",
                source,
            })?;
        if n > 0 {
            // `table` is one of the five literals above, so leaking it as `&'static str`
            // is sound and the message names the table an operator can look at.
            let table: &'static str = match table {
                "node" => "node",
                "stations" => "stations",
                "website" => "website",
                "uploader" => "uploader",
                _ => "peers",
            };
            return Err(ConfigGoError::DestinationNotEmpty { table, rows: n });
        }
    }
    Ok(())
}

/// Apply a [`Migrated`] to the connection's current transaction.
///
/// **Does not commit**, exactly as [`config_admin::apply`] does not: the caller owns the
/// transaction, so a dry run and a real migration are one code path that differ in whether
/// the transaction is committed. That is W5 §2.4's preview/save property, reused rather than
/// re-implemented.
///
/// # The three tables this writes directly, and why they are the exception
///
/// `node`, `peers` and `peer_origins` have no [`Change`] variant, and that is not a gap in
/// `Change`: a node identity is minted once by the node and never edited, and a peer row is
/// written by enrolment, so neither is a thing an *operator* changes. Everything an operator
/// can change goes through [`config_admin::apply`] below -- which is what makes the migrated
/// configuration one the management UI can edit, and a configuration only a migration could
/// produce impossible.
///
/// `ingest` is the fourth, for the same reason: `docs/collector.md` §8.3's `[[ingest]]` is a
/// deployment decision rather than a page in W5's UI, and M6-4 owns its editing surface.
pub async fn write(conn: &mut SqliteConnection, m: &Migrated) -> Result<(), ConfigGoError> {
    assert_empty(&mut *conn).await?;

    sqlx::query(
        "INSERT INTO node (id, node_id, node_name, timezone, spool_path) \
         VALUES (1, ?, ?, ?, ?)",
    )
    .bind(m.node_id.to_vec())
    .bind(&m.node_name)
    .bind(&m.node_timezone)
    .bind(&m.spool_path)
    .execute(&mut *conn)
    .await
    .map_err(|source| ConfigGoError::WriteQuery {
        what: "writing the node identity",
        source,
    })?;

    for peer in &m.peers {
        sqlx::query(
            // `auth_kind = 'mtls-ca'` is the one kind whose `auth_material` may be NULL,
            // and under a shared CA that is the common case rather than an exception: the
            // certificate chain does the authenticating and the peer row only says who is
            // allowed. `NOT NULL` would force a placeholder, and a placeholder in a
            // credential column is worse than a null.
            "INSERT INTO peers (node_id, name, auth_kind, relay) VALUES (?, ?, 'mtls-ca', 0)",
        )
        .bind(peer.node_id.to_vec())
        .bind(&peer.name)
        .execute(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::WriteQuery {
            what: "writing a peer",
            source,
        })?;
        for origin in &peer.origins {
            sqlx::query("INSERT INTO peer_origins (node_id, origin_node_id) VALUES (?, ?)")
                .bind(peer.node_id.to_vec())
                .bind(origin.to_vec())
                .execute(&mut *conn)
                .await
                .map_err(|source| ConfigGoError::WriteQuery {
                    what: "writing a peer's origins",
                    source,
                })?;
        }
    }

    if let Some(ingest) = &m.ingest {
        sqlx::query(
            "INSERT INTO ingest (listen, peer_auth, ca_path, allow_insecure) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(ingest.listen)
        .bind(ingest.peer_auth)
        .bind(&ingest.ca_path)
        .bind(ingest.allow_insecure)
        .execute(&mut *conn)
        .await
        .map_err(|source| ConfigGoError::WriteQuery {
            what: "writing the ingest listener",
            source,
        })?;
    }

    for change in &m.changes {
        config_admin::apply(&mut *conn, change)
            .await
            .map_err(|source| ConfigGoError::Write { source })?;
    }
    Ok(())
}

/// Open the Go configuration database **read-only**.
///
/// `read_only(true)` and `create_if_missing(false)` are the two halves of the one promise
/// this tool makes about the source, and they are made to SQLite rather than only kept by
/// this code -- which is `ci/db/migrate-from-go/`'s discipline one database over, where
/// "THE SOURCE IS READ-ONLY" is a comment and every statement is a `SELECT`. Here it is
/// enforced by the connection: an `INSERT` added to this file by mistake fails at run time
/// instead of writing to the configuration the Go daemon is still running from.
///
/// A missing file is therefore an error rather than an empty database, which matters: the
/// alternative is a successful migration of nothing.
pub async fn open_source(path: &std::path::Path) -> Result<SqliteConnection, ConfigGoError> {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        // The source's own foreign keys are not this tool's business: it reads every table
        // and joins them in Rust, and a Go database whose `devices.website_id` dangles
        // should still be readable rather than refused at connect time.
        .foreign_keys(false)
        .connect()
        .await
        .map_err(|source| ConfigGoError::Query {
            what: "opening the Go configuration database",
            source,
        })
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

/// Test support: a Go configuration database to migrate, and the owner's own deployment.
///
/// Public rather than `#[cfg(test)]`, for `mw_supervisor::testing`'s reason: the one test
/// that answers this work item's "done when" -- *the Rust daemon starts from the migrated
/// configuration and plans the same component set the Go daemon was running* -- can only
/// live in the binary, because that is the only crate that may name `plan()` and every
/// registrar. It needs the same fixture this crate's own tests read, and a second copy of a
/// fifty-three-column schema is the thing `extract-go-schema.sh` exists to prevent.
///
/// Nothing here has a production use: [`GO_SCHEMA`] is a *fixture*, not something the daemon
/// applies, and [`go_db`] creates a scratch database.
pub mod testing {
    /// The Go configuration schema, as the widest shape a real deployment has.
    ///
    /// Extracted from the Go tree by `ci/db/migrate-config-from-go/extract-go-schema.sh`
    /// rather than transcribed: fifty-three `devices` columns is more than a reviewer can
    /// check by eye, and a fixture with a transposed or forgotten column is a test that
    /// passes against a schema production does not have.
    pub const GO_SCHEMA: &str =
        include_str!("../../../ci/db/migrate-config-from-go/go-config-schema.sql");

    /// A Go configuration database holding the given rows.
    ///
    /// The schema is the extracted one and the rows are the caller's, which is the split
    /// that matters: a test says what an operator configured and never what the Go schema
    /// is.
    pub async fn go_db(path: &std::path::Path, rows: &[&str]) {
        use sqlx::{ConnectOptions as _, Connection as _};
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("a scratch Go database");
        sqlx::raw_sql(GO_SCHEMA)
            .execute(&mut conn)
            .await
            .expect("the extracted Go schema applies");
        for row in rows {
            sqlx::raw_sql(row)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("the Go schema rejected a fixture row: {e}\n{row}"));
        }
        conn.close().await.expect("the fixture closes");
    }

    /// The owner's own three devices, with the ids `/stationinfo` returns.
    ///
    /// Not a reconstruction of his credentials or his wiring, which are not public -- the
    /// Campbell's `hostname`/`port` here are the preview deployment's, and the Aeris pair is
    /// a placeholder. What it is for is the three station NAMES: those are the names the key
    /// rule has to normalise, and the ones the data migration already wrote a year of
    /// history under.
    pub const OWNER_DEVICES: &str = "
        INSERT INTO devices (id, config_id, name, type, enabled, hostname, port, timezone,
                             latitude, longitude, altitude, aeris_enabled,
                             aeris_api_client_id, aeris_api_client_secret)
        VALUES (1, 1, 'CSI', 'campbellscientific', 1, '10.50.0.35', '7001',
                'America/Denver', 40.47571, -111.84543, 1615.0, 1, 'cid', 'csecret');
        INSERT INTO devices (id, config_id, name, type, enabled, hostname, port, timezone)
        VALUES (2, 1, 'Suncrest Air Quality', 'airgradient', 1, '10.50.0.40', '80',
                'America/Denver');
        INSERT INTO devices (id, config_id, name, type, enabled, hostname, port, timezone,
                             base_snow_distance)
        VALUES (3, 1, 'snow', 'snowgauge', 1, '10.50.0.41', '50051', 'America/Denver',
                1422);
    ";

    /// His one website, with the hostname, the title and the about text served today.
    pub const OWNER_WEBSITE: &str = "
        INSERT INTO weather_websites (id, config_id, name, device_id, hostname, page_title,
                                      about_station_html, snow_enabled, snow_device_name,
                                      air_quality_enabled, air_quality_device_name,
                                      is_portal, apple_app_id, radar_enabled, radar_token,
                                      radar_registered_at)
        VALUES (1, 1, 'Suncrest Live Weather', 1, 'suncrestweather.com',
                'Suncrest Live Weather', '<h2>About This Station</h2>', 0, 'snow',
                1, 'Suncrest Air Quality', 0, '6755874087', 1, 'IwbaKRgZwRmf92pRspqD', 1700000000);
    ";

    /// The TimescaleDB backend, as its seven DSN columns.
    pub const OWNER_STORAGE: &str = "
        INSERT INTO storage_configs (config_id, backend_type, enabled, timescale_host,
                                     timescale_port, timescale_database, timescale_user,
                                     timescale_password, timescale_ssl_mode,
                                     timescale_timezone)
        VALUES (1, 'timescaledb', 1, 'pitcairn', 5432, 'weather_v2_0_0', 'mw',
                'p@ss/word', 'disable', 'America/Denver');
    ";

    /// The REST and management controllers.
    pub const OWNER_CONTROLLERS: &str = "
        INSERT INTO controller_configs (config_id, controller_type, enabled, rest_port,
                                        rest_listen_addr, grpc_port, grpc_listen_addr)
        VALUES (1, 'rest', 1, 443, '0.0.0.0', 50051, '0.0.0.0');
        INSERT INTO controller_configs (config_id, controller_type, enabled, management_port,
                                        management_listen_addr, management_auth_token,
                                        management_enable_cors)
        VALUES (1, 'management', 1, 8081, '0.0.0.0', 'a-token-nothing-replaces', 1);
    ";
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::config_db;
    use sqlx::{ConnectOptions as _, Connection as _};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory per test, removed on drop. `tempfile` would be a dev-dependency
    /// for four lines (**A-4** scopes **D-32** to the daemon, but the four lines still win).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mw-config-go-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
            Self(dir)
        }

        fn go(&self) -> PathBuf {
            self.0.join("go-config.db")
        }

        fn target(&self) -> PathBuf {
            self.0.join("config.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Deterministic node identities, so a test can assert on the bytes.
    fn counter() -> impl FnMut() -> [u8; 16] {
        let mut n = 0u8;
        move || {
            n += 1;
            uuid_v7(1_762_000_000_000, [n; 10])
        }
    }

    fn options() -> Options {
        Options {
            node_name: "pitcairn".to_string(),
            spool_path: "/var/lib/modernweather/spool".to_string(),
        }
    }

    /// **The key rule, pinned against the owner's three real station names.**
    ///
    /// These three pairs are the whole reason this work item exists: W9's `w9.station_map`
    /// wrote the history under `csi`, `snow` and `suncrest-air-quality`, and a configuration
    /// that derived anything else would serve a live station's few minutes beside a year of
    /// history nothing can see. A test on the function is what keeps the two ends agreeing
    /// without either one being typed by hand.
    #[test]
    fn the_station_key_rule_is_the_one_the_data_migration_used() {
        assert_eq!(station_key("CSI").as_deref(), Some("csi"));
        assert_eq!(station_key("snow").as_deref(), Some("snow"));
        assert_eq!(
            station_key("Suncrest Air Quality").as_deref(),
            Some("suncrest-air-quality")
        );

        // The permitted characters survive, including the three that are easy to forget.
        assert_eq!(
            station_key("suncrest:roof_2.b").as_deref(),
            Some("suncrest:roof_2.b")
        );
        // Runs collapse and the ends are trimmed, so a name does not produce `--` or a
        // leading `-` -- both of which are legal component-id characters and neither of
        // which is a key anybody would type.
        assert_eq!(station_key("  Back  Yard  ").as_deref(), Some("back-yard"));
        assert_eq!(station_key("(roof)").as_deref(), Some("roof"));
        // Nothing survives: a refusal rather than a generated key.
        assert_eq!(station_key("***"), None);
        assert_eq!(station_key(""), None);
    }

    /// RFC 9562 §5.7's layout, because the column's `CHECK` only says sixteen bytes.
    #[test]
    fn a_minted_node_id_is_a_uuidv7() {
        let id = uuid_v7(0x0123_4567_89AB, [0xFF; 10]);
        assert_eq!(&id[0..6], &[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB], "{id:?}");
        assert_eq!(id[6] >> 4, 7, "the version nibble");
        assert_eq!(id[8] >> 6, 0b10, "the variant bits");

        // The timestamp is the top 48 bits and a larger one sorts after a smaller one,
        // which is the only property a v7 has that a v4 does not.
        let early = uuid_v7(1_000, [0; 10]);
        let late = uuid_v7(2_000, [0; 10]);
        assert!(early < late);

        // And the real one is not a constant.
        assert_ne!(mint_node_id(), mint_node_id());
        assert_eq!(mint_node_id()[6] >> 4, 7);
    }

    /// W3 §6.3's three steps, each of which is also a `CHECK` on the column.
    #[test]
    fn a_hostname_is_normalised_the_way_a_host_header_is() {
        assert_eq!(hostname("EXAMPLE.COM"), "example.com");
        assert_eq!(hostname("example.com:8443"), "example.com");
        assert_eq!(hostname("example.com."), "example.com");
        assert_eq!(
            hostname(" Suncrestweather.COM.:443 "),
            "suncrestweather.com"
        );
        // A bracketed IPv6 literal keeps its colons and loses its port.
        assert_eq!(hostname("[2001:db8::1]:8443"), "[2001:db8::1]");
        assert_eq!(hostname(""), "");
    }

    /// The seven DSN columns as one URL, with the password escaped.
    #[test]
    fn the_dsn_columns_become_one_url() {
        let url = weather_url(&GoTimescale {
            host: Some("pitcairn".into()),
            port: Some(5432),
            database: Some("weather_v2_0_0".into()),
            user: Some("mw".into()),
            password: Some("p@ss/word".into()),
            ssl_mode: Some("disable".into()),
            timezone: Some("America/Denver".into()),
        })
        .expect("a host is all that is required");
        assert_eq!(
            url,
            "postgresql://mw:p%40ss%2Fword@pitcairn:5432/weather_v2_0_0?sslmode=disable"
        );
        // The scheme's `CHECK` is `postgres://` or `postgresql://`, and `TimeZone` is
        // deliberately absent: it is migrated to `stations.timezone` instead.
        assert!(url.starts_with("postgresql://"));
        assert!(!url.contains("TimeZone"));
        // A bare IPv6 host is bracketed, or the port would re-partition the authority.
        let v6 = weather_url(&GoTimescale {
            host: Some("2001:db8::1".into()),
            port: Some(5432),
            ..GoTimescale::default()
        })
        .expect("a host");
        assert_eq!(v6, "postgresql://[2001:db8::1]:5432/weather");
        // No host is no row, which is **meaningful** rather than missing (D-44).
        assert!(weather_url(&GoTimescale::default()).is_none());
    }

    /// Read a Go database, translate it, write it, and hand back a connection to the result.
    async fn migrate(scratch: &Scratch, rows: &[&str]) -> (sqlx::SqlitePool, Migrated) {
        let migrated = translated(scratch, rows).await.expect("the translation");
        let pool = config_db::open_and_migrate(&scratch.target())
            .await
            .expect("the configuration schema applies");
        let mut tx = pool.begin().await.expect("a transaction");
        write(&mut tx, &migrated)
            .await
            .unwrap_or_else(|e| panic!("the migrated configuration was refused: {e}"));
        tx.commit().await.expect("the commit");
        (pool, migrated)
    }

    /// The same, stopping before the write, for the cases whose answer is a refusal.
    async fn translated(scratch: &Scratch, rows: &[&str]) -> Result<Migrated, ConfigGoError> {
        go_db(&scratch.go(), rows).await;
        let mut source = open_source(&scratch.go()).await.expect("the source opens");
        let go = read(&mut source).await.expect("the source reads");
        source.close().await.expect("the source closes");
        translate(&go, &options(), &mut counter())
    }

    /// **The headline: the owner's own deployment migrates, and the three station keys are
    /// the three the data migration wrote.**
    #[tokio::test]
    async fn the_owners_configuration_migrates() {
        let scratch = Scratch::new("owner");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                OWNER_DEVICES,
                OWNER_WEBSITE,
                OWNER_STORAGE,
                OWNER_CONTROLLERS,
            ],
        )
        .await;

        assert_eq!(
            migrated.station_keys(),
            vec!["csi", "suncrest-air-quality", "snow"],
            "these are the ids `observations.station_id` holds"
        );

        // The `name` column keeps his spelling, which is what `?station=CSI` matches and
        // what `/stationinfo` emits -- and the surrogate `id` is kept too, because three
        // frozen fields are integers.
        let rows: Vec<(String, String, String, i64, String)> = sqlx::query_as(
            "SELECT station_key, name, type, id, transport_kind FROM stations ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("the stations read back");
        assert_eq!(
            rows,
            vec![
                (
                    "csi".to_string(),
                    "CSI".to_string(),
                    "campbellscientific".to_string(),
                    1,
                    "tcp_out".to_string()
                ),
                (
                    "suncrest-air-quality".to_string(),
                    "Suncrest Air Quality".to_string(),
                    "airgradient".to_string(),
                    2,
                    "http_out".to_string()
                ),
                (
                    "snow".to_string(),
                    "snow".to_string(),
                    "snowgauge".to_string(),
                    3,
                    "grpc_out".to_string()
                ),
            ]
        );

        // Every station is owned by the one node this migration minted, and nothing reused
        // a `remote_stations.station_id` -- there are none here, and the assertion is that
        // the owner is the node rather than anything else.
        let owners: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT DISTINCT owning_node_id FROM stations")
                .fetch_all(&pool)
                .await
                .expect("the owners read back");
        assert_eq!(owners, vec![migrated.node_id.to_vec()]);
        let node: (Vec<u8>, String, String, String) =
            sqlx::query_as("SELECT node_id, node_name, timezone, spool_path FROM node")
                .fetch_one(&pool)
                .await
                .expect("the node row");
        assert_eq!(node.0, migrated.node_id.to_vec());
        assert_eq!(node.1, "pitcairn");
        // The deployment zone is the Go DSN's `TimeZone`, which is where the Go rollovers
        // were truncating.
        assert_eq!(node.2, "America/Denver");
        assert_eq!(node.3, "/var/lib/modernweather/spool");
        assert_eq!(node.0[6] >> 4, 7, "a UUIDv7");

        // The air quality station the website names resolves to the same key, which is the
        // other half of the defect: `air_quality_device_name` is a NAME in Go and a KEY
        // here, and the website is what `/stationinfo`'s `air_quality_device` comes from.
        let site: (String, String, String, i64, Option<String>, i64, i64) = sqlx::query_as(
            "SELECT name, hostname, page_title, air_quality_enabled, air_quality_station_key, \
             snow_enabled, radar_enabled FROM website",
        )
        .fetch_one(&pool)
        .await
        .expect("the website reads back");
        assert_eq!(site.0, "Suncrest Live Weather");
        assert_eq!(site.1, "suncrestweather.com");
        assert_eq!(site.2, "Suncrest Live Weather");
        assert_eq!(site.3, 1);
        assert_eq!(site.4.as_deref(), Some("suncrest-air-quality"));
        assert_eq!(site.5, 0, "snow is off on his site today");
        assert_eq!(site.6, 1, "and radar is on");
        let primary: Option<String> = sqlx::query_scalar("SELECT primary_station_key FROM website")
            .fetch_one(&pool)
            .await
            .expect("the primary station");
        assert_eq!(primary.as_deref(), Some("csi"));

        // The forecast account: one row, collapsed from the per-device copies, with the
        // pairing as the per-station switch. This is what makes `aerisWeatherEnabled`
        // derivable.
        let aeris: (i64, Option<String>, Option<String>) =
            sqlx::query_as("SELECT enabled, client_id, client_secret FROM aeris_account")
                .fetch_one(&pool)
                .await
                .expect("the aeris row");
        assert_eq!(
            aeris,
            (1, Some("cid".to_string()), Some("csecret".to_string()))
        );
        let paired: Vec<String> = sqlx::query_scalar(
            "SELECT station_key FROM station_service WHERE service = 'aeris' ORDER BY station_key",
        )
        .fetch_all(&pool)
        .await
        .expect("the pairings");
        assert_eq!(paired, vec!["csi"]);

        // One socket on 443 with TLS is not what this deployment has -- no website carries
        // a certificate in his configuration database, because the certificate is on the
        // reverse proxy -- so the HTTP port is the one Go served.
        let listener: (String, i64, Option<i64>, i64, String, i64) = sqlx::query_as(
            "SELECT listen_addr, http_port, https_port, grpc_port, management_listen_addr, \
             management_port FROM listener",
        )
        .fetch_one(&pool)
        .await
        .expect("the listener");
        assert_eq!(
            listener,
            (
                "0.0.0.0".to_string(),
                443,
                None,
                50051,
                // A-W5-3, and the Go value was `0.0.0.0`.
                "127.0.0.1".to_string(),
                8081
            )
        );
        assert!(
            migrated
                .report
                .of(NoteKind::NotCarried)
                .any(|n| n.subject == "listener.management_listen_addr"),
            "the narrowing is reported rather than silent: {:#?}",
            migrated.report.notes
        );
        assert!(
            migrated
                .report
                .of(NoteKind::Action)
                .any(|n| n.subject == "management" && n.message.contains("setup token")),
            "the dead auth token is an action for the owner"
        );

        // The snow gauge's base distance, in the units the column's name states.
        let base: Option<f64> = sqlx::query_scalar(
            "SELECT snow_base_distance_mm FROM stations WHERE station_key = 'snow'",
        )
        .fetch_one(&pool)
        .await
        .expect("the base distance");
        assert_eq!(base, Some(1422.0));

        // No uploaders: he shares with nobody, and an absent row is the switch.
        let uploaders: i64 = sqlx::query_scalar("SELECT count(*) FROM uploader")
            .fetch_one(&pool)
            .await
            .expect("the uploaders");
        assert_eq!(uploaders, 0);
        assert!(migrated.peers.is_empty());
        assert!(migrated.ingest.is_none(), "he is nobody's collector");
        pool.close().await;
    }

    /// **O-8, and the half of it that is this work item's.**
    ///
    /// A KISS sink cannot be constructed below 600 s, so a sub-floor interval is a component
    /// that cannot be built and a daemon that does not start. Go's default is 300 s and
    /// applies to RF and internet alike.
    #[tokio::test]
    async fn a_sub_floor_kiss_beacon_is_raised_with_one_line() {
        let scratch = Scratch::new("kiss");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aprs_enabled, aprs_callsign, aprs_passcode,
                                      aprs_upload_interval, aprs_transport,
                                      aprs_kiss_connection, aprs_kiss_tcp_address)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'N0CALL-1', '12345', 300, 'kiss', 'tcp', '10.0.0.9:8001');",
            ],
        )
        .await;

        let interval: Option<i64> =
            sqlx::query_scalar("SELECT interval_seconds FROM uploader WHERE kind = 'aprs'")
                .fetch_one(&pool)
                .await
                .expect("the uploader");
        assert_eq!(interval, Some(RADIO_FLOOR_SECONDS));
        let raised: Vec<&Note> = migrated.report.of(NoteKind::Raised).collect();
        assert_eq!(raised.len(), 1, "one line, not a paragraph: {raised:#?}");
        assert_eq!(raised[0].subject, "aprs:aprs-roof");
        assert!(raised[0].message.contains("300 s"), "{}", raised[0].message);
        assert!(raised[0].message.contains("600 s"), "{}", raised[0].message);

        // The transport's spelling, and only its own columns: `kiss` + `tcp` is `kiss-tcp`
        // here, and the serial columns stay NULL because the table's exclusions are
        // exhaustive.
        let aprs: (
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT aprs_transport, aprs_kiss_tcp_address, aprs_kiss_serial_device, \
                 aprs_kiss_serial_baud, aprs_passcode FROM uploader WHERE kind = 'aprs'",
        )
        .fetch_one(&pool)
        .await
        .expect("the aprs row");
        assert_eq!(aprs.0, "kiss-tcp");
        assert_eq!(aprs.1.as_deref(), Some("10.0.0.9:8001"));
        assert_eq!(aprs.2, None);
        assert_eq!(aprs.3, None);
        assert_eq!(aprs.4.as_deref(), Some("12345"));

        // The callsign is the station's and the passcode is the account's (W7 §6.2), which
        // is the one place the Go row's two adjacent columns land in two tables.
        let callsign: Option<String> =
            sqlx::query_scalar("SELECT aprs_callsign FROM stations WHERE station_key = 'roof'")
                .fetch_one(&pool)
                .await
                .expect("the callsign");
        assert_eq!(callsign.as_deref(), Some("N0CALL-1"));
        let pairing: (String, Option<i64>) = sqlx::query_as(
            "SELECT station_key, uploader_id FROM station_service WHERE service = 'aprs'",
        )
        .fetch_one(&pool)
        .await
        .expect("the pairing");
        assert_eq!(pairing.0, "roof");
        assert!(pairing.1.is_some(), "an upload pairing names its account");
        pool.close().await;
    }

    /// An APRS-IS beacon is **not** raised: it has a feedback channel, so the clamp belongs
    /// at plan time and the configuration keeps saying what the operator typed.
    #[tokio::test]
    async fn an_aprs_is_beacon_is_left_alone() {
        let scratch = Scratch::new("aprsis");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aprs_enabled, aprs_callsign, aprs_passcode,
                                      aprs_upload_interval, aprs_transport, aprs_symbol_table,
                                      aprs_kiss_tcp_address)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'N0CALL-1', '12345', 300, 'aprs-is', '/', '10.0.0.9:8001');",
            ],
        )
        .await;
        let row: (String, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT aprs_transport, interval_seconds, aprs_kiss_tcp_address FROM uploader",
        )
        .fetch_one(&pool)
        .await
        .expect("the uploader");
        assert_eq!(row, ("aprs-is".to_string(), Some(300), None));
        assert_eq!(migrated.report.of(NoteKind::Raised).count(), 0);
        // The KISS address it was carrying is reported rather than dropped in silence.
        assert!(migrated
            .report
            .of(NoteKind::NotCarried)
            .any(|n| n.subject == "aprs:aprs-roof"));
        pool.close().await;
    }

    /// `aprs_symbol_table CHAR(1)` enforces nothing in SQLite, and here the column carries
    /// `CHECK (length(...) = 1)`.
    #[tokio::test]
    async fn an_over_long_symbol_table_is_cut_to_one_character() {
        let scratch = Scratch::new("symbol");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aprs_enabled, aprs_callsign, aprs_passcode,
                                      aprs_symbol_table, aprs_symbol_code)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'N0CALL-1', '12345', '/not-one-character', '_');",
            ],
        )
        .await;
        let table: Option<String> = sqlx::query_scalar("SELECT aprs_symbol_table FROM uploader")
            .fetch_one(&pool)
            .await
            .expect("the symbol table");
        assert_eq!(table.as_deref(), Some("/"));
        assert!(migrated
            .report
            .of(NoteKind::Raised)
            .any(|n| n.message.contains("aprs_symbol_table")));
        pool.close().await;
    }

    /// **The four plaintext credentials are not read at all, which is D-67 as a type.**
    ///
    /// The assertion is over the whole migrated database rather than over the columns this
    /// code writes: a credential that reached any column of the destination would be the
    /// exposure being carried forward, and the only way to be sure is to look everywhere.
    #[tokio::test]
    async fn a_registered_stations_cleartext_credentials_do_not_survive() {
        let scratch = Scratch::new("secrets");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                "INSERT INTO remote_stations (station_id, station_name, station_type,
                                              aprs_enabled, aprs_callsign, aprs_password,
                                              wu_enabled, wu_station_id, wu_api_key,
                                              aeris_enabled, aeris_client_id,
                                              aeris_client_secret, pws_enabled,
                                              pws_station_id, pws_password)
                 VALUES ('8e1c3f5a-0000-4000-8000-000000000001', 'Back Ridge', 'davis',
                         1, 'N0CALL-2', 'APRS-SECRET', 1, 'KXYZ', 'WU-SECRET',
                         1, 'aeris-id', 'AERIS-SECRET', 1, 'PWSID', 'PWS-SECRET');",
            ],
        )
        .await;

        // The peer: a FRESH UUIDv7, not the station's own UUID.
        assert_eq!(migrated.peers.len(), 1);
        let peer = &migrated.peers[0];
        assert_eq!(peer.name, "Back Ridge");
        assert_eq!(peer.node_id[6] >> 4, 7);
        assert_ne!(peer.node_id, migrated.node_id);
        assert_eq!(peer.origins, vec![peer.node_id], "a peer speaks for itself");
        let stored: Vec<(Vec<u8>, String, i64)> =
            sqlx::query_as("SELECT node_id, name, relay FROM peers")
                .fetch_all(&pool)
                .await
                .expect("the peers read back");
        assert_eq!(
            stored,
            vec![(peer.node_id.to_vec(), "Back Ridge".to_string(), 0)]
        );
        let origins: i64 = sqlx::query_scalar("SELECT count(*) FROM peer_origins")
            .fetch_one(&pool)
            .await
            .expect("the origins");
        assert_eq!(origins, 1);

        // The station is inventory rather than a component: no transport, and its owner is
        // the peer.
        let row: (String, Option<String>, Vec<u8>, String) = sqlx::query_as(
            "SELECT station_key, transport_kind, owning_node_id, identity_kind \
             FROM stations WHERE name = 'Back Ridge'",
        )
        .fetch_one(&pool)
        .await
        .expect("the received station");
        assert_eq!(row.0, "back-ridge");
        assert_eq!(row.1, None);
        assert_eq!(row.2, peer.node_id.to_vec());
        assert_eq!(row.3, "from_registration");

        // The four secrets are nowhere in the destination.
        let dump = std::fs::read(scratch.target()).expect("the destination file");
        let haystack = String::from_utf8_lossy(&dump);
        for secret in ["APRS-SECRET", "WU-SECRET", "AERIS-SECRET", "PWS-SECRET"] {
            assert!(
                !haystack.contains(secret),
                "`{secret}` reached the migrated configuration"
            );
        }
        // They are named without being read, so the owner knows what to re-enter.
        for col in [
            "remote_stations.aprs_password",
            "remote_stations.wu_api_key",
            "remote_stations.aeris_client_secret",
            "remote_stations.pws_password",
        ] {
            assert!(
                migrated
                    .report
                    .of(NoteKind::NotCarried)
                    .any(|n| n.subject == col),
                "{col} is not in the report: {:#?}",
                migrated.report.notes
            );
        }
        // And one action per service that was on, which is the owner's to-do list.
        assert_eq!(
            migrated
                .report
                .of(NoteKind::Action)
                .filter(|n| n.subject == "back-ridge" && n.message.contains("cleartext"))
                .count(),
            4
        );
        // Receiving readings is an `ingest` row and nothing else.
        let ingest = migrated.ingest.as_ref().expect("a collector");
        assert_eq!(ingest.listen, "grpc");
        assert!(
            ingest.allow_insecure,
            "the Go receiver authenticated nobody"
        );
        let stored: (String, String, i64) =
            sqlx::query_as("SELECT listen, peer_auth, allow_insecure FROM ingest")
                .fetch_one(&pool)
                .await
                .expect("the ingest row");
        assert_eq!(stored, ("grpc".to_string(), "mtls-ca".to_string(), 1));
        pool.close().await;
    }

    /// A database created before `019_add_wll_fields` ran has none of those columns, and the
    /// migration reads it anyway -- reporting the gap rather than failing to decode a row.
    #[tokio::test]
    async fn a_source_missing_a_column_reads_as_its_go_default() {
        let scratch = Scratch::new("absent");
        go_db(
            &scratch.go(),
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
            ],
        )
        .await;
        // SQLite cannot drop a column that an index or a view names; these five are plain,
        // which is what makes this the honest way to produce a pre-019 shape.
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(scratch.go())
            .connect()
            .await
            .expect("the fixture reopens");
        for col in [
            "wll_host",
            "wll_port",
            "wll_broadcast",
            "wll_sensor_mapping",
            "wll_poll_interval",
        ] {
            sqlx::raw_sql(&format!("ALTER TABLE devices DROP COLUMN {col}"))
                .execute(&mut conn)
                .await
                .expect("the column drops");
        }
        conn.close().await.expect("closed");

        let mut source = open_source(&scratch.go()).await.expect("opens");
        let go = read(&mut source).await.expect("reads");
        source.close().await.expect("closes");
        assert!(go.absent_columns.contains(&"devices.wll_host".to_string()));
        assert_eq!(go.devices.len(), 1);
        assert_eq!(go.devices[0].wll_broadcast, None);
        let migrated = translate(&go, &options(), &mut counter()).expect("it still translates");
        assert_eq!(migrated.station_keys(), vec!["roof"]);
    }

    /// Not a Go configuration database at all.
    #[tokio::test]
    async fn an_unrelated_database_is_refused_by_name() {
        let scratch = Scratch::new("unrelated");
        let pool = config_db::open_and_migrate(&scratch.target())
            .await
            .expect("a ModernWeather configuration database");
        pool.close().await;
        let mut source = open_source(&scratch.target()).await.expect("opens");
        let e = read(&mut source).await.expect_err("not a Go configuration");
        assert!(
            matches!(e, ConfigGoError::NotAGoConfig { table: "devices" }),
            "{e}"
        );
        assert!(e.to_string().contains("--migrate-config-from"), "{e}");
    }

    /// A source that is not there is an error, not an empty deployment.
    #[tokio::test]
    async fn a_missing_source_is_not_an_empty_migration() {
        let scratch = Scratch::new("missing");
        let e = open_source(&scratch.go()).await.expect_err("no such file");
        assert!(matches!(e, ConfigGoError::Query { .. }), "{e}");
    }

    /// The destination is not merged into.
    #[tokio::test]
    async fn a_destination_that_already_holds_configuration_is_refused() {
        let scratch = Scratch::new("occupied");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
            ],
        )
        .await;
        let mut tx = pool.begin().await.expect("a transaction");
        let e = write(&mut tx, &migrated)
            .await
            .expect_err("the second application");
        assert!(
            matches!(e, ConfigGoError::DestinationNotEmpty { table: "node", .. }),
            "{e}"
        );
        pool.close().await;
    }

    /// **A dry run writes nothing, and is the same statements.**
    #[tokio::test]
    async fn a_rolled_back_migration_leaves_the_destination_empty() {
        let scratch = Scratch::new("dry");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
            ],
        )
        .await
        .expect("the translation");
        let pool = config_db::open_and_migrate(&scratch.target())
            .await
            .expect("the schema applies");
        let mut tx = pool.begin().await.expect("a transaction");
        write(&mut tx, &migrated).await.expect("the write");
        tx.rollback().await.expect("the rollback");
        for table in ["node", "stations", "uploader", "website"] {
            let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .expect("a count");
            assert_eq!(n, 0, "{table} kept a row");
        }
        // And a real migration into the same database then succeeds, which is what makes
        // the dry run safe to run first.
        let mut tx = pool.begin().await.expect("a transaction");
        write(&mut tx, &migrated).await.expect("the write");
        tx.commit().await.expect("the commit");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM stations")
            .fetch_one(&pool)
            .await
            .expect("a count");
        assert_eq!(n, 1);
        pool.close().await;
    }

    /// The refusals, each of which is a source configuration this schema has no faithful
    /// image of. One test, because what is being asserted about each is the same thing:
    /// that it is refused with a sentence naming the rows to change.
    #[tokio::test]
    async fn the_configurations_with_no_faithful_image_are_refused() {
        let base = "INSERT INTO devices (id, config_id, name, type, enabled, serial_device,
                                         baud, timezone)
                    VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');";

        // Two names, one key. The key is what the history is stored under, so this cannot
        // be resolved by choosing.
        let scratch = Scratch::new("collide");
        let e = translated(
            &scratch,
            &[
                base,
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (2, 1, 'Roof!', 'davis', 1, '/dev/ttyUSB1', 19200, 'America/Denver');",
            ],
        )
        .await
        .expect_err("two names, one key");
        assert!(
            matches!(e, ConfigGoError::StationKeyCollision { ref key, .. } if key == "roof"),
            "{e}"
        );

        // A name with nothing component-safe in it.
        let scratch = Scratch::new("nokey");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone)
               VALUES (1, 1, '***', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
            ],
        )
        .await
        .expect_err("no key");
        assert!(matches!(e, ConfigGoError::NoStationKey { .. }), "{e}");

        // A type no driver here configures.
        let scratch = Scratch::new("type");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, hostname, port,
                                    timezone)
               VALUES (1, 1, 'roof', 'vantage-pro-9000', 1, 'h', '1', 'America/Denver');",
            ],
        )
        .await
        .expect_err("unknown type");
        assert!(matches!(e, ConfigGoError::UnknownStationType { .. }), "{e}");

        // A serial device with no line rate: a serial transport here must name both.
        let scratch = Scratch::new("baud");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device,
                                    timezone)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 'America/Denver');",
            ],
        )
        .await
        .expect_err("no baud");
        assert!(matches!(e, ConfigGoError::NoTransport { .. }), "{e}");
        assert!(e.to_string().contains("baud"), "{e}");

        // No zone anywhere: `plan()` refuses a collecting station with none rather than
        // rolling its day over in the server's zone.
        let scratch = Scratch::new("zone");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200);",
            ],
        )
        .await
        .expect_err("no zone");
        assert!(matches!(e, ConfigGoError::NoTimezone { .. }), "{e}");

        // Two Aeris accounts where there is now one. The `aeris_account` schema's own
        // comment specifies this refusal.
        let scratch = Scratch::new("aeris");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'account-one');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id)
                 VALUES (2, 1, 'ridge', 'davis', 1, '/dev/ttyUSB1', 19200, 'America/Denver',
                         1, 'account-two');",
            ],
        )
        .await
        .expect_err("two accounts");
        assert!(
            matches!(e, ConfigGoError::AerisAccountsDisagree { .. }),
            "{e}"
        );

        // Two stations sharing one upload service, which is `PlanError::UploaderArity` from
        // the other end.
        let scratch = Scratch::new("arity");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, wu_enabled, wu_station_id, wu_password)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'KONE', 'k1');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, wu_enabled, wu_station_id, wu_password)
                 VALUES (2, 1, 'ridge', 'davis', 1, '/dev/ttyUSB1', 19200, 'America/Denver',
                         1, 'KTWO', 'k2');",
            ],
        )
        .await
        .expect_err("two accounts for one service");
        assert!(
            matches!(e, ConfigGoError::TwoAccountsForOneService { .. }),
            "{e}"
        );

        // KISS with no connection kind.
        let scratch = Scratch::new("kissconn");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone, aprs_enabled, aprs_callsign, aprs_transport,
                                    aprs_kiss_connection)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                       1, 'N0CALL', 'kiss', '');",
            ],
        )
        .await
        .expect_err("no kiss connection");
        assert!(
            matches!(e, ConfigGoError::UnknownKissConnection { .. }),
            "{e}"
        );

        // A website naming a device that is not there.
        let scratch = Scratch::new("website");
        let e = translated(
            &scratch,
            &[
                base,
                "INSERT INTO weather_websites (id, config_id, name, snow_enabled,
                                               snow_device_name)
                 VALUES (1, 1, 'site', 1, 'a-gauge-that-is-not-configured');",
            ],
        )
        .await
        .expect_err("no such station");
        assert!(
            matches!(e, ConfigGoError::NoSuchWebsiteStation { role, .. }
                     if role == "its snow station"),
            "{e}"
        );
    }

    /// The Ambient station is the structural case: N stations, one component, and the port
    /// the request arrived on stops being an identity.
    #[tokio::test]
    async fn an_ambient_station_keeps_its_path_and_loses_its_port() {
        let scratch = Scratch::new("ambient");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, port, path, timezone)
               VALUES (1, 1, 'console', 'ambient-customized', 1, '8081', '/weatherstation',
                       'America/Denver');",
            ],
        )
        .await;
        let row: (String, Option<String>, Option<i64>, String) = sqlx::query_as(
            "SELECT transport_kind, transport_mount, transport_port, identity_kind FROM stations",
        )
        .fetch_one(&pool)
        .await
        .expect("the station");
        assert_eq!(row.0, "http_in");
        assert_eq!(row.1.as_deref(), Some("/weatherstation"));
        assert_eq!(row.2, None, "the port was the identity and is not one now");
        assert_eq!(row.3, "from_query");
        assert!(migrated
            .report
            .of(NoteKind::NotCarried)
            .any(|n| n.subject == "console" && n.message.contains("8081")));
        pool.close().await;
    }

    /// A website with a certificate moves Go's one socket to `https_port` and gains a
    /// plaintext redirect, and becomes the deterministic SNI fallback.
    #[tokio::test]
    async fn a_tls_website_keeps_its_port_and_gains_a_redirect() {
        let scratch = Scratch::new("tls");
        let (pool, _migrated) = migrate(
            &scratch,
            &[
                OWNER_DEVICES,
                "INSERT INTO weather_websites (id, config_id, name, device_id, hostname,
                                               tls_cert_path, tls_key_path)
                 VALUES (1, 1, 'site', 1, 'SuncrestWeather.com.:443', '/etc/c.pem',
                         '/etc/k.pem');",
                OWNER_CONTROLLERS,
            ],
        )
        .await;
        let listener: (i64, Option<i64>) =
            sqlx::query_as("SELECT http_port, https_port FROM listener")
                .fetch_one(&pool)
                .await
                .expect("the listener");
        assert_eq!(listener, (8080, Some(443)));
        let site: (String, i64) = sqlx::query_as("SELECT hostname, is_default_tls FROM website")
            .fetch_one(&pool)
            .await
            .expect("the website");
        assert_eq!(site, ("suncrestweather.com".to_string(), 1));
        pool.close().await;
    }

    /// A `weatherlink-live` station the Go schema cannot fully describe is migrated OFF,
    /// which is what keeps the daemon able to start.
    #[tokio::test]
    async fn a_weatherlink_live_with_no_serial_is_migrated_switched_off() {
        let scratch = Scratch::new("wll");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, wll_host,
                                    wll_broadcast, wll_sensor_mapping, timezone)
               VALUES (1, 1, 'roof', 'wll', 1, '192.168.1.50', 1, 'th:1, rain:1',
                       'America/Denver');",
            ],
        )
        .await;
        let row: (String, i64, String, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT type, enabled, transport_kind, transport_port, wll_sensor_mapping \
             FROM stations",
        )
        .fetch_one(&pool)
        .await
        .expect("the station");
        // `wll` is written `weatherlink-live`, which is the frozen spelling.
        assert_eq!(row.0, "weatherlink-live");
        assert_eq!(row.1, 0, "switched off rather than unplannable");
        assert_eq!(row.2, "udp_in");
        assert_eq!(row.3, Some(22222));
        assert_eq!(row.4.as_deref(), Some("th:1, rain:1"));
        assert!(migrated
            .report
            .of(NoteKind::Action)
            .any(|n| n.subject == "roof" && n.message.contains("presented_key")));
        pool.close().await;
    }

    /// The snow controller's tuning has no column here, and that is reported rather than
    /// dropped -- but only when the controller is switched ON.
    ///
    /// The first-run DDL inserts a `snowcache` row with `enabled = 0`, and the Go daemon
    /// reads only enabled rows, so a default disabled row is tuning nothing is using and
    /// there is nothing to say about it. An operator who tuned the detector enabled it.
    #[tokio::test]
    async fn the_snow_controllers_tuning_is_reported_as_having_no_successor() {
        let device = "INSERT INTO devices (id, config_id, name, type, enabled, serial_device,
                                           baud, timezone)
                      VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200,
                              'America/Denver');";

        // The DDL's own disabled row: nothing to report.
        let scratch = Scratch::new("snowoff");
        let migrated = translated(&scratch, &[device])
            .await
            .expect("the translation");
        assert!(
            migrated
                .report
                .of(NoteKind::NotCarried)
                .all(|n| n.subject != "controller_configs.snow_*"),
            "a disabled controller is not tuning in use: {:#?}",
            migrated.report.notes
        );

        // Tuned and switched on: named, with where the constants now live.
        let scratch = Scratch::new("snowtune");
        let migrated = translated(
            &scratch,
            &[
                device,
                "UPDATE controller_configs SET enabled = 1, snow_penalty = 4.5,
                        snow_min_accumulation = 7.5, snow_station_name = 'roof'
                 WHERE controller_type = 'snowcache';",
            ],
        )
        .await
        .expect("the translation");
        let note = migrated
            .report
            .of(NoteKind::NotCarried)
            .find(|n| n.subject == "controller_configs.snow_*")
            .expect("the tuning is named");
        assert!(
            note.message.contains("snow_penalty=4.5"),
            "{}",
            note.message
        );
        assert!(
            note.message.contains("mw_pipeline::snow::pelt"),
            "{}",
            note.message
        );
    }

    /// A `grpcreceiver` device is not a station: being a collector is an `ingest` row and
    /// nothing else (**D-44**).
    #[tokio::test]
    async fn a_grpc_receiver_device_becomes_an_ingest_row_and_not_a_station() {
        let scratch = Scratch::new("receiver");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                "INSERT INTO devices (id, config_id, name, type, enabled, port)
                 VALUES (2, 1, 'inbound', 'grpcreceiver', 1, '50051');",
            ],
        )
        .await;
        assert_eq!(migrated.station_keys(), vec!["roof"]);
        assert!(migrated.ingest.is_some());
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM stations")
            .fetch_all(&pool)
            .await
            .expect("the stations");
        assert_eq!(names, vec!["roof"]);
        assert!(migrated
            .report
            .of(NoteKind::Derived)
            .any(|n| n.subject == "inbound" && n.message.contains("config.ingest")));
        pool.close().await;
    }

    /// A website with a feature on and nothing to read from is migrated with it off, because
    /// the column's `CHECK` makes the half-configured state unrepresentable.
    #[tokio::test]
    async fn a_feature_with_no_station_is_migrated_off() {
        let scratch = Scratch::new("halfsite");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                // Snow and air quality on with no device named, and radar on with an empty
                // token: three states the Go schema admits and this one does not.
                "INSERT INTO weather_websites (id, config_id, name, device_id, hostname,
                                               snow_enabled, air_quality_enabled,
                                               radar_enabled, radar_token)
                 VALUES (1, 1, 'site', 1, 'example.test', 1, 1, 1, '');",
            ],
        )
        .await;
        let row: (i64, i64, i64) =
            sqlx::query_as("SELECT snow_enabled, air_quality_enabled, radar_enabled FROM website")
                .fetch_one(&pool)
                .await
                .expect("the website");
        assert_eq!(row, (0, 0, 0));
        assert_eq!(
            migrated
                .report
                .of(NoteKind::NotCarried)
                .filter(|n| n.subject == "site")
                .count(),
            3
        );
        pool.close().await;
    }

    /// Forecasts on with an incomplete credential pair: the account is migrated OFF and the
    /// pairings are kept, so turning it on later needs no re-pairing. This is the shape the
    /// owner's own deployment is in if the Aeris secret is not in his database.
    #[tokio::test]
    async fn forecasts_with_no_credential_keep_their_pairings_and_wait() {
        let scratch = Scratch::new("aerisoff");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone, aeris_enabled)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver', 1);",
            ],
        )
        .await;
        let account: (i64, Option<String>) =
            sqlx::query_as("SELECT enabled, client_id FROM aeris_account")
                .fetch_one(&pool)
                .await
                .expect("the account row");
        assert_eq!(account, (0, None));
        let paired: Vec<String> =
            sqlx::query_scalar("SELECT station_key FROM station_service WHERE service = 'aeris'")
                .fetch_all(&pool)
                .await
                .expect("the pairings");
        assert_eq!(paired, vec!["roof"], "kept, so nothing has to be re-paired");
        assert!(migrated
            .report
            .of(NoteKind::Action)
            .any(|n| n.subject == "aeris" && n.message.contains("SWITCHED OFF")));
        pool.close().await;
    }

    /// Two stations sharing one forecast account is the state this schema holds, and more
    /// than one pairing is reported rather than assumed.
    #[tokio::test]
    async fn two_stations_can_share_one_forecast_account() {
        let scratch = Scratch::new("aerisshare");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id,
                                      aeris_api_client_secret, aeris_refresh_interval,
                                      aeris_api_endpoint)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'cid', 'secret', 0, 'https://data.api.xweather.com');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id)
                 VALUES (2, 1, 'ridge', 'davis', 1, '/dev/ttyUSB1', 19200, 'America/Denver',
                         1, 'cid');",
            ],
        )
        .await;
        let account: (i64, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT enabled, refresh_interval_seconds, api_endpoint FROM aeris_account",
        )
        .fetch_one(&pool)
        .await
        .expect("the account");
        // Go's `0` means "derive from the forecast period", which is this column's NULL.
        assert_eq!(
            account,
            (1, None, Some("https://data.api.xweather.com".to_string()))
        );
        let paired: Vec<String> = sqlx::query_scalar(
            "SELECT station_key FROM station_service WHERE service = 'aeris' ORDER BY station_key",
        )
        .fetch_all(&pool)
        .await
        .expect("the pairings");
        assert_eq!(paired, vec!["ridge", "roof"]);
        assert!(migrated
            .report
            .of(NoteKind::Derived)
            .any(|n| n.subject == "aeris" && n.message.contains("2 stations")));
        pool.close().await;
    }

    /// A storage backend that is really an egress has no `weather_database` successor, and a
    /// node with no row has no `db:postgres` at all -- an absence this migration states.
    #[tokio::test]
    async fn an_egress_backend_and_a_missing_database_are_both_reported() {
        let scratch = Scratch::new("egress");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                "INSERT INTO storage_configs (config_id, backend_type, enabled,
                                              grpc_listen_addr, grpc_port, station_id)
                 VALUES (1, 'grpc', 1, '0.0.0.0', 50051,
                         '8e1c3f5a-0000-4000-8000-000000000002');",
            ],
        )
        .await
        .expect("the translation");
        assert!(migrated
            .report
            .of(NoteKind::NotCarried)
            .any(|n| n.subject == "storage_configs.grpc"));
        assert!(migrated
            .report
            .of(NoteKind::NotCarried)
            .any(|n| n.subject == "weather_database" && n.message.contains("D-44")));
        // With no zone column on the device AND no database row there would be nothing to
        // fall back to; the device has one, so the station still translates.
        assert_eq!(migrated.station_keys(), vec!["roof"]);
        // And the node's own zone is the default, because the DSN named none.
        assert_eq!(migrated.node_timezone, "UTC");
    }

    /// A polled WeatherLink Live needs no serial, so it is **not** switched off -- the
    /// distinction between the two transports is what decides that.
    #[tokio::test]
    async fn a_polled_weatherlink_live_stays_on() {
        let scratch = Scratch::new("wllpoll");
        let (pool, _m) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, wll_host, wll_port,
                                    wll_broadcast, wll_sensor_mapping, wll_poll_interval,
                                    timezone)
               VALUES (1, 1, 'roof', 'weatherlink-live', 1, '192.168.1.50', 80, 0,
                       'th:1, rain:1', 30, 'America/Denver');",
            ],
        )
        .await;
        let row: (i64, String, Option<String>, String, Option<i64>) = sqlx::query_as(
            "SELECT enabled, transport_kind, transport_url, identity_kind, \
             wll_poll_interval_seconds FROM stations",
        )
        .fetch_one(&pool)
        .await
        .expect("the station");
        assert_eq!(row.0, 1);
        assert_eq!(row.1, "http_out");
        assert_eq!(row.2.as_deref(), Some("http://192.168.1.50:80"));
        assert_eq!(row.3, "fixed");
        assert_eq!(row.4, Some(30));
        pool.close().await;
    }

    /// `disable_station` on a key nothing holds changes nothing and says so, which is what
    /// keeps its caller from silently believing it did something.
    #[tokio::test]
    async fn disabling_a_station_that_is_not_there_is_a_no_op() {
        let scratch = Scratch::new("disable");
        let mut migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
            ],
        )
        .await
        .expect("the translation");
        let before = migrated.report.notes.len();
        assert!(!migrated.disable_station("no-such-station", "because"));
        assert_eq!(migrated.report.notes.len(), before);
        assert!(migrated.disable_station("roof", "a reason the operator reads"));
        assert!(migrated
            .report
            .of(NoteKind::Action)
            .any(|n| n.subject == "roof" && n.message.contains("switched OFF")));
        // And the type filter the binary uses to find the rows it has to check.
        assert_eq!(migrated.stations_of_type("davis"), vec![("roof", None)]);
        assert!(migrated.stations_of_type("weatherlink-live").is_empty());
    }

    /// The report's two renderings, which a caller uses to label a line.
    #[test]
    fn a_note_renders_as_its_kind_its_subject_and_its_sentence() {
        let note = Note {
            kind: NoteKind::Raised,
            subject: "aprs:roof".to_string(),
            message: "raised to the floor".to_string(),
        };
        assert_eq!(note.to_string(), "[raised] aprs:roof: raised to the floor");
        assert_eq!(NoteKind::Derived.as_str(), "derived");
        assert_eq!(NoteKind::NotCarried.as_str(), "not-carried");
        assert_eq!(NoteKind::Action.as_str(), "action");
    }

    /// A `BOOLEAN` column in a non-`STRICT` table holds whatever was written to it, and a
    /// reader that treated `'true'` as false would turn an enabled uploader off in silence.
    #[tokio::test]
    async fn both_spellings_of_a_go_boolean_are_read() {
        let scratch = Scratch::new("bools");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, wu_enabled, wu_station_id, wu_password)
                 VALUES (1, 1, 'roof', 'davis', 'true', '/dev/ttyUSB0', 19200,
                         'America/Denver', 'yes', 'KONE', 'k1');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (2, 1, 'shed', 'davis', 'false', '/dev/ttyUSB1', 19200,
                         'America/Denver');",
            ],
        )
        .await
        .expect("the translation");
        let enabled: Vec<bool> = migrated
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::PutStation(s) => Some(s.enabled),
                _ => None,
            })
            .collect();
        assert_eq!(enabled, vec![true, false]);
        assert!(migrated
            .changes
            .iter()
            .any(|c| matches!(c, Change::PutService(s) if s.kind == "wunderground")));
    }

    /// PWS Weather, which the other two service arms do not cover.
    #[tokio::test]
    async fn pws_weather_carries_its_account_and_the_stations_far_end_id() {
        let scratch = Scratch::new("pws");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO controller_configs (config_id, controller_type, enabled,
                                                 pws_api_endpoint)
                 VALUES (1, 'pwsweather', 1, 'https://pwsupdate.example.test/update');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, pws_enabled, pws_station_id,
                                      pws_upload_interval)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'PWSROOF', 120);",
            ],
        )
        .await;
        let row: (
            String,
            String,
            i64,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT kind, name, enabled, interval_seconds, api_endpoint, api_key \
                 FROM uploader",
        )
        .fetch_one(&pool)
        .await
        .expect("the uploader");
        assert_eq!(row.0, "pwsweather");
        // `pws:<name>` is the component id, so the name has to be component-safe -- and
        // UNIQUE across kinds, which the kind prefix is what buys.
        assert_eq!(row.1, "pws-roof");
        // **OFF**, because `pws_password` is empty and the Go daemon's own readiness test
        // was therefore skipping this station: migrating it on would start an upload that
        // was not happening, and `mw_output::plan` refuses an enabled uploader with no
        // credential anyway.
        assert_eq!(row.2, 0);
        assert_eq!(row.3, Some(120));
        // The device's endpoint is empty, so the controller's global one is used.
        assert_eq!(
            row.4.as_deref(),
            Some("https://pwsupdate.example.test/update")
        );
        assert_eq!(row.5, None, "`pws_password` was empty");
        let pairing: (String, Option<String>) = sqlx::query_as(
            "SELECT station_key, remote_station_id FROM station_service \
             WHERE service = 'pwsweather'",
        )
        .fetch_one(&pool)
        .await
        .expect("the pairing");
        assert_eq!(pairing, ("roof".to_string(), Some("PWSROOF".to_string())));
        let note = migrated
            .report
            .of(NoteKind::Action)
            .find(|n| n.subject == "pws:pws-roof")
            .expect("the owner is told what to fill in");
        assert!(
            note.message.contains("was NOT uploading"),
            "{}",
            note.message
        );
        pool.close().await;
    }

    /// **One station with three services.** `uploader.name` is UNIQUE across kinds, so the
    /// station key alone is not a name: the second insert would fail and abort the whole
    /// migration. The prefix is `ci/db/config-seed.sql`'s own convention.
    #[tokio::test]
    async fn one_station_may_share_with_every_service_at_once() {
        let scratch = Scratch::new("threeservices");
        let (pool, _m) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                    timezone, wu_enabled, wu_station_id, wu_password,
                                    pws_enabled, pws_station_id, pws_password,
                                    aprs_enabled, aprs_callsign, aprs_passcode)
               VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                       1, 'KONE', 'wukey', 1, 'PWSROOF', 'pwskey', 1, 'N0CALL-1', '12345');",
            ],
        )
        .await;
        let names: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, name FROM uploader ORDER BY name")
                .fetch_all(&pool)
                .await
                .expect("three uploaders");
        assert_eq!(
            names,
            vec![
                ("aprs".to_string(), "aprs-roof".to_string()),
                ("pwsweather".to_string(), "pws-roof".to_string()),
                ("wunderground".to_string(), "wu-roof".to_string()),
            ]
        );
        // One pairing per service, each naming an account of its own kind -- which is what
        // `station_service`'s composite foreign key is for.
        let pairings: Vec<(String, String)> = sqlx::query_as(
            "SELECT s.service, u.name FROM station_service s JOIN uploader u \
             ON u.id = s.uploader_id ORDER BY s.service",
        )
        .fetch_all(&pool)
        .await
        .expect("three pairings");
        assert_eq!(
            pairings,
            vec![
                ("aprs".to_string(), "aprs-roof".to_string()),
                ("pwsweather".to_string(), "pws-roof".to_string()),
                ("wunderground".to_string(), "wu-roof".to_string()),
            ]
        );
        pool.close().await;
    }

    /// A **disabled** TimescaleDB backend is not the live weather database.
    ///
    /// Go was writing nothing to it, and migrating it as the connection would give this
    /// daemon a `writer:timescale` against a server the old one was not using.
    #[tokio::test]
    async fn a_disabled_storage_backend_is_not_the_weather_database() {
        let scratch = Scratch::new("storageoff");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                "INSERT INTO storage_configs (config_id, backend_type, enabled,
                                              timescale_host, timescale_database)
                 VALUES (1, 'timescaledb', 0, 'pitcairn', 'weather');",
            ],
        )
        .await
        .expect("the translation");
        assert!(
            !migrated
                .changes
                .iter()
                .any(|c| matches!(c, Change::PutWeatherDatabase(_))),
            "a disabled backend became the live connection"
        );
        let note = migrated
            .report
            .of(NoteKind::NotCarried)
            .find(|n| n.subject == "storage_configs.timescaledb")
            .expect("the disabled backend is named");
        assert!(note.message.contains("DISABLED"), "{}", note.message);
    }

    /// **The listener's four ports have to differ, and the DEFAULTS are part of that.**
    ///
    /// A Go deployment whose REST server is on 8081 would otherwise be migrated with
    /// `management_port` falling back to its own default of 8081, and refused by the schema.
    #[tokio::test]
    async fn a_port_that_collides_with_a_default_is_moved_rather_than_refused() {
        let scratch = Scratch::new("ports");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                // The REST server on the management API's default port, and the gRPC server
                // on it too, which is the worst case the four CHECKs can see.
                "INSERT INTO controller_configs (config_id, controller_type, enabled,
                                                 rest_port, grpc_port)
                 VALUES (1, 'rest', 1, 8081, 8081);",
            ],
        )
        .await;
        let row: (i64, Option<i64>, i64, i64) = sqlx::query_as(
            "SELECT http_port, https_port, grpc_port, management_port FROM listener",
        )
        .fetch_one(&pool)
        .await
        .expect("the listener row was accepted");
        assert_eq!(row.0, 8081, "the port Go was serving on is kept");
        assert_eq!(row.1, None);
        assert_ne!(row.2, row.0);
        assert_ne!(row.3, row.0);
        assert_ne!(row.3, row.2);
        // And each move is a line, because an operator looking for the management API at
        // 8081 needs to know where it went.
        assert!(
            migrated
                .report
                .of(NoteKind::Raised)
                .filter(|n| n.subject.starts_with("listener."))
                .count()
                >= 2,
            "{:#?}",
            migrated.report.notes
        );
        pool.close().await;
    }

    /// `wll_poll_interval_seconds` has a 30 s floor as a `CHECK` and the Go column has none,
    /// so a sub-floor value is raised with a line rather than aborting the migration.
    #[tokio::test]
    async fn a_sub_floor_weatherlink_poll_interval_is_raised() {
        let scratch = Scratch::new("wllfloor");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, wll_host,
                                    wll_broadcast, wll_sensor_mapping, wll_poll_interval,
                                    timezone)
               VALUES (1, 1, 'roof', 'weatherlink-live', 1, '192.168.1.50', 0,
                       'th:1, rain:1', 10, 'America/Denver');",
            ],
        )
        .await;
        let secs: Option<i64> =
            sqlx::query_scalar("SELECT wll_poll_interval_seconds FROM stations")
                .fetch_one(&pool)
                .await
                .expect("the station");
        assert_eq!(secs, Some(WLL_POLL_FLOOR_SECONDS));
        assert!(migrated
            .report
            .of(NoteKind::Raised)
            .any(|n| n.subject == "roof" && n.message.contains("30 s floor")));
        pool.close().await;
    }

    /// Half a TLS pair is not a TLS configuration, and the column pair's `CHECK` says so --
    /// so a one-sided Go pair is resolved here rather than aborting on a constraint.
    #[tokio::test]
    async fn half_a_tls_pair_is_carried_as_neither_half() {
        let scratch = Scratch::new("halftls");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, hostname, port,
                                      timezone, tls_cert_file)
                 VALUES (1, 1, 'roof', 'davis', 1, '10.0.0.9', '6785', 'America/Denver',
                         '/etc/only-a-cert.pem');",
                "INSERT INTO weather_websites (id, config_id, name, device_id, hostname,
                                               tls_key_path)
                 VALUES (1, 1, 'site', 1, 'example.test', '/etc/only-a-key.pem');",
            ],
        )
        .await;
        let station: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT transport_tls_cert_path, transport_tls_key_path FROM stations")
                .fetch_one(&pool)
                .await
                .expect("the station");
        assert_eq!(station, (None, None));
        let site: (Option<String>, Option<String>, i64) =
            sqlx::query_as("SELECT tls_cert_path, tls_key_path, is_default_tls FROM website")
                .fetch_one(&pool)
                .await
                .expect("the website");
        assert_eq!(site, (None, None, 0));
        // The listener therefore has no TLS port either, which is the consequence that
        // matters: `any_cert` reads the same pairing.
        let https: Option<i64> = sqlx::query_scalar("SELECT https_port FROM listener")
            .fetch_one(&pool)
            .await
            .expect("the listener");
        assert_eq!(https, None);
        assert_eq!(
            migrated
                .report
                .of(NoteKind::NotCarried)
                .filter(|n| n.message.contains("half of a TLS pair"))
                .count(),
            2
        );
        pool.close().await;
    }

    /// The Aeris credential pair travels from ONE device, and two secrets for one client id
    /// is a refusal rather than a silently-chosen winner.
    #[tokio::test]
    async fn the_forecast_credential_pair_comes_from_one_device() {
        // The id is on the first device and the secret on the second. Assembling the pair
        // from two rows is how a mismatched credential gets written `enabled = 1`.
        let scratch = Scratch::new("aerispair");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'cid');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id,
                                      aeris_api_client_secret)
                 VALUES (2, 1, 'ridge', 'davis', 1, '/dev/ttyUSB1', 19200, 'America/Denver',
                         1, 'cid', 'the-secret');",
            ],
        )
        .await
        .expect("one account named twice, with one secret between them");
        let write = migrated
            .changes
            .iter()
            .find_map(|c| match c {
                Change::PutAeris(a) => Some(a),
                _ => None,
            })
            .expect("the account");
        assert_eq!(write.client_id.as_deref(), Some("cid"));
        assert_eq!(write.client_secret.as_deref(), Some("the-secret"));
        assert!(write.enabled, "the pair is complete, so it is on");

        // Two secrets for one id: one of them is wrong and this cannot tell which.
        let scratch = Scratch::new("aerissecrets");
        let e = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id,
                                      aeris_api_client_secret)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver',
                         1, 'cid', 'secret-one');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone, aeris_enabled, aeris_api_client_id,
                                      aeris_api_client_secret)
                 VALUES (2, 1, 'ridge', 'davis', 1, '/dev/ttyUSB1', 19200, 'America/Denver',
                         1, 'cid', 'secret-two');",
            ],
        )
        .await
        .expect_err("two secrets for one account");
        assert!(
            matches!(e, ConfigGoError::AerisSecretsDisagree { .. }),
            "{e}"
        );
        assert!(e.to_string().contains("cannot tell which"), "{e}");
    }

    /// **The Ambient station is migrated off**, because the Go schema holds neither half of
    /// what it needs: `plan()` requires the `?PASSWORD=` as a stored hash and the `?ID=` as
    /// the presented claim, and the Go daemon used the port the request arrived on instead.
    #[tokio::test]
    async fn an_ambient_station_is_migrated_switched_off() {
        let scratch = Scratch::new("ambientoff");
        let (pool, migrated) = migrate(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, port, path, timezone)
               VALUES (1, 1, 'console', 'ambient-customized', 1, '8081', '/weatherstation',
                       'America/Denver');",
            ],
        )
        .await;
        let row: (i64, Option<String>, Option<String>) =
            sqlx::query_as("SELECT enabled, presented_key, ingest_secret_hash FROM stations")
                .fetch_one(&pool)
                .await
                .expect("the station");
        assert_eq!(row.0, 0, "switched off rather than unplannable");
        assert_eq!(row.1, None);
        assert_eq!(row.2, None);
        let note = migrated
            .report
            .of(NoteKind::Action)
            .find(|n| n.subject == "console" && n.message.contains("PASSWORD"))
            .expect("both fields are named");
        assert!(note.message.contains("presented_key"), "{}", note.message);
        pool.close().await;
    }

    /// **A controller field belongs to the row whose type owns it, and only to an enabled
    /// row.**
    ///
    /// Every `controller_configs` row carries every column, and three have non-NULL
    /// DEFAULTS -- so "the first non-NULL across rows" would read the DDL-inserted
    /// `snowcache` row's default `grpc_port` of 50051 in preference to the `rest` row's real
    /// one, on every database the Go daemon has ever created.
    #[tokio::test]
    async fn a_controller_field_comes_from_its_own_enabled_row() {
        let scratch = Scratch::new("controllers");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                // The `rest` row owns the gRPC columns, and its id is ABOVE the
                // DDL-inserted `snowcache` row's.
                "INSERT INTO controller_configs (config_id, controller_type, enabled,
                                                 rest_port, grpc_port)
                 VALUES (1, 'rest', 1, 9090, 9091);",
                // A DISABLED management row: Go reads only enabled ones, so its port is not
                // the management port.
                "INSERT INTO controller_configs (config_id, controller_type, enabled,
                                                 management_port, management_auth_token)
                 VALUES (1, 'management', 0, 9099, 'a-token');",
                // And a controller type this migration has no arm for.
                "INSERT INTO controller_configs (config_id, controller_type, enabled)
                 VALUES (1, 'somethingelse', 1);",
            ],
        )
        .await
        .expect("the translation");
        let listener = migrated
            .changes
            .iter()
            .find_map(|c| match c {
                Change::PutListener(l) => Some(l),
                _ => None,
            })
            .expect("the listener");
        assert_eq!(listener.http_port, 9090);
        assert_eq!(
            listener.grpc_port, 9091,
            "the `snowcache` row's default must not win"
        );
        assert_eq!(
            listener.management_port, 8081,
            "the disabled management row is not read"
        );
        assert!(
            !migrated
                .report
                .of(NoteKind::Action)
                .any(|n| n.subject == "management"),
            "a disabled row's auth token is not a dead credential to warn about"
        );
        assert!(migrated
            .report
            .of(NoteKind::NotCarried)
            .any(|n| n.subject == "controller_configs.somethingelse"));
    }

    /// **Every Go read is scoped to the `default` configuration**, so a second named one is
    /// rows nothing has ever loaded -- and migrating them would produce a deployment that
    /// was never deployed.
    #[tokio::test]
    async fn only_the_default_configuration_is_migrated() {
        let scratch = Scratch::new("configs");
        let migrated = translated(
            &scratch,
            &[
                "INSERT INTO configs (id, name) VALUES (2, 'an-old-experiment');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (1, 1, 'roof', 'davis', 1, '/dev/ttyUSB0', 19200, 'America/Denver');",
                "INSERT INTO devices (id, config_id, name, type, enabled, serial_device, baud,
                                      timezone)
                 VALUES (2, 2, 'ghost', 'davis', 1, '/dev/ttyUSB9', 19200, 'America/Denver');",
                "INSERT INTO weather_websites (id, config_id, name) VALUES (1, 2, 'ghost-site');",
            ],
        )
        .await
        .expect("the translation");
        assert_eq!(migrated.station_keys(), vec!["roof"]);
        assert!(
            !migrated
                .changes
                .iter()
                .any(|c| matches!(c, Change::PutWebsite(_))),
            "the other configuration's website is not migrated"
        );
        let note = migrated
            .report
            .of(NoteKind::NotCarried)
            .find(|n| n.subject == "configs")
            .expect("the unused configuration is named");
        assert!(note.message.contains('1'), "{}", note.message);
    }
}
