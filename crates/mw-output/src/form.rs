//! [`Form`] — a `application/x-www-form-urlencoded` query, built key by key.
//!
//! Both HTTP uploaders send their measurements as a query string: Weather Underground and
//! PWS Weather are `GET` endpoints that have accepted the same parameter vocabulary since
//! they were WeatherUnderground's "rapid fire" protocol. `net/url`'s `url.Values` is what
//! the Go side uses; there is no crate in **D-32**'s table for it and what is needed is
//! thirty lines a golden test can pin, which is the same trade `mw_station::http` and
//! `mw_station::mdns` make for their own byte strings.
//!
//! # Why `Debug` redacts
//!
//! The query carries `PASSWORD=` for both services. A `String` would be printable by any
//! `{:?}` anywhere — a `tracing` field, a `#[derive(Debug)]` on a struct that happens to
//! hold one, a test failure message — and **D-33** makes a credential's debug
//! representation redact. So the payload the encoders produce is this type, and the only
//! way to the bytes is [`Form::as_str`], which reads as what it is at the one call site
//! that writes it to a socket.
//!
//! # Why the parameters keep their insertion order
//!
//! `url.Values.Encode()` sorts by key, which makes a Go-produced query diffable and makes
//! the *declaration* order invisible. Here the order is the order the encoder wrote them,
//! because that is the order its declaration table reads in — so a golden test over the
//! query is a test over the table, and a reviewer comparing the encoder against the
//! service's documentation sees the same sequence in both.

use std::fmt;

/// A form-encoded query string.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Form {
    query: String,
    /// How many parameters carry a measurement rather than an identifier — what
    /// `crate::encoder` counts to decide whether there is anything worth sending.
    measurements: usize,
}

impl Form {
    /// An empty query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `key=value`, percent-encoding both.
    ///
    /// `&mut self` and a plain `set` rather than a builder chain, because an encoder's body
    /// is a sequence of conditional appends and `if let Some(v) = … { f.set(…) }` is what
    /// that looks like.
    pub fn set(&mut self, key: &str, value: &str) {
        if !self.query.is_empty() {
            self.query.push('&');
        }
        encode_into(&mut self.query, key);
        self.query.push('=');
        encode_into(&mut self.query, value);
    }

    /// Append a parameter that carries a **measurement**, which is additionally counted.
    ///
    /// The count is what separates "this station reported nothing" from "this station
    /// reported nothing and we told the service about it anyway" — see
    /// `crate::error::EncodeError::NothingToSend`.
    pub fn measurement(&mut self, key: &str, value: &str) {
        self.set(key, value);
        self.measurements += 1;
    }

    /// How many measurements it carries.
    pub fn measurements(&self) -> usize {
        self.measurements
    }

    /// The query, for the one line that writes it.
    pub fn as_str(&self) -> &str {
        &self.query
    }

    /// How many bytes the query is.
    pub fn len(&self) -> usize {
        self.query.len()
    }

    /// Whether nothing has been appended.
    pub fn is_empty(&self) -> bool {
        self.query.is_empty()
    }
}

impl fmt::Debug for Form {
    /// Redacts (**D-33**): the query holds `PASSWORD=`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Form(<redacted; {} bytes, {} measurements>)",
            self.query.len(),
            self.measurements
        )
    }
}

/// Percent-encode into an existing buffer.
///
/// The unreserved set of RFC 3986 plus nothing: `-`, `.`, `_`, `~` and alphanumerics pass
/// through and everything else becomes `%XX`. A **space becomes `%20`** rather than `+`,
/// which both forms are legal for in a query and only one of which is unambiguous — the
/// `+` convention is `application/x-www-form-urlencoded`'s, and a service that reads the
/// query with a generic URL parser sees a literal plus.
fn encode_into(out: &mut String, text: &str) {
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                out.push('%');
                out.push(char::from(HEX[usize::from(other >> 4)]));
                out.push(char::from(HEX[usize::from(other & 0x0F)]));
            }
        }
    }
}

/// Uppercase hex digits, which is what RFC 3986 prefers for a percent-encoding.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_keep_their_insertion_order() {
        let mut f = Form::new();
        f.set("ID", "KWABACKYARD");
        f.measurement("tempf", "72.3");
        f.measurement("humidity", "55");
        assert_eq!(f.as_str(), "ID=KWABACKYARD&tempf=72.3&humidity=55");
        assert_eq!(f.measurements(), 2);
        assert!(!f.is_empty());
        assert_eq!(f.len(), f.as_str().len());
    }

    /// Everything outside the unreserved set is escaped, which is what stops a password
    /// containing `&` from appending a parameter of its own.
    #[test]
    fn a_credential_cannot_inject_a_parameter() {
        let mut f = Form::new();
        f.set("PASSWORD", "p&ss=w rd#1+2");
        assert_eq!(f.as_str(), "PASSWORD=p%26ss%3Dw%20rd%231%2B2");
        // One parameter, whatever the value contained.
        assert_eq!(f.as_str().matches('&').count(), 0);

        let mut dates = Form::new();
        dates.set("dateutc", "2026-11-11 05:00:00");
        assert_eq!(dates.as_str(), "dateutc=2026-11-11%2005%3A00%3A00");

        // Non-ASCII is escaped per UTF-8 byte.
        let mut utf8 = Form::new();
        utf8.set("softwaretype", "mw-é");
        assert_eq!(utf8.as_str(), "softwaretype=mw-%C3%A9");
    }

    /// **D-33.** The query is not printable through `Debug`, which is the only way it could
    /// reach a `tracing` field or a test failure message by accident.
    #[test]
    fn the_query_does_not_reach_a_log_line_through_debug() {
        let mut f = Form::new();
        f.set("PASSWORD", "hunter2");
        f.measurement("tempf", "72.3");
        let rendered = format!("{f:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("PASSWORD=h"), "{rendered}");
        assert_eq!(rendered, "Form(<redacted; 27 bytes, 1 measurements>)");
    }

    #[test]
    fn an_empty_form_is_an_empty_query() {
        let f = Form::new();
        assert!(f.is_empty());
        assert_eq!(f.as_str(), "");
        assert_eq!(f.measurements(), 0);
    }
}
