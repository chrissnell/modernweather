//! [`QueryParams`] — a URL query string, decoded.
//!
//! One Ambient station's "customized" upload is `GET /ingest/ambient?ID=...&PASSWORD=...`
//! with up to about a hundred and thirty measurements in the query string, so the query
//! *is* the payload and decoding it is a codec's job.
//!
//! **Why not a crate.** `url` and `form_urlencoded` are not in **D-32**'s table, so either
//! would be an amendment, and what they would buy over the sixty lines below is IDNA,
//! relative-reference resolution and a `Serialize` impl. `mw_station::endpoint` made the
//! same call for the same reason and said so.
//!
//! **Why it is here and not in the driver.** The driver needs `?ID=` and `?PASSWORD=` to
//! build a `FrameMeta`, and the codec needs the other hundred and twenty-eight keys. A
//! parser on each side is two implementations of `+`-means-space that can disagree about
//! one station's password, so there is one parser and the driver reaches it through
//! [`crate::ambient`].
//!
//! # What it does with a malformed escape, and why that is Go's answer
//!
//! `net/url`'s `ParseQuery` decodes each pair independently: a pair whose key or value
//! contains an invalid escape is **skipped**, the error is recorded, and parsing continues
//! — and `r.URL.Query()` discards that error, which is what the Go Ambient handler calls.
//! So one mangled parameter costs that parameter and not the upload. This does the same,
//! because the alternative — refusing the whole frame — would lose a hundred and twenty-nine
//! good measurements over one bad one, and because a station that uploads one unescaped `%`
//! in a text field would otherwise stop reporting entirely.

/// A decoded query string.
///
/// Pairs in the order they appeared, which is what makes [`QueryParams::get`] "the first
/// value" — `url.Values.Get` returns `v[key][0]`, and an Ambient station that repeated a
/// parameter would otherwise resolve differently here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryParams {
    pairs: Vec<(String, String)>,
    skipped: usize,
}

impl QueryParams {
    /// Decode `query`, which is everything after the `?` and does not include it.
    ///
    /// A leading `?` is tolerated, because the one thing a caller is likely to hand this is
    /// a `Uri::query()` on one side and a recorded transcript on the other.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let query = query.strip_prefix('?').unwrap_or(query);
        let mut pairs = Vec::new();
        let mut skipped = 0;
        for field in query.split('&') {
            if field.is_empty() {
                continue;
            }
            // Go 1.17 stopped accepting `;` as a separator and made a pair containing one
            // a skipped pair with an error recorded (`net/url.parseQuery`), which
            // `r.URL.Query()` drops on the floor -- so `ID=roof;x=1` yields no `ID` at all
            // over there and the handler answers 400 for a missing identifier. Skipped
            // here for the same reason it is skipped there: a reader who sees `;` cannot
            // tell whether the sender meant a separator or a literal, and guessing would
            // make `ID` resolve to a different station on the two implementations.
            if field.contains(';') {
                skipped += 1;
                continue;
            }
            let (key, value) = match field.split_once('=') {
                Some((k, v)) => (k, v),
                // `&flag&` is a key with an empty value in `net/url`, not a skipped pair.
                None => (field, ""),
            };
            match (decode(key), decode(value)) {
                (Some(key), Some(value)) => pairs.push((key, value)),
                _ => skipped += 1,
            }
        }
        Self { pairs, skipped }
    }

    /// The first value for `key`, or `None`.
    ///
    /// Case-sensitive, because the protocol is: Ambient sends `ID`, `PASSWORD` and `UV` in
    /// upper case and `tempf` in lower, and a case-insensitive lookup would make `uv` and
    /// `UV` the same parameter when the vendor has shipped both spellings for different
    /// things.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The first value for `key`, treating an empty one as absent.
    ///
    /// `?tempf=&humidity=41` is what an Ambient console sends for a sensor it has lost, and
    /// `""` parses as no number — so this is the lookup every measurement uses and
    /// [`QueryParams::get`] is for the parameters where empty and absent differ.
    #[must_use]
    pub fn non_empty(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.is_empty())
    }

    /// How many parameters there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// How many pairs were dropped for carrying an invalid escape.
    ///
    /// Exposed so that a driver can say so once rather than silently: `net/url` records the
    /// same number in an error its caller throws away.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Every key, in order, for a test and for a log line that says what arrived.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.pairs.iter().map(|(k, _)| k.as_str())
    }
}

/// Percent-decode one component, with `+` as a space. `None` for an invalid escape.
fn decode(text: &str) -> Option<String> {
    if !text.contains('%') && !text.contains('+') {
        // The overwhelmingly common case: a measurement is digits and a dot.
        return Some(text.to_string());
    }
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' => {
                let hex = bytes.get(at + 1..at + 3)?;
                let high = (hex[0] as char).to_digit(16)?;
                let low = (hex[1] as char).to_digit(16)?;
                // Both digits are 0-15, so the product is a byte by construction.
                out.push((high * 16 + low) as u8);
                at += 3;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    // A decoded component that is not UTF-8 is refused rather than replaced: `net/url`
    // produces a Go string, which may hold arbitrary bytes, and a Rust `String` may not.
    // The alternative is `from_utf8_lossy`, which would turn one bad byte in a station name
    // into a U+FFFD nobody can match against a configured key.
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_decodes_to_its_pairs_in_order() {
        let q = QueryParams::parse("ID=roof&PASSWORD=hunter2&tempf=68.4&action=updateraw");
        assert_eq!(q.len(), 4);
        assert_eq!(q.get("ID"), Some("roof"));
        assert_eq!(q.get("PASSWORD"), Some("hunter2"));
        assert_eq!(q.get("tempf"), Some("68.4"));
        assert_eq!(q.get("action"), Some("updateraw"));
        assert_eq!(q.get("humidity"), None);
        assert_eq!(
            q.keys().collect::<Vec<_>>(),
            vec!["ID", "PASSWORD", "tempf", "action"]
        );
        assert!(!q.is_empty());
        assert_eq!(q.skipped(), 0);
    }

    #[test]
    fn a_leading_question_mark_and_empty_fields_are_tolerated() {
        let q = QueryParams::parse("?&ID=roof&&tempf=1&");
        assert_eq!(q.len(), 2);
        assert_eq!(q.get("ID"), Some("roof"));
        assert!(QueryParams::parse("").is_empty());
        assert!(QueryParams::parse("?").is_empty());
    }

    #[test]
    fn percent_escapes_and_plus_both_decode() {
        let q = QueryParams::parse("dateutc=2026-11-10+22%3A49%3A09&tz=America%2FDenver");
        assert_eq!(q.get("dateutc"), Some("2026-11-10 22:49:09"));
        assert_eq!(q.get("tz"), Some("America/Denver"));
        // Upper and lower case hex, and a literal percent.
        let q = QueryParams::parse("a=%2f&b=%2F&c=100%25");
        assert_eq!(q.get("a"), Some("/"));
        assert_eq!(q.get("b"), Some("/"));
        assert_eq!(q.get("c"), Some("100%"));
    }

    #[test]
    fn a_key_can_be_escaped_too() {
        let q = QueryParams::parse("temp%31f=68.4");
        assert_eq!(q.get("temp1f"), Some("68.4"));
    }

    /// `net/url`'s behaviour: the bad pair goes and the good ones stay.
    #[test]
    fn a_pair_carrying_a_semicolon_is_skipped_the_way_go_skips_it() {
        // The dangerous case is the first one: an identifier that resolved here and not
        // over there would be a station admitted on one implementation and refused on the
        // other, off the same request.
        let q = QueryParams::parse("ID=roof;x=1&PASSWORD=p&tempf=70.0");
        assert_eq!(q.get("ID"), None);
        assert_eq!(q.get("PASSWORD"), Some("p"));
        assert_eq!(q.get("tempf"), Some("70.0"));
        assert_eq!(q.skipped(), 1);
        // A semicolon on the value's side is the same pair and the same skip.
        let q = QueryParams::parse("a=1;b=2");
        assert_eq!(q.get("a"), None);
        assert_eq!(q.skipped(), 1);
        // Percent-encoded, it is a literal and survives -- which is how a console sends
        // one on purpose.
        let q = QueryParams::parse("note=a%3Bb");
        assert_eq!(q.get("note"), Some("a;b"));
        assert_eq!(q.skipped(), 0);
    }

    #[test]
    fn a_pair_with_an_invalid_escape_is_skipped_and_the_rest_survive() {
        for bad in ["x=%zz", "x=%2", "x=%", "%zz=1"] {
            let q = QueryParams::parse(&format!("tempf=68.4&{bad}&humidity=41"));
            assert_eq!(q.len(), 2, "`{bad}` should cost one pair");
            assert_eq!(q.skipped(), 1, "`{bad}`");
            assert_eq!(q.get("tempf"), Some("68.4"));
            assert_eq!(q.get("humidity"), Some("41"));
        }
        // A decoded value that is not UTF-8 is the same answer.
        let q = QueryParams::parse("name=%FF%FE&tempf=1");
        assert_eq!(q.get("tempf"), Some("1"));
        assert_eq!(q.get("name"), None);
        assert_eq!(q.skipped(), 1);
    }

    #[test]
    fn a_flag_with_no_equals_is_a_key_with_an_empty_value() {
        let q = QueryParams::parse("updateraw&tempf=1");
        assert_eq!(q.get("updateraw"), Some(""));
        assert_eq!(q.non_empty("updateraw"), None);
        assert_eq!(q.skipped(), 0);
    }

    /// The lookup every measurement uses: a console that has lost a sensor sends the key
    /// with nothing after it.
    #[test]
    fn an_empty_value_is_absent_to_non_empty_and_present_to_get() {
        let q = QueryParams::parse("tempf=&humidity=41");
        assert_eq!(q.get("tempf"), Some(""));
        assert_eq!(q.non_empty("tempf"), None);
        assert_eq!(q.non_empty("humidity"), Some("41"));
    }

    #[test]
    fn a_repeated_parameter_resolves_to_its_first_value() {
        let q = QueryParams::parse("tempf=68.4&tempf=99.9");
        assert_eq!(q.get("tempf"), Some("68.4"));
        assert_eq!(q.len(), 2, "both are kept; only the lookup prefers one");
    }

    #[test]
    fn the_lookup_is_case_sensitive_because_the_protocol_is() {
        let q = QueryParams::parse("UV=3&uv=9");
        assert_eq!(q.get("UV"), Some("3"));
        assert_eq!(q.get("uv"), Some("9"));
    }
}
