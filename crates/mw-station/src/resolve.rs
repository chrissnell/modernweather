//! Turning a configured host into an address, by the one of three routes it needs.
//!
//! ```text
//!  "192.168.1.50"   -> parsed. No resolver involved, and the case the owner's
//!                      WeatherLink Live is configured as today.
//!  "wll.local"      -> crate::mdns. `getaddrinfo` cannot answer this in a static musl
//!                      build (D-43, W8 OQ-4), so the multicast query is sent in-process.
//!  "wll.lan"        -> the system resolver, which is where an ordinary name belongs.
//! ```
//!
//! The split is the whole of `docs/reconciliation.md` §1 row 16 as code. It is in one
//! function so that there is one answer to "how does a station's host become an address",
//! and so that the `.local` case cannot be reached by accident from a path that thought it
//! was calling the system resolver.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use crate::mdns::{self, MdnsError, LOCAL_SUFFIX};

/// Why a configured host did not become an address.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// A `.local` name that mDNS could not answer.
    #[error(transparent)]
    Mdns(#[from] MdnsError),

    /// The system resolver refused or found nothing.
    #[error("`{host}` could not be resolved")]
    System {
        /// What was configured.
        host: String,
        /// The cause.
        #[source]
        source: std::io::Error,
    },

    /// The system resolver answered with no addresses, which it is permitted to do.
    #[error("`{host}` resolved to no addresses")]
    NoAddresses {
        /// What was configured.
        host: String,
    },
}

impl mw_types::Classify for ResolveError {
    /// Retryable, all three. A name that does not resolve now is the ordinary state of a
    /// device that is unplugged, and the next occurrence is the retry (W2a Decision 29).
    fn class(&self) -> mw_types::Class {
        mw_types::Class::RETRYABLE
    }
}

/// Resolve `host` to one socket address on `port`.
pub async fn socket_addr(
    host: &str,
    port: u16,
    mdns_timeout: Duration,
) -> Result<SocketAddr, ResolveError> {
    // A bracketed IPv6 literal arrives here unbracketed: `Endpoint` strips the brackets,
    // because they are a URL's syntax and not part of the address.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    if is_local(host) {
        let ip = mdns::resolve(host, mdns_timeout).await?;
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|source| ResolveError::System {
            host: host.to_string(),
            source,
        })?;
    addrs.next().ok_or_else(|| ResolveError::NoAddresses {
        host: host.to_string(),
    })
}

/// Whether this name is one only mDNS can answer.
///
/// Case-insensitively, because a host name is case-insensitive and an operator who typed
/// `WLL.Local` must not silently get the system resolver — which in a static musl build is
/// a resolver that will certainly fail, with a message about the name rather than about the
/// linkage.
pub fn is_local(host: &str) -> bool {
    let host = host.trim_end_matches('.').as_bytes();
    // Over bytes, not over `&str`. `transport_host` and `transport_url` are free-text
    // columns, so a pasted IDN name or one stray non-ASCII character puts `len() - 6`
    // inside a multi-byte character -- and slicing a `&str` there panics. The first poll
    // occurrence would then be a panicking component instead of the
    // `ResolveError::System` message this module exists to produce.
    host.len() > LOCAL_SUFFIX.len()
        && host[host.len() - LOCAL_SUFFIX.len()..].eq_ignore_ascii_case(LOCAL_SUFFIX.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dot_local_name_goes_to_mdns() {
        assert!(is_local("wll.local"));
        assert!(is_local("WLL.LOCAL"));
        assert!(is_local("wll.local."), "a fully-qualified name");
        assert!(!is_local("wll.lan"));
        assert!(!is_local("local"), "the suffix is not a name");
        assert!(!is_local(".local"), "nor is a bare suffix");
        assert!(!is_local("192.168.1.50"));
        // A non-ASCII host used to panic here: the suffix boundary lands inside a
        // multi-byte character. `transport_host` is free text, so this is one paste away.
        assert!(!is_local("ab\u{20AC}cdef"));
        assert!(!is_local("\u{1F600}"));
        assert!(
            is_local("caf\u{e9}.local"),
            "a non-ASCII label is still .local"
        );
    }

    #[tokio::test]
    async fn an_address_needs_no_resolver_at_all() {
        let v4 = socket_addr("192.168.1.50", 80, Duration::ZERO)
            .await
            .expect("an address parses");
        assert_eq!(v4.to_string(), "192.168.1.50:80");
        let v6 = socket_addr("fd00::1", 22222, Duration::ZERO)
            .await
            .expect("an address parses");
        assert_eq!(v6.to_string(), "[fd00::1]:22222");
    }

    /// Not a network test: `.invalid` is reserved by RFC 2606 precisely so that it never
    /// resolves, so this asserts the error's shape without depending on a nameserver.
    #[tokio::test]
    async fn a_name_that_cannot_resolve_names_itself() {
        let e = socket_addr("station.invalid", 80, Duration::ZERO)
            .await
            .expect_err("reserved for non-resolution");
        assert!(e.to_string().contains("station.invalid"), "{e}");
        assert!(
            matches!(
                e,
                ResolveError::System { .. } | ResolveError::NoAddresses { .. }
            ),
            "the system resolver answered, not mDNS"
        );
    }

    /// With a zero timeout the multicast query cannot be answered, so this exercises the
    /// route rather than the LAN: what matters is that a `.local` name produced an mDNS
    /// error and not a system-resolver one.
    #[tokio::test]
    async fn a_dot_local_name_fails_as_mdns_rather_than_as_getaddrinfo() {
        let e = socket_addr("wll.local", 80, Duration::from_millis(1))
            .await
            .expect_err("nothing answers in a millisecond");
        assert!(matches!(e, ResolveError::Mdns(_)), "{e}");
    }
}
