//! The query string, parsed the way Go's `url.Values.Get` reads it.
//!
//! Go's `URL.Query()` builds a `map[string][]string` and `Get` returns the **first** value
//! for a key. `serde_urlencoded`, which `axum::extract::Query` uses, keeps the **last** --
//! and, more importantly, it *fails the whole request* on input it cannot deserialise,
//! which on a frozen route would turn a malformed query into a response the contract has no
//! bytes for. So this is thirty lines of `std` rather than an extractor.
//!
//! Percent-decoding and `+`-as-space are both Go's `url.ParseQuery` behaviour. An invalid
//! escape is **kept literally** rather than rejected: Go returns the values it could parse
//! *and* an error, and `URL.Query()` discards the error -- so `?station=%zz` is a station
//! named `%zz`, which is a `404` and not a `400`.

/// One request's query string, decoded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Params(Vec<(String, String)>);

impl Params {
    /// Parse a raw query string, with or without a leading `?`.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        let raw = raw.strip_prefix('?').unwrap_or(raw);
        let mut out = Vec::new();
        for pair in raw.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                // `?format` with no `=` is a present key with an empty value, which is
                // what Go does and what makes `format=msgpack`'s absence testable.
                None => (pair, ""),
            };
            out.push((decode(key), decode(value)));
        }
        Self(out)
    }

    /// The **first** value for a key, as Go's `Values.Get` returns.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The first value, treating an empty one as absent.
    ///
    /// Which is what every Go call site here does: `req.URL.Query().Get("station")` is
    /// compared against `""`, so `?station=` and no `station` at all are the same request.
    pub fn non_empty(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.is_empty())
    }
}

/// Percent-decoding with `+` as a space, keeping an invalid escape literally.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match hex(bytes[i + 1]).zip(hex(bytes[i + 2])) {
                Some((hi, lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Lossy, because a client can send any bytes and a frozen route has no response for
    // "your query string was not UTF-8". The replacement character makes it a name no
    // station has, which is the `404` the contract already covers.
    String::from_utf8_lossy(&out).into_owned()
}

/// One hex digit.
fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_query_string_is_an_empty_set() {
        let p = Params::parse(None);
        assert_eq!(p.get("station"), None);
        assert_eq!(Params::parse(Some("")).get("station"), None);
    }

    #[test]
    fn the_first_value_wins_the_way_gos_does() {
        let p = Params::parse(Some("station=a&station=b"));
        assert_eq!(p.get("station"), Some("a"));
    }

    #[test]
    fn an_empty_value_is_the_same_request_as_an_absent_key() {
        let p = Params::parse(Some("station=&station_id=3"));
        assert_eq!(p.get("station"), Some(""));
        assert_eq!(p.non_empty("station"), None);
        assert_eq!(p.non_empty("station_id"), Some("3"));
    }

    #[test]
    fn a_value_is_percent_decoded_and_plus_is_a_space() {
        let p = Params::parse(Some("station=davis%2Dfull&name=a+b&x=%E2%98%83"));
        assert_eq!(p.get("station"), Some("davis-full"));
        assert_eq!(p.get("name"), Some("a b"));
        assert_eq!(p.get("x"), Some("☃"));
    }

    /// An invalid escape is a literal, so `?station=%zz` is a `404` and not a `400`.
    /// `URL.Query()` discards `ParseQuery`'s error, which is the behaviour this keeps.
    #[test]
    fn an_invalid_escape_is_kept_literally() {
        assert_eq!(
            Params::parse(Some("station=%zz")).get("station"),
            Some("%zz")
        );
        assert_eq!(Params::parse(Some("station=%")).get("station"), Some("%"));
        assert_eq!(Params::parse(Some("station=%4")).get("station"), Some("%4"));
    }

    #[test]
    fn a_leading_question_mark_is_optional() {
        assert_eq!(
            Params::parse(Some("?period=3")).get("period"),
            Some("3"),
            "whether the caller kept the `?` is not the contract"
        );
    }

    #[test]
    fn a_bare_key_is_present_with_an_empty_value() {
        let p = Params::parse(Some("format&station=x"));
        assert_eq!(p.get("format"), Some(""));
        assert_eq!(p.non_empty("format"), None);
        assert_eq!(p.get("station"), Some("x"));
    }
}
