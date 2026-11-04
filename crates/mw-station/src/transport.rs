//! [`Transport`] — W2a §4a.3's enum, and the two implications `plan()` checks through it.
//!
//! A transport is *how the bytes move*, and nothing else: it holds no timer, no codec and
//! no station. The absence of a timer is the decision rather than an omission. W2a §4a.3
//! answers the prompt's `HttpPoll` question by making an HTTP poll `Trigger::Every` wrapped
//! around one request, because a transport carrying `every: Duration` would be the one
//! place in the design where a component schedules itself and **D-25** gives every clock to
//! the supervisor.
//!
//! Each variant therefore *implies* its trigger and its identity shape, and both
//! implications are checked at plan time rather than documented:
//!
//! | Transport | Continuity | Trigger | Initiation |
//! |---|---|---|---|
//! | `Serial`, `TcpOut`, `GrpcOut { streaming: true }`, `MqttSub` | `Stream` | `Service` | outbound |
//! | `HttpOut`, `GrpcOut { streaming: false }` | `Request` | `Every` | outbound |
//! | `HttpIn`, `UdpIn` | `Stream` | `Service` | inbound |
//!
//! [`Transport::shape`] produces the [`IdentityShape`] that encodes it, and
//! `mw_component::IdentityShape::problem` is what rejects an impossible combination —
//! including the Ambient defect, an inbound endpoint with a `Fixed` identity.
//!
//! **All seven variants are here and only two are driven by a component in this
//! milestone.** The enum is pure declaration, `plan()`'s checks are over the declaration,
//! and a `Transport` with no driver is a plan error that names the station and the work
//! item that adds one (`crate::plan`) — not a station that starts and reports nothing,
//! which is the failure mode this programme keeps finding.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

use mw_component::{Cardinality, Continuity, IdentityShape, IdentitySource, Initiation};
use secrecy::SecretString;

use crate::endpoint::{Endpoint, EndpointError};
use crate::identity::StationIdentity;

/// How a station's bytes move.
#[derive(Debug, Clone)]
pub enum Transport {
    // --- we initiate ---
    /// A serial port. Davis and Campbell (M6-1).
    Serial {
        /// The device path.
        dev: PathBuf,
        /// The line rate.
        baud: u32,
    },
    /// An outbound TCP connection. A Davis console on a serial-to-Ethernet bridge, or
    /// APRS-IS as an input (§4a.7).
    TcpOut {
        /// The host, as configured.
        host: String,
        /// The port.
        port: u16,
        /// Whether to wrap the connection in TLS, and with what.
        tls: Option<TlsFiles>,
    },
    /// **One request per occurrence.** `Trigger::Every` owns the cadence, not this.
    HttpOut {
        /// The device's address. No path: the request paths are the driver's
        /// (`crate::endpoint`).
        url: Endpoint,
        /// The method, which is `GET` for every driver that exists.
        method: HttpMethod,
        /// A credential for a vendor cloud API (§4a.7). Never logged (**D-33**).
        auth: Option<SecretString>,
    },
    /// An outbound gRPC channel. The snow gauge (M6-3).
    GrpcOut {
        /// The peer's address.
        endpoint: Endpoint,
        /// Whether the call is a stream, which is what decides `Service` against `Every`.
        streaming: bool,
    },
    /// An MQTT subscription — the §4a.7 flexibility test, and the case that corrected the
    /// cardinality invariant.
    MqttSub {
        /// The broker.
        broker: Endpoint,
        /// The topic filter, whose wildcard segment carries the station identity.
        topic: String,
    },

    // --- they initiate ---
    /// A route on the one listener. Ambient (M6-1), which provides `HttpRoutes`.
    HttpIn {
        /// Where it mounts.
        mount: String,
    },
    /// A UDP receive socket. WeatherLink Live's broadcast.
    ///
    /// The one inbound transport that does **not** provide `HttpRoutes` and the one place
    /// outside `mw-http` that may open a socket: `ci/layers.toml`'s
    /// `only-mw-http-binds-a-socket` rule deliberately omits `UdpSocket::bind`, because
    /// §1.4's monopoly is over *listeners* and a receive socket is not one.
    UdpIn {
        /// The port the device broadcasts to — 22222 for a WeatherLink Live.
        port: u16,
        /// A multicast group to join, for a device that multicasts rather than broadcasts.
        group: Option<IpAddr>,
    },
}

/// The methods a station transport uses.
///
/// Two, because that is how many any driver needs, and a closed enum rather than
/// `http::Method` so that `transport_method` cannot hold `CONNECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HttpMethod {
    /// The default, and what every station driver uses.
    #[default]
    Get,
    /// For a vendor API that takes a body.
    Post,
}

impl HttpMethod {
    /// The method name, for the request line.
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(HttpMethod::Get),
            "POST" => Some(HttpMethod::Post),
            _ => None,
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three TLS paths a `tcp_out` transport can carry.
///
/// Paths rather than loaded material: loading is phase 2 (`build`), and a certificate that
/// cannot be read is a `BuildError` and therefore a lifecycle transition — which is the
/// whole of W2a Decision 3. A spec that held the bytes would have to read the filesystem to
/// be constructed, and `plan()` is pure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TlsFiles {
    /// The CA bundle to verify the peer against.
    pub ca: Option<PathBuf>,
    /// A client certificate.
    pub cert: Option<PathBuf>,
    /// Its key.
    pub key: Option<PathBuf>,
}

/// The `transport_*` columns of one `config.stations` row, as text.
///
/// The shape of the *table*, not of the enum: the binary copies the row into this and
/// `Transport::from_columns` validates it. That split is `ci/layers.toml`'s doing and is
/// the right way round — `mw-store` owns the query and may not name this crate, this crate
/// owns what a transport is and may not name `mw-store`, and the binary is the one place
/// they meet (docs/service-architecture.md §1.3, rule 1).
#[derive(Debug, Clone, Default)]
pub struct TransportColumns {
    /// `transport_kind`.
    pub kind: String,
    /// `transport_dev`.
    pub dev: Option<String>,
    /// `transport_baud`.
    pub baud: Option<i64>,
    /// `transport_host`.
    pub host: Option<String>,
    /// `transport_port`.
    pub port: Option<i64>,
    /// `transport_url`.
    pub url: Option<String>,
    /// `transport_method`.
    pub method: Option<String>,
    /// `transport_auth`.
    pub auth: Option<SecretString>,
    /// `transport_streaming`.
    pub streaming: Option<bool>,
    /// `transport_mount`.
    pub mount: Option<String>,
    /// `transport_group`.
    pub group: Option<String>,
    /// `transport_topic`.
    pub topic: Option<String>,
    /// `transport_tls_ca_path`.
    pub tls_ca: Option<String>,
    /// `transport_tls_cert_path`.
    pub tls_cert: Option<String>,
    /// `transport_tls_key_path`.
    pub tls_key: Option<String>,
}

/// Why a row's `transport_*` columns are not a [`Transport`].
///
/// Every variant duplicates a `CHECK` in `migrations/config/`, and the duplication is the
/// point: the schema stops a bad row being *written* and this stops the daemon acting on
/// one that is already there — a column added by hand, or by an older build. "A schema that
/// can hold the state means every reader has to handle it" is the comment on the schema's
/// own identity CHECK, and this is that reader.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// A `transport_kind` this build does not know.
    #[error("`{kind}` is not a transport kind")]
    UnknownKind {
        /// What the column held.
        kind: String,
    },
    /// A column the kind requires is NULL.
    #[error("a `{kind}` transport needs `{column}`, which is not set")]
    Missing {
        /// The kind.
        kind: String,
        /// The column.
        column: &'static str,
    },
    /// A column holds something outside its own range.
    #[error("`{column}` holds `{value}`, which is not {expected}")]
    OutOfRange {
        /// The column.
        column: &'static str,
        /// What it held.
        value: String,
        /// What it should have been.
        expected: &'static str,
    },
    /// `transport_url` is not an address.
    #[error("`{column}`: {source}")]
    Address {
        /// Which column.
        column: &'static str,
        /// Why it is not an address.
        #[source]
        source: EndpointError,
    },
}

impl Transport {
    /// Validate one row's transport columns.
    pub fn from_columns(c: &TransportColumns) -> Result<Self, TransportError> {
        let kind = c.kind.as_str();
        match kind {
            "serial" => Ok(Transport::Serial {
                dev: PathBuf::from(text(c.dev.as_deref(), kind, "transport_dev")?),
                baud: u32::try_from(number(c.baud, kind, "transport_baud")?).map_err(|_| {
                    TransportError::OutOfRange {
                        column: "transport_baud",
                        value: c.baud.unwrap_or_default().to_string(),
                        expected: "a line rate",
                    }
                })?,
            }),
            "tcp_out" => Ok(Transport::TcpOut {
                host: text(c.host.as_deref(), kind, "transport_host")?.to_string(),
                port: port(c.port, kind, "transport_port")?,
                tls: tls(c),
            }),
            "http_out" => Ok(Transport::HttpOut {
                url: address(c.url.as_deref(), kind, "transport_url")?,
                method: match c.method.as_deref() {
                    None => HttpMethod::default(),
                    Some(m) => HttpMethod::parse(m).ok_or(TransportError::OutOfRange {
                        column: "transport_method",
                        value: m.to_string(),
                        expected: "GET or POST",
                    })?,
                },
                auth: c.auth.clone(),
            }),
            "grpc_out" => Ok(Transport::GrpcOut {
                endpoint: address(c.url.as_deref(), kind, "transport_url")?,
                // The schema requires this column for `grpc_out` precisely because it is
                // what the trigger implication reads (W2a §4a.3), so its absence is a
                // transport whose trigger cannot be decided.
                streaming: c.streaming.ok_or(TransportError::Missing {
                    kind: kind.to_string(),
                    column: "transport_streaming",
                })?,
            }),
            "mqtt_sub" => Ok(Transport::MqttSub {
                broker: address(c.url.as_deref(), kind, "transport_url")?,
                topic: text(c.topic.as_deref(), kind, "transport_topic")?.to_string(),
            }),
            "http_in" => Ok(Transport::HttpIn {
                mount: text(c.mount.as_deref(), kind, "transport_mount")?.to_string(),
            }),
            "udp_in" => Ok(Transport::UdpIn {
                port: port(c.port, kind, "transport_port")?,
                group: match c.group.as_deref().filter(|g| !g.is_empty()) {
                    None => None,
                    Some(g) => {
                        Some(
                            g.parse::<IpAddr>()
                                .map_err(|_| TransportError::OutOfRange {
                                    column: "transport_group",
                                    value: g.to_string(),
                                    expected: "a multicast address",
                                })?,
                        )
                    }
                },
            }),
            other => Err(TransportError::UnknownKind {
                kind: other.to_string(),
            }),
        }
    }

    /// The `transport_kind` value this is.
    pub const fn kind(&self) -> &'static str {
        match self {
            Transport::Serial { .. } => "serial",
            Transport::TcpOut { .. } => "tcp_out",
            Transport::HttpOut { .. } => "http_out",
            Transport::GrpcOut { .. } => "grpc_out",
            Transport::MqttSub { .. } => "mqtt_sub",
            Transport::HttpIn { .. } => "http_in",
            Transport::UdpIn { .. } => "udp_in",
        }
    }

    /// Who opens the connection.
    pub const fn initiation(&self) -> Initiation {
        match self {
            Transport::HttpIn { .. } | Transport::UdpIn { .. } => Initiation::Inbound,
            _ => Initiation::Outbound,
        }
    }

    /// Whether this is a long-lived stream or one request per occurrence.
    ///
    /// The table in this module's documentation, as code. `GrpcOut` is the one variant
    /// whose answer is in its own payload, which is why `transport_streaming` is a column
    /// rather than a convention.
    pub const fn continuity(&self) -> Continuity {
        match self {
            Transport::HttpOut { .. } => Continuity::Request,
            Transport::GrpcOut { streaming, .. } => {
                if *streaming {
                    Continuity::Stream
                } else {
                    Continuity::Request
                }
            }
            _ => Continuity::Stream,
        }
    }

    /// The identity shape this transport and identity strategy declare together.
    ///
    /// Handed to `plan()` on the spec, where `IdentityShape::problem` rejects the three
    /// impossible combinations — a cardinality that disagrees with the identity source, an
    /// inbound endpoint with a `Fixed` identity (the Ambient defect), and a trigger that
    /// disagrees with the continuity.
    pub fn shape(&self, identity: &StationIdentity) -> IdentityShape {
        let source = identity.source();
        IdentityShape {
            initiation: self.initiation(),
            source,
            // W2a Decision 42: `One` if and only if `Fixed`. Derived from the source rather
            // than configured, so the invariant holds by construction here and
            // `IdentityShape::problem` is left to catch a shape some *other* registrar
            // built wrong.
            cardinality: if source == IdentitySource::Fixed {
                Cardinality::One
            } else {
                Cardinality::Many
            },
            continuity: self.continuity(),
        }
    }
}

fn text<'a>(
    value: Option<&'a str>,
    kind: &str,
    column: &'static str,
) -> Result<&'a str, TransportError> {
    value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| TransportError::Missing {
            kind: kind.to_string(),
            column,
        })
}

fn number(value: Option<i64>, kind: &str, column: &'static str) -> Result<i64, TransportError> {
    value.ok_or_else(|| TransportError::Missing {
        kind: kind.to_string(),
        column,
    })
}

fn port(value: Option<i64>, kind: &str, column: &'static str) -> Result<u16, TransportError> {
    let n = number(value, kind, column)?;
    u16::try_from(n)
        .ok()
        .filter(|p| *p != 0)
        .ok_or(TransportError::OutOfRange {
            column,
            value: n.to_string(),
            expected: "a port between 1 and 65535",
        })
}

fn address(
    value: Option<&str>,
    kind: &str,
    column: &'static str,
) -> Result<Endpoint, TransportError> {
    Endpoint::parse(text(value, kind, column)?)
        .map_err(|source| TransportError::Address { column, source })
}

fn tls(c: &TransportColumns) -> Option<TlsFiles> {
    let files = TlsFiles {
        ca: c.tls_ca.as_deref().map(PathBuf::from),
        cert: c.tls_cert.as_deref().map(PathBuf::from),
        key: c.tls_key.as_deref().map(PathBuf::from),
    };
    // No paths at all is plain TCP, which is what every Davis bridge on a LAN is.
    if files.ca.is_none() && files.cert.is_none() && files.key.is_none() {
        None
    } else {
        Some(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::Trigger;
    use std::time::Duration;

    fn columns(kind: &str) -> TransportColumns {
        TransportColumns {
            kind: kind.to_string(),
            ..TransportColumns::default()
        }
    }

    #[test]
    fn a_weatherlink_live_broadcast_row_becomes_a_udp_receive_socket() {
        let t = Transport::from_columns(&TransportColumns {
            port: Some(22222),
            ..columns("udp_in")
        })
        .expect("a valid row");
        assert!(matches!(
            t,
            Transport::UdpIn {
                port: 22222,
                group: None
            }
        ));
        assert_eq!(t.initiation(), Initiation::Inbound);
        assert_eq!(t.continuity(), Continuity::Stream);
        assert_eq!(t.kind(), "udp_in");
    }

    #[test]
    fn a_weatherlink_live_poll_row_becomes_one_request_per_occurrence() {
        let t = Transport::from_columns(&TransportColumns {
            url: Some("http://192.168.1.50".into()),
            ..columns("http_out")
        })
        .expect("a valid row");
        assert_eq!(t.continuity(), Continuity::Request, "W2a Decision 40");
        assert_eq!(t.initiation(), Initiation::Outbound);
        let Transport::HttpOut { url, method, auth } = &t else {
            panic!("not an http_out: {t:?}");
        };
        assert_eq!(url.authority(), "192.168.1.50");
        assert_eq!(*method, HttpMethod::Get);
        assert!(auth.is_none());
    }

    /// The implication the whole enum exists to make checkable.
    #[test]
    fn each_variant_implies_its_trigger() {
        let request = Transport::from_columns(&TransportColumns {
            url: Some("http://h".into()),
            ..columns("http_out")
        })
        .expect("valid");
        let stream = Transport::from_columns(&TransportColumns {
            port: Some(22222),
            ..columns("udp_in")
        })
        .expect("valid");
        let every = Trigger::every(Duration::from_secs(60), Duration::from_secs(10));

        // A request transport with a `Service` trigger, and a stream with `Every`: both
        // rejected by `plan()` through the shape.
        assert!(request
            .shape(&StationIdentity::Fixed)
            .problem(&Trigger::Service)
            .is_some());
        assert!(stream
            .shape(&StationIdentity::FromPayload)
            .problem(&every)
            .is_some());
        // ... and the two correct pairings are accepted.
        assert_eq!(request.shape(&StationIdentity::Fixed).problem(&every), None);
        assert_eq!(
            stream
                .shape(&StationIdentity::FromPayload)
                .problem(&Trigger::Service),
            None
        );
    }

    /// The Ambient defect, made unrepresentable rather than fixed.
    #[test]
    fn an_inbound_transport_with_a_fixed_identity_is_rejected() {
        let inbound = Transport::from_columns(&TransportColumns {
            mount: Some("/ingest/ambient".into()),
            ..columns("http_in")
        })
        .expect("valid");
        let problem = inbound
            .shape(&StationIdentity::Fixed)
            .problem(&Trigger::Service)
            .expect("rejected");
        assert_eq!(
            problem.to_string(),
            "an endpoint the station initiates cannot have a Fixed identity -- the station \
             would be whichever listener received the request"
        );
    }

    /// MQTT: outbound and 1:N, which is the case that corrected the invariant.
    #[test]
    fn an_outbound_subscription_may_serve_many_stations() {
        let mqtt = Transport::from_columns(&TransportColumns {
            url: Some("http://broker.example:1883".into()),
            topic: Some("weather/+/obs".into()),
            ..columns("mqtt_sub")
        })
        .expect("valid");
        let shape = mqtt.shape(&StationIdentity::FromPath);
        assert_eq!(shape.initiation, Initiation::Outbound);
        assert_eq!(shape.cardinality, Cardinality::Many);
        assert_eq!(shape.problem(&Trigger::Service), None);
    }

    #[test]
    fn every_kind_names_the_column_it_is_missing() {
        for (kind, column) in [
            ("serial", "transport_dev"),
            ("tcp_out", "transport_host"),
            ("http_out", "transport_url"),
            ("grpc_out", "transport_url"),
            ("mqtt_sub", "transport_url"),
            ("http_in", "transport_mount"),
            ("udp_in", "transport_port"),
        ] {
            let e = Transport::from_columns(&columns(kind)).expect_err("an empty row");
            assert!(
                e.to_string().contains(column),
                "a {kind} row should name {column}, said: {e}"
            );
        }
        assert!(matches!(
            Transport::from_columns(&columns("carrier_pigeon")),
            Err(TransportError::UnknownKind { .. })
        ));
    }

    #[test]
    fn a_streaming_flag_is_required_where_it_decides_the_trigger() {
        let e = Transport::from_columns(&TransportColumns {
            url: Some("http://peer:50051".into()),
            ..columns("grpc_out")
        })
        .expect_err("no streaming flag");
        assert!(e.to_string().contains("transport_streaming"), "{e}");
        let streamed = Transport::from_columns(&TransportColumns {
            url: Some("http://peer:50051".into()),
            streaming: Some(true),
            ..columns("grpc_out")
        })
        .expect("valid");
        assert_eq!(streamed.continuity(), Continuity::Stream);
        let unary = Transport::from_columns(&TransportColumns {
            url: Some("http://peer:50051".into()),
            streaming: Some(false),
            ..columns("grpc_out")
        })
        .expect("valid");
        assert_eq!(unary.continuity(), Continuity::Request);
    }

    #[test]
    fn a_port_outside_its_range_is_refused_with_what_was_held() {
        let e = Transport::from_columns(&TransportColumns {
            port: Some(70_000),
            ..columns("udp_in")
        })
        .expect_err("out of range");
        assert!(e.to_string().contains("70000"), "{e}");
    }

    #[test]
    fn a_url_with_a_path_is_refused_by_the_column_that_holds_it() {
        let e = Transport::from_columns(&TransportColumns {
            url: Some("http://h/v1/current_conditions".into()),
            ..columns("http_out")
        })
        .expect_err("a path");
        assert!(e.to_string().starts_with("`transport_url`:"), "{e}");
    }

    #[test]
    fn tls_paths_are_absent_rather_than_empty_for_a_plain_connection() {
        let plain = Transport::from_columns(&TransportColumns {
            host: Some("davis.lan".into()),
            port: Some(22222),
            ..columns("tcp_out")
        })
        .expect("valid");
        assert!(matches!(plain, Transport::TcpOut { tls: None, .. }));
        let secured = Transport::from_columns(&TransportColumns {
            host: Some("davis.lan".into()),
            port: Some(22222),
            tls_ca: Some("/etc/ssl/ca.pem".into()),
            ..columns("tcp_out")
        })
        .expect("valid");
        let Transport::TcpOut { tls: Some(f), .. } = &secured else {
            panic!("not secured");
        };
        assert_eq!(
            f.ca.as_deref(),
            Some(std::path::Path::new("/etc/ssl/ca.pem"))
        );
    }

    #[test]
    fn a_multicast_group_is_parsed_and_a_non_address_is_refused() {
        let t = Transport::from_columns(&TransportColumns {
            port: Some(5353),
            group: Some("239.1.2.3".into()),
            ..columns("udp_in")
        })
        .expect("valid");
        assert!(matches!(t, Transport::UdpIn { group: Some(_), .. }));
        assert!(matches!(
            Transport::from_columns(&TransportColumns {
                port: Some(5353),
                group: Some("not-an-address".into()),
                ..columns("udp_in")
            }),
            Err(TransportError::OutOfRange {
                column: "transport_group",
                ..
            })
        ));
    }

    #[test]
    fn the_method_column_takes_the_two_methods_a_driver_uses() {
        for (text, want) in [("get", HttpMethod::Get), ("POST", HttpMethod::Post)] {
            let t = Transport::from_columns(&TransportColumns {
                url: Some("http://h".into()),
                method: Some(text.into()),
                ..columns("http_out")
            })
            .expect("valid");
            assert!(matches!(t, Transport::HttpOut { method, .. } if method == want));
        }
        assert!(matches!(
            Transport::from_columns(&TransportColumns {
                url: Some("http://h".into()),
                method: Some("CONNECT".into()),
                ..columns("http_out")
            }),
            Err(TransportError::OutOfRange {
                column: "transport_method",
                ..
            })
        ));
        assert_eq!(HttpMethod::Get.to_string(), "GET");
    }
}
