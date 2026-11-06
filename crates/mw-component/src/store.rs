//! The reading store's **write** half: the handle `writer:timescale` resolves.
//!
//! # Why this is a separate trait from the read side, and why it shares its capability
//!
//! W3 §2a.1 names three shapes over **one** `sqlx::PgPool` — `TimescaleSink` (write),
//! `ReadingStore` (read) and the fan-out tap (live) — and says they are "named separately
//! so nobody makes the website a sink" (**D-30**). The separation that does the work is
//! the one in the types: `api:legacy` is handed `LatestSnapshot` and never a store handle,
//! so it *cannot* query (**D-35**), and nothing that serves bytes can reach [`ReadingSink`]
//! because nothing that serves bytes declares `Capability::ReadingStore`.
//!
//! It shares `Capability::ReadingStore` with the read half for the same reason
//! [`crate::Ingress`] shares `Capability::ReadingStream` with [`crate::ReadingStream`]: it
//! is the same resource seen from the other end, and `Access::Read` versus `Access::Write`
//! is what distinguishes the two consumers. W2a §4.1b writes `writer:timescale`'s
//! requirement as `ReadingStore` **H W** — hard, write — which is that distinction
//! declared rather than inferred.
//!
//! **Settled here, by the item that needed it (M3-4).** `db:postgres` publishes exactly
//! one handle per capability, and until this item that handle was `Arc<dyn ReadingSink>` —
//! the write half alone. The read half (`window`, and the `span`/`latest`/`almanac` that
//! arrive with M4-1 and M4-2 — W3 §2a.1) needed somewhere to live, and the design left two
//! shapes:
//!
//! 1. one published handle offering both halves, each still its own trait, which keeps
//!    `Capability::ReadingStore` as "the pool is usable"; or
//! 2. the write half travelling outside the capability bus, which is what `Ingress` does
//!    today — `main` hands it to the configuration rather than publishing it (`mw_station`'s
//!    `config` module records why).
//!
//! **Shape 1 is taken**, and shape 2 is not available here for a reason worth stating
//! rather than rediscovering: the `Ingress` precedent works because `main` *constructs* the
//! spool, so it has something to hand over before `plan()` runs. The pool does not exist
//! until `db:postgres`'s own task has connected and asserted **D-62** against it, so there
//! is nothing for a registrar to hand anybody. Making shape 2 work would mean moving the
//! pool into `main` as a fourth root resource, which is the `ValidateTimescaleDBConfig`-in-
//! a-constructor shape **D-27** rejects and which W2a §4.1 split `db:postgres` from
//! `writer:timescale` precisely to avoid.
//!
//! So [`Store`] is the published type: a trait with **no methods of its own**, whose only
//! content is that it is both halves. That is the part **D-30** is about — "named
//! separately so nobody makes the website a sink" — and it survives intact, because
//! [`ReadingSink`] and [`ReadingStore`] are still two traits with two query sets and
//! `mw-api-legacy` is handed neither. What a consumer does with the handle is upcast it to
//! the half it declared: `writer:timescale` keeps an `Arc<dyn ReadingSink>` and
//! `window:rollup` keeps an `Arc<dyn ReadingStore>`, so neither can reach the other's
//! methods after `build` even though both resolved the same capability.
//!
//! Recorded as **A-M3-4-1** in `docs/reconciliation.md`'s *Proposed amendments* section,
//! because it resolves a choice the design deliberately left open rather than contradicting
//! a decision.
//!
//! **The third shape — two handle *types* published under one capability — is not
//! representable, and it is the one somebody will reach for.** `Capability::ReadingStore`
//! is `Arity::Exclusive`; `Publisher::publish` replaces per `(provider, capability)` and
//! `CapabilitySlot::sole` returns the single entry, so a `db:postgres` that published both
//! `Arc<dyn ReadingSink>` and `Arc<dyn ReadingStore>` would have the second clobber the
//! first, and the first consumer's `cx.hard()` would then fail with
//! `BuildError::HandleTypeMismatch` — at build time, not compile time. That is why neither
//! half implements [`crate::CapabilityHandle`] on its own: the only type the bus will
//! accept for this capability is `Arc<dyn Store>`, so the mistake does not compile.

use std::sync::Arc;

use mw_types::Envelope;

use crate::body::BoxFuture;
use crate::capability::Capability;
use crate::cx::CapabilityHandle;

/// What one `store` call did.
///
/// `stored` is the length of the **prefix** that is durably in the database, not a count
/// of successes: a `Sink` must be able to say "the first 100 of these 256 are in", and a
/// count with holes in it is not a position a cursor can take (`mw_pipeline::Acked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stored {
    /// How many envelopes, from the start of the batch, are durable.
    pub stored: usize,
    /// How many rows the statement actually wrote.
    ///
    /// Lower than `stored` when two envelopes in one batch carry the same
    /// `(station_id, time)`: the unique index means the second would overwrite the first,
    /// so they are collapsed before the statement runs. Counted rather than silent,
    /// because a station whose every reading collapses is a station whose observations are
    /// one row per batch and nothing else would say so.
    pub rows: u64,
}

/// The write half of the reading store (W3 §2a.1's `TimescaleSink`).
///
/// One method, because a handle trait is the query set its consumer writes and
/// `writer:timescale` writes exactly one query. **D-56**'s projections are part of it
/// rather than a second method: `stations.last_seen` and `station_metrics` are maintained
/// by the same component, in the same transaction as the rows they describe, so a caller
/// cannot store readings and forget them.
pub trait ReadingSink: Send + Sync + 'static {
    /// Store a batch in order, returning how much of it is durable.
    ///
    /// **Idempotent over a replayed prefix**, which for `writer:timescale` is
    /// `ON CONFLICT (station_id, time) DO UPDATE` — the property W3 §2.3 requires of every
    /// durable sink, because a cancelled delivery replays whatever prefix it had already
    /// written.
    ///
    /// **An unreported metric is `NULL`, never `0.0`** (**D-26**). The frozen contract's
    /// zeros are produced one layer later, at the DTO (§1.7).
    fn store<'a>(&'a self, batch: &'a [Envelope]) -> BoxFuture<'a, Result<Stored, StoreError>>;
}

/// The read half of the reading store (W3 §2a.1's `ReadingStore`).
///
/// **Named separately from [`ReadingSink`] and kept that way** (**D-30**): delivery policy,
/// batching and idempotence on one side; bucket selection and the roll-up's inputs on the
/// other. Nothing that serves bytes declares `Capability::ReadingStore`, so nothing that
/// serves bytes can reach either half — which is the structural form of "the website is not
/// a sink", and of W2a Decision 35's `/latest` handler that *cannot* query.
///
/// # The query set is three methods, and each has exactly one caller
///
/// A handle trait is the query set its consumer writes (this crate's module doc), and the
/// consumer here is `window:rollup`. The three are not three ways of saying the same thing:
///
/// | Method | Answers | Why it cannot be one of the others |
/// |---|---|---|
/// | [`ReadingStore::buckets`] | the recent 1-minute buckets, whole | the ring resolves *any* declared `(metric, agg)`, so it needs every column — but only ever an hour of them |
/// | [`ReadingStore::totals`] | one sum per `(level, range)` | a year of 1-minute buckets is 525,600 rows; summed server-side at the level that tiles the range it is ~530 |
/// | [`ReadingStore::series`] | one `total` per bucket, in order | a **gap** is not expressible as a sum, and the storm's start is the far side of a dry gap |
///
/// `span`, `latest` and `almanac` (W3 §2a.1) are absent because nothing calls them yet:
/// `snapshot:warm` needs `latest` (M4-1) and `/span` and `/almanac` need the other two
/// (M4-2). They are additions to this trait when those items land, which is the same rule
/// [`crate::StationRegistry`] grew under — a method with no caller is a signature nobody
/// has checked against a query.
pub trait ReadingStore: Send + Sync + 'static {
    /// Every 1-minute aggregate bucket for one station at or after `since`, oldest first.
    ///
    /// The whole bucket: one value per column the 1-minute level declares, in
    /// `mw_types::LEVELS[0].columns` order, so a consumer indexes rather than names. That
    /// is 624 columns today, which sounds like a lot and is the only answer that works —
    /// `window:rollup` publishes a capability whose declarations it does not know until
    /// `aprs:*` and `wu:*` arrive (M6-2), so a narrower query would be a guess at somebody
    /// else's table. It stays cheap because the refresh is **incremental**: a tick asks for
    /// the buckets after the newest one it holds, which in the steady state is one.
    fn buckets<'a>(&'a self, q: BucketQuery) -> BoxFuture<'a, Result<Vec<Bucket>, StoreError>>;

    /// One sum of a `total`-strategy metric per requested piece, in the order asked.
    ///
    /// Each [`Piece`] names the aggregate **level** it is read from, because the caller is
    /// the only thing that knows a range's boundaries are bucket-aligned at that level —
    /// which is what makes stitching levels exact rather than approximate. **D-13**'s
    /// composable state is what licenses it: a 1-hour bucket's `total` is the sum of its
    /// sixty 1-minute buckets' totals, in double precision (**D-14**), so replacing them
    /// with it changes no value.
    ///
    /// `None` for a piece no bucket fell in, which the caller combines rather than
    /// defaulting: the sum of "nothing recorded" and "0.3 inches" is 0.3 inches, and the
    /// sum of nothing at all is **not** zero (**D-26**). `COALESCE(SUM(...), 0)` is exactly
    /// what `rainfall_calc.go` writes and exactly what makes an offline gauge report a dry
    /// day.
    fn totals<'a>(&'a self, q: TotalsQuery) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>>;

    /// A `total`-strategy metric's value per bucket at one level, oldest first.
    ///
    /// The one query whose *shape* is the answer rather than its sum. `window:rollup` finds
    /// the current rain event's start by looking for the most recent dry gap, and a gap
    /// cannot be expressed as a total — which is why the Go implementation's
    /// `calculate_storm_rainfall` gives up and defines the storm as "any rainfall in the
    /// last 24 hours" with a comment saying `-- Simple approach`.
    fn series<'a>(&'a self, q: SeriesQuery) -> BoxFuture<'a, Result<Vec<Bucketed>, StoreError>>;
}

/// Both halves of the reading store, as the one handle `db:postgres` publishes.
///
/// No methods of its own, on purpose: the module documentation has the whole of why, and
/// the short version is that merging two query sets into one eight-method trait is the
/// thing **D-30** is against, while publishing two handle *types* under one exclusive
/// capability is not representable.
pub trait Store: ReadingSink + ReadingStore {}

/// Anything that is both halves is the handle, so an implementation never writes this.
impl<T> Store for T where T: ReadingSink + ReadingStore {}

impl CapabilityHandle for Arc<dyn Store> {
    /// The capability both halves share, because it is one pool seen from two ends —
    /// exactly as [`crate::Ingress`] and [`crate::ReadingStream`] share
    /// `Capability::ReadingStream`. `Access` is what distinguishes the two consumers, not
    /// the capability: W2a §4.1b writes `writer:timescale`'s requirement as
    /// `ReadingStore` **H W** and `window:rollup`'s as `ReadingStore` **H**.
    const CAPABILITY: Capability = Capability::ReadingStore;
}

/// Which level of the aggregate hierarchy a read comes from.
///
/// A closed enum rather than the `&'static str` the generated `Level` carries, so that a
/// typo is a compile error and the mapping onto a view name happens once. The four are
/// `metrics/registry.toml`'s four levels (**D-11**, extended by W4's 5-minute level); the
/// raw hypertable is deliberately **not** one of them, because every read on this trait is
/// of composable aggregate state and raw rows are the writer's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BucketLevel {
    /// 1-minute buckets, the only level that reads raw.
    Minute,
    /// 5-minute buckets. W4's level, which `/span` serves its default band from.
    FiveMinute,
    /// 1-hour buckets.
    Hour,
    /// 1-day buckets, in **UTC** — `time_bucket` without a zone, which is why a
    /// station-local month is stitched from day buckets plus hour and minute edges.
    Day,
}

impl BucketLevel {
    /// Every level, coarsest last.
    pub const ALL: &'static [BucketLevel] = &[
        BucketLevel::Minute,
        BucketLevel::FiveMinute,
        BucketLevel::Hour,
        BucketLevel::Day,
    ];

    /// The registry's name for this level, which is also the key into `mw_types::LEVELS`.
    pub const fn as_str(self) -> &'static str {
        match self {
            BucketLevel::Minute => "1m",
            BucketLevel::FiveMinute => "5m",
            BucketLevel::Hour => "1h",
            BucketLevel::Day => "1d",
        }
    }

    /// How long one bucket at this level covers.
    ///
    /// Exact for all four, which is the property the caller's range arithmetic needs: a
    /// `time_bucket(INTERVAL '1 day', time)` boundary is a fixed 86,400-second grid from
    /// the epoch and **not** a calendar day, so a day bucket really is a duration here
    /// even though a station's local day is not.
    pub const fn span(self) -> std::time::Duration {
        match self {
            BucketLevel::Minute => std::time::Duration::from_secs(60),
            BucketLevel::FiveMinute => std::time::Duration::from_secs(300),
            BucketLevel::Hour => std::time::Duration::from_secs(3600),
            BucketLevel::Day => std::time::Duration::from_secs(86_400),
        }
    }
}

impl std::fmt::Display for BucketLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which buckets [`ReadingStore::buckets`] should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketQuery {
    /// Whose.
    pub station: mw_types::StationKey,
    /// The oldest bucket wanted, inclusive.
    pub since: chrono::DateTime<chrono::Utc>,
    /// How many to return at most, newest-biased by the `ORDER BY`.
    ///
    /// A bound rather than a convenience: the first tick after a restart asks for an hour
    /// and a tick after a long stall could ask for a week, and a `Vec` sized by how long
    /// the daemon was down is the shape that turns an outage into an allocation failure.
    pub limit: usize,
}

/// One 1-minute aggregate bucket, as composable state.
///
/// `values` is one entry per column of `mw_types::LEVELS[0].columns`, in that order, which
/// is what lets a consumer resolve a `(metric, agg)` by looking the column up once and
/// indexing thereafter. **Every value is an `f64` and none of them is a finished average**
/// — the level stores `_sum` and `_count`, or `_sin_sum`/`_cos_sum`/`_dir_count`, and the
/// finalisation is the consumer's (**D-11**, **D-13**).
///
/// `f64` for all four of the level's SQL types (`double precision`, `bigint`, `float4`,
/// `int4`) because the roll-up arithmetic is in double precision anyway (**D-14**) and a
/// count that does not fit in an `f64` exactly is a count above 2^53 in one minute. One
/// decode path rather than four is also what keeps a 624-column read from being a
/// 624-branch `match`.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    /// The bucket's start instant.
    pub bucket: chrono::DateTime<chrono::Utc>,
    /// One value per declared column, in `mw_types::LEVELS[0].columns` order. `None` is a
    /// column no reading in the bucket carried.
    pub values: Vec<Option<f64>>,
}

/// One bucket of a single metric's `total`, for [`ReadingStore::series`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucketed {
    /// The bucket's start instant.
    pub bucket: chrono::DateTime<chrono::Utc>,
    /// The bucket's total, or `None` if no reading in it carried the metric.
    pub total: Option<f64>,
}

/// One half-open bucket range at one level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    /// Which level's buckets.
    pub level: BucketLevel,
    /// The first bucket wanted, inclusive.
    pub from: chrono::DateTime<chrono::Utc>,
    /// The first bucket **not** wanted.
    pub until: chrono::DateTime<chrono::Utc>,
}

/// Which sums [`ReadingStore::totals`] should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotalsQuery {
    /// Whose.
    pub station: mw_types::StationKey,
    /// Which metric. Must declare `Strategy::Total` at every level named below, which the
    /// implementation checks rather than assumes.
    pub metric: mw_types::MetricKey,
    /// The pieces, each answered in this order.
    pub pieces: Vec<Piece>,
}

/// Which series [`ReadingStore::series`] should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesQuery {
    /// Whose.
    pub station: mw_types::StationKey,
    /// Which metric. Must declare `Strategy::Total` at `level`.
    pub metric: mw_types::MetricKey,
    /// Which level's buckets.
    pub level: BucketLevel,
    /// The oldest bucket wanted, inclusive.
    pub since: chrono::DateTime<chrono::Utc>,
    /// How many to return at most. Bounded for [`BucketQuery::limit`]'s reason.
    pub limit: usize,
}

/// Why a store call failed.
///
/// Classified here rather than at the call site (§4.2), because what happens next differs:
/// a connection that died is a batch that replays, and a statement the server rejected is
/// a build that disagrees with its schema and will reject the same batch forever.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database is unreachable, or the connection died mid-statement.
    ///
    /// The cursor does not advance, so the batch replays; the component goes to `Backoff`
    /// and `mw_spool_lag_envelopes` rises, which is the visible-and-lossless shape W3 §2.3
    /// specifies.
    #[error("the weather database could not be reached")]
    Unavailable {
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The server rejected the statement, or a value would not encode.
    ///
    /// `Fatal`: a column that is not there is not there on the next attempt either.
    ///
    /// **Known gap — the classification changes nothing today, and that is worth saying
    /// rather than discovering.** `From<StoreError> for ComponentError` wraps this in
    /// `ComponentError::tick`, whose own `Classify` is unconditionally `Retryable` and
    /// deliberately does not consult the boxed source (`mw_component::error`'s comment
    /// explains why), and `mw_supervisor::task::interpret` branches only on `Ignorable`.
    /// So a permanently-rejected batch — a `CHECK` violation, a NUL byte in a text metric
    /// — replays forever until the spool's `max_lag` abandons the cursor seven days later,
    /// exactly as a database outage would. What the variant buys today is the **log line**
    /// and the cause chain under it.
    ///
    /// Recorded the way `mw_pipeline::sink` records its own `Acked(0)` divergence, and for
    /// the same reason: the alternative is a comment that reads as a behavioural promise.
    /// Closing it needs `Sink::deliver`'s error to reach the supervisor with its concrete
    /// type intact, which is `mw-component`'s trait and `mw-supervisor`'s driver.
    #[error("the weather database rejected the write")]
    Rejected {
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl mw_types::Classify for StoreError {
    fn class(&self) -> mw_types::Class {
        match self {
            StoreError::Unavailable { .. } => mw_types::Class::RETRYABLE,
            StoreError::Rejected { .. } => mw_types::Class::Fatal,
        }
    }
}

impl StoreError {
    /// A transient failure: the batch replays.
    pub fn unavailable<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        StoreError::Unavailable {
            source: Box::new(source),
        }
    }

    /// A permanent failure: this build and this schema disagree.
    pub fn rejected<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        StoreError::Rejected {
            source: Box::new(source),
        }
    }
}

/// A failed write is a failed occurrence, with the action named (§5.9).
///
/// `mw_pipeline::Sink` requires it: the adapter turns a sink's own error into the
/// supervisor's vocabulary, and the conversion belongs with the error rather than at the
/// one call site, so a second sink over the same store cannot word it differently.
impl From<StoreError> for crate::error::ComponentError {
    fn from(e: StoreError) -> Self {
        crate::error::ComponentError::tick("store a batch of readings", e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Class, Classify};

    #[test]
    fn a_dead_connection_replays_and_a_rejected_statement_does_not() {
        assert_eq!(
            StoreError::unavailable(std::io::Error::other("connection reset")).class(),
            Class::RETRYABLE
        );
        assert_eq!(
            StoreError::rejected(std::io::Error::other("column \"snowdepth\" does not exist"))
                .class(),
            Class::Fatal
        );
    }

    #[test]
    fn a_store_failure_keeps_its_chain() {
        let e = StoreError::unavailable(std::io::Error::other("no route to host"));
        assert_eq!(
            mw_types::DisplayChain(&e).to_string(),
            "the weather database could not be reached: no route to host"
        );
    }
}
