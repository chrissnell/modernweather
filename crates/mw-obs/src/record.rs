//! The structured log record, and the visitor that builds one.
//!
//! **The rule this file exists to enforce: the ring never stores formatted text** (§5.8).
//! The Go implementation serialises zap output to JSON and then `json.Unmarshal`s it back
//! into structs to feed the UI (`internal/log/log.go:48`); here a record is built from the
//! event's typed values and ANSI is a rendering produced from it, never scraped from
//! stderr and never re-parsed.
//!
//! Two representation notes, because §5.8's sketch names types this crate does not have:
//!
//! - `spans`, `fields` and `causes` are `Vec` rather than `SmallVec`, and `message` is a
//!   `String` rather than a `CompactString`. Both of those crates would be new entries in
//!   DESIGN.md D-32's table -- `compact_str` is not in `[workspace.dependencies]` at all --
//!   and the allocation they avoid is one per *buffered* record, on a path that is already
//!   writing to stderr. The load-bearing half of §5.8 is that the values are typed, and
//!   that is what [`Value`] is.
//! - `causes` is filled from the event's `err` field, which §4.3's convention renders with
//!   [`mw_types::DisplayChain`] -- so one `err` field is the whole chain, already walked,
//!   and splitting it back apart on `": "` would corrupt any message containing a colon.
//!   A caller that wants the array form has [`mw_types::DisplayChain::causes`].

use std::fmt;

use chrono::{DateTime, Utc};
use tracing::field::{Field, Visit};
use tracing::Level;
use tracing_subscriber::layer::Context;

/// Which ring a record belongs in (§5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The application log.
    App,
    /// The HTTP access log, which is one to two orders of magnitude the volume.
    Access,
}

impl Stream {
    /// The `stream` label, for `ring_dropped_total` and the merged record.
    pub const fn as_str(self) -> &'static str {
        match self {
            Stream::App => "app",
            Stream::Access => "access",
        }
    }
}

/// A field value, kept in the type the event recorded it in.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A string, or a `Display`/`Debug` value rendered once.
    Str(String),
    /// A signed integer.
    I64(i64),
    /// An unsigned integer.
    U64(u64),
    /// A float.
    F64(f64),
    /// A boolean.
    Bool(bool),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => f.write_str(s),
            Value::I64(n) => write!(f, "{n}"),
            Value::U64(n) => write!(f, "{n}"),
            Value::F64(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
        }
    }
}

/// One buffered record (§5.8).
#[derive(Debug, Clone)]
pub struct Record {
    /// When it happened, RFC3339 with `Z` in the JSON rendering.
    pub ts: DateTime<Utc>,
    /// Its level.
    pub level: Level,
    /// The `target`, which is the module path unless the caller overrode it.
    pub target: &'static str,
    /// The emitting module.
    pub module: &'static str,
    /// The span chain, outermost first.
    pub spans: Vec<&'static str>,
    /// Every field, the event's own last so that it wins when a span recorded the same
    /// name -- which is what makes a per-request `station` override the component's.
    pub fields: Vec<(&'static str, Value)>,
    /// The `message` field, pulled out because every rendering puts it somewhere else.
    pub message: String,
    /// The walked error chain, from the event's `err` field.
    pub causes: Vec<String>,
    /// Which ring this is in.
    pub stream: Stream,
}

impl Record {
    /// One field's value, by name. The last occurrence wins, matching `fields`' order.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .rev()
            .find_map(|(k, v)| (*k == name).then_some(v))
    }

    /// Build a record from an event and its span scope.
    ///
    /// The one place a [`Record`] is made from a live event, so the ring and the stderr
    /// renderer cannot disagree about what an event contains -- which is the whole of
    /// §5.8's rule that the ring stores structure and ANSI is a rendering.
    pub fn from_event<S>(event: &tracing::Event<'_>, ctx: &Context<'_, S>, stream: Stream) -> Self
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        let meta = event.metadata();
        let own = Fields::of(event);
        let (spans, mut fields) = crate::span::chain(event, ctx);
        // The event's own fields last, so `field()` -- which reads backwards -- prefers
        // them over a span that recorded the same name.
        fields.extend(own.values);
        Record {
            ts: Utc::now(),
            level: *meta.level(),
            target: meta.target(),
            module: meta.module_path().unwrap_or(meta.target()),
            spans,
            fields,
            message: own.message.unwrap_or_default(),
            causes: own.causes,
            stream,
        }
    }

    /// A synthetic record standing in for evicted ones (§5.7).
    ///
    /// The merged stream emits this rather than silently skipping, so the UI shows "412
    /// entries dropped" -- which is W5 §5's "what does the operator see when the buffer
    /// overflows", answered.
    pub fn gap(stream: Stream, dropped: u64, ts: DateTime<Utc>) -> Self {
        Record {
            ts,
            level: Level::WARN,
            target: GAP_TARGET,
            module: GAP_TARGET,
            spans: Vec::new(),
            fields: vec![
                ("kind", Value::Str("gap".into())),
                ("stream", Value::Str(stream.as_str().into())),
                ("dropped", Value::U64(dropped)),
            ],
            message: format!("{dropped} buffered entries were dropped"),
            causes: Vec::new(),
            stream,
        }
    }

    /// Whether this is a [`Record::gap`] rather than something that was logged.
    pub fn is_gap(&self) -> bool {
        self.target == GAP_TARGET
    }
}

/// The `target` a gap record carries. Not a real module, and deliberately inside the
/// reserved `mw::` namespace so it cannot collide with one.
pub const GAP_TARGET: &str = "mw::ring";

/// Collects an event's or a span's fields into typed values.
#[derive(Debug, Default)]
pub struct Fields {
    /// Every field except `message`.
    pub values: Vec<(&'static str, Value)>,
    /// The `message` field, which `tracing` records like any other.
    pub message: Option<String>,
    /// Every `err` field, rendered.
    pub causes: Vec<String>,
}

impl Fields {
    /// Visit a source of fields -- an event or a span's attributes.
    pub fn of<T: VisitSource>(source: &T) -> Self {
        let mut me = Fields::default();
        source.record_into(&mut me);
        me
    }

    /// One field's value, by name, last occurrence first.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values
            .iter()
            .rev()
            .find_map(|(k, v)| (*k == name).then_some(v))
    }

    fn push(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => self.message = Some(value.to_string()),
            // §4.3's convention: `err = %DisplayChain(&e)` is the whole chain. Kept as a
            // cause AND as a field, because the JSON rendering wants `err` and the pretty
            // one wants a `caused by:` line.
            name @ ("err" | "error") => {
                self.causes.push(value.to_string());
                self.values.push((name, value));
            }
            name => self.values.push((name, value)),
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, Value::Str(value.to_owned()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, Value::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, Value::U64(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, Value::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field, Value::F64(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        // The chain, walked by the one thing in the workspace that knows how (§4.3).
        let chain = mw_types::DisplayChain(value);
        self.causes.extend(chain.causes());
        self.values
            .push((field.name(), Value::Str(chain.to_string())));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%value` arrives here too: `tracing` wraps a `Display` in `format_args!`, whose
        // `Debug` is its `Display`. So this is one branch rather than two.
        self.push(field, Value::Str(format!("{value:?}")));
    }
}

/// Something whose fields can be visited -- an [`tracing::Event`] or a span's
/// [`tracing::span::Attributes`].
///
/// A trait rather than two call sites because both layers in this crate want both, and
/// `Event::record` and `Attributes::record` are different inherent methods with the same
/// shape.
pub trait VisitSource {
    /// Record into the visitor.
    fn record_into(&self, fields: &mut Fields);
}

impl VisitSource for tracing::Event<'_> {
    fn record_into(&self, fields: &mut Fields) {
        self.record(fields);
    }
}

impl VisitSource for tracing::span::Attributes<'_> {
    fn record_into(&self, fields: &mut Fields) {
        self.record(fields);
    }
}

impl VisitSource for tracing::span::Record<'_> {
    fn record_into(&self, fields: &mut Fields) {
        self.record(fields);
    }
}
