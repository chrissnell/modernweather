//! Turning `config.stations` rows into [`ComponentSpec`]s. Pure: no I/O, no sockets, no
//! database.
//!
//! **Why this is a free function and not an `impl Registrar`.** `mw_supervisor::Registrar`
//! is in the `service` layer and so is this crate, and `ci/layers.toml` rejects a
//! same-layer dependency — `mw-supervisor`'s own `forbidden` list names `mw-station`
//! explicitly, because the supervisor must not know that a station exists. So the trait is
//! implemented in the binary, over this function. That is the same §1.3 rule 1 shape as
//! every other seam here: the adapter is four lines in the one crate that may name both
//! sides, and adding a component crate still edits only `REGISTRARS`.
//!
//! # What a row becomes
//!
//! | Row | Components |
//! |---|---|
//! | `udp_in`, no `transport_host` | the shared `station:wll:udp:<port>` listener |
//! | `udp_in`, with `transport_host` | the listener, **and** `station:wll:<key>:rearm` |
//! | `http_out` | `station:wll:<key>`, polling `/v1/current_conditions` |
//! | disabled, or `transport_kind IS NULL` | nothing, and that is not an error |
//!
//! `transport_host` is what distinguishes a unit this node arms from one something else
//! arms, and the schema makes it optional for `udp_in` — so a node that only listens is a
//! configuration rather than a mode.
//!
//! # A station whose driver has not landed is an error, not a silence
//!
//! Every station type but `weatherlink-live` is M6-1's, and a row for one is refused here
//! by name. The alternative — skipping it — is the defect this programme keeps finding: a
//! configured station that produces no readings, no error and no log line. The refusal
//! disappears one driver at a time, and until then it tells the operator exactly which
//! work item they are waiting for.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use mw_codec::{MappingError, SensorMapping, WllConditions};
use mw_component::{
    Builder, ComponentId, ComponentIdError, ComponentSpec, ConfigDigest, Need, Trigger,
};
use mw_types::StationKey;

use crate::config::{StationConfig, StationRow};
use crate::endpoint::Endpoint;
use crate::identity::{IdentityError, StationIdentity};
use crate::transport::{Transport, TransportError};
use crate::wll::{self, BroadcastListener, BroadcastUnit, Poll, Rearm};

/// The per-occurrence deadline for an HTTP poll.
///
/// Larger than [`crate::wll::REQUEST_TIMEOUT`] by a margin, so that a device that answers
/// slowly produces the request timeout's message — which names the device — rather than
/// `DeadlineExceeded`, which names nothing.
const POLL_DEADLINE: Duration = Duration::from_secs(15);

/// The per-occurrence deadline for a re-arm. The same request, the same reasoning.
const REARM_DEADLINE: Duration = Duration::from_secs(15);

/// Why a set of station rows does not produce a plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StationPlanError {
    /// A station type no driver in this build serves.
    #[error(
        "station `{station}` is a `{kind}`, and this build has no driver for it. \
         implementation-plan.md M6-1 adds Davis, Campbell Scientific, Ambient, AirGradient \
         and the snow gauge; M3-1 added `weatherlink-live`."
    )]
    NoDriver {
        /// Which station.
        station: String,
        /// Its configured type.
        kind: String,
    },

    /// A transport the configured station type does not have.
    #[error("station `{station}` is a `{kind}` with a `{transport}` transport, which that driver has no mode for")]
    WrongTransport {
        /// Which station.
        station: String,
        /// Its configured type.
        kind: String,
        /// Its configured transport kind.
        transport: &'static str,
    },

    /// A `transport_*` column is not usable.
    #[error("station `{station}`: {source}")]
    Transport {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: TransportError,
    },

    /// An `identity_kind` column is not usable.
    #[error("station `{station}`: {source}")]
    Identity {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: IdentityError,
    },

    /// A required column is NULL.
    #[error("station `{station}` is a `{kind}` and needs `{column}`, which is not set{because}")]
    Missing {
        /// Which station.
        station: String,
        /// Its configured type.
        kind: String,
        /// The column.
        column: &'static str,
        /// A sentence saying what the column is for, or nothing.
        because: Because,
    },

    /// `wll_sensor_mapping` does not parse.
    #[error("station `{station}`: `wll_sensor_mapping` is not a mapping: {source}")]
    Mapping {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: MappingError,
    },

    /// The station key cannot be a component id, and therefore cannot be a metric label.
    #[error("station `{station}`: its key cannot name a component: {source}")]
    Key {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },

    /// Two stations claim the same identifier on one endpoint.
    #[error("stations `{first}` and `{second}` both present `{claim}`, so a frame carrying it resolves to neither")]
    DuplicateClaim {
        /// The first station with the claim.
        first: String,
        /// The second.
        second: String,
        /// What they both present.
        claim: String,
    },

    /// This node has stations to collect from and nowhere to put their readings.
    #[error(
        "station `{station}` would collect readings and this process has no reading \
         stream, so there is nothing to admit them to"
    )]
    NoIngress {
        /// Which station.
        station: String,
    },

    /// An identity strategy the configured transport cannot use.
    #[error(
        "station `{station}` has a `{transport}` transport and identity `{identity}`: \
         this driver resolves a broadcasting unit by the serial in its datagrams \
         (`from_payload`) and a polled unit from configuration (`fixed`)"
    )]
    WrongIdentity {
        /// Which station.
        station: String,
        /// Its configured transport kind.
        transport: &'static str,
        /// Its configured identity strategy.
        identity: &'static str,
    },

    /// `transport_host` is not a bare host.
    #[error("station `{station}`: `transport_host` {source}")]
    Host {
        /// Which station.
        station: String,
        /// Why.
        #[source]
        source: crate::endpoint::EndpointError,
    },

    /// A WeatherLink Live over TLS, which does not exist.
    #[error(
        "station `{station}` is configured as `https`: a WeatherLink Live's local API is \
         plain HTTP and the unit has no TLS listener, so this address can never answer"
    )]
    NotTls {
        /// Which station.
        station: String,
    },
}

/// A clause appended to [`StationPlanError::Missing`], or nothing.
///
/// A newtype rather than an `Option<&str>` so that the `Display` can put the separator in
/// exactly once: a message that reads "which is not set." for one column and "which is not
/// set: ..." for another is two formats for one error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Because(pub &'static str);

impl Because {
    /// No clause.
    pub const NOTHING: Self = Because("");
}

impl fmt::Display for Because {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            Ok(())
        } else {
            write!(f, ". {}", self.0)
        }
    }
}

/// The broadcast units arriving on one endpoint, with the row each came from.
///
/// A named type because the registrar groups by endpoint before it emits anything, and the
/// grouping's key is the endpoint while its value needs both halves: the unit the listener
/// decodes with, and the row whose digest the component's fingerprint is over.
type Endpoints<'a> = BTreeMap<(u16, Option<IpAddr>), Vec<(BroadcastUnit, &'a StationRow)>>;

/// Which component admits readings for which station, for a `Fixed` identity.
///
/// `StationRegistry::resolve` is handed a `FrameMeta` whose `presented_key` is `None` for a
/// `Fixed` transport — configuration already says which station, so the frame presents
/// nothing — which means the registry resolves such a frame by its **receiver**. The
/// mapping from receiver to station is a fact about the component ids this module mints, so
/// it is produced here and not reconstructed by parsing an id somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedReceiver {
    /// The component that will receive frames.
    pub component: ComponentId,
    /// The station they are from.
    pub station: StationKey,
}

/// Derive every station component from configuration.
pub fn register<C>(cfg: &C, out: &mut Vec<ComponentSpec>) -> Result<(), StationPlanError>
where
    C: StationConfig + ?Sized,
{
    let rows: Vec<&StationRow> = cfg
        .stations()
        .iter()
        .filter(|r| r.is_collected_here())
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let ingress = cfg.ingress().ok_or_else(|| StationPlanError::NoIngress {
        station: rows[0].key.clone(),
    })?;

    // Broadcast units, grouped by the endpoint they arrive on, because the listener is one
    // component per endpoint (`crate::wll`, decision 1).
    let mut listeners: Endpoints<'_> = BTreeMap::new();

    for row in rows {
        let identity = StationIdentity::parse(&row.identity_kind).map_err(|source| {
            StationPlanError::Identity {
                station: row.key.clone(),
                source,
            }
        })?;
        // `is_collected_here` is what guarantees the transport is present.
        let columns = row.transport.as_ref().expect("a collected station");
        let transport =
            Transport::from_columns(columns).map_err(|source| StationPlanError::Transport {
                station: row.key.clone(),
                source,
            })?;

        if row.kind != wll::STATION_TYPE {
            return Err(StationPlanError::NoDriver {
                station: row.key.clone(),
                kind: row.kind.clone(),
            });
        }

        let station = StationKey::new(row.key.as_str());
        let codec = WllConditions::new(mapping(row)?);

        // The schema's identity CHECK rejects `fixed` with an inbound transport, which is
        // the Ambient defect; what it cannot say is which of the four non-`fixed`
        // strategies *this driver* implements. A `udp_in` WeatherLink Live resolves by the
        // serial in its datagrams and nothing else, so `from_path` or `from_query` on one
        // is a row that would decode and then resolve to nothing.
        let expected = match &transport {
            Transport::UdpIn { .. } => Some(StationIdentity::FromPayload),
            Transport::HttpOut { .. } => Some(StationIdentity::Fixed),
            _ => None,
        };
        if let Some(expected) = expected {
            if identity != expected {
                return Err(StationPlanError::WrongIdentity {
                    station: row.key.clone(),
                    transport: transport.kind(),
                    identity: identity.as_str(),
                });
            }
        }

        match &transport {
            Transport::UdpIn { port, group } => {
                let serial = row.presented_key.clone().filter(|s| !s.is_empty()).ok_or(
                    StationPlanError::Missing {
                        station: row.key.clone(),
                        kind: row.kind.clone(),
                        column: "presented_key",
                        because: Because(
                            "a broadcasting WeatherLink Live identifies itself by the \
                             device serial in each datagram, and that serial is the \
                             allow-list this endpoint resolves against",
                        ),
                    },
                )?;
                listeners.entry((*port, *group)).or_default().push((
                    BroadcastUnit {
                        serial,
                        station: station.clone(),
                        codec,
                    },
                    row,
                ));
                // A unit this node arms as well as listens to. `transport_host` absent is a
                // unit something else arms, which is a configuration and not a mistake.
                if let Some(host) = columns.host.as_deref().filter(|h| !h.is_empty()) {
                    // Through `bare_host` and not `Endpoint::http`: `transport_host` is a
                    // free-text column, and a value with a scheme, a port or -- the one
                    // that matters -- a credential in it would otherwise become the host
                    // verbatim and be printed back in every request error.
                    let endpoint =
                        Endpoint::bare_host(host).map_err(|source| StationPlanError::Host {
                            station: row.key.clone(),
                            source,
                        })?;
                    out.push(rearm_spec(row, endpoint, *port, &station)?);
                }
            }
            Transport::HttpOut { url, method, auth } => {
                if url.is_tls() {
                    return Err(StationPlanError::NotTls {
                        station: row.key.clone(),
                    });
                }
                out.push(poll_spec(
                    row,
                    &transport,
                    &identity,
                    url.clone(),
                    *method,
                    auth.clone(),
                    station,
                    codec,
                    Arc::clone(ingress),
                )?);
            }
            other => {
                return Err(StationPlanError::WrongTransport {
                    station: row.key.clone(),
                    kind: row.kind.clone(),
                    transport: other.kind(),
                })
            }
        }
    }

    for ((port, group), units) in listeners {
        out.push(listener_spec(port, group, units, Arc::clone(ingress))?);
    }
    Ok(())
}

/// Every component that admits readings for a `Fixed`-identity station, with the station.
///
/// Derived from the same rows and the same filters as [`register`], and asserted to agree
/// with it by `tests/plan.rs` — because two functions that mint the same ids independently
/// are two functions that can disagree.
pub fn fixed_receivers<C>(cfg: &C) -> Result<Vec<FixedReceiver>, StationPlanError>
where
    C: StationConfig + ?Sized,
{
    let mut out = Vec::new();
    for row in cfg.stations().iter().filter(|r| r.is_collected_here()) {
        let identity = StationIdentity::parse(&row.identity_kind).map_err(|source| {
            StationPlanError::Identity {
                station: row.key.clone(),
                source,
            }
        })?;
        if identity != StationIdentity::Fixed {
            continue;
        }
        // The same guard `register` applies, and for the same reason: without it a `davis`
        // row with `fixed`/`http_out` yields a receiver whose component is
        // `station:wll:<key>` -- an id `register` refuses to emit. It is safe today only
        // because `boot.rs` calls `register` first and aborts on its error, and a
        // correctness that depends on call order is one a later edit breaks silently.
        if row.kind != wll::STATION_TYPE {
            continue;
        }
        let columns = row.transport.as_ref().expect("a collected station");
        // Only a transport that *emits readings* needs a receiver entry. The re-arm is a
        // control call and admits nothing, so it is deliberately not here: an entry for it
        // would let a frame resolve through a component that never receives one.
        if columns.kind != "http_out" {
            continue;
        }
        out.push(FixedReceiver {
            component: component_id(row, wll::poll_id)?,
            station: StationKey::new(row.key.as_str()),
        });
    }
    Ok(out)
}

fn mapping(row: &StationRow) -> Result<SensorMapping, StationPlanError> {
    let text = row
        .wll_sensor_mapping
        .as_deref()
        .filter(|m| !m.is_empty())
        .ok_or(StationPlanError::Missing {
            station: row.key.clone(),
            kind: row.kind.clone(),
            column: "wll_sensor_mapping",
            because: Because(
                "nothing in a WeatherLink Live's payload says which transmitter is the \
                 outdoor one, so a station without a mapping decodes to an empty reading",
            ),
        })?;
    SensorMapping::parse(text).map_err(|source| StationPlanError::Mapping {
        station: row.key.clone(),
        source,
    })
}

fn component_id(
    row: &StationRow,
    mint: fn(&str) -> Result<ComponentId, ComponentIdError>,
) -> Result<ComponentId, StationPlanError> {
    mint(&row.key).map_err(|source| StationPlanError::Key {
        station: row.key.clone(),
        source,
    })
}

#[allow(clippy::too_many_arguments)]
fn poll_spec(
    row: &StationRow,
    transport: &Transport,
    identity: &StationIdentity,
    url: Endpoint,
    method: crate::transport::HttpMethod,
    auth: Option<secrecy::SecretString>,
    station: StationKey,
    codec: WllConditions,
    ingress: Arc<dyn mw_component::Ingress>,
) -> Result<ComponentSpec, StationPlanError> {
    let id = component_id(row, wll::poll_id)?;
    let trigger = Trigger::Every {
        period: row.poll_period(),
        deadline: POLL_DEADLINE,
        first: mw_component::FirstRun::AtPhase,
        phase: mw_component::Phase::Spread,
        // Declared here, enforced once in `plan()`: W2a Decision 56, and the reason a
        // sub-floor period is a startup refusal rather than a per-occurrence clamp.
        floor: wll::POLL_FLOOR,
    };
    Ok(ComponentSpec::new(
        id,
        trigger,
        Builder::tick(move |_cx| {
            let body = Poll::new(
                url.clone(),
                method,
                auth.clone(),
                station.clone(),
                codec.clone(),
                Arc::clone(&ingress),
            );
            async move { Ok(body) }
        }),
    )
    // The declaration, not the resolution: see `crate::config`'s module documentation
    // for why the handle arrives through the registrar and the need stays here.
    .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
    .identity(transport.shape(identity))
    .config_digest(ConfigDigest::of(row)))
}

fn rearm_spec(
    row: &StationRow,
    endpoint: Endpoint,
    port: u16,
    station: &StationKey,
) -> Result<ComponentSpec, StationPlanError> {
    let id = component_id(row, wll::rearm_id)?;
    let trigger = Trigger::Every {
        period: wll::rearm_period(wll::REARM_DURATION_SECONDS),
        deadline: REARM_DEADLINE,
        // Immediate, and this is the one component in the driver where it matters: until
        // the unit is armed it broadcasts nothing, so a re-arm that waited for its first
        // phase slot would leave the listener silent for forty-eight minutes after a
        // restart. `AtPhase` is the right default everywhere that an occurrence is a
        // *sample* (W2a Decision 10); this occurrence is a *precondition*.
        first: mw_component::FirstRun::Immediate,
        phase: mw_component::Phase::Spread,
        floor: Duration::ZERO,
    };
    let station = station.clone();
    Ok(ComponentSpec::new(
        id,
        trigger,
        Builder::tick(move |_cx| {
            let body = Rearm::new(endpoint.clone(), port, station.clone());
            async move { Ok(body) }
        }),
    )
    // No needs, and no `identity`. W2a §4.1b lists `ConfigStore` for this component,
    // which is what a component that read the device row at run time would need; the
    // row is closed over at declaration time instead (§1.1), so requiring the capability
    // would make the station wait on a provider it never consults. And `identity` is
    // `None` because this component admits no readings -- §4a's checks are about
    // collection, and there is nothing here to identify.
    .config_digest(ConfigDigest::of(row)))
}

fn listener_spec(
    port: u16,
    group: Option<IpAddr>,
    units: Vec<(BroadcastUnit, &StationRow)>,
    ingress: Arc<dyn mw_component::Ingress>,
) -> Result<ComponentSpec, StationPlanError> {
    // Two stations presenting one serial would make resolution ambiguous, and the listener
    // keys its units by serial -- so one would silently shadow the other. The schema's
    // `stations_presented_key` unique index says the same thing; this is the reader that
    // must handle the state anyway.
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for (unit, row) in &units {
        let claim = unit.serial.to_ascii_lowercase();
        if let Some(first) = seen.insert(claim.clone(), row.key.as_str()) {
            return Err(StationPlanError::DuplicateClaim {
                first: first.to_string(),
                second: row.key.clone(),
                claim: unit.serial.clone(),
            });
        }
    }

    let id = wll::listener_id(port).map_err(|source| StationPlanError::Key {
        station: format!("the broadcast listener on port {port}"),
        source,
    })?;
    // Every row the listener closes over, so that adding a station to the endpoint or
    // changing one unit's mapping restarts it. One digest over the group rather than one
    // per row, because the component is per endpoint.
    let digest = ConfigDigest::of(&units.iter().map(|(_, row)| *row).collect::<Vec<_>>());
    let bodies: Vec<BroadcastUnit> = units.into_iter().map(|(unit, _)| unit).collect();
    Ok(ComponentSpec::new(
        id,
        Trigger::Service,
        Builder::serve(move |_cx| {
            let listener =
                BroadcastListener::new(port, group, bodies.clone(), Arc::clone(&ingress));
            async move { Ok(listener) }
        }),
    )
    .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
    .identity(Transport::UdpIn { port, group }.shape(&StationIdentity::FromPayload))
    .config_digest(digest))
}
