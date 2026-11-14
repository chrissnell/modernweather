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
//! | `type` | `transport_kind` | Components |
//! |---|---|---|
//! | `weatherlink-live` | `udp_in`, no `transport_host` | the shared `station:wll:udp:<port>` listener |
//! | `weatherlink-live` | `udp_in`, with `transport_host` | the listener, **and** `station:wll:<key>:rearm` |
//! | `weatherlink-live` | `http_out` | `station:wll:<key>`, polling `/v1/current_conditions` |
//! | `davis` | `serial` or `tcp_out` | `station:davis:<key>`, the wake-and-`LOOP` conversation |
//! | `campbellscientific` | `serial` or `tcp_out` | `station:campbell:<key>`, the line reader |
//! | `airgradient` | `http_out` | `station:airgradient:<key>`, polling `/measures/current` |
//! | `ambient-customized` | `http_in` | **one** `station:ambient` for every such row |
//! | `snowgauge` | `grpc_out` | validated here, planned by `mw-grpc` (see below) |
//! | disabled, or `transport_kind IS NULL` | | nothing, and that is not an error |
//!
//! `transport_host` is what distinguishes a WeatherLink Live this node arms from one
//! something else arms, and the schema makes it optional for `udp_in` — so a node that only
//! listens is a configuration rather than a mode.
//!
//! # Ambient is the structural row: N stations, one component
//!
//! Every `ambient-customized` row joins **one** `station:ambient` component, because W2a
//! §4a.5 gives that endpoint `Cardinality::Many` and §4a.4's invariant is that
//! `cardinality == One` if and only if `identity == Fixed`. A second Ambient station is a
//! configuration row; in the Go tree it is a second `http.Server` on a second port, and the
//! station's identity is *the port the request arrived on*.
//!
//! Two consequences this function enforces, each of which would otherwise be a silently
//! wrong configuration:
//!
//! - **Every Ambient row must agree about `transport_mount`.** One endpoint means one path.
//!   Two mounts is [`StationPlanError::TwoMounts`], naming both stations — rather than one
//!   of them silently never receiving anything.
//! - **An Ambient row must carry `ingest_secret_hash`.** `?PASSWORD=` is the only credential
//!   the transport has, so a row with no stored secret is one whose uploads nothing can
//!   check — which is exactly the Go behaviour §4a.6 calls a defect.
//!
//! # The snow gauge is validated here and planned elsewhere
//!
//! `ci/layers.toml` forbids this crate from naming `tonic` (§1.4: the gRPC socket and every
//! gRPC service belong to `mw-http` and `mw-grpc`), so the component that dials a gauge
//! cannot be built here. What this function does with a `snowgauge` row is **check its
//! shape** — `grpc_out`, streaming, `Fixed` — so that a malformed one is refused by the same
//! function and with the same sentences as every other station's, and then emit nothing.
//! `crate::snowgauge` has the whole of that split, and `mw_grpc::snowgauge::register` is the
//! other half.
//!
//! # A station whose driver has not landed is an error, not a silence
//!
//! A row whose `type` no driver in this build serves is refused here by name. The
//! alternative — skipping it — is the defect this programme keeps finding: a configured
//! station that produces no readings, no error and no log line.

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
use crate::driver::Conversation;
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

/// The longest a station key may be before some component id built from it overflows.
///
/// [`ComponentId::MAX_LEN`] is 128 and the longest wrapper **in this crate** is
/// `station:airgradient:` at 20 bytes, so 108 names a component for every driver here and
/// 109 does not — which is what `tests/station_key_width.rs` checks, in both directions
/// and against the mint functions rather than against this number.
///
/// **The margin is one byte, and the binding wrapper is not in this crate.**
/// `mw_output::aeris` builds `aeris:forecast:<key>:<span_hours>`, 19 bytes around the key
/// at a three-digit span, which allows 109. `ci/layers.toml` puts `mw-output` above this
/// crate, so neither the constant nor the test can see it: a longer prefix added *there*
/// invalidates this number silently, and that is the one direction to check by hand.
///
/// It is here because the limit is not only the daemon's. The Go-to-Rust migration derives
/// a station key per station and has to refuse one it could not configure afterwards, which
/// `w9.station_key_problem` does with this same limit — a key the migration minted and the
/// plan then rejected is M5-1b. That function is emitted by `xtask/src/legacy.rs`.
pub const MAX_KEY_LEN: usize = 108;

/// Why a set of station rows does not produce a plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StationPlanError {
    /// A station type no driver in this build serves.
    #[error(
        "station `{station}` is a `{kind}`, and this build has no driver for it. The \
         configured types it serves are `weatherlink-live`, `davis`, \
         `campbellscientific`, `ambient-customized`, `airgradient` and `snowgauge`."
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

    /// A device whose local API is plain HTTP, configured as `https`.
    #[error(
        "station `{station}` is a `{kind}` configured as `https`: that device's local API \
         is plain HTTP and it has no TLS listener, so this address can never answer"
    )]
    NotTls {
        /// Which station.
        station: String,
        /// Its configured type.
        kind: String,
    },

    /// A station whose identity strategy its driver does not implement.
    ///
    /// Distinct from [`StationPlanError::WrongIdentity`], which is the WeatherLink Live's
    /// own sentence about its two modes. This one is for every driver with exactly one
    /// answer, and it is the error that catches the Ambient defect from the other side: a
    /// `from_payload` Ambient row would decode and then resolve against the wrong namespace.
    #[error(
        "station `{station}` is a `{kind}` with identity `{found}`, and that driver \
         resolves `{expected}`"
    )]
    IdentityMismatch {
        /// Which station.
        station: String,
        /// Its configured type.
        kind: String,
        /// What the driver implements.
        expected: &'static str,
        /// What the row holds.
        found: &'static str,
    },

    /// Two Ambient stations on two different mounts.
    #[error(
        "stations `{first}` and `{second}` are both `ambient-customized` and mount \
         `{first_mount}` and `{second_mount}`: Ambient is N stations on ONE endpoint \
         (docs/component-model.md §4a.5), so every such row has to carry the same \
         `transport_mount`"
    )]
    TwoMounts {
        /// The first station, in plan order.
        first: String,
        /// Its mount.
        first_mount: String,
        /// The second station.
        second: String,
        /// Its mount.
        second_mount: String,
    },

    /// An Ambient station with no stored secret.
    #[error(
        "station `{station}` is `ambient-customized` and has no `ingest_secret_hash`: \
         `?PASSWORD=` is the only credential that transport has, so without one its \
         uploads cannot be authenticated -- which is the defect \
         docs/component-model.md §4a.6 names. Set the station's ingest secret."
    )]
    NoSecret {
        /// Which station.
        station: String,
    },

    /// An Ambient station in a process with no listener.
    #[error(
        "station `{station}` is `ambient-customized` and this process has no HTTP \
         listener, so there is no route for its uploads to arrive on"
    )]
    NoUploadState {
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

    // Broadcast units, grouped by the endpoint they arrive on, because the WeatherLink
    // Live listener is one component per endpoint (`crate::wll`, decision 1).
    let mut listeners: Endpoints<'_> = BTreeMap::new();
    // Every Ambient station, which becomes ONE component (§4a.5). The mount travels with
    // the first row so that a second one that disagrees is a named refusal.
    let mut ambient: Vec<(crate::ambient::AmbientUnit, &StationRow)> = Vec::new();
    // Owned rather than borrowed: the mount lives inside the per-row `Transport`, which is
    // dropped at the end of each iteration.
    let mut ambient_mount: Option<(String, String)> = None;

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
        let station = StationKey::new(row.key.as_str());

        match row.kind.as_str() {
            wll::STATION_TYPE => {
                wll_row(
                    row,
                    &transport,
                    &identity,
                    columns,
                    &station,
                    ingress,
                    &mut listeners,
                    out,
                )?;
            }
            crate::davis::STATION_TYPE => {
                expect_identity(row, &identity, StationIdentity::Fixed)?;
                out.push(stream_spec(
                    row,
                    &transport,
                    &identity,
                    crate::davis::station_id,
                    Conversation::DavisLoop {
                        run_length: crate::davis::RUN_LENGTH,
                    },
                    Arc::new(mw_codec::DavisLoop::new(row.wind_dir_correction())),
                    station,
                    Arc::clone(ingress),
                )?);
            }
            crate::campbell::STATION_TYPE => {
                expect_identity(row, &identity, StationIdentity::Fixed)?;
                out.push(stream_spec(
                    row,
                    &transport,
                    &identity,
                    crate::campbell::station_id,
                    Conversation::Lines,
                    Arc::new(mw_codec::CampbellAscii::new(row.wind_dir_correction())),
                    station,
                    Arc::clone(ingress),
                )?);
            }
            crate::airgradient::STATION_TYPE => {
                expect_identity(row, &identity, StationIdentity::Fixed)?;
                out.push(airgradient_spec(
                    row,
                    &transport,
                    &identity,
                    station,
                    Arc::clone(ingress),
                )?);
            }
            crate::ambient::STATION_TYPE => {
                expect_identity(row, &identity, StationIdentity::FromQuery)?;
                let Transport::HttpIn { mount } = &transport else {
                    return Err(StationPlanError::WrongTransport {
                        station: row.key.clone(),
                        kind: row.kind.clone(),
                        transport: transport.kind(),
                    });
                };
                // One endpoint, one path. A second mount is two endpoints, and this build
                // serves one (§4a.5).
                match &ambient_mount {
                    None => ambient_mount = Some((mount.clone(), row.key.clone())),
                    Some((first_mount, first)) if first_mount != mount => {
                        return Err(StationPlanError::TwoMounts {
                            first: first.clone(),
                            first_mount: first_mount.clone(),
                            second: row.key.clone(),
                            second_mount: mount.clone(),
                        })
                    }
                    Some(_) => {}
                }
                if !row.has_ingest_secret {
                    return Err(StationPlanError::NoSecret {
                        station: row.key.clone(),
                    });
                }
                let claim = row.presented_key.clone().filter(|c| !c.is_empty()).ok_or(
                    StationPlanError::Missing {
                        station: row.key.clone(),
                        kind: row.kind.clone(),
                        column: "presented_key",
                        because: Because(
                            "an Ambient console identifies itself with the `?ID=` an \
                             operator typed into its web interface, and that identifier is \
                             what this endpoint resolves against",
                        ),
                    },
                )?;
                ambient.push((
                    crate::ambient::AmbientUnit {
                        claim,
                        station,
                        codec: mw_codec::AmbientQuery::new(row.wind_dir_correction()),
                    },
                    row,
                ));
            }
            crate::snowgauge::STATION_TYPE => {
                // Checked and not planned: see this module's documentation and
                // `crate::snowgauge`. `mw_grpc::snowgauge::register` emits the component,
                // and calls this same function rather than trusting that this one ran.
                check_snowgauge(row)?;
            }
            _ => {
                return Err(StationPlanError::NoDriver {
                    station: row.key.clone(),
                    kind: row.kind.clone(),
                })
            }
        }
    }

    for ((port, group), units) in listeners {
        out.push(listener_spec(port, group, units, Arc::clone(ingress))?);
    }
    if !ambient.is_empty() {
        let (mount, _) = ambient_mount.expect("a non-empty Ambient set has a mount");
        out.push(ambient_spec(&mount, ambient, cfg, Arc::clone(ingress))?);
    }
    Ok(())
}

/// The path the Ambient ingest endpoint would claim, if this configuration has one.
///
/// **The registrar's own predicate, asked by the binary.** The `axum` route has to be built
/// *before* `plan()` runs — the component installs its handler into a slot the route reads,
/// and both come from crates `mw-http` may not name (`crate::ambient`) — so something has to
/// answer "is there an Ambient endpoint, and where" without planning. A second reading of
/// the configuration is how a table gets mounted that no component fills, which is the
/// defect M4-2's review found on the frozen surface; this is the same filters and the same
/// column, in one place.
///
/// The **first** such row's mount, in plan order. A configuration whose Ambient rows
/// disagree is [`StationPlanError::TwoMounts`] from [`register`], and
/// `crates/modernweather/src/boot.rs` checks after `plan()` that what was mounted and what
/// was planned agree — so disagreeing here is a loud failure rather than a silent one.
pub fn ambient_mount<C>(cfg: &C) -> Option<String>
where
    C: StationConfig + ?Sized,
{
    cfg.stations()
        .iter()
        .filter(|row| row.is_collected_here() && row.kind == crate::ambient::STATION_TYPE)
        .filter_map(|row| row.transport.as_ref()?.mount.clone())
        .find(|mount| !mount.is_empty())
}

/// Validate a `snowgauge` row, returning the component id it will be planned under.
///
/// **Public because it has two callers and one of them is in another crate.**
/// `mw_grpc::snowgauge::register` builds the component this row describes -- `ci/layers.toml`
/// forbids `mw-station` from naming `tonic`, which is the whole of why the split exists --
/// and it used to filter on `kind` alone and rely on [`plan`] having already refused a row
/// of the wrong shape. That held only because `Stations` precedes `SnowGauges` in the
/// binary's registrar list, which `crate::boot`'s own comment describes as having no effect
/// on the plan; reordering the list would have produced a component declaring
/// `GrpcOut { streaming: true }` for a row configured request/response, or a `from_path`
/// row with no receiver entry and every reading `unknown_station`. One function, called by
/// both, is the arrangement in which the two cannot disagree.
pub fn check_snowgauge(row: &StationRow) -> Result<ComponentId, StationPlanError> {
    let identity = StationIdentity::parse(&row.identity_kind).map_err(|source| {
        StationPlanError::Identity {
            station: row.key.clone(),
            source,
        }
    })?;
    expect_identity(row, &identity, StationIdentity::Fixed)?;
    let columns = row
        .transport
        .as_ref()
        .ok_or_else(|| StationPlanError::WrongTransport {
            station: row.key.clone(),
            kind: row.kind.clone(),
            transport: "none",
        })?;
    let transport =
        Transport::from_columns(columns).map_err(|source| StationPlanError::Transport {
            station: row.key.clone(),
            source,
        })?;
    let Transport::GrpcOut { streaming, .. } = &transport else {
        return Err(StationPlanError::WrongTransport {
            station: row.key.clone(),
            kind: row.kind.clone(),
            transport: transport.kind(),
        });
    };
    if !streaming {
        return Err(StationPlanError::Missing {
            station: row.key.clone(),
            kind: row.kind.clone(),
            column: "transport_streaming",
            because: Because(
                "a snow gauge streams readings, so the flag has to be set: \
                 `transport_streaming = 0` would declare one request per occurrence and no \
                 gauge answers one",
            ),
        });
    }
    component_id(row, crate::snowgauge::station_id)
}

/// Refuse a row whose identity strategy its driver does not implement.
///
/// The schema's own `CHECK` rejects `fixed` with an inbound transport, which is the Ambient
/// defect made unrepresentable; what it cannot say is which of the five strategies *this
/// driver* resolves. A `from_payload` Ambient row would decode and then resolve against the
/// wrong namespace, and a `from_query` Davis row would present a claim no Davis console
/// sends.
fn expect_identity(
    row: &StationRow,
    found: &StationIdentity,
    expected: StationIdentity,
) -> Result<(), StationPlanError> {
    if *found == expected {
        return Ok(());
    }
    Err(StationPlanError::IdentityMismatch {
        station: row.key.clone(),
        kind: row.kind.clone(),
        expected: expected.as_str(),
        found: found.as_str(),
    })
}

/// One WeatherLink Live row: the broadcast listener's unit, the re-arm, or the poll.
#[allow(clippy::too_many_arguments)]
fn wll_row<'a>(
    row: &'a StationRow,
    transport: &Transport,
    identity: &StationIdentity,
    columns: &crate::transport::TransportColumns,
    station: &StationKey,
    ingress: &Arc<dyn mw_component::Ingress>,
    listeners: &mut Endpoints<'a>,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), StationPlanError> {
    // **The transport first, then the columns.** `mapping` reads
    // `wll_sensor_mapping`, which only a WeatherLink Live row has any use for, so running
    // it before the dispatch below told an operator with
    // `type = 'weatherlink-live', transport_kind = 'serial'` to set a column that row will
    // never use -- instead of naming the transport, which is the thing that is wrong.
    if !matches!(
        transport,
        Transport::UdpIn { .. } | Transport::HttpOut { .. }
    ) {
        return Err(StationPlanError::WrongTransport {
            station: row.key.clone(),
            kind: row.kind.clone(),
            transport: transport.kind(),
        });
    }
    let codec = WllConditions::new(mapping(row)?);
    // The schema's identity CHECK rejects `fixed` with an inbound transport, which is the
    // Ambient defect; what it cannot say is which of the four non-`fixed` strategies *this
    // driver* implements. A `udp_in` WeatherLink Live resolves by the serial in its
    // datagrams and nothing else, so `from_path` or `from_query` on one is a row that would
    // decode and then resolve to nothing.
    let expected = match transport {
        Transport::UdpIn { .. } => Some(StationIdentity::FromPayload),
        Transport::HttpOut { .. } => Some(StationIdentity::Fixed),
        _ => None,
    };
    if let Some(expected) = expected {
        if *identity != expected {
            return Err(StationPlanError::WrongIdentity {
                station: row.key.clone(),
                transport: transport.kind(),
                identity: identity.as_str(),
            });
        }
    }

    match transport {
        Transport::UdpIn { port, group } => {
            let serial = row.presented_key.clone().filter(|s| !s.is_empty()).ok_or(
                StationPlanError::Missing {
                    station: row.key.clone(),
                    kind: row.kind.clone(),
                    column: "presented_key",
                    because: Because(
                        "a broadcasting WeatherLink Live identifies itself by the device \
                         serial in each datagram, and that serial is the allow-list this \
                         endpoint resolves against",
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
                // free-text column, and a value with a scheme, a port or -- the one that
                // matters -- a credential in it would otherwise become the host verbatim
                // and be printed back in every request error.
                let endpoint =
                    Endpoint::bare_host(host).map_err(|source| StationPlanError::Host {
                        station: row.key.clone(),
                        source,
                    })?;
                out.push(rearm_spec(row, endpoint, *port, station)?);
            }
            Ok(())
        }
        Transport::HttpOut { url, method, auth } => {
            refuse_tls(row, url)?;
            out.push(poll_spec(
                row,
                transport,
                identity,
                url.clone(),
                *method,
                auth.clone(),
                station.clone(),
                codec,
                Arc::clone(ingress),
            )?);
            Ok(())
        }
        other => Err(StationPlanError::WrongTransport {
            station: row.key.clone(),
            kind: row.kind.clone(),
            transport: other.kind(),
        }),
    }
}

/// A device whose local API is plain HTTP, configured as `https`.
///
/// Both of this build's polled devices have one -- a WeatherLink Live and an AirGradient --
/// and neither has a TLS listener, so the address can never answer. A plan error rather
/// than a component in permanent `Backoff`, which is the shape `plan()` is pure in order to
/// catch.
fn refuse_tls(row: &StationRow, url: &Endpoint) -> Result<(), StationPlanError> {
    if url.is_tls() {
        return Err(StationPlanError::NotTls {
            station: row.key.clone(),
            kind: row.kind.clone(),
        });
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
        let columns = row.transport.as_ref().expect("a collected station");
        // The same dispatch `register` makes, over the same rows, and the reason both
        // exist is that a `Fixed` frame resolves **by its receiver**: a station whose
        // component id here differs from the one `register` emits is a station whose every
        // reading is `unknown_station`, with nothing naming it.
        //
        // Only a component that *emits readings* belongs here. The WeatherLink Live re-arm
        // is a control call and admits nothing, so it is deliberately absent: an entry for
        // it would let a frame resolve through a component that never receives one. The
        // snow gauge IS here, even though `register` does not emit its spec --
        // `crate::snowgauge` has why, and `mw_grpc` mints the same id from the same
        // function.
        let mint: fn(&str) -> Result<ComponentId, ComponentIdError> =
            match (row.kind.as_str(), columns.kind.as_str()) {
                (wll::STATION_TYPE, "http_out") => wll::poll_id,
                (crate::davis::STATION_TYPE, "serial" | "tcp_out") => crate::davis::station_id,
                (crate::campbell::STATION_TYPE, "serial" | "tcp_out") => {
                    crate::campbell::station_id
                }
                (crate::airgradient::STATION_TYPE, "http_out") => crate::airgradient::station_id,
                (crate::snowgauge::STATION_TYPE, "grpc_out") => crate::snowgauge::station_id,
                // Every other pairing is one `register` refuses, so there is no component
                // for a frame to arrive at. Skipped rather than refused here, because
                // `register` is where a configuration error gets its sentence and two
                // functions reporting the same one is two sentences to keep in step.
                _ => continue,
            };
        out.push(FixedReceiver {
            component: component_id(row, mint)?,
            station: StationKey::new(row.key.as_str()),
        });
    }
    Ok(out)
}

/// One streaming station: Davis or Campbell, `Serial` or `TcpOut`.
///
/// `Trigger::Service`, because a conversation owns its own loop; the identity shape is
/// `Fixed`/`One`/`Stream`/`Outbound`, and `IdentityShape::problem` is what rejects a
/// trigger that disagrees with it.
#[allow(clippy::too_many_arguments)]
fn stream_spec(
    row: &StationRow,
    transport: &Transport,
    identity: &StationIdentity,
    mint: fn(&str) -> Result<ComponentId, ComponentIdError>,
    conversation: Conversation,
    codec: Arc<dyn mw_codec::Codec>,
    station: StationKey,
    ingress: Arc<dyn mw_component::Ingress>,
) -> Result<ComponentSpec, StationPlanError> {
    // The two transports a conversation runs over, and nothing else. `crate::stream::open`
    // refuses the rest too, and this is where it is a configuration error an operator can
    // read rather than a component that fails every start attempt.
    match transport {
        Transport::Serial { .. } | Transport::TcpOut { .. } => {}
        other => {
            return Err(StationPlanError::WrongTransport {
                station: row.key.clone(),
                kind: row.kind.clone(),
                transport: other.kind(),
            })
        }
    }
    let id = component_id(row, mint)?;
    // Before the clone moves into the builder: the shape is what `plan()` checks the
    // trigger against, and it is derived from the transport rather than restated.
    let shape = transport.shape(identity);
    let transport = transport.clone();
    Ok(ComponentSpec::new(
        id,
        Trigger::Service,
        Builder::serve(move |_cx| {
            let transport = transport.clone();
            let codec = Arc::clone(&codec);
            let station = station.clone();
            let ingress = Arc::clone(&ingress);
            async move {
                // The connection is the BUILD, so a console that is unplugged is
                // `Backoff { until }` with a readable reason rather than a startup abort
                // (W2a Decision 3). `crate::driver` has the whole of why there is no
                // reconnect loop below this.
                let station = crate::driver::StreamStation::connect(
                    &transport,
                    conversation,
                    codec,
                    station,
                    ingress,
                )
                .await?;
                Ok(station)
            }
        }),
    )
    .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
    .identity(shape)
    .config_digest(ConfigDigest::of(row)))
}

/// One AirGradient monitor: `HttpOut`, `Every { 3s }`.
fn airgradient_spec(
    row: &StationRow,
    transport: &Transport,
    identity: &StationIdentity,
    station: StationKey,
    ingress: Arc<dyn mw_component::Ingress>,
) -> Result<ComponentSpec, StationPlanError> {
    let Transport::HttpOut { url, .. } = transport else {
        return Err(StationPlanError::WrongTransport {
            station: row.key.clone(),
            kind: row.kind.clone(),
            transport: transport.kind(),
        });
    };
    refuse_tls(row, url)?;
    let id = component_id(row, crate::airgradient::station_id)?;
    let url = url.clone();
    let trigger = Trigger::Every {
        period: crate::airgradient::DEFAULT_POLL,
        // Equal to the period, which is the largest `plan()` permits: the request's own
        // timeout is shorter still, so the message an operator reads names the device.
        deadline: crate::airgradient::DEFAULT_POLL,
        first: mw_component::FirstRun::AtPhase,
        phase: mw_component::Phase::Spread,
        // Declared by the driver, enforced once here: W2a Decision 56, and the reason a
        // sub-floor period is a startup refusal rather than a per-occurrence clamp.
        floor: crate::airgradient::POLL_FLOOR,
    };
    Ok(ComponentSpec::new(
        id,
        trigger,
        Builder::tick(move |_cx| {
            let body =
                crate::airgradient::Poll::new(url.clone(), station.clone(), Arc::clone(&ingress));
            async move { Ok(body) }
        }),
    )
    .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
    .identity(transport.shape(identity))
    .config_digest(ConfigDigest::of(row)))
}

/// The **one** Ambient endpoint, for every `ambient-customized` row.
///
/// `Trigger::Service` and `Provision::routes`: the component holds its handler installed in
/// the slot the binary owns, so "the route answers" and "the component is `Running`" are one
/// fact (`crate::ambient`).
fn ambient_spec<C>(
    mount: &str,
    units: Vec<(crate::ambient::AmbientUnit, &StationRow)>,
    cfg: &C,
    ingress: Arc<dyn mw_component::Ingress>,
) -> Result<ComponentSpec, StationPlanError>
where
    C: StationConfig + ?Sized,
{
    // Two stations presenting one `?ID=` would make resolution ambiguous, and this endpoint
    // keys its units by claim -- so one would silently shadow the other. The schema's
    // `stations_presented_key` unique index says the same thing; this is the reader that
    // must handle the state anyway.
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for (unit, row) in &units {
        let claim = unit.claim.to_ascii_lowercase();
        if let Some(first) = seen.insert(claim, row.key.as_str()) {
            return Err(StationPlanError::DuplicateClaim {
                first: first.to_string(),
                second: row.key.clone(),
                claim: unit.claim.clone(),
            });
        }
    }

    let first = units[0].1;
    let state =
        Arc::clone(
            cfg.ambient_upload_state()
                .ok_or_else(|| StationPlanError::NoUploadState {
                    station: first.key.clone(),
                })?,
        );
    let id = crate::ambient::endpoint_id().map_err(|source| StationPlanError::Key {
        station: format!("the Ambient ingest endpoint on `{mount}`"),
        source,
    })?;
    // Every row the endpoint closes over, so that adding a station to it or changing one
    // console's calibration restarts it. One digest over the group rather than one per row,
    // because the component is per endpoint.
    let digest = ConfigDigest::of(&units.iter().map(|(_, row)| *row).collect::<Vec<_>>());
    let bodies: Vec<crate::ambient::AmbientUnit> =
        units.into_iter().map(|(unit, _)| unit).collect();
    let mount = mount.to_string();
    let receiver = id.clone();
    // The pure half of the provision (**D-51**) and the identity shape, both over the one
    // mount -- computed before the builder's closure takes its own copy, so there is one
    // string and the three cannot disagree about which path is claimed.
    let manifest = crate::ambient::manifest(&mount);
    let shape = Transport::HttpIn {
        mount: mount.clone(),
    }
    .shape(&StationIdentity::FromQuery);
    Ok(ComponentSpec::new(
        id,
        Trigger::Service,
        Builder::serve(move |cx| {
            let state = Arc::clone(&state);
            let bodies = bodies.clone();
            let ingress = Arc::clone(&ingress);
            let mount = mount.clone();
            let receiver = receiver.clone();
            async move {
                let routes = cx.publisher::<Arc<dyn mw_component::RouteProvider>>()?;
                let handler = Arc::new(crate::ambient::Handler::new(receiver, bodies, ingress));
                Ok(crate::ambient::Endpoint::new(state, handler, routes, mount))
            }
        }),
    )
    .requires([Need::hard_write(mw_component::Capability::ReadingStream)])
    .provides([mw_component::Provision::routes(manifest)])
    .identity(shape)
    .config_digest(digest))
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
