//! [`Endpoint`] — a parsed `scheme://host[:port]` with no path.
//!
//! **Why not the `url` crate.** `url` is not in **D-32**'s table, so adding it would be an
//! amendment rather than a manifest edit, and what it would buy is percent-decoding,
//! userinfo, query parsing and IDNA for a value that is a LAN appliance's address. What is
//! needed is a host, a port and whether it is TLS; the parse below is that and refuses
//! everything else by name, which is a better error than a silently-accepted path.
//!
//! The one thing it is strict about is the **path**, and that is deliberate. W2a §4a.3
//! writes `HttpOut { url }` with the full request path in it, and a WeatherLink Live driver
//! needs two paths off one device — `/v1/current_conditions` and `/v1/real_time` — so the
//! paths are the driver's knowledge and the configuration holds the device. A
//! `transport_url` with a path in it is therefore a configuration that says two things
//! about one request, and it is rejected at plan time with the sentence that explains it
//! rather than quietly ignored.

use std::fmt;

/// A scheme, a host and a port. No path, no query, no userinfo.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    tls: bool,
    host: String,
    port: u16,
}

/// Why a string is not an [`Endpoint`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// No `http://` or `https://`.
    #[error("`{text}` has no scheme; a device address is `http://host` or `https://host`")]
    NoScheme {
        /// What was configured.
        text: String,
    },
    /// A scheme this is not a transport for.
    #[error("`{scheme}` is not a scheme this transport speaks; it speaks `http` and `https`")]
    BadScheme {
        /// What was configured.
        scheme: String,
    },
    /// Nothing between the scheme and the port.
    #[error("`{text}` names no host")]
    NoHost {
        /// What was configured.
        text: String,
    },
    /// A port that is not one.
    #[error("`{text}`: `{port}` is not a port")]
    BadPort {
        /// What was configured.
        text: String,
        /// The text where the port goes.
        port: String,
    },
    /// A path, a query or a fragment, on a value that may carry none.
    #[error(
        "`{text}` carries `{extra}`: this is the device's address and the request path is \
         the driver's, so it must be `http://host` with nothing after the host"
    )]
    HasPath {
        /// What was configured.
        text: String,
        /// The part that may not be there.
        extra: String,
    },
    /// Userinfo, which is a credential in a URL.
    ///
    /// Carries **no** field, deliberately: the rejected text is `http://user:password@host`,
    /// and a variant that quoted it back would put the password into the plan error, the
    /// startup log and the management API's response -- which is the whole of what this
    /// rejection exists to prevent.
    #[error(
        "a device address may not carry a user name: a credential belongs in \
         `transport_auth`, where it is a secret, and not in a column the management API \
         returns"
    )]
    HasUserinfo,
}

impl Endpoint {
    /// The default port for each scheme, which is what an address with no `:port` means.
    const HTTP: u16 = 80;
    const HTTPS: u16 = 443;

    /// Parse `http://host[:port]`, rejecting anything after the host.
    pub fn parse(text: &str) -> Result<Self, EndpointError> {
        let text = text.trim();
        let (scheme, rest) = text
            .split_once("://")
            .ok_or_else(|| EndpointError::NoScheme {
                text: text.to_string(),
            })?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            _ => {
                return Err(EndpointError::BadScheme {
                    scheme: scheme.to_string(),
                })
            }
        };
        if let Some(cut) = rest.find(['/', '?', '#']) {
            // A bare trailing slash is an address, not a path: `http://host/` and
            // `http://host` are the same device, and refusing the first would reject what
            // every browser produces.
            let extra = &rest[cut..];
            if extra != "/" {
                return Err(EndpointError::HasPath {
                    text: text.to_string(),
                    extra: extra.to_string(),
                });
            }
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return Err(EndpointError::HasUserinfo);
        }

        // A bracketed IPv6 literal, or a host and an optional port.
        let (host, port) = if let Some(end) = authority.strip_prefix('[') {
            let (host, rest) = end.split_once(']').ok_or_else(|| EndpointError::NoHost {
                text: text.to_string(),
            })?;
            (host.to_string(), rest.strip_prefix(':').map(str::to_string))
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host.to_string(), Some(port.to_string())),
                None => (authority.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(EndpointError::NoHost {
                text: text.to_string(),
            });
        }
        let port = match port {
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|n| *n != 0)
                .ok_or(EndpointError::BadPort {
                    text: text.to_string(),
                    port: p.clone(),
                })?,
            None if tls => Self::HTTPS,
            None => Self::HTTP,
        };
        Ok(Self { tls, host, port })
    }

    /// A plain-HTTP endpoint for a host on the default port — what a WeatherLink Live's
    /// local API is, and what a `udp_in` station's `transport_host` resolves to.
    pub fn http(host: impl Into<String>) -> Self {
        Self {
            tls: false,
            host: host.into(),
            port: Self::HTTP,
        }
    }

    /// The host, as configured. Not resolved: see `crate::resolve`.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether this endpoint is TLS.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// The `Host` header and the request URI's authority — the host and, unless it is the
    /// scheme's default, the port.
    pub fn authority(&self) -> String {
        let bracketed = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default = if self.tls { Self::HTTPS } else { Self::HTTP };
        if self.port == default {
            bracketed
        } else {
            format!("{bracketed}:{}", self.port)
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}", self.authority())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_address_takes_its_schemes_default_port() {
        let e = Endpoint::parse("http://192.168.1.50").expect("parses");
        assert_eq!(
            (e.host(), e.port(), e.is_tls()),
            ("192.168.1.50", 80, false)
        );
        assert_eq!(e.to_string(), "http://192.168.1.50");
        let s = Endpoint::parse("https://wll.example").expect("parses");
        assert_eq!((s.port(), s.is_tls()), (443, true));
    }

    #[test]
    fn an_explicit_port_survives_into_the_authority() {
        let e = Endpoint::parse("http://wll.local:8080").expect("parses");
        assert_eq!(e.authority(), "wll.local:8080");
        assert_eq!(e.to_string(), "http://wll.local:8080");
    }

    #[test]
    fn an_ipv6_literal_stays_bracketed() {
        let e = Endpoint::parse("http://[fd00::1]:8080").expect("parses");
        assert_eq!(e.host(), "fd00::1");
        assert_eq!(e.authority(), "[fd00::1]:8080");
        let d = Endpoint::parse("http://[fd00::1]").expect("parses");
        assert_eq!(d.authority(), "[fd00::1]");
    }

    #[test]
    fn a_trailing_slash_is_an_address_and_a_path_is_not() {
        assert_eq!(
            Endpoint::parse("http://host/").expect("parses"),
            Endpoint::http("host")
        );
        let e = Endpoint::parse("http://host/v1/current_conditions").expect_err("rejected");
        assert!(matches!(e, EndpointError::HasPath { .. }), "{e}");
        assert!(
            e.to_string().contains("the request path is the driver's"),
            "{e}"
        );
        assert!(matches!(
            Endpoint::parse("http://host?x=1"),
            Err(EndpointError::HasPath { .. })
        ));
    }

    #[test]
    fn the_four_other_refusals_name_what_was_configured() {
        assert!(matches!(
            Endpoint::parse("192.168.1.50"),
            Err(EndpointError::NoScheme { .. })
        ));
        assert!(matches!(
            Endpoint::parse("ftp://host"),
            Err(EndpointError::BadScheme { .. })
        ));
        assert!(matches!(
            Endpoint::parse("http://"),
            Err(EndpointError::NoHost { .. })
        ));
        assert!(matches!(
            Endpoint::parse("http://host:0"),
            Err(EndpointError::BadPort { .. })
        ));
        assert!(matches!(
            Endpoint::parse("http://host:http"),
            Err(EndpointError::BadPort { .. })
        ));
    }

    /// A credential in a URL is a credential in a column the management API returns.
    #[test]
    fn userinfo_is_refused_rather_than_dropped() {
        let e = Endpoint::parse("http://admin:hunter2@host").expect_err("rejected");
        assert_eq!(e, EndpointError::HasUserinfo);
        assert!(!e.to_string().contains("hunter2"), "{e}");
    }

    #[test]
    fn the_scheme_is_case_insensitive_because_a_url_scheme_is() {
        assert!(!Endpoint::parse("HTTP://host").expect("parses").is_tls());
        assert!(Endpoint::parse("HttpS://host").expect("parses").is_tls());
    }
}
