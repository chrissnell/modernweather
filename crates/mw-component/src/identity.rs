//! [`IdentityShape`] — the declarative half of W2a §4a, which is the half `plan()` can
//! check.
//!
//! **Why this is here and not in `mw-station`.** `Transport`, `Session` and
//! `StationIdentity` are `mw-station`'s (docs/service-architecture.md §1.2), and
//! `mw-supervisor` may not depend on `mw-station` — so `plan()` cannot see a
//! `Transport::HttpIn`. But two of W2a's structural errors are about exactly that:
//! `cardinality == One` iff `identity == Fixed` (**W2a Decision 42**) and each transport
//! implies its trigger (**W2a Decision 41**). So the *shape* of a station component's
//! identity is declared here, as pure data, and `mw-station` maps its own enums onto it.
//! That split is the same one §1.1 makes for the whole model: declaration is pure and
//! comes first; the impure thing it describes comes second.
//!
//! The payoff is that the Ambient bug is unrepresentable rather than fixed: an inbound
//! endpoint with a fixed identity — "whichever listener got the request is the station" —
//! fails `plan()` with both the component and the reason named.

use std::fmt;

use crate::trigger::Trigger;

/// How a station component's endpoint identifies the station a frame came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityShape {
    /// Who opens the connection.
    pub initiation: Initiation,
    /// Where the station identity comes from.
    pub source: IdentitySource,
    /// How many stations one endpoint serves.
    pub cardinality: Cardinality,
    /// Whether the endpoint is a long-lived stream or one request per occurrence.
    pub continuity: Continuity,
}

impl IdentityShape {
    /// The common case: one station on one wire, we initiate, long-lived.
    ///
    /// This is the convenience W2a §4a.3 asks for, so the abstraction earns its keep on
    /// Ambient and WeatherLink Live without taxing Davis.
    pub const FIXED_STREAM: Self = Self {
        initiation: Initiation::Outbound,
        source: IdentitySource::Fixed,
        cardinality: Cardinality::One,
        continuity: Continuity::Stream,
    };

    /// One station, we initiate, one request per occurrence — an HTTP poll.
    pub const FIXED_REQUEST: Self = Self {
        initiation: Initiation::Outbound,
        source: IdentitySource::Fixed,
        cardinality: Cardinality::One,
        continuity: Continuity::Request,
    };

    /// Why this shape cannot exist, if it cannot.
    ///
    /// Three rules, in the order they were decided:
    ///
    /// 1. **`cardinality == One` iff `source == Fixed`** (W2a Decision 42). Connection
    ///    direction only sets the *default*: the naive direction-based invariant was
    ///    broken by the MQTT flexibility test, which is an outbound 1:N subscription, and
    ///    that is what the test was for.
    /// 2. **An inbound endpoint cannot have a fixed identity.** That is the Ambient defect
    ///    — the station is identified positionally, by which `http.Server` received the
    ///    request, even though the request carries `?ID=`.
    /// 3. **Continuity implies the trigger** (W2a Decision 41): a stream is a `Service`
    ///    and a request/response is an occurrence, because a transport carrying
    ///    `every: Duration` would be the one place in the design where a component
    ///    schedules itself (W2a Decision 40).
    pub fn problem(&self, trigger: &Trigger) -> Option<IdentityProblem> {
        let fixed = self.source == IdentitySource::Fixed;
        let one = self.cardinality == Cardinality::One;
        if fixed != one {
            return Some(IdentityProblem::CardinalityDisagreesWithSource {
                source: self.source,
                cardinality: self.cardinality,
            });
        }
        if self.initiation == Initiation::Inbound && fixed {
            return Some(IdentityProblem::InboundCannotBeFixed);
        }
        if self.initiation == Initiation::Inbound && self.continuity != Continuity::Stream {
            return Some(IdentityProblem::InboundMustBeAStream);
        }
        match (self.continuity, trigger) {
            (Continuity::Stream, Trigger::Service) => None,
            (Continuity::Request, Trigger::Every { .. } | Trigger::OnDemand { .. }) => None,
            (continuity, trigger) => Some(IdentityProblem::TriggerDisagreesWithContinuity {
                continuity,
                trigger: trigger.kind(),
            }),
        }
    }
}

/// Who opens the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initiation {
    /// We do — serial, outbound TCP, an HTTP request, an MQTT subscribe.
    Outbound,
    /// They do — an HTTP endpoint, a UDP listener, an inbound gRPC service.
    Inbound,
}

/// Where the station identity comes from. W2a §4a.4's five strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// Configuration says which station. Exactly one station per endpoint.
    Fixed,
    /// A path segment — `/ingest/ambient/{station_key}`, or an MQTT topic segment.
    FromPath,
    /// A query parameter — Ambient sends `?ID=` and `?PASSWORD=`, identity and secret in
    /// one request.
    FromQuery,
    /// Inside the payload — WeatherLink Live broadcasts its DSN; APRS-IS carries a
    /// callsign.
    FromPayload,
    /// A peer registers and is issued an id it presents thereafter. One strategy of five,
    /// which is what generalises `RemoteStationRegistry` instead of special-casing it.
    FromRegistration,
}

impl IdentitySource {
    /// The spelling `config.stations.identity_kind` holds.
    ///
    /// Here rather than in the two crates that need it, because both of them do:
    /// `mw-station` parses the column into its own `StationIdentity` and `mw-store`'s
    /// registry has to compare a row's configured strategy against the one a frame
    /// arrived under. They are in the same layer and may not name each other, so a mapping
    /// in each is a mapping that can disagree — and the thing it would disagree about is
    /// which strategy authenticated a reading.
    ///
    /// Distinct from [`IdentitySource`]'s `Display`, which is the internal vocabulary
    /// `IdentityProblem` renders (**D-36**) and is deliberately not a column value.
    pub const fn schema_name(self) -> &'static str {
        match self {
            IdentitySource::Fixed => "fixed",
            IdentitySource::FromPath => "from_path",
            IdentitySource::FromQuery => "from_query",
            IdentitySource::FromPayload => "from_payload",
            IdentitySource::FromRegistration => "from_registration",
        }
    }

    /// Read the `identity_kind` column. `None` for a value its own `CHECK` would reject.
    pub fn from_schema_name(name: &str) -> Option<Self> {
        [
            IdentitySource::Fixed,
            IdentitySource::FromPath,
            IdentitySource::FromQuery,
            IdentitySource::FromPayload,
            IdentitySource::FromRegistration,
        ]
        .into_iter()
        .find(|s| s.schema_name() == name)
    }
}

impl fmt::Display for IdentitySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IdentitySource::Fixed => "Fixed",
            IdentitySource::FromPath => "FromPath",
            IdentitySource::FromQuery => "FromQuery",
            IdentitySource::FromPayload => "FromPayload",
            IdentitySource::FromRegistration => "FromRegistration",
        })
    }
}

/// How many stations one endpoint serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// One. The component is per station.
    One,
    /// Many. The component is per **endpoint**, so a second station is a configuration
    /// row rather than a second HTTP server.
    Many,
}

impl fmt::Display for Cardinality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Cardinality::One => "One",
            Cardinality::Many => "Many",
        })
    }
}

/// Whether the endpoint is a long-lived stream or one request per occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    /// A long-lived receive loop: serial, outbound TCP, a listener, a streaming RPC, an
    /// MQTT subscription.
    Stream,
    /// One request and one response per occurrence: an HTTP poll, a unary RPC.
    Request,
}

impl fmt::Display for Continuity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Continuity::Stream => "Stream",
            Continuity::Request => "Request",
        })
    }
}

/// Why an [`IdentityShape`] cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityProblem {
    /// The rule of W2a Decision 42, violated in one direction or the other.
    CardinalityDisagreesWithSource {
        /// What was declared.
        source: IdentitySource,
        /// What was declared.
        cardinality: Cardinality,
    },
    /// The Ambient defect: the station identified by whichever listener received the
    /// request.
    InboundCannotBeFixed,
    /// An endpoint they initiate is a listener, and a listener is a stream.
    InboundMustBeAStream,
    /// W2a Decision 41: a stream is a `Service`; a request/response is an occurrence.
    TriggerDisagreesWithContinuity {
        /// What was declared.
        continuity: Continuity,
        /// The trigger's `kind()`.
        trigger: &'static str,
    },
}

impl fmt::Display for IdentityProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityProblem::CardinalityDisagreesWithSource {
                source,
                cardinality,
            } => write!(
                f,
                "identity {source} with cardinality {cardinality}: one station per endpoint \
                 is permitted exactly when the identity is Fixed"
            ),
            IdentityProblem::InboundCannotBeFixed => f.write_str(
                "an endpoint the station initiates cannot have a Fixed identity -- the station \
                 would be whichever listener received the request",
            ),
            IdentityProblem::InboundMustBeAStream => {
                f.write_str("an endpoint the station initiates is a listener, which is a stream")
            }
            IdentityProblem::TriggerDisagreesWithContinuity {
                continuity,
                trigger,
            } => write!(
                f,
                "a {continuity} endpoint cannot have trigger `{trigger}`: a stream is a Service \
                 and a request/response is one occurrence"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn every() -> Trigger {
        Trigger::every(Duration::from_secs(30), Duration::from_secs(10))
    }

    /// The column's `CHECK` list and this mapping are one vocabulary, so they round-trip.
    #[test]
    fn every_schema_spelling_round_trips() {
        for source in [
            IdentitySource::Fixed,
            IdentitySource::FromPath,
            IdentitySource::FromQuery,
            IdentitySource::FromPayload,
            IdentitySource::FromRegistration,
        ] {
            let name = source.schema_name();
            assert_eq!(IdentitySource::from_schema_name(name), Some(source));
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "`{name}` is not a column spelling"
            );
            assert_ne!(
                name,
                source.to_string(),
                "the column value and the internal name are two vocabularies (D-36)"
            );
        }
        assert_eq!(IdentitySource::from_schema_name("whoever"), None);
    }

    #[test]
    fn the_common_case_is_accepted() {
        assert_eq!(IdentityShape::FIXED_STREAM.problem(&Trigger::Service), None);
        assert_eq!(IdentityShape::FIXED_REQUEST.problem(&every()), None);
    }

    /// Ambient, as it exists today: one station per listener, identified by the listener.
    #[test]
    fn an_inbound_endpoint_with_a_fixed_identity_is_rejected() {
        let ambient_today = IdentityShape {
            initiation: Initiation::Inbound,
            source: IdentitySource::Fixed,
            cardinality: Cardinality::One,
            continuity: Continuity::Stream,
        };
        assert_eq!(
            ambient_today.problem(&Trigger::Service),
            Some(IdentityProblem::InboundCannotBeFixed)
        );
    }

    /// MQTT: outbound and 1:N, which is why the invariant is about identity rather than
    /// direction.
    #[test]
    fn an_outbound_endpoint_may_serve_many_stations() {
        let mqtt = IdentityShape {
            initiation: Initiation::Outbound,
            source: IdentitySource::FromPath,
            cardinality: Cardinality::Many,
            continuity: Continuity::Stream,
        };
        assert_eq!(mqtt.problem(&Trigger::Service), None);
    }

    #[test]
    fn cardinality_and_source_must_agree() {
        let many_but_fixed = IdentityShape {
            cardinality: Cardinality::Many,
            ..IdentityShape::FIXED_STREAM
        };
        assert!(matches!(
            many_but_fixed.problem(&Trigger::Service),
            Some(IdentityProblem::CardinalityDisagreesWithSource { .. })
        ));
        let one_but_from_payload = IdentityShape {
            source: IdentitySource::FromPayload,
            ..IdentityShape::FIXED_STREAM
        };
        assert!(matches!(
            one_but_from_payload.problem(&Trigger::Service),
            Some(IdentityProblem::CardinalityDisagreesWithSource { .. })
        ));
    }

    #[test]
    fn continuity_implies_the_trigger() {
        assert!(matches!(
            IdentityShape::FIXED_STREAM.problem(&every()),
            Some(IdentityProblem::TriggerDisagreesWithContinuity { .. })
        ));
        assert!(matches!(
            IdentityShape::FIXED_REQUEST.problem(&Trigger::Service),
            Some(IdentityProblem::TriggerDisagreesWithContinuity { .. })
        ));
    }
}
