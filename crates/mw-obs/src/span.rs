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
///
/// **Two invariants this storage depends on, both load-bearing.** At least one layer with
/// no per-layer filter must call [`store`], or a span the filtered layers rejected has no
/// fields recorded at all; and **a layer that reads the scope must not sit behind
/// `with_filter`**, because `Filtered` hands the wrapped layer a `Context` carrying its own
/// `FilterId` and [`Context::event_scope`] then yields only the spans *that* filter
/// enabled. The second one is why [`crate::RingLayer`] owns its filter and consults it
/// inside `on_event` instead: with `with_filter`, a ring filter narrower than the span's
/// level silently produced records with no `station` and no span chain -- in exactly the
/// configuration §5.5's "raise the global level to DEBUG" creates.
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
///
/// **Replaces by name rather than appending**, for two reasons that both bite. It is
/// `tracing`'s own semantic -- `Span::record` on a field that already has a value is an
/// overwrite, not a second value -- and it is what makes this function *idempotent*, which
/// it has to be: four layers in this stack implement `on_record`, so one
/// `Span::record("status", 200)` arrives here four times. Appending stored it four times,
/// printed it four times in the pretty rendering, and left a long-lived span's field list
/// growing without bound -- a `Cx` component span lives for the process, and
/// [`chain`] clones its whole list into every record beneath it, so §5.7's "~6 MB
/// resident, bounded and constant" stopped holding.
pub fn extend<S>(id: &Id, values: &SpanRecord<'_>, ctx: &Context<'_, S>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let Some(span) = ctx.span(id) else { return };
    let mut ext = span.extensions_mut();
    let late = Fields::of(values);
    match ext.get_mut::<SpanFields>() {
        Some(SpanFields(f)) => {
            for (name, value) in late.values {
                match f.values.iter_mut().find(|(n, _)| *n == name) {
                    Some(slot) => slot.1 = value,
                    None => f.values.push((name, value)),
                }
            }
            // A re-recorded `err` replaces the chain rather than adding a second one, for
            // the same reason: the four `on_record` calls are one event.
            if !late.causes.is_empty() {
                f.causes = late.causes;
            }
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
