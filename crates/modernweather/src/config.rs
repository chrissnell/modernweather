//! `Config`, and the step 4 of W3 §3.1 that produces it.
//!
//! **Why the type lives in the binary.** §1.3 rule 2 writes the signature as
//! `plan(&Config, &Registrars)` and makes `plan()` generic over the configuration type,
//! because the supervisor must not know that a station or a database exists. The
//! configuration *tree* has the same problem one level down -- it describes stations and
//! websites -- so the crate that names the type is the crate that may name everything,
//! which is this one.
//!
//! **What it holds, and what it does not.** Three tables: the listener, the weather
//! database and the node row. Those are what the boot sequence itself needs -- where to
//! bind, whether **D-62** has a server to assert, and whether this node has started
//! before. Everything else in `migrations/config/` is read by the component that consumes
//! it, and a field here for a table no consumer exists for yet would be this work item
//! guessing at the shape M3-1 and M7 want.
//!
//! The first registrar (implementation-plan.md M3-1's `mw_station::register`) is also the
//! point at which this type stops being private to the binary: a registrar takes `&C`, and
//! `mw-station` may not depend on `mw-store`. What it will need is a *trait* this type
//! implements -- one per component crate, naming only the rows that crate plans from --
//! which keeps the tree here and the queries there. That is M3-1's to design against a
//! real consumer rather than this item's to invent without one.

use mw_component::Generation;
use mw_store::config_db::{self, ConfigDbError, Listener, Node, WeatherDatabase};
use sqlx::SqlitePool;

/// Everything `plan()` and the boot sequence read from the configuration database.
#[derive(Debug)]
pub struct Config {
    /// Where the one listener binds (W3 §6.1).
    pub listener: Listener,
    /// The weather database, if one is configured. **Its absence is meaningful**: a node
    /// with no row has no `db:postgres` and therefore no `writer:timescale` (**D-44**).
    pub weather_database: Option<WeatherDatabase>,
    /// This node's identity, if it has started before. `None` is a first start.
    pub node: Option<Node>,
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
        Ok(Config {
            listener: config_db::listener(pool).await?,
            weather_database: config_db::weather_database(pool).await?,
            node: config_db::node(pool).await?,
            generation: Generation::ZERO,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(addr: &str, port: u16) -> Config {
        Config {
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
