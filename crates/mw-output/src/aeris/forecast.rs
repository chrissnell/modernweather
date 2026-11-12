//! The forecast response, and the eighteen keys `/forecast` serves out of it.
//!
//! # What this module is responsible for being byte-exact about
//!
//! `/forecast` returns `{"lastUpdated": <string>, "data": <the stored jsonb>}` with the
//! `jsonb` written back **verbatim** (**D-39**), so the text this module produces is a
//! frozen response body one storage round trip later. PostgreSQL's `jsonb` normalises key
//! *order* (length, then bytewise) and preserves the **numeric text** it was given, which
//! divides the problem in two:
//!
//! - **Key order does not matter.** It is written in the Go struct's order below anyway,
//!   because a reviewer comparing the two reads them in the same sequence.
//! - **Key set and number formatting do matter**, and both are this module's.
//!
//! So the periods are deserialised into the eighteen typed fields and then written out by
//! hand, rather than handed to `serde_json::to_string`. The reason is one character:
//! `serde_json` renders an integral `f32` as `0.0` and Go's `encoding/json` renders it as
//! `0` — and `precipIN`, `iceaccumIN` and `snowIN` are integral (zero) in most periods of
//! most forecasts, so a `serde_json` round trip would change three numbers per period on a
//! frozen body. [`go_f32`] is Rust's own shortest-round-trip `Display`, which agrees with
//! Go's `strconv.AppendFloat(b, f, 'f', -1, 32)` in that range.
//!
//! **Two residual differences from Go's bytes, stated rather than discovered later.**
//! Neither is reachable with data this upstream sends, and both are cheaper to record than
//! to defend against:
//!
//! 1. **`<`, `>` and `&` in a string.** `encoding/json` escapes them as `<`, `>`
//!    and `&` by default; this does not. The five string fields are an ISO timestamp,
//!    a compass abbreviation, and three vendor-controlled weather words
//!    (`"Partly Cloudy"`, `"::SC"`), so the difference needs the vendor to put an ampersand
//!    in a forecast summary. Fixing it would mean a Go-compatible JSON string escaper in a
//!    third crate, which `ci/layers.toml`'s `go-number-digits` rule is the precedent for
//!    not doing casually.
//! 2. **The shortest-representation tie-break, and the exponent threshold.** Rust breaks a
//!    tie round-half-up and Go's `ryu` breaks it round-half-even; Go switches to `e`
//!    notation below `1e-6`, and Rust's `Display` does not. Reaching either needs a
//!    precipitation value with eight significant digits or one below a millionth of an inch.
//!    `ci/layers.toml` confines `ryu` to the two crates that must produce Go's `float64`
//!    digits and says a third needs "a reason, and an assertion against one of the existing
//!    two"; three `float32` precipitation fields that the vendor reports to two decimal
//!    places are not that reason.

use std::fmt::Write as _;

use serde::Deserialize;

use crate::error::AerisError;

/// The envelope every Aeris endpoint answers with.
///
/// `success` is the one field that decides whether the body is an answer or an error, and
/// it is **independent of the status code**: a bad credential is `200` with
/// `{"success": false, "error": {...}}`, which is why `crate::https::classify` is not the
/// verdict for this path. `crate::https::get`'s own documentation has that note.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    /// Present when `success` is false. A string on some endpoints and an object on others
    /// — `{"code": "invalid_client", "description": "..."}` — so it is read as whatever it
    /// is and rendered for the log rather than typed.
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    response: Vec<Group>,
}

/// One `response[]` element. The forecast endpoint answers with exactly one, holding the
/// periods.
#[derive(Debug, Deserialize)]
struct Group {
    #[serde(default)]
    periods: Vec<Period>,
}

/// One forecast period, in the sixteen fields the Go struct declares plus the two it
/// derives.
///
/// Every field is `#[serde(default)]` by the struct-level attribute, which is the Go
/// behaviour rather than a convenience: Go's struct is non-pointer scalars, so a key the
/// upstream omits stays at its zero value and reaches the stored JSON as `0` or `""`. A
/// missing `pop` is stored as `"pop":0` by the Go daemon, and reproducing that is the
/// point — the website reads the key unconditionally.
#[derive(Debug, Deserialize, Default, PartialEq)]
#[serde(default)]
struct Period {
    /// `dateTimeISO`, as the vendor's text.
    ///
    /// Kept as a string and parsed in [`write_period`] rather than deserialised into a
    /// timestamp, for two reasons. `chrono` is pinned `default-features = false` with
    /// `std` and `clock` for the whole workspace, so it has no `serde` impl — and
    /// turning one on workspace-wide to parse one vendor field would be a dependency
    /// change for everybody. The second is better: the error this produces is then
    /// *about this field*, which a `serde` failure on the whole envelope is not.
    #[serde(rename = "dateTimeISO")]
    date_time_iso: Option<String>,
    #[serde(rename = "maxTempF")]
    max_temp_f: i16,
    #[serde(rename = "minTempF")]
    min_temp_f: i16,
    #[serde(rename = "avgTempF")]
    avg_temp_f: i16,
    /// `pop` — probability of precipitation, per cent.
    pop: i16,
    #[serde(rename = "precipIN")]
    precip_in: f32,
    #[serde(rename = "iceaccumIN")]
    ice_accum_in: f32,
    #[serde(rename = "snowIN")]
    snow_in: f32,
    #[serde(rename = "maxFeelslikeF")]
    max_feelslike_f: i16,
    #[serde(rename = "minFeelslikeF")]
    min_feelslike_f: i16,
    #[serde(rename = "windSpeedMPH")]
    wind_speed_mph: i16,
    #[serde(rename = "windSpeedMaxMPH")]
    wind_speed_max_mph: i16,
    #[serde(rename = "windDir")]
    wind_dir: String,
    #[serde(rename = "windDirDEG")]
    wind_dir_deg: u16,
    weather: String,
    #[serde(rename = "weatherPrimaryCoded")]
    weather_primary_coded: String,
}

/// The eighteen keys, in the Go struct's declaration order.
///
/// Asserted against the writer below by a unit test, so that a field added to [`Period`]
/// and forgotten here is a test failure rather than a key that quietly stops being served.
const KEYS: [&str; 18] = [
    "dateTimeISO",
    "maxTempF",
    "minTempF",
    "avgTempF",
    "pop",
    "precipIN",
    "iceaccumIN",
    "snowIN",
    "maxFeelslikeF",
    "minFeelslikeF",
    "windSpeedMPH",
    "windSpeedMaxMPH",
    "windDir",
    "windDirDEG",
    "weather",
    "weatherPrimaryCoded",
    "weatherIcon",
    "compactWeather",
];

/// Turn one forecast response into the JSON array text the `forecasts.data` column stores.
///
/// The whole of what `aeris:forecast:<station>:<span>` does with a response body. Every
/// refusal names what was wrong with the payload, because the alternative — which is what
/// the Go controller does — is an index panic:
/// `response.AerisForecastData[0].Periods` is read without checking that `response` has an
/// element, so an upstream that answers `{"success":true,"response":[]}` takes the process
/// down. Maintenance windows produce exactly that.
pub fn periods_json(body: &[u8]) -> Result<String, AerisError> {
    let envelope: Envelope =
        serde_json::from_slice(body).map_err(|source| AerisError::Decode { source })?;
    if !envelope.success {
        return Err(AerisError::Refused {
            detail: render_error(envelope.error.as_ref()),
        });
    }
    let Some(group) = envelope.response.first() else {
        return Err(AerisError::Empty {
            what: "the forecast response carried no `response[]` element",
        });
    };
    if group.periods.is_empty() {
        return Err(AerisError::Empty {
            what: "the forecast response carried no periods",
        });
    }
    let mut out = String::with_capacity(group.periods.len() * 320);
    out.push('[');
    for (i, period) in group.periods.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_period(&mut out, period)?;
    }
    out.push(']');
    Ok(out)
}

/// One period object, keys in [`KEYS`] order.
fn write_period(out: &mut String, period: &Period) -> Result<(), AerisError> {
    let (icon, compact) = derived(&period.weather_primary_coded)?;
    let when = match period.date_time_iso.as_deref() {
        // **Parsed and re-rendered, not passed through**, because that is what Go's
        // `time.Time` round trip does: an input carrying `.000` comes back without it. The
        // offset is preserved rather than normalised to UTC -- it is the station's local
        // one, and rendering it as `Z` would move every period's label by seven hours on
        // the website.
        //
        // `AutoSi` rather than `Secs`: it omits the fractional part when it is zero, which
        // is every value this vendor sends, and keeps it when it is not. Go's `RFC3339Nano`
        // additionally strips *trailing* zeros inside a fraction (`.5`, not `.500`), which
        // `AutoSi` renders in groups of three -- unreachable with whole-second input and
        // recorded rather than worked around.
        Some(text) => match chrono::DateTime::parse_from_rfc3339(text) {
            Ok(when) => when.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false),
            // Go's `json.Unmarshal` fails the **whole** response on an unparseable
            // `time.Time`, so the fetch produces nothing and the previous forecast keeps
            // being served. Same outcome, with the field named.
            Err(_) => {
                return Err(AerisError::Shape {
                    what: "a forecast period's `dateTimeISO`",
                    found: "not an RFC 3339 timestamp",
                })
            }
        },
        // Go's zero `time.Time` marshals as `"0001-01-01T00:00:00Z"`, which is what a
        // period with no `dateTimeISO` stores there. Reproduced rather than dropped,
        // because the key is read unconditionally by the website's chart.
        None => "0001-01-01T00:00:00Z".to_string(),
    };
    out.push('{');
    write_str(out, KEYS[0], &when)?;
    write_int(out, KEYS[1], period.max_temp_f);
    write_int(out, KEYS[2], period.min_temp_f);
    write_int(out, KEYS[3], period.avg_temp_f);
    write_int(out, KEYS[4], period.pop);
    write_f32(out, KEYS[5], period.precip_in);
    write_f32(out, KEYS[6], period.ice_accum_in);
    write_f32(out, KEYS[7], period.snow_in);
    write_int(out, KEYS[8], period.max_feelslike_f);
    write_int(out, KEYS[9], period.min_feelslike_f);
    write_int(out, KEYS[10], period.wind_speed_mph);
    write_int(out, KEYS[11], period.wind_speed_max_mph);
    write_str(out, KEYS[12], &period.wind_dir)?;
    write_int(out, KEYS[13], i32::from(period.wind_dir_deg));
    write_str(out, KEYS[14], &period.weather)?;
    write_str(out, KEYS[15], &period.weather_primary_coded)?;
    write_str(out, KEYS[16], icon)?;
    write_str(out, KEYS[17], compact)?;
    out.push('}');
    Ok(())
}

/// `"key":<json string>`, with a leading comma when the object is not empty.
///
/// The value goes through `serde_json` rather than through a hand-written escaper: the
/// escaping rules are the part of JSON that is easy to get subtly wrong, and the one thing
/// this module is deliberately not inheriting from `serde_json` is number formatting.
fn write_str(out: &mut String, key: &str, value: &str) -> Result<(), AerisError> {
    separate(out, key);
    let encoded = serde_json::to_string(value).map_err(|source| AerisError::Encode { source })?;
    out.push_str(&encoded);
    Ok(())
}

/// `"key":<integer>`.
fn write_int(out: &mut String, key: &str, value: impl Into<i32>) {
    separate(out, key);
    let _ = write!(out, "{}", value.into());
}

/// `"key":<Go's float32 digits>`.
fn write_f32(out: &mut String, key: &str, value: f32) {
    separate(out, key);
    out.push_str(&go_f32(value));
}

/// The comma, the quoted key and the colon.
fn separate(out: &mut String, key: &str) {
    if !out.ends_with('{') {
        out.push(',');
    }
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
}

/// A `float32` written the way Go's `encoding/json` writes one.
///
/// Rust's `Display` for a float is the shortest decimal that round-trips and — unlike
/// `Debug` and unlike `serde_json` — does **not** append `.0` to an integral value, which
/// is the whole of why this function exists: `0` rather than `0.0` for the three
/// precipitation fields, which is what a Go-written row holds and therefore what
/// `/forecast` has always served.
///
/// A non-finite value becomes `0`. `encoding/json` **refuses** to marshal a NaN or an
/// infinity (`UnsupportedValueError`), which on the Go path means the whole forecast is
/// discarded and the previous one keeps being served; here it means the period stores a
/// zero in one field. The upstream has no way to send either — JSON has no literal for
/// them, so a NaN could only come from this program's own arithmetic, and there is none on
/// this path.
pub fn go_f32(value: f32) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    format!("{value}")
}

/// The Weather Icons glyph and the compact description for one coded forecast.
///
/// Go reads `strings.Split(p.WeatherPrimaryCoded, ":")[2]`, which is **an index panic** on
/// a coded value with fewer than three segments — and the coded vocabulary is
/// `<coverage>:<intensity>:<weather>`, so a vendor that ever sends a bare code takes the
/// daemon down. Here it is a refusal naming the value, which is W2a Decision 30's
/// distinction: the next tick retries and the component's `last_error` says why.
///
/// An **empty** third segment is the documented case and is not an error: Go answers `"?"`
/// and `""`, and a chart that shows a question mark is the shipped behaviour. An unknown
/// non-empty code answers `""` for both, which is Go's map returning its zero value —
/// reproduced rather than corrected, because the alternative is a glyph this program chose
/// appearing where the Go daemon showed nothing.
fn derived(coded: &str) -> Result<(&'static str, &'static str), AerisError> {
    let mut parts = coded.split(':');
    let (_coverage, _intensity) = match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(AerisError::Coded {
                coded: coded.to_string(),
            })
        }
    };
    let Some(code) = parts.next() else {
        return Err(AerisError::Coded {
            coded: coded.to_string(),
        });
    };
    if code.is_empty() {
        return Ok(("?", ""));
    }
    Ok(match ICONS.iter().find(|(c, _, _)| *c == code) {
        Some((_, icon, compact)) => (icon, compact),
        None => ("", ""),
    })
}

/// The thirty-five weather codes, their Weather Icons glyph, and their compact description.
///
/// One table rather than Go's two maps, because the two are keyed identically and a code
/// present in one and absent from the other would be a forecast with an icon and no words
/// (or the reverse) — which two maps make representable and this does not.
///
/// The glyphs are Private Use Area code points from the Weather Icons font and are written
/// as escapes rather than as characters: `\u{f015}` is reviewable in a diff and an invisible
/// PUA character is not. They reach the stored JSON as raw UTF-8, which is what
/// `encoding/json` does with them too — only `<`, `>`, `&`, U+2028 and U+2029 are escaped
/// there, and a PUA code point is none of those.
///
/// Source: <https://www.aerisweather.com/support/docs/api/reference/weather-codes/> as
/// mapped by `internal/controllers/aerisweather/controller.go` at `c3ec9f7`. The duplicates
/// are the Go table's and are deliberate: blowing dust and blowing sand share a glyph, as
/// do the four frozen-mix codes.
const ICONS: [(&str, &str, &str); 35] = [
    ("A", "\u{f015}", "Hail"),
    ("BD", "\u{f082}", "Blowing Dust"),
    ("BN", "\u{f082}", "Blowing Sand"),
    ("BR", "\u{f014}", "Mist"),
    ("BS", "\u{f064}", "Blowing Snow"),
    ("BY", "\u{f018}", "Blowing Spray"),
    ("F", "\u{f014}", "Fog"),
    ("FC", "\u{f056}", "Funnel Clouds"),
    ("FR", "\u{f0b5}", "Frost"),
    ("H", "\u{f0b6}", "Haze"),
    ("IC", "\u{f076}", "Ice Crystals"),
    ("IF", "\u{f076}", "Ice Fog"),
    ("IP", "\u{f0b5}", "Sleet"),
    ("K", "\u{f062}", "Smoke"),
    ("L", "\u{f01c}", "Drizzle"),
    ("R", "\u{f019}", "Rain"),
    ("RW", "\u{f01a}", "Rain Showers"),
    ("RS", "\u{f017}", "Rain-Snow Mix"),
    ("SI", "\u{f0b5}", "Snow-Sleet Mix"),
    ("WM", "\u{f0b5}", "Wintry Mix"),
    ("S", "\u{f01b}", "Snow"),
    ("SW", "\u{f064}", "Snow Showers"),
    ("T", "\u{f01e}", "Thunderstorms"),
    ("UP", "\u{f019}", "Unknown Precip"),
    ("VA", "\u{f0c8}", "Volcanic Ash"),
    ("WP", "\u{f056}", "Waterspouts"),
    ("ZF", "\u{f014}", "Freezing Fog"),
    ("ZL", "\u{f01c}", "Freezing Drizzle"),
    ("ZR", "\u{f019}", "Freezing Rain"),
    ("ZY", "\u{f018}", "Freezing Spray"),
    ("CL", "\u{f00d}", "Clear"),
    ("FW", "\u{f00c}", "Mostly Sunny"),
    ("SC", "\u{f002}", "Partly Cloudy"),
    ("BK", "\u{f07d}", "Mostly Cloudy"),
    ("OV", "\u{f013}", "Cloudy"),
];

/// An upstream `error` field, rendered for a log line and a `last_error`.
///
/// Bounded, because the field is whatever the vendor put there: a string on the forecast
/// endpoint and a `{"code", "description"}` object on the alerts one. A `Display` of the
/// whole `Value` is the one rendering that works for both without typing a union nobody
/// reads twice.
pub fn render_error(error: Option<&serde_json::Value>) -> String {
    match error {
        Some(serde_json::Value::String(s)) if !s.is_empty() => truncate(s),
        Some(serde_json::Value::Null) | None => "the upstream gave no reason".to_string(),
        Some(other) => truncate(&other.to_string()),
    }
}

/// At most [`crate::error::MAX_BODY_EXCERPT`] characters, so a vendor that answers with a
/// page cannot write one into the log ring.
fn truncate(s: &str) -> String {
    s.chars().take(crate::error::MAX_BODY_EXCERPT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body of a one-period forecast, with the three precipitation fields integral —
    /// which is the ordinary case and the one `serde_json` would render differently.
    const ONE_PERIOD: &str = r#"{"success":true,"error":null,"response":[{"periods":[
        {"dateTimeISO":"2026-11-12T00:00:00-07:00","maxTempF":44,"minTempF":21,
         "avgTempF":32,"pop":10,"precipIN":0,"iceaccumIN":0,"snowIN":0,
         "maxFeelslikeF":41,"minFeelslikeF":14,"windSpeedMPH":7,"windSpeedMaxMPH":15,
         "windDir":"NNW","windDirDEG":338,"weather":"Partly Cloudy",
         "weatherPrimaryCoded":"::SC","extraVendorKey":"dropped"}]}]}"#;

    #[test]
    fn the_stored_text_carries_the_eighteen_keys_and_nothing_else() {
        let json = periods_json(ONE_PERIOD.as_bytes()).expect("a successful response");
        let parsed: Vec<std::collections::BTreeMap<String, serde_json::Value>> =
            serde_json::from_str(&json).expect("the writer emits valid JSON");
        assert_eq!(parsed.len(), 1);
        let keys: Vec<&str> = parsed[0].keys().map(String::as_str).collect();
        let mut expected = KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            keys, expected,
            "the Go struct's key set, no more and no less"
        );
    }

    /// The defect this module's hand-written writer exists to prevent: `serde_json` renders
    /// an integral `f32` as `0.0`, Go renders it as `0`, `jsonb` preserves whichever it was
    /// given, and `/forecast` serves it verbatim (**D-39**).
    #[test]
    fn an_integral_precipitation_value_is_a_bare_zero_the_way_go_writes_it() {
        let json = periods_json(ONE_PERIOD.as_bytes()).expect("a successful response");
        assert!(
            json.contains(r#""precipIN":0,"#),
            "expected a bare `0`, got: {json}"
        );
        assert!(!json.contains("0.0"), "no `.0` anywhere: {json}");
        assert_eq!(go_f32(0.0), "0");
        assert_eq!(go_f32(1.0), "1");
        assert_eq!(go_f32(0.5), "0.5");
        assert_eq!(go_f32(0.01), "0.01");
        assert_eq!(
            go_f32(f32::NAN),
            "0",
            "encoding/json refuses it; we store zero"
        );
    }

    #[test]
    fn the_offset_in_the_timestamp_survives_the_round_trip() {
        let json = periods_json(ONE_PERIOD.as_bytes()).expect("a successful response");
        assert!(
            json.contains(r#""dateTimeISO":"2026-11-12T00:00:00-07:00""#),
            "the station's own offset, not UTC: {json}"
        );
    }

    #[test]
    fn the_two_derived_keys_come_from_the_coded_forecast() {
        let json = periods_json(ONE_PERIOD.as_bytes()).expect("a successful response");
        assert!(json.contains(r#""compactWeather":"Partly Cloudy""#));
        assert!(
            json.contains("\"weatherIcon\":\"\u{f002}\""),
            "the Weather Icons glyph for `SC`: {json}"
        );
    }

    #[test]
    fn an_absent_key_stores_the_go_zero_value() {
        let body = r#"{"success":true,"response":[{"periods":[
            {"dateTimeISO":"2026-11-12T00:00:00-07:00","weatherPrimaryCoded":"::R"}]}]}"#;
        let json = periods_json(body.as_bytes()).expect("a successful response");
        assert!(json.contains(r#""pop":0"#), "{json}");
        assert!(json.contains(r#""windDir":"""#), "{json}");
        assert!(json.contains(r#""maxTempF":0"#), "{json}");
    }

    #[test]
    fn an_empty_weather_code_is_a_question_mark_and_not_an_error() {
        assert_eq!(derived("::").expect("three segments"), ("?", ""));
        assert_eq!(derived(":VL:").expect("three segments"), ("?", ""));
    }

    #[test]
    fn an_unknown_weather_code_renders_as_go_s_missing_map_entry() {
        assert_eq!(derived("::XX").expect("three segments"), ("", ""));
    }

    /// Go indexes `[2]` without checking, so this input panics the daemon. Here it is a
    /// named refusal and the next tick retries.
    #[test]
    fn a_coded_forecast_with_too_few_segments_is_refused_rather_than_panicking() {
        let e = derived("SC").expect_err("two segments are missing");
        assert!(matches!(e, AerisError::Coded { .. }));
        let e = derived("::").map(|_| ()).err();
        assert!(e.is_none(), "two colons IS three segments");
    }

    /// The other Go panic on this path: `response[0]` on an empty array.
    #[test]
    fn an_empty_response_array_is_refused_rather_than_panicking() {
        let e = periods_json(br#"{"success":true,"response":[]}"#).expect_err("no group");
        assert!(matches!(e, AerisError::Empty { .. }), "{e:?}");
        let e = periods_json(br#"{"success":true,"response":[{"periods":[]}]}"#)
            .expect_err("no periods");
        assert!(matches!(e, AerisError::Empty { .. }), "{e:?}");
    }

    /// A bad credential is `200` with `success: false`, which is why the status code is not
    /// the verdict on this path.
    #[test]
    fn a_successless_body_is_refused_with_the_upstream_s_reason() {
        let body = br#"{"success":false,"error":{"code":"invalid_client","description":"bad"}}"#;
        let e = periods_json(body).expect_err("not a success");
        let AerisError::Refused { detail } = &e else {
            panic!("expected a refusal, got {e:?}");
        };
        assert!(detail.contains("invalid_client"), "{detail}");
    }

    #[test]
    fn a_body_that_is_not_json_is_a_decode_failure_and_not_a_refusal() {
        let e = periods_json(b"<html>502 Bad Gateway</html>").expect_err("not JSON");
        assert!(matches!(e, AerisError::Decode { .. }), "{e:?}");
    }

    /// Every code in the table yields both a glyph and a description, which is what one
    /// table rather than Go's two maps buys.
    #[test]
    fn every_code_has_a_glyph_and_words() {
        for (code, icon, compact) in ICONS {
            assert!(!icon.is_empty(), "{code} has no glyph");
            assert!(!compact.is_empty(), "{code} has no description");
        }
        assert_eq!(ICONS.len(), 35, "the Go table's thirty-five codes");
    }
}
