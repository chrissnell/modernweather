//! The two translations between the management API's types and the configuration
//! database's, in the one crate allowed to name both.
//!
//! `mw-api-mgmt` owns what a request body is and may not name `mw-store`; `mw-store` owns
//! what a column is and may not name a route. The copy between them is the same one
//! [`crate::config`] already makes column-into-field for every registrar, and it buys the
//! same property: the API cannot grow a query and the store cannot grow a route.
//!
//! # The direction that matters more
//!
//! [`of`] is the read, and the rule it enforces is **D-67**'s: a credential is reported as
//! *whether* and never as *what*. There is no redaction anywhere in this file, because a
//! redacted field is a value a client can round-trip back into a `PUT` -- which is how
//! `"********"` becomes a stored password eight asterisks long.
//!
//! [`into_store_change`] is the write, and its rule is the mirror: an **omitted** credential
//! means "keep the stored one", never "clear it". That is why no `PUT` can erase a secret and
//! why clearing one is an action of its own.

use std::collections::{BTreeMap, BTreeSet};

use mw_api_mgmt::change::{Change as ApiChange, ServiceInput, StationInput};
use mw_api_mgmt::view::{
    ConfigView, Credential, ForecastsView, ListenerView, NodeView, PairingView, ServiceView,
    StationView, StorageView, TransportView, WebsiteView,
};
use mw_api_mgmt::FieldError;
use secrecy::ExposeSecret;
use sqlx::SqlitePool;

use crate::config::Config;

/// What a station's hardware type is called on screen (W5 §0.2).
///
/// The left column is the `stations.type` column's own `CHECK` list, which is the frozen
/// contract's spelling; the right is what an operator reads. A type this build does not know
/// renders as itself rather than as a blank -- a station whose kind came from a newer
/// migration should still be listable.
fn kind_display(kind: &str) -> String {
    match kind {
        "campbellscientific" => "Campbell Scientific station",
        "davis" => "Davis station",
        "snowgauge" => "Snow depth sensor",
        "ambient-customized" => "Ambient Weather station (sends to us)",
        "airgradient" => "AirGradient air-quality sensor",
        "weatherlink-live" => "WeatherLink Live station",
        other => other,
    }
    .to_string()
}

/// What an upload service is called on screen (W5 §0.2).
fn service_display(kind: &str) -> String {
    match kind {
        "wunderground" => "Weather Underground",
        "pwsweather" => "PWS Weather",
        // Kept verbatim (§0.3): the audience holds a callsign.
        "aprs" => "APRS / CWOP",
        other => other,
    }
    .to_string()
}

/// The configuration, as a read returns it — **every secret omitted**.
pub fn of(cfg: &Config, generation: u64) -> ConfigView {
    let services = services(cfg);
    let forecasts = forecasts(cfg);

    // Which services each station's readings go to, for the station page's read-only
    // "Shared with:" line. Computed once here rather than per station, which is also what
    // keeps it consistent with the per-service pages: one source, two renderings.
    let mut shared: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for s in &services {
        for p in &s.stations {
            shared
                .entry(p.station_key.as_str())
                .or_default()
                .insert(s.service.as_str());
        }
    }
    if let Some(f) = &forecasts {
        for key in &f.stations {
            shared.entry(key.as_str()).or_default().insert("forecasts");
        }
    }

    ConfigView {
        generation,
        node: cfg.node.as_ref().map(|n| NodeView {
            name: n.node_name.clone(),
            // Hyphenated, because that is how the owner will paste it into a peer's
            // configuration. The column is 16 raw bytes; a hex blob is not an identifier a
            // person can compare by eye.
            node_id: uuid_string(&n.node_id),
            timezone: n.timezone.clone(),
            spool_path: n.spool_path.clone(),
            spool_ceiling_bytes: n.spool_ceiling_bytes,
            spool_max_lag_seconds: n.spool_max_lag_seconds,
        }),
        listener: ListenerView {
            listen_addr: cfg.listener.listen_addr.clone(),
            http_port: cfg.listener.http_port,
            https_port: cfg.listener.https_port,
            grpc_port: cfg.listener.grpc_port,
            management_listen_addr: cfg.listener.management_listen_addr.clone(),
            // What the socket is actually on, which differs from the configured address
            // until M7-2 gives this API something to authenticate.
            management_bound_addr: cfg
                .management_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|_| cfg.listener.management_listen_addr.clone()),
            management_port: cfg.listener.management_port,
            management_tls_cert_path: cfg.listener.management_tls_cert_path.clone(),
            management_tls_key_path: cfg.listener.management_tls_key_path.clone(),
        },
        stations: cfg
            .stations
            .iter()
            .map(|r| StationView {
                key: r.station_key.clone(),
                id: r.id,
                name: r.name.clone(),
                kind: r.kind.clone(),
                kind_display: kind_display(&r.kind),
                collecting: r.enabled,
                identity_kind: r.identity_kind.clone(),
                presented_key: r.presented_key.clone(),
                protocol_secret: Credential::of(r.ingest_secret_hash.as_ref()),
                latitude: r.latitude,
                longitude: r.longitude,
                altitude_m: r.altitude_m,
                timezone: r.timezone.clone(),
                aprs_callsign: r.aprs_callsign.clone(),
                wind_dir_correction_degrees: r.wind_dir_correction_degrees,
                snow_base_distance_mm: r.snow_base_distance_mm,
                transport: TransportView {
                    kind: r.transport_kind.clone(),
                    device: r.transport_dev.clone(),
                    baud: r.transport_baud,
                    host: r.transport_host.clone(),
                    port: r.transport_port,
                    url: r.transport_url.clone(),
                    method: r.transport_method.clone(),
                    streaming: r.transport_streaming,
                    topic: r.transport_topic.clone(),
                    mount: r.transport_mount.clone(),
                    group: r.transport_group.clone(),
                    tls_ca_path: r.transport_tls_ca_path.clone(),
                    tls_cert_path: r.transport_tls_cert_path.clone(),
                    tls_key_path: r.transport_tls_key_path.clone(),
                    credential: Credential::of(r.transport_auth.as_ref()),
                },
                sensor_mapping: r.wll_sensor_mapping.clone(),
                poll_interval_seconds: r.wll_poll_interval_seconds,
                shared_with: shared
                    .get(r.station_key.as_str())
                    .map(|set| set.iter().map(|s| (*s).to_string()).collect())
                    .unwrap_or_default(),
            })
            .collect(),
        websites: cfg
            .websites
            .iter()
            .map(|w| WebsiteView {
                id: w.id,
                name: w.name.clone(),
                hostname: w.hostname.clone(),
                page_title: w.page_title.clone(),
                about_station_html: w.about_station_html.clone(),
                primary_station_key: w.primary_station_key.clone(),
                snow_enabled: w.snow_enabled,
                snow_station_key: w.snow_station_key.clone(),
                air_quality_enabled: w.air_quality_enabled,
                air_quality_station_key: w.air_quality_station_key.clone(),
                is_portal: w.is_portal,
                apple_app_id: w.apple_app_id.clone(),
                radar_enabled: w.radar_enabled,
                // **Not the token** (§1.5): the screen shows "Registered on …" and a
                // Re-register action. `radar_registered_at` is not on the row this crate
                // reads, so the presence of a token is what says it is registered -- which
                // is the same fact the column's own `CHECK` ties them together with.
                radar_registered_at: None,
                tls_cert_path: w.tls_cert_path.clone(),
                tls_key_path: w.tls_key_path.clone(),
                is_default_tls: w.is_default_tls,
            })
            .collect(),
        storage: cfg.weather_database.as_ref().map(|db| {
            let (host, database) = split_pg_url(db.url.expose_secret());
            StorageView {
                id: 1,
                host,
                database,
                connection: Credential { is_set: true },
            }
        }),
        services,
        forecasts,
    }
}

/// The upload services, one entry per `uploader` row.
///
/// Built from `cfg.declared_uploaders`, which is **one row per pairing** -- so the rows are
/// folded back by service here. The fold is by `kind` rather than by `name`, because `kind`
/// is the path segment and a second row for the same kind is W7 **D-38**'s arity, which
/// `plan()` refuses by name.
fn services(cfg: &Config) -> Vec<ServiceView> {
    let mut by_kind: BTreeMap<String, ServiceView> = BTreeMap::new();
    for row in &cfg.services {
        let kind = row.kind.as_str().to_string();
        let entry = by_kind.entry(kind.clone()).or_insert_with(|| ServiceView {
            service: kind.clone(),
            display: service_display(&kind),
            name: row.name.clone(),
            enabled: row.enabled,
            interval_seconds: row.interval_seconds,
            api_endpoint: row.api_endpoint.clone(),
            api_key: Credential::of(row.api_key.as_ref()),
            passcode: Credential::of(row.aprs_passcode.as_ref()),
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
            stations: Vec::new(),
        });
        if let Some(station) = &row.station_key {
            entry.stations.push(PairingView {
                station_key: station.clone(),
                remote_station_id: row.remote_station_id.clone(),
            });
        }
    }
    by_kind.into_values().collect()
}

/// Forecasts and alerts, from the `aeris_account` row and its pairings.
fn forecasts(cfg: &Config) -> Option<ForecastsView> {
    let row = cfg.aeris_account.as_ref()?;
    Some(ForecastsView {
        enabled: row.enabled,
        client_id: row.client_id.clone(),
        client_secret: Credential::of(row.client_secret.as_ref()),
        api_endpoint: row.api_endpoint.clone(),
        refresh_interval_seconds: row.refresh_interval_seconds,
        stations: cfg
            .declared_aeris
            .iter()
            .map(|s| s.key.as_str().to_string())
            .collect(),
    })
}

/// `node.node_id`'s 16 bytes as a hyphenated UUID.
///
/// By hand rather than through a `uuid` crate, because adding one to `[workspace.dependencies]`
/// would be a **D-32** amendment for a sixteen-byte string format -- and `UUIDv7`'s layout is
/// already assumed by the column's own `CHECK (length(node_id) = 16)`.
fn uuid_string(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The host and the database name out of a PostgreSQL URL, with the credential left behind.
///
/// Parsed by hand rather than with a URL crate, for [`uuid_string`]'s reason and one more:
/// this has to be **incapable** of returning the password, and the way to guarantee that is
/// to take only the two substrings that are wanted.
fn split_pg_url(url: &str) -> (String, String) {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    // Everything before the last `@` is `user:password`, and the last one is what separates
    // them: a password may legitimately contain one.
    let authority = rest.rsplit_once('@').map(|(_, a)| a).unwrap_or(rest);
    let (hostport, tail) = authority.split_once('/').unwrap_or((authority, ""));
    let database = tail.split(['?', '#']).next().unwrap_or("").to_string();
    (hostport.to_string(), database)
}

/// A management API change, as the statements that apply it.
///
/// Fallible because two of them need something the body does not carry: a new station needs
/// the node's identity to own it, and a protocol secret has to be **hashed** before it is
/// stored. Both are reported as field errors rather than as internal failures, because both
/// are things the operator can act on -- a node that has never started has no identity to
/// own a station, and that is a sentence rather than a `500`.
pub async fn into_store_change(
    change: &ApiChange,
    pool: &SqlitePool,
) -> Result<mw_store::Change, Vec<FieldError>> {
    Ok(match change {
        ApiChange::PutStation { key, station } => {
            let key = key
                .clone()
                .or_else(|| station.key.clone())
                .unwrap_or_default();
            mw_store::Change::PutStation(station_write(key, station, pool).await?)
        }
        ApiChange::DeleteStation { key } => mw_store::Change::DeleteStation(key.clone()),
        ApiChange::ClearStationSecret { key } => mw_store::Change::ClearIngestSecret(key.clone()),
        ApiChange::PutWebsite { id, website } => {
            mw_store::Change::PutWebsite(mw_store::WebsiteWrite {
                id: *id,
                name: website.name.clone(),
                hostname: website.hostname.clone(),
                page_title: website.page_title.clone(),
                about_station_html: website.about_station_html.clone(),
                primary_station_key: blank_to_none(&website.primary_station_key),
                snow_enabled: website.snow_enabled,
                snow_station_key: blank_to_none(&website.snow_station_key),
                air_quality_enabled: website.air_quality_enabled,
                air_quality_station_key: blank_to_none(&website.air_quality_station_key),
                is_portal: website.is_portal,
                apple_app_id: website.apple_app_id.clone(),
                tls_cert_path: blank_to_none(&website.tls_cert_path),
                tls_key_path: blank_to_none(&website.tls_key_path),
                is_default_tls: website.is_default_tls,
            })
        }
        ApiChange::DeleteWebsite { id } => mw_store::Change::DeleteWebsite(*id),
        ApiChange::RegisterRadar { id, radar } => mw_store::Change::RegisterRadar {
            website: *id,
            token: radar.token.clone(),
        },
        ApiChange::UnregisterRadar { id } => mw_store::Change::UnregisterRadar(*id),
        ApiChange::PutStorage { storage } => {
            mw_store::Change::PutWeatherDatabase(storage.url.clone())
        }
        ApiChange::DeleteStorage => mw_store::Change::DeleteWeatherDatabase,
        ApiChange::PutService { service, settings } => {
            mw_store::Change::PutService(service_write(service, settings))
        }
        ApiChange::DeleteService { service } => mw_store::Change::DeleteService(service.clone()),
        ApiChange::PutForecasts { forecasts } => mw_store::Change::PutAeris(mw_store::AerisWrite {
            enabled: forecasts.enabled,
            client_id: blank_to_none(&forecasts.client_id),
            client_secret: forecasts.client_secret.clone(),
            api_endpoint: blank_to_none(&forecasts.api_endpoint),
            refresh_interval_seconds: forecasts.refresh_interval_seconds,
            stations: forecasts.stations.clone(),
        }),
        ApiChange::DeleteForecasts => mw_store::Change::DeleteAeris,
        ApiChange::PutListener { listener } => {
            mw_store::Change::PutListener(mw_store::ListenerWrite {
                listen_addr: listener.listen_addr.clone(),
                http_port: i64::from(listener.http_port),
                https_port: listener.https_port.map(i64::from),
                grpc_port: i64::from(listener.grpc_port),
                management_listen_addr: listener.management_listen_addr.clone(),
                management_port: i64::from(listener.management_port),
                management_tls_cert_path: blank_to_none(&listener.management_tls_cert_path),
                management_tls_key_path: blank_to_none(&listener.management_tls_key_path),
            })
        }
    })
}

/// An empty string is an operator clearing a field, which the column stores as NULL.
///
/// `Some("")` and `None` are the same intention on a text input and have to become the same
/// column value: a `primary_station_key` of `""` would be a foreign key to a station that
/// cannot exist, and the `CHECK`s that pair a certificate with its key read `IS NULL`.
///
/// **The exception is a credential**, which is why this is not applied to one: an omitted
/// credential keeps the stored value, so `None` and `Some("")` have to stay distinguishable
/// there.
fn blank_to_none(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn station_write(
    key: String,
    station: &StationInput,
    pool: &SqlitePool,
) -> Result<mw_store::StationWrite, Vec<FieldError>> {
    // Hashed here, by the function that lives beside the one that verifies it -- which is
    // the only arrangement in which the two cannot disagree about the algorithm
    // (`mw_store::registry::hash_ingest_secret`'s own note).
    let ingest_secret_hash = match station.protocol_secret.as_deref().filter(|s| !s.is_empty()) {
        Some(secret) => Some(
            mw_store::hash_ingest_secret(&secrecy::SecretString::from(secret.to_string()))
                .map_err(|_| {
                    vec![FieldError::new(
                        "protocol_secret",
                        "That secret could not be stored. Try a different one.",
                    )]
                })?,
        ),
        None => None,
    };

    // Only an insert needs an owner, and only a node that has started has one to give.
    let owning_node_id = match station_exists(&key, pool).await {
        true => None,
        false => {
            let mut conn = pool.acquire().await.map_err(|_| {
                vec![FieldError::new(
                    "",
                    "The configuration database could not be read.",
                )]
            })?;
            let node = mw_store::config_db::node(&mut *conn).await.map_err(|_| {
                vec![FieldError::new(
                    "",
                    "The configuration database could not be read.",
                )]
            })?;
            Some(
                node.ok_or_else(|| {
                    vec![FieldError::new(
                        "",
                        "This daemon has no identity yet, so it cannot own a station. Start \
                         it once to create one.",
                    )]
                })?
                .node_id,
            )
        }
    };

    Ok(mw_store::StationWrite {
        station_key: key,
        id: None,
        name: station.name.clone(),
        kind: station.kind.clone(),
        enabled: station.collecting,
        owning_node_id,
        identity_kind: station.identity_kind.clone(),
        presented_key: blank_to_none(&station.presented_key),
        ingest_secret_hash,
        latitude: station.latitude,
        longitude: station.longitude,
        altitude_m: station.altitude_m,
        timezone: blank_to_none(&station.timezone),
        aprs_callsign: blank_to_none(&station.aprs_callsign),
        wind_dir_correction_degrees: station.wind_dir_correction_degrees,
        snow_base_distance_mm: station.snow_base_distance_mm,
        transport_kind: blank_to_none(&station.transport.kind),
        transport_dev: blank_to_none(&station.transport.device),
        transport_baud: station.transport.baud,
        transport_host: blank_to_none(&station.transport.host),
        transport_port: station.transport.port,
        transport_url: blank_to_none(&station.transport.url),
        transport_method: blank_to_none(&station.transport.method),
        transport_auth: station
            .transport
            .credential
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        transport_streaming: station.transport.streaming,
        transport_mount: blank_to_none(&station.transport.mount),
        transport_group: blank_to_none(&station.transport.group),
        transport_topic: blank_to_none(&station.transport.topic),
        transport_tls_ca_path: blank_to_none(&station.transport.tls_ca_path),
        transport_tls_cert_path: blank_to_none(&station.transport.tls_cert_path),
        transport_tls_key_path: blank_to_none(&station.transport.tls_key_path),
        wll_sensor_mapping: blank_to_none(&station.sensor_mapping),
        wll_poll_interval_seconds: station.poll_interval_seconds,
    })
}

/// Whether a station with this key is already stored.
///
/// A read outside the write transaction, which is safe for exactly one reason: all it
/// decides is whether to *offer* an owning node id, and `apply` re-reads the row inside the
/// transaction to choose between `INSERT` and `UPDATE`. A concurrent create would make this
/// answer `false` and the insert collide, which the key's `UNIQUE` refuses by name.
async fn station_exists(key: &str, pool: &SqlitePool) -> bool {
    sqlx::query("SELECT 1 FROM stations WHERE station_key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .is_some()
}

fn service_write(service: &str, s: &ServiceInput) -> mw_store::ServiceWrite {
    mw_store::ServiceWrite {
        kind: service.to_string(),
        id: None,
        name: s.name.clone(),
        enabled: s.enabled,
        interval_seconds: s.interval_seconds,
        api_endpoint: blank_to_none(&s.api_endpoint),
        api_key: s.api_key.clone().filter(|v| !v.is_empty()),
        aprs_passcode: s.passcode.clone().filter(|v| !v.is_empty()),
        aprs_symbol_table: blank_to_none(&s.aprs_symbol_table),
        aprs_symbol_code: blank_to_none(&s.aprs_symbol_code),
        aprs_comment: blank_to_none(&s.aprs_comment),
        aprs_server: blank_to_none(&s.aprs_server),
        aprs_transport: blank_to_none(&s.aprs_transport),
        aprs_kiss_serial_device: blank_to_none(&s.aprs_kiss_serial_device),
        aprs_kiss_serial_baud: s.aprs_kiss_serial_baud,
        aprs_kiss_tcp_address: blank_to_none(&s.aprs_kiss_tcp_address),
        aprs_kiss_path: blank_to_none(&s.aprs_kiss_path),
        aprs_kiss_destination: blank_to_none(&s.aprs_kiss_destination),
        pairings: s
            .stations
            .iter()
            .map(|p| mw_store::Pairing {
                station_key: p.station_key.clone(),
                remote_station_id: blank_to_none(&p.remote_station_id),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The password never leaves this function.** The storage screen shows the host and
    /// the database name, and `StorageView::connection` says only whether a URL is stored --
    /// so the parse has to take the two substrings it wants rather than return a structure
    /// the password is reachable through.
    #[test]
    fn the_storage_view_cannot_carry_the_password() {
        let (host, db) = split_pg_url("postgres://mw:s3cr3t@db.example.com:5432/weather");
        assert_eq!(host, "db.example.com:5432");
        assert_eq!(db, "weather");
        // A password with an `@` in it, which is why the split is from the right.
        let (host, db) = split_pg_url("postgresql://mw:p@ss@10.0.0.5/weather?sslmode=disable");
        assert_eq!(host, "10.0.0.5");
        assert_eq!(db, "weather");
        // No credential at all, which is the local-socket form.
        let (host, db) = split_pg_url("postgres://localhost/weather");
        assert_eq!(host, "localhost");
        assert_eq!(db, "weather");
    }

    /// An empty text input is a cleared field, and a cleared `primary_station_key` has to be
    /// NULL rather than `""` -- which would be a foreign key to a station that cannot exist.
    #[test]
    fn a_blank_field_becomes_null() {
        assert_eq!(blank_to_none(&Some(String::new())), None);
        assert_eq!(blank_to_none(&Some("  ".to_string())), None);
        assert_eq!(blank_to_none(&None), None);
        assert_eq!(
            blank_to_none(&Some(" backyard ".to_string())),
            Some("backyard".to_string())
        );
    }

    /// The node id is hyphenated, because the owner pastes it into a peer's configuration
    /// and a 32-character hex run is not something a person compares by eye.
    #[test]
    fn the_node_id_renders_as_a_uuid() {
        let bytes: Vec<u8> = (0u8..16).collect();
        assert_eq!(uuid_string(&bytes), "00010203-0405-0607-0809-0a0b0c0d0e0f");
    }
}
