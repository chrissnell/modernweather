//! [`StationRow`] and [`StationConfig`] — what this crate plans from.
//!
//! **Why there is a trait here at all.** `plan()` is generic over the configuration type
//! and the binary chooses it (docs/service-architecture.md §1.3, rule 2), so a registrar
//! takes `&C`. `crates/modernweather/src/config.rs` anticipated the shape this would need:
//! *"a **trait** this type implements — one per component crate, naming only the rows that
//! crate plans from — which keeps the tree here and the queries there"*. This is that
//! trait, and [`StationRow`] is the row it names.
//!
//! **Why the row is declared here and the query is in `mw-store`.** `ci/layers.toml` puts
//! both crates in the `service` layer, so neither may name the other; the binary is the one
//! place they meet. The row type therefore lives in the crate that owns *what a station
//! is*, the `SELECT` lives in the crate that owns the database, and the binary copies
//! column into field. The copy is twenty lines of nothing, and it is what buys the property
//! that `mw-store` cannot grow a `Transport` and this crate cannot grow a query.
//!
//! **Why the write end of the pipeline arrives through here.** A station declares
//! `Need { ReadingStream, Hard, Write }` (W3 §2.3) and resolves nothing from the capability
//! bus, because the bus holds one handle *type* per provider and the spine publishes
//! `Arc<dyn ReadingStream>` — the read end. `crates/modernweather/src/spool.rs` records the
//! seam and leaves the call to this item; the call is that **the handle comes from the
//! registrar's configuration and the declaration stays**, for three reasons:
//!
//! 1. `ReadingStream` is a **root** capability. It is published before any component exists
//!    and is never revoked, so the live-watch property that resolving from the bus would buy
//!    is worth nothing here — a station can no more observe its absence than the supervisor
//!    can.
//! 2. The declaration still does the job it is for: `plan()` rejects a station in a process
//!    with no stream, which is a node configured to collect with nothing to collect into.
//! 3. The alternative — making `Access` part of handle resolution, so `Read` and `Write`
//!    are two slots — is a change to the capability bus for one consumer, and the first
//!    *non-root* write capability is the thing that should force it. `StationRegistry` is
//!    `Write` for `station:grpcrx` alone (W2a §4.1b), so M6-4 is where that bill comes due.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use mw_component::Ingress;
use secrecy::{ExposeSecret, SecretString};

use crate::transport::TransportColumns;

/// One `config.stations` row, as this crate needs it.
///
/// Field names are the column names. Validation is `crate::plan`'s and
/// `Transport::from_columns`'s, so a row that the schema's own `CHECK`s would have rejected
/// becomes a plan error that names the station and the column rather than a panic.
#[derive(Debug, Clone, Default)]
pub struct StationRow {
    /// `station_key` — the domain identity, and the instance part of the component id.
    pub key: String,
    /// `name` — what an operator calls it.
    pub name: String,
    /// `type` — the **configured** device type, in the spelling the frozen contract uses
    /// (`weatherlink-live`, `campbellscientific`). Not the driver name.
    pub kind: String,
    /// `enabled` — W5 §1.2's "Collecting readings" toggle. A disabled station produces no
    /// components, which under W2a §2.5 is a plan diff that stops them.
    pub enabled: bool,
    /// `identity_kind`.
    pub identity_kind: String,
    /// `presented_key` — the identifier a non-`fixed` station presents. A WeatherLink
    /// Live's serial.
    pub presented_key: Option<String>,
    /// The `transport_*` columns, or `None` for a station received from a peer: that
    /// station is inventory on this node, not a component.
    pub transport: Option<TransportColumns>,
    /// `wll_sensor_mapping` — "which sensor is which" (W5 §1.6).
    pub wll_sensor_mapping: Option<String>,
    /// `wll_poll_interval_seconds`.
    pub wll_poll_interval_seconds: Option<i64>,
}

impl Hash for StationRow {
    /// Every value a builder closes over, which is what makes the plan diff accurate
    /// (`mw_component::ConfigDigest`). Hand-written because `SecretString` is deliberately
    /// not `Hash`.
    ///
    /// **The credential is folded in**, and that is a decision rather than an oversight. A
    /// digest that omitted it would compare equal across a credential rotation, so the
    /// component would not be restarted and would keep using the old one — which is the one
    /// failure the digest exists to prevent, stated on `SpecFingerprint` about an APRS
    /// callsign. What leaks is one bit of a 64-bit hash of twenty fields, and
    /// `ConfigDigest::get` is a log field; a credential that has to survive that is a
    /// credential that should not be in a database column.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
        self.name.hash(state);
        self.kind.hash(state);
        self.enabled.hash(state);
        self.identity_kind.hash(state);
        self.presented_key.hash(state);
        self.wll_sensor_mapping.hash(state);
        self.wll_poll_interval_seconds.hash(state);
        match &self.transport {
            None => false.hash(state),
            Some(t) => {
                true.hash(state);
                t.kind.hash(state);
                t.dev.hash(state);
                t.baud.hash(state);
                t.host.hash(state);
                t.port.hash(state);
                t.url.hash(state);
                t.method.hash(state);
                t.auth.as_ref().map(ExposeSecret::expose_secret).hash(state);
                t.streaming.hash(state);
                t.mount.hash(state);
                t.group.hash(state);
                t.topic.hash(state);
                t.tls_ca.hash(state);
                t.tls_cert.hash(state);
                t.tls_key.hash(state);
            }
        }
    }
}

impl StationRow {
    /// Whether this row describes a station this node collects from.
    ///
    /// Two ways not to: the operator switched it off, or it arrived by registration and is
    /// owned by a peer. Neither is an error, and neither produces a component.
    pub fn is_collected_here(&self) -> bool {
        self.enabled && self.transport.is_some()
    }

    /// The poll period this row asks for, or the driver's default.
    pub fn poll_period(&self) -> std::time::Duration {
        self.wll_poll_interval_seconds
            .and_then(|n| u64::try_from(n).ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(crate::wll::DEFAULT_POLL)
    }

    /// The credential on the transport, if there is one.
    pub fn auth(&self) -> Option<&SecretString> {
        self.transport.as_ref().and_then(|t| t.auth.as_ref())
    }
}

/// What `mw-station`'s registrar reads out of the binary's configuration type.
pub trait StationConfig {
    /// Every `config.stations` row, including the ones this node does not collect from —
    /// filtering is the registrar's, so that "this station is switched off" and "this
    /// station belongs to a peer" are decisions in one place.
    fn stations(&self) -> &[StationRow];

    /// The write end of the reading pipeline, when this process has one.
    ///
    /// `None` is a legitimate configuration — a node that serves but does not ingest — and
    /// the registrar then refuses to plan a station rather than planning one that cannot
    /// store anything. See this module's documentation for why the handle arrives here.
    fn ingress(&self) -> Option<&Arc<dyn Ingress>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::ConfigDigest;

    fn row() -> StationRow {
        StationRow {
            key: "suncrest:roof".into(),
            name: "roof".into(),
            kind: crate::wll::STATION_TYPE.into(),
            enabled: true,
            identity_kind: "from_payload".into(),
            presented_key: Some("001D0A700002".into()),
            transport: Some(TransportColumns {
                kind: "udp_in".into(),
                port: Some(22222),
                host: Some("192.168.1.50".into()),
                ..TransportColumns::default()
            }),
            wll_sensor_mapping: Some("th:1, wind:1".into()),
            wll_poll_interval_seconds: None,
        }
    }

    #[test]
    fn a_disabled_station_and_a_peers_station_are_both_not_collected_here() {
        assert!(row().is_collected_here());
        assert!(!StationRow {
            enabled: false,
            ..row()
        }
        .is_collected_here());
        assert!(!StationRow {
            identity_kind: "from_registration".into(),
            transport: None,
            ..row()
        }
        .is_collected_here());
    }

    #[test]
    fn the_digest_follows_every_value_a_builder_closes_over() {
        let base = ConfigDigest::of(&row());
        assert_eq!(base, ConfigDigest::of(&row()), "stable for equal input");
        for changed in [
            StationRow {
                wll_sensor_mapping: Some("th:1, wind:2".into()),
                ..row()
            },
            StationRow {
                presented_key: Some("001D0A7FFFFF".into()),
                ..row()
            },
            StationRow {
                wll_poll_interval_seconds: Some(30),
                ..row()
            },
            StationRow {
                transport: Some(TransportColumns {
                    kind: "udp_in".into(),
                    port: Some(22223),
                    ..TransportColumns::default()
                }),
                ..row()
            },
        ] {
            assert_ne!(base, ConfigDigest::of(&changed));
        }
    }

    /// The property the hand-written `Hash` exists for: a rotated credential restarts the
    /// component.
    #[test]
    fn rotating_a_credential_changes_the_digest() {
        let with = |secret: &str| StationRow {
            transport: Some(TransportColumns {
                kind: "http_out".into(),
                url: Some("http://vendor.example".into()),
                auth: Some(SecretString::from(secret.to_string())),
                ..TransportColumns::default()
            }),
            ..row()
        };
        assert_ne!(
            ConfigDigest::of(&with("old")),
            ConfigDigest::of(&with("new"))
        );
        assert_eq!(
            ConfigDigest::of(&with("old")),
            ConfigDigest::of(&with("old"))
        );
    }

    #[test]
    fn the_poll_period_falls_back_to_the_drivers_default() {
        assert_eq!(row().poll_period(), crate::wll::DEFAULT_POLL);
        assert_eq!(
            StationRow {
                wll_poll_interval_seconds: Some(45),
                ..row()
            }
            .poll_period(),
            std::time::Duration::from_secs(45)
        );
        // A negative value cannot be written -- the column's CHECK requires >= 30 -- and if
        // one is there anyway the default is used rather than a panic on the conversion.
        assert_eq!(
            StationRow {
                wll_poll_interval_seconds: Some(-1),
                ..row()
            }
            .poll_period(),
            crate::wll::DEFAULT_POLL
        );
    }
}
