//! The stderr layer: one rendering of the structured record, chosen once at startup.
//!
//! **No journald-native layer** (§5.3). journald captures stderr and parses one JSON
//! object per line perfectly well, and `libsystemd` is a shared library that a statically
//! linked musl build does not have -- the same decision as §3.5's `sd-notify`, for the
//! same reason.

use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::format::Shape;
use crate::record::{Record, Stream};
use crate::ring::ACCESS_TARGET;
use crate::{render, span, stderr};

/// Writes every event that survives the filters to standard error.
#[derive(Debug)]
pub struct WriteLayer {
    shape: Shape,
}

impl WriteLayer {
    /// A layer rendering in one shape.
    pub fn new(shape: Shape) -> Self {
        WriteLayer { shape }
    }
}

impl<S> Layer<S> for WriteLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        span::store(attrs, id, &ctx);
    }

    fn on_record(&self, id: &Id, values: &SpanRecord<'_>, ctx: Context<'_, S>) {
        span::extend(id, values, &ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let stream = if event.metadata().target() == ACCESS_TARGET {
            Stream::Access
        } else {
            Stream::App
        };
        let record = Record::from_event(event, &ctx, stream);
        let line = match self.shape {
            Shape::Pretty { ansi } => render::pretty(&record, ansi),
            Shape::Json => render::json(&record),
        };
        stderr::write_line(&line);
    }
}
