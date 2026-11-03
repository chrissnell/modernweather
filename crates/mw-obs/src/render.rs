//! One formatter, three renderings (§5.8).
//!
//! > **The rule: the ring never stores formatted text. ANSI is a rendering, produced from
//! > the record by the same formatter that writes stderr -- never scraped from stderr and
//! > never re-parsed.**
//!
//! That rule is why this module takes a [`Record`] rather than an event: the stderr
//! stream, the management API's `?format=ansi` escape hatch and the UI's SSE records all
//! start from the same structure, so a line an operator pastes into a ticket looks like
//! the line the UI showed them. The Go implementation serialises zap output to JSON and
//! then `json.Unmarshal`s it back into structs to feed the UI; there is one direction
//! here, and it starts at the typed record.
//!
//! The palette is §5.8's, and the UI's CSS tokens use the same assignment: ERROR red, WARN
//! yellow, INFO green, DEBUG blue, TRACE magenta, `target` and `module` dimmed, field
//! names dim italic, the message in the default foreground.

use tracing::Level;

use crate::record::{Record, Value};

/// SGR reset.
const RESET: &str = "\x1b[0m";
/// Dim, for `target` and `module`.
const DIM: &str = "\x1b[2m";
/// Dim italic, for a field's name.
const DIM_ITALIC: &str = "\x1b[2;3m";

/// The level colour (§5.8).
const fn colour(level: Level) -> &'static str {
    match level {
        Level::ERROR => "\x1b[31m",
        Level::WARN => "\x1b[33m",
        Level::INFO => "\x1b[32m",
        Level::DEBUG => "\x1b[34m",
        Level::TRACE => "\x1b[35m",
    }
}

/// The level, padded so that five levels align in a column.
const fn level_label(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN ",
        Level::INFO => "INFO ",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

/// How far the continuation lines are indented: `HH:MM:SS.mmm ` plus `LEVEL `.
const CONTINUATION: &str = "                   ";

/// The human rendering: one line for the event, one indented line for its fields, one per
/// cause.
///
/// Fields go on their own line rather than running past the message because §5.9's
/// standard puts four things on every `WARN` or above -- the subject, the quantity
/// affected, the disposition and the cause chain -- and a message followed by six fields
/// does not fit a terminal.
pub fn pretty(record: &Record, ansi: bool) -> String {
    let (c, dim, dim_italic, reset) = if ansi {
        (colour(record.level), DIM, DIM_ITALIC, RESET)
    } else {
        ("", "", "", "")
    };
    let mut out = String::with_capacity(160);
    out.push_str(&record.ts.format("%H:%M:%S%.3f").to_string());
    out.push(' ');
    out.push_str(c);
    out.push_str(level_label(record.level));
    out.push_str(reset);
    out.push(' ');
    out.push_str(dim);
    out.push_str(record.target);
    out.push_str(reset);
    for span in &record.spans {
        out.push(' ');
        out.push_str(dim);
        out.push_str(span);
        out.push_str(reset);
    }
    if !record.message.is_empty() {
        out.push_str("  ");
        out.push_str(&record.message);
    }
    let fields = displayable_fields(record);
    if !fields.is_empty() {
        out.push('\n');
        out.push_str(CONTINUATION);
        for (i, (name, value)) in fields.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(dim_italic);
            out.push_str(name);
            out.push_str(reset);
            out.push('=');
            out.push_str(&value.to_string());
        }
    }
    for cause in &record.causes {
        out.push('\n');
        out.push_str(CONTINUATION);
        out.push_str(dim);
        out.push_str("caused by: ");
        out.push_str(reset);
        out.push_str(cause);
    }
    out
}

/// The machine rendering: one JSON object per line, with §5.3's key names.
///
/// `flatten_event(true)` in §5.2's sketch is this: the event's own fields sit beside
/// `message` rather than under a `fields` object, because every consumer of this stream --
/// `jq`, journald's reader, a log shipper -- addresses a field by its own name.
pub fn json(record: &Record) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "ts".into(),
        record
            .ts
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            .into(),
    );
    map.insert("level".into(), record.level.as_str().into());
    map.insert("target".into(), record.target.into());
    map.insert("module".into(), record.module.into());
    map.insert("stream".into(), record.stream.as_str().into());
    if !record.spans.is_empty() {
        map.insert("span".into(), serde_json::Value::from(record.spans.clone()));
    }
    map.insert("message".into(), record.message.as_str().into());
    if !record.causes.is_empty() {
        map.insert("err".into(), serde_json::Value::from(record.causes.clone()));
    }
    for (name, value) in displayable_fields(record) {
        // A field may not shadow a reserved key: `level = "whatever"` would otherwise
        // make the stream unparseable by anything that trusts its own schema. Skipped
        // rather than renamed, because a renamed key is a field nobody can find either.
        if map.contains_key(name) {
            continue;
        }
        map.insert(name.to_string(), json_value(value));
    }
    serde_json::Value::Object(map).to_string()
}

/// The fields worth printing: everything except the error chain, which both renderings
/// present as causes rather than as one long field.
fn displayable_fields(record: &Record) -> Vec<(&'static str, &Value)> {
    record
        .fields
        .iter()
        .filter(|(name, _)| !matches!(*name, "err" | "error"))
        .map(|(name, value)| (*name, value))
        .collect()
}

fn json_value(value: &Value) -> serde_json::Value {
    match value {
        Value::Str(s) => s.as_str().into(),
        Value::I64(n) => (*n).into(),
        Value::U64(n) => (*n).into(),
        // A non-finite float has no JSON spelling, so it becomes its Rust one rather than
        // `null`: a `NaN` that reached a log line is a defect worth reading.
        Value::F64(n) if n.is_finite() => (*n).into(),
        Value::F64(n) => n.to_string().into(),
        Value::Bool(b) => (*b).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Stream;
    use chrono::TimeZone;

    fn record() -> Record {
        Record {
            ts: chrono::Utc
                .with_ymd_and_hms(2026, 11, 3, 4, 5, 6)
                .unwrap()
                .with_timezone(&chrono::Utc),
            level: Level::WARN,
            target: "mw_pipeline::enrich",
            module: "mw_pipeline::enrich",
            spans: vec!["backyard"],
            fields: vec![
                ("metric", Value::Str("out_temp".into())),
                ("value", Value::F64(-412.0)),
                ("limit", Value::F64(-100.0)),
                ("reason", Value::Str("below_range".into())),
                ("disposition", Value::Str("dropped".into())),
            ],
            message: "reading discarded".into(),
            causes: Vec::new(),
            stream: Stream::App,
        }
    }

    #[test]
    fn the_pretty_rendering_names_the_subject_the_quantity_and_the_disposition() {
        let line = pretty(&record(), false);
        assert_eq!(
            line,
            "04:05:06.000 WARN  mw_pipeline::enrich backyard  reading discarded\n\
             \x20                  metric=out_temp value=-412 limit=-100 \
             reason=below_range disposition=dropped"
        );
    }

    #[test]
    fn ansi_is_off_unless_asked_for_and_wraps_the_level_when_it_is() {
        let plain = pretty(&record(), false);
        assert!(!plain.contains('\x1b'), "no escapes without ansi: {plain}");
        let coloured = pretty(&record(), true);
        assert!(coloured.contains("\x1b[33mWARN \x1b[0m"), "{coloured}");
    }

    #[test]
    fn a_cause_chain_is_its_own_line_in_both_renderings() {
        let mut r = record();
        r.causes = vec!["query failed: connection closed by peer".into()];
        r.fields.push((
            "err",
            Value::Str("query failed: connection closed by peer".into()),
        ));
        let line = pretty(&r, false);
        assert!(
            line.ends_with("caused by: query failed: connection closed by peer"),
            "{line}"
        );
        assert!(
            !line.contains("err="),
            "the chain is not also a field: {line}"
        );

        let obj = json(&r);
        assert!(
            obj.contains(r#""err":["query failed: connection closed by peer"]"#),
            "{obj}"
        );
        assert!(
            !obj.contains(r#""err":"query"#),
            "err is an array, not a string: {obj}"
        );
    }

    #[test]
    fn the_json_rendering_uses_section_5_3_key_names_and_an_rfc3339_z_timestamp() {
        let obj = json(&record());
        for key in [
            r#""ts":"2026-11-03T04:05:06.000Z""#,
            r#""level":"WARN""#,
            r#""target":"mw_pipeline::enrich""#,
            r#""module":"mw_pipeline::enrich""#,
            r#""span":["backyard"]"#,
            r#""message":"reading discarded""#,
            r#""metric":"out_temp""#,
            r#""value":-412.0"#,
        ] {
            assert!(obj.contains(key), "{key} missing from {obj}");
        }
    }

    #[test]
    fn a_field_may_not_shadow_a_reserved_key() {
        let mut r = record();
        r.fields.push(("level", Value::Str("not-a-level".into())));
        let obj = json(&r);
        assert!(obj.contains(r#""level":"WARN""#), "{obj}");
        assert!(!obj.contains("not-a-level"), "{obj}");
    }

    #[test]
    fn a_non_finite_float_is_rendered_rather_than_nulled() {
        let mut r = record();
        r.fields.push(("ratio", Value::F64(f64::NAN)));
        let obj = json(&r);
        assert!(obj.contains(r#""ratio":"NaN""#), "{obj}");
    }
}
