//! The query string, parsed the way Go's `url.Values.Get` reads it.
//!
//! Go's `URL.Query()` builds a `map[string][]string` and `Get` returns the **first** value
//! for a key. `serde_urlencoded`, which `axum::extract::Query` uses, keeps the **last** --
//! and, more importantly, it *fails the whole request* on input it cannot deserialise,
//! which on a frozen route would turn a malformed query into a response the contract has no
//! bytes for. So this is thirty lines of `std` rather than an extractor.
//!
//! Percent-decoding and `+`-as-space are both Go's `url.ParseQuery` behaviour. So is what
//! it does with input it cannot decode, and that is **not** "keep it literally":
//! `parseQuery` discards the whole key/value pair and carries on with the rest, collecting
//! an error that `URL.Query()` then throws away. Two consequences, both verified against
//! `net/url` at Go 1.21:
//!
//! - an invalid escape anywhere in a pair drops **that pair**, so `?station=%zz` is a
//!   request with no `station` at all -- which on `/latest` is the website's primary
//!   device and a `200`, not a `404` for a station named `%zz`;
//! - a `;` anywhere in a pair drops that pair too, which Go 1.17 made deliberate
//!   (`golang/go#25192`: a semicolon is no longer a separator and is refused rather than
//!   silently read as data).
//!
//! The rest of the query survives in both cases: `?x=1&station=%zz` keeps `x`.
//!
//! Reading `%zz` as a station name instead turned four `200`s into `404`s and a `400`, on
//! three frozen endpoints. The old note had the mechanism exactly backwards.

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
            // A semicolon makes the pair unparseable rather than being a separator
            // (Go 1.17, golang/go#25192). The pair is dropped; the rest of the query is not.
            if pair.contains(';') {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                // `?format` with no `=` is a present key with an empty value, which is
                // what Go does and what makes `format=msgpack`'s absence testable.
                None => (pair, ""),
            };
            // An escape that will not decode drops the pair, in either half of it. This is
            // `parseQuery`'s `continue`, which is the one thing the module documentation
            // used to have backwards.
            let (Some(key), Some(value)) = (decode(key), decode(value)) else {
                continue;
            };
            out.push((key, value));
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

/// Percent-decoding with `+` as a space, or `None` for an escape that will not decode.
///
/// `None` is `url.QueryUnescape` returning an error, which makes `parseQuery` drop the
/// pair. `%` as the last one or two bytes of the string is the same failure as `%zz`, and
/// the old bounds check (`i + 2 < bytes.len()`) read a trailing `%` as a literal instead.
fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let (hi, lo) = (bytes.get(i + 1)?, bytes.get(i + 2)?);
                let (hi, lo) = (hex(*hi)?, hex(*lo)?);
                out.push(hi << 4 | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Lossy, because `QueryUnescape` returns a Go `string`, which is a byte sequence with
    // no UTF-8 requirement -- so an undecodable byte is a station name no station has,
    // which is the `404` the contract already covers, rather than a dropped pair.
    Some(String::from_utf8_lossy(&out).into_owned())
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

    /// **An undecodable pair is dropped, not kept as a literal.**
    ///
    /// This test asserted the opposite, from a misreading of `URL.Query()` discarding
    /// `ParseQuery`'s error: the error is discarded, and the pair that caused it was already
    /// `continue`d past. Every expectation below was run through `net/url` at Go 1.21.
    ///
    /// It matters because `?station=%zz` then resolves to *no* station, which on `/latest`
    /// is the website's primary device and a `200` -- where a station named `%zz` was a
    /// `404`. Four frozen-endpoint statuses were wrong.
    #[test]
    fn an_undecodable_pair_is_dropped_the_way_go_drops_it() {
        for raw in [
            "station=%zz",
            "station=%",
            "station=%4",
            // The key as well as the value.
            "stat%zzion=x",
            // A semicolon is refused rather than read as a separator or as data
            // (Go 1.17, golang/go#25192).
            "station=a;b",
        ] {
            assert_eq!(
                Params::parse(Some(raw)).get("station"),
                None,
                "`{raw}` must drop the pair"
            );
        }
        // And only that pair: the rest of the query survives.
        let p = Params::parse(Some("x=1&station=%zz&y=2"));
        assert_eq!(p.get("x"), Some("1"));
        assert_eq!(p.get("y"), Some("2"));
        assert_eq!(p.get("station"), None);
        let p = Params::parse(Some("period=1;2&station=davis-full"));
        assert_eq!(p.get("period"), None);
        assert_eq!(p.get("station"), Some("davis-full"));
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
