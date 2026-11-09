//! `/forecast/{span}` -- the `jsonb` splice (**D-39**).
//!
//! # Nothing parses the stored document, and that is the decision
//!
//! The response is `{"lastUpdated": "<string>", "data": <the stored jsonb>}`, and `data` is
//! PostgreSQL's own rendering of the `jsonb` column written back **byte for byte**. That is
//! how three properties survive that no re-encoding would preserve:
//!
//! * **jsonb's key order**, which is normalised by key length and then bytewise -- not the
//!   order the writer used and not alphabetical either;
//! * **the numeric text as stored**, so `"snowIN": 0.0` stays `0.0` where every
//!   hand-encoded endpoint on this surface would write `0` (Q-13). The same contract
//!   therefore contains both, which is why the `/forecast` fixtures carry
//!   `"numeric_convention": "as_stored"` and the runner does not apply its Go-encoder check
//!   to them;
//! * **the missing trailing newline** (Q-12). `json.NewEncoder(w).Encode` appends one and
//!   `WriteRawJSON` does not, and this is the endpoint that does not.
//!
//! So the body is assembled as three byte strings: a prefix, the column's text, a suffix.
//! There is no `serde_json` on this path at all.
//!
//! # The one branch that does parse, and why it has to
//!
//! `?period=N` replaces `data` with the single element at that index -- **an object, not an
//! array**. Go does that by unmarshalling into `[]interface{}`, indexing, and
//! re-marshalling, which **loses** the two properties above: the re-encoded element has
//! Go's alphabetical key order and Go's number format. That is visible in the fixtures
//! (`forecast-single-period`'s element is alphabetical where `forecast-240`'s is
//! jsonb-ordered), so it is reproduced rather than improved: the element is parsed, and
//! re-encoded with this crate's Go-compatible number writer.
//!
//! # Four failure shapes, and three of them are not plain text
//!
//! | Request | Status | Body | `Content-Type` |
//! |---|---|---|---|
//! | a span failing `^\d{1,4}$` | `400` | **empty** | absent |
//! | a span with no record | `404` | **empty** | absent |
//! | a `station_id` with no record | `404` | **empty** | absent |
//! | a `station` name matching no device | `404` | `{"error":"Station not found"}` | `text/plain` |
//! | a `period` that is not a number, or out of range | `400` | `{"error":…}` | `text/plain` |
//!
//! The last two are Q-10: `w.WriteHeader(400)` runs before the formatter, so Go has already
//! committed the sniffed content type and a JSON body goes out labelled as text.

use axum::body::Body;
use axum::http::{Response, StatusCode};
use mw_component::ForecastQuery;

use crate::component::Handlers;
use crate::dto::GoNumbers;
use crate::params::Params;
use crate::reply::{self, Cache};

/// The `503` this endpoint gives when no website resolves: `/forecast`, whose message is its own and whose content type is sniffed
/// (`handlers.go:615`).
const NO_WEBSITE: reply::NoWebsite = reply::NO_WEBSITE_FORECAST;

/// Serve `/forecast/{span}`.
pub async fn handle(h: &Handlers, host: &str, span: &str, params: &Params) -> Response<Body> {
    if let Err(response) = h.website(host, NO_WEBSITE) {
        return *response;
    }

    // `^\d{1,4}$` and nothing else. A span that fails it is a `400` with **zero bytes**,
    // which is the shape `w.WriteHeader(400); return` produces (Q-11) -- not a message.
    let Some(span_hours) = parse_span(span) else {
        return reply::empty(StatusCode::BAD_REQUEST);
    };

    // The `period` is parsed before the record is read, which is the Go order and is
    // observable: `?period=abc` is a `400` even for a span that has no record.
    let period = match params.non_empty("period") {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Err(_) => {
                return reply::json_as_text(
                    StatusCode::BAD_REQUEST,
                    "{\"error\":\"Invalid period parameter - must be a number\"}",
                )
            }
            Ok(n) if n < 0 => {
                return reply::json_as_text(
                    StatusCode::BAD_REQUEST,
                    "{\"error\":\"period must be a non-negative number\"}",
                )
            }
            Ok(n) => Some(n),
        },
    };

    // Which station's forecast. `station_id` is **not validated against the configuration**
    // -- the Go handler queries with whatever integer it was given and a row that is not
    // there is the empty `404`. A `station` NAME is resolved through the device list, and an
    // unknown one is the JSON-as-text `404`. The asymmetry is the contract's.
    let station = match params.non_empty("station_id") {
        Some(raw) => match raw.parse::<i64>() {
            Err(_) => {
                return reply::json_as_text(
                    StatusCode::BAD_REQUEST,
                    "{\"error\":\"Invalid station_id parameter\"}",
                )
            }
            Ok(id) => match h.directory.station_by_id(id) {
                Some(s) => Some(s.key.clone()),
                // An id with no device: the Go query runs against the id itself and finds
                // nothing, so the answer is the **empty** `404` rather than a message.
                None => return reply::empty(StatusCode::NOT_FOUND),
            },
        },
        None => match params.non_empty("station") {
            Some(name) => match h.directory.station_by_name(name) {
                Some(s) => Some(s.key.clone()),
                None => {
                    return reply::json_as_text(
                        StatusCode::NOT_FOUND,
                        "{\"error\":\"Station not found\"}",
                    )
                }
            },
            // No station at all: any station's forecast for this span.
            None => None,
        },
    };

    let Some(store) = h.forecasts.load() else {
        // The database is down. The contract's bytes for "no record" are an empty `404`,
        // which is also the honest answer here -- and is not a `503` (**D-48**). The
        // difference is in the log line and in `X-MW-Degraded` on the endpoints that carry
        // it.
        tracing::warn!("the forecast store is unavailable, so /forecast answers its no-record 404");
        return reply::empty(StatusCode::NOT_FOUND);
    };

    let record = match store
        .forecast(ForecastQuery {
            station,
            span_hours,
        })
        .await
    {
        Ok(Some(record)) => record,
        // **The empty 404** (Q-11): no `Content-Type`, no body, no newline.
        Ok(None) => return reply::empty(StatusCode::NOT_FOUND),
        // **Also the empty 404, not a 500.** Go never inspects `result.Error`: it tests
        // `result.RowsAffected == 0` and answers `w.WriteHeader(404)` with no body
        // (`handlers.go:722-726`). A query that failed has `RowsAffected == 0`, so a
        // transient connection loss produced the same bytes as a missing record. The `500`
        // this used to give is the wrong convention twice over -- it is `text/plain` with a
        // message where Go sends nothing, and a `500` on a frozen route is D-48's subject.
        // `error encoding forecast data` is what Go emits for a failed *encode*
        // (`handlers.go:772`), which is a different event.
        Err(e) => {
            tracing::warn!(
                err = %mw_types::DisplayChain(&e),
                "the forecast could not be read, so /forecast answers its no-record 404"
            );
            return reply::empty(StatusCode::NOT_FOUND);
        }
    };

    let data = match period {
        None => record.data,
        Some(index) => match element(&record.data, index) {
            Ok(text) => text,
            Err(PeriodError::NotAnArray) => {
                return reply::json_as_text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "{\"error\":\"Error processing forecast data\"}",
                )
            }
            Err(PeriodError::OutOfRange { len }) => {
                // **Signed, so an empty array really does say `0--1`.** Go formats
                // `len(data)-1` as an `int` (`handlers.go:748`), and the column's default is
                // `'[]'` -- `ci/conformance/rust-seed/04-caches.sql` stores exactly that for
                // `csi-minimal`, so the state is in the rig. `saturating_sub` said `0-0`,
                // which is a different byte string and a range that contains the rejected
                // index.
                let last = len as i64 - 1;
                return reply::json_as_text(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "{{\"error\":\"period {index} exceeds available periods \
                         (valid range: 0-{last})\"}}"
                    ),
                );
            }
        },
    };

    // The splice. `lastUpdated` is Go's `time.Time.String()` layout, which is **not**
    // RFC 3339 -- `weather-utils.js` parses it with the moment format
    // `YYYY-MM-DD HH:mm:ss.SSSSSS-ZZ`, so the space and the trailing zone word are part of
    // the contract.
    let body = format!(
        "{{\"lastUpdated\": \"{}\", \"data\": {}}}",
        go_time_string(record.updated_at),
        data
    );
    // **No trailing newline** (Q-12), and no `Cache-Control`.
    reply::json(StatusCode::OK, Cache::Unset, body.into_bytes())
}

/// `^\d{1,4}$`, as a parse rather than a regex.
///
/// One to four ASCII digits and nothing else -- so `99999` fails on length, `24h` on the
/// trailing character, and `-24` on the sign. Leading zeros are accepted and parse to the
/// number they name, which is what the Go regex and `strconv` do between them: `/forecast/0024`
/// is `/forecast/24`.
fn parse_span(span: &str) -> Option<u32> {
    if span.is_empty() || span.len() > 4 || !span.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    span.parse().ok()
}

/// Why a `?period=` could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeriodError {
    /// The stored document is not an array, which the table's `CHECK` makes
    /// unrepresentable and which is answered rather than asserted.
    NotAnArray,
    /// The index is past the end. The message names `len - 1`, which the fixture asserts.
    OutOfRange { len: usize },
}

/// One element of the stored array, re-encoded.
///
/// **This is the one place the stored text is parsed**, and the module documentation has
/// why it has to be: the Go handler unmarshals into `[]interface{}` and re-marshals the
/// element, so the element's key order and number format are Go's rather than jsonb's, and
/// the fixtures recorded that. [`GoNumbers`] is what makes the number half match;
/// `serde_json`'s `Map` is a `BTreeMap` without the `preserve_order` feature, which makes
/// the key half alphabetical -- the same order `json.Marshal` of a `map[string]interface{}`
/// produces, for the same reason.
fn element(stored: &str, index: i64) -> Result<String, PeriodError> {
    let value: serde_json::Value =
        serde_json::from_str(stored).map_err(|_| PeriodError::NotAnArray)?;
    let array = value.as_array().ok_or(PeriodError::NotAnArray)?;
    let at = usize::try_from(index).map_err(|_| PeriodError::OutOfRange { len: array.len() })?;
    let element = array
        .get(at)
        .ok_or(PeriodError::OutOfRange { len: array.len() })?;
    let mut out = Vec::with_capacity(512);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, GoNumbers);
    serde::Serialize::serialize(element, &mut ser).map_err(|_| PeriodError::NotAnArray)?;
    String::from_utf8(out).map_err(|_| PeriodError::NotAnArray)
}

/// Go's `time.Time.String()`: `2006-01-02 15:04:05.999999999 -0700 MST`.
///
/// **Not RFC 3339**, and the difference is the whole of the quirk: a space instead of `T`,
/// a space before the offset, the offset without a colon, and a trailing zone abbreviation.
/// `UTC` is the abbreviation for a UTC instant, which is what the column holds and what the
/// fixture recorded (`2026-10-28 15:45:12.482913 +0000 UTC`).
///
/// The fraction is Go's `.999999999`: trailing zeros removed, and the whole fraction gone
/// when it is zero.
///
/// **`+0000 UTC` is hard-coded, and that is a deployment-shaped limit rather than a code
/// one.** Go renders `record.UpdatedAt.String()` in the zone the driver's session is in, and
/// `pkg/config/provider.go` puts `TimeZone=` in the DSN -- so a deployment configured
/// `TimeZone=America/Denver` emits `2026-10-28 09:45:12.482913 -0600 MDT`. W1's rig sets
/// none, so the fixture recorded UTC and this reproduces the fixture. Recorded so the next
/// reader does not take the literal for an oversight.
fn go_time_string(t: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Timelike;
    let date = t.format("%Y-%m-%d %H:%M:%S");
    if t.nanosecond() == 0 {
        return format!("{date} +0000 UTC");
    }
    let frac = format!("{:09}", t.nanosecond());
    let frac = frac.trim_end_matches('0');
    format!("{date}.{frac} +0000 UTC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn the_span_grammar_is_one_to_four_digits() {
        assert_eq!(parse_span("24"), Some(24));
        assert_eq!(parse_span("240"), Some(240));
        assert_eq!(parse_span("0"), Some(0));
        assert_eq!(parse_span("0024"), Some(24));
        assert_eq!(parse_span("9999"), Some(9999));
        assert_eq!(parse_span("99999"), None, "five digits");
        assert_eq!(parse_span(""), None);
        assert_eq!(parse_span("24h"), None);
        assert_eq!(parse_span("-24"), None);
        assert_eq!(parse_span(" 24"), None);
    }

    /// Q-12 and Q-13 in one assertion: the stored text is spliced verbatim, so `0.0` stays
    /// `0.0` and the body does not end in a newline.
    #[test]
    fn the_stored_text_is_spliced_verbatim() {
        let stored = "[{\"snowIN\": 0.0, \"pop\": 10}]";
        let body = format!(
            "{{\"lastUpdated\": \"{}\", \"data\": {}}}",
            go_time_string(
                chrono::Utc
                    .with_ymd_and_hms(2026, 10, 28, 15, 45, 12)
                    .single()
                    .expect("a real instant")
            ),
            stored
        );
        assert_eq!(
            body,
            "{\"lastUpdated\": \"2026-10-28 15:45:12 +0000 UTC\", \
             \"data\": [{\"snowIN\": 0.0, \"pop\": 10}]}"
        );
        assert!(
            !body.ends_with('\n'),
            "Q-12: /forecast is not newline-terminated"
        );
    }

    /// Go's `time.Time.String()`, with and without a fraction.
    #[test]
    fn last_updated_is_gos_time_string_and_not_rfc_3339() {
        let base = chrono::Utc
            .with_ymd_and_hms(2026, 10, 28, 15, 45, 12)
            .single()
            .expect("a real instant");
        assert_eq!(go_time_string(base), "2026-10-28 15:45:12 +0000 UTC");
        assert_eq!(
            go_time_string(base + chrono::Duration::microseconds(482_913)),
            "2026-10-28 15:45:12.482913 +0000 UTC",
            "the fixture's own value"
        );
        assert_eq!(
            go_time_string(base + chrono::Duration::nanoseconds(100_000_000)),
            "2026-10-28 15:45:12.1 +0000 UTC",
            "trailing zeros are dropped"
        );
    }

    /// The `?period=` branch: the element, alphabetically keyed and Go-numbered, which is
    /// what `json.Marshal` of a `map[string]interface{}` produces and what the fixture
    /// recorded.
    #[test]
    fn a_period_is_the_element_reencoded_the_way_go_reencodes_it() {
        let stored = "[{\"zebra\": 1, \"pop\": 10.0, \"avgTempF\": 33}, {\"a\": 2}]";
        assert_eq!(
            element(stored, 0).expect("in range"),
            "{\"avgTempF\":33,\"pop\":10,\"zebra\":1}",
            "alphabetical, and 10.0 re-encoded as Go writes it"
        );
        assert_eq!(element(stored, 1).expect("in range"), "{\"a\":2}");
    }

    /// The out-of-range message names `len - 1`, which the fixture asserts byte for byte.
    #[test]
    fn an_out_of_range_period_reports_the_available_range() {
        let stored = format!("[{}]", ["{}"; 10].join(","));
        assert_eq!(
            element(&stored, 99),
            Err(PeriodError::OutOfRange { len: 10 })
        );
        // An empty array, which the rig really stores (`04-caches.sql`'s `csi-minimal`
        // 240-hour row) and which the column's own default produces.
        assert_eq!(element("[]", 0), Err(PeriodError::OutOfRange { len: 0 }));
    }

    /// The out-of-range body, end to end, for the empty array Go renders as `0--1`.
    ///
    /// Go computes `len(data)-1` in a signed `int`, so the message really does carry two
    /// minus signs. `saturating_sub` said `0-0` instead -- a different byte string, and a
    /// range that contains the index it is rejecting.
    #[tokio::test]
    async fn an_empty_forecast_reports_gos_negative_range() {
        let r = match element("[]", 0) {
            Err(PeriodError::OutOfRange { len }) => reply::json_as_text(
                StatusCode::BAD_REQUEST,
                &format!(
                    "{{\"error\":\"period 0 exceeds available periods (valid range: 0-{})\"}}",
                    len as i64 - 1
                ),
            ),
            other => panic!("{other:?}"),
        };
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(r.into_body(), 4096)
            .await
            .expect("small");
        assert_eq!(
            String::from_utf8(body.to_vec()).expect("utf-8"),
            "{\"error\":\"period 0 exceeds available periods (valid range: 0--1)\"}\n"
        );
    }

    #[test]
    fn a_stored_document_that_is_not_an_array_is_a_named_refusal() {
        assert_eq!(element("{\"a\":1}", 0), Err(PeriodError::NotAnArray));
        assert_eq!(element("not json", 0), Err(PeriodError::NotAnArray));
    }
}
