//! Two rings, one API (§5.7), and the layer that fills them (§5.8).
//!
//! The separation is justified by **measured** eviction asymmetry, not by taste: `/latest`
//! is 93% of ~19,000 requests, so the access stream is one to two orders of magnitude the
//! volume of the application stream and a single ring would mean an afternoon of normal
//! traffic evicting the one `ERROR` the operator is looking for. What was wrong with the
//! Go version is the *presentation* split -- two screens with two clocks, when correlating
//! "this request 500'd" with "the query failed" is the whole job -- so there are two rings
//! and [`LogRings::merged`] is one view.
//!
//! Sizes: 4,096 application records (about 8x the Go version's 500) and 16,384 access
//! records. At ~300 bytes a record that is ~1.2 MB and ~5 MB, so **~6 MB resident, bounded
//! and constant**, which is affordable on the 512 MB target and is stated here so nobody
//! has to guess.
//!
//! **What is not here: the live subscription.** §5.7's `subscribe()` merges both rings in
//! timestamp order and then follows with live records, and a broadcast channel needs
//! `tokio`, which `ci/layers.toml` forbids this crate -- the same boundary that keeps a
//! codec golden-testable. So this file owns the buffer and the structure, and the SSE
//! route that follows it belongs with the management API that serves it
//! (implementation-plan.md M7-1). The backfill half is [`LogRings::merged`] and is here.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Filter, Layer};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

use crate::record::{Record, Stream};
use crate::span;

/// The `target` that routes an event to the access ring (§5.2).
pub const ACCESS_TARGET: &str = "mw::access";

/// A bounded buffer of records, newest last.
///
/// On overflow the oldest record is dropped and `ring_dropped_total{stream}` is
/// incremented. The synthetic gap record is **not** stored -- it is produced on read from
/// the counter, so a ring that overflows does not spend one of its slots saying so.
#[derive(Debug)]
pub struct Ring {
    stream: Stream,
    capacity: usize,
    records: Mutex<VecDeque<Arc<Record>>>,
    dropped: AtomicU64,
}

impl Ring {
    /// An empty ring.
    pub fn new(stream: Stream, capacity: usize) -> Self {
        assert!(capacity > 0, "a ring with no slots is not a ring");
        Ring {
            stream,
            capacity,
            records: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            dropped: AtomicU64::new(0),
        }
    }

    /// Append, evicting the oldest record if the ring is full.
    pub fn push(&self, record: Record) {
        // `Arc` rather than the record itself, because [`Ring::snapshot`] is the UI's
        // backfill and it runs while every event in the process is trying to push: a deep
        // clone of 16,384 records -- thousands of allocations and ~5 MB for the access
        // ring -- held this mutex for the duration, so one management-UI connect stalled
        // the whole logging path. A record is immutable once pushed, so sharing it is
        // free.
        let record = Arc::new(record);
        let mut q = self.lock();
        let evicted = q.len() == self.capacity && q.pop_front().is_some();
        q.push_back(record);
        // The counter is touched outside the lock: the recorder takes locks of its own,
        // and the ring is on every event's path. Counted and exported rather than logged,
        // because a log line about the log buffer overflowing is one more record in the
        // ring that overflowed.
        drop(q);
        if evicted {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            metrics::counter!("mw_ring_dropped_total", "stream" => self.stream.as_str())
                .absolute(dropped);
        }
    }

    /// How many records have been evicted since the process started.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How many records are buffered.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The ring's contents, newest last, preceded by a gap record if anything was
    /// evicted. This is the backfill a subscriber gets on connect.
    pub fn snapshot(&self) -> Vec<Arc<Record>> {
        let dropped = self.dropped();
        let buffered = self.lock().iter().cloned().collect::<Vec<_>>();
        if dropped == 0 {
            return buffered;
        }
        // Dated at the oldest surviving record, which is where the hole actually is.
        let at = buffered.first().map_or_else(Utc::now, |r| r.ts);
        let mut out = Vec::with_capacity(buffered.len() + 1);
        out.push(Arc::new(Record::gap(self.stream, dropped, at)));
        out.extend(buffered);
        out
    }

    /// A poisoned ring lock would mean losing the UI's log view for the life of the
    /// process, and every critical section here is a `VecDeque` operation that cannot
    /// panic -- so the lock is recovered rather than propagated.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<Record>>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The two rings (§5.7).
#[derive(Debug)]
pub struct LogRings {
    app: Ring,
    access: Ring,
}

impl LogRings {
    /// §5.7's application capacity.
    pub const APP_CAPACITY: usize = 4096;
    /// §5.7's access capacity. One to two orders of magnitude the application stream.
    pub const ACCESS_CAPACITY: usize = 16384;

    /// Both rings at their stated sizes.
    pub fn new() -> Self {
        Self::with_capacities(Self::APP_CAPACITY, Self::ACCESS_CAPACITY)
    }

    /// Both rings at chosen sizes, for a test that wants to see an eviction without
    /// emitting four thousand events.
    pub fn with_capacities(app: usize, access: usize) -> Self {
        LogRings {
            app: Ring::new(Stream::App, app),
            access: Ring::new(Stream::Access, access),
        }
    }

    /// The ring one stream lands in.
    pub fn ring(&self, stream: Stream) -> &Ring {
        match stream {
            Stream::App => &self.app,
            Stream::Access => &self.access,
        }
    }

    /// Both rings merged in timestamp order, each record carrying its `stream`.
    ///
    /// One `merged()` is what collapses `/logs` and `/http-logs` into one route with a
    /// `stream` filter: an operator correlating a 500 with the query that failed reads one
    /// screen with one clock.
    pub fn merged(&self) -> Vec<Arc<Record>> {
        let mut all = self.app.snapshot();
        all.extend(self.access.snapshot());
        // Stable, so two records with the same millisecond keep the order they were
        // buffered in rather than an arbitrary one.
        all.sort_by_key(|r| r.ts);
        all
    }
}

impl Default for LogRings {
    fn default() -> Self {
        Self::new()
    }
}

/// The layer that visits an event's fields and builds a [`Record`] from the typed values
/// (§5.8).
///
/// One type, two instances: [`RingLayer::app`] takes everything except
/// [`ACCESS_TARGET`] and [`RingLayer::access`] takes only it. §5.2 lists them as two
/// layers, and a target comparison inside one implementation is the same thing without a
/// second copy of the field visiting.
///
/// # Why the filter is owned here rather than applied with `with_filter`
///
/// §5.2 writes the ring layer as `ring_layer.with_filter(ring_filter)` -- "the UI buffers;
/// independently filterable". Written literally that way it is a defect: `Filtered` hands
/// the wrapped layer a `Context` carrying its own `FilterId`, and `Context::event_scope`
/// then yields only the spans *that* filter enabled. So a ring filter narrower than a
/// span's level produced records with no `station` and no span chain -- while the
/// unfiltered stderr layer kept both, which is the one thing §5.8 exists to prevent. The
/// configuration that triggered it is not exotic: it is §5.5's "raise `mw_station::davis`
/// to DEBUG for ten minutes", which raises the *global* directive while this filter stays
/// at its startup level, and §5.4's entire requirement is that the operator can then
/// isolate that one device's story.
///
/// So the filter lives here and is consulted inside `on_event`, leaving the `Context`
/// unfiltered. One consequence worth knowing: a directive's **span-field** predicates
/// (`[span{field=value}]`) need the per-layer-filter machinery to maintain their state, so
/// this consults the directive as a target-and-level matcher. §5.4's field predicate is a
/// `FilterFn` over a station id and belongs to the management API that sets it
/// (implementation-plan.md M7-1), which is where the reloadable half goes too.
#[derive(Debug)]
pub struct RingLayer {
    rings: Arc<LogRings>,
    stream: Stream,
    filter: Option<EnvFilter>,
}

impl RingLayer {
    /// The application ring: every event whose target is not [`ACCESS_TARGET`].
    pub fn app(rings: Arc<LogRings>) -> Self {
        RingLayer {
            rings,
            stream: Stream::App,
            filter: None,
        }
    }

    /// The access ring: only events on [`ACCESS_TARGET`].
    pub fn access(rings: Arc<LogRings>) -> Self {
        RingLayer {
            rings,
            stream: Stream::Access,
            filter: None,
        }
    }

    /// Narrow what this ring buffers, independently of the global level.
    #[must_use]
    pub fn filtered(mut self, filter: EnvFilter) -> Self {
        self.filter = Some(filter);
        self
    }

    fn takes(&self, target: &str) -> bool {
        (target == ACCESS_TARGET) == (self.stream == Stream::Access)
    }
}

impl<S> Layer<S> for RingLayer
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
        if !self.takes(event.metadata().target()) {
            return;
        }
        if let Some(filter) = &self.filter {
            if !Filter::enabled(filter, event.metadata(), &ctx) {
                return;
            }
        }
        self.rings
            .ring(self.stream)
            .push(Record::from_event(event, &ctx, self.stream));
    }
}
