//! Span fields, stored once and read by every layer that needs them.
//!
//! §5.4's requirement is that an operator can isolate one device's entire story, which
//! needs `station` to be *inherited* from the enclosing span rather than repeated on every
//! event: `Cx` carries a pre-entered span with `component = %id`, so a component is
//! attributed without doing anything (§5.1), and the ingest, delivery and request paths
//! each add `station`.
//!
//! `tracing`'s registry does not keep a span's recorded values for a layer to read, so one
//! of them has to. Doing it once, in a type both layers name, is why this module exists:
//! the ring needs every field for the record, the dedupe layer needs `station` and `class`
//! for its key, and visiting the same `Attributes` twice would be the only alternative.

use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::record::{Fields, Value};

/// One span's recorded fields, in the registry's per-span extensions.
///
/// Whichever layer's `on_new_span` runs first inserts it and the others find it, which is
/// also why it is `pub`: two private copies in two modules would be two types and two
/// visits.
#[derive(Debug)]
pub struct SpanFields(pub Fields);

/// Record a new span's fields, unless another layer already did.
pub fn store<S>(attrs: &Attributes<'_>, id: &Id, ctx: &Context<'_, S>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let Some(span) = ctx.span(id) else { return };
    let mut ext = span.extensions_mut();
    if ext.get_mut::<SpanFields>().is_none() {
        ext.insert(SpanFields(Fields::of(attrs)));
    }
}

/// Fold a late `Span::record` call into the stored fields.
pub fn extend<S>(id: &Id, values: &SpanRecord<'_>, ctx: &Context<'_, S>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let Some(span) = ctx.span(id) else { return };
    let mut ext = span.extensions_mut();
    let late = Fields::of(values);
    match ext.get_mut::<SpanFields>() {
        Some(SpanFields(f)) => {
            f.values.extend(late.values);
            f.causes.extend(late.causes);
            if let Some(m) = late.message {
                f.message = Some(m);
            }
        }
        None => ext.insert(SpanFields(late)),
    }
}

/// The nearest enclosing span's value for a field, innermost first.
///
/// "Nearest wins" is the rule §5.4 needs: a request span's `station`, resolved after host
/// routing, is more specific than the component span's.
pub fn nearest<S>(event: &Event<'_>, ctx: &Context<'_, S>, name: &str) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let scope = ctx.event_scope(event)?;
    for span in scope {
        let ext = span.extensions();
        if let Some(SpanFields(f)) = ext.get::<SpanFields>() {
            if let Some(v) = f.get(name) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Every enclosing span's name and fields, **outermost first**.
///
/// Outermost first so that a consumer reading the field list backwards finds the innermost
/// value, and so that the `span` chain in the JSON rendering reads in the order the spans
/// were entered.
pub fn chain<S>(
    event: &Event<'_>,
    ctx: &Context<'_, S>,
) -> (Vec<&'static str>, Vec<(&'static str, Value)>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut names = Vec::new();
    let mut fields = Vec::new();
    if let Some(scope) = ctx.event_scope(event) {
        for span in scope.from_root() {
            names.push(span.name());
            let ext = span.extensions();
            if let Some(SpanFields(f)) = ext.get::<SpanFields>() {
                fields.extend(f.values.iter().cloned());
            }
        }
    }
    (names, fields)
}
