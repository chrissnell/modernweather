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

use mw_types::{Envelope, Reading};

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
/// | [`ReadingStore::latest`] | the newest **raw** reading, whole | the three above read aggregate state, and a snapshot is rendered from a reading |
///
/// `latest` is M4-1's addition and has exactly one caller: `snapshot:warm`, the
/// `OnStartup` component that seeds the snapshot table so a restart does not 404 a station
/// that was inside the 10-minute window (**D-37**). **It is not on a request path** —
/// `api:legacy` is handed `LatestSnapshot` and never this trait, which is what makes
/// `/latest`'s zero queries structural (**D-35**) — and W3 §2a.1 says so in the signature's
/// own comment.
///
/// `span` and `almanac` (W3 §2a.1) are still absent because nothing calls them yet: both
/// are M4-2's. They are additions to this trait when that item lands, which is the same
/// rule [`crate::StationRegistry`] grew under — a method with no caller is a signature
/// nobody has checked against a query.
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

    /// A `mean`-strategy metric's **finalised mean** per bucket at one level, oldest first.
    ///
    /// [`ReadingStore::series`]' counterpart for the other strategy, and a separate method
    /// rather than a flag on that one because the arithmetic differs: a `total` metric is
    /// one column and a `mean` metric is a `_sum`/`_count` pair that has to be divided
    /// (**D-11** — the aggregate levels hold composable *state*, not values). A caller that
    /// had to know that would be a caller reimplementing `mw_circular_mean()`'s mistake in
    /// a different place.
    ///
    /// Added by the `snow:*` components (M6-3), which are the first consumer of a shaped
    /// read of a non-`total` metric: every one of the seven numbers `/snow` serves is a
    /// function of the *shape* of `snowdistance` over a window — a dual-threshold walk, a
    /// quantile smoother, a changepoint detector — and none of them is expressible as a
    /// sum. `window:rollup` never needed it because the nine values it maintains are gusts,
    /// rates and calendar totals, which are maxima and sums.
    ///
    /// `None` for a bucket whose `_count` is zero or NULL, which the caller skips rather
    /// than interpolates: a gauge that was unplugged for an hour did not measure a depth of
    /// zero, and [`ReadingStore::totals`]' note has the general form of that argument.
    ///
    /// `Err` for a metric that declares no `mean` strategy at `level`, which is a property
    /// of the *request* and therefore `Fatal` — the next call asks the same thing.
    fn mean_series<'a>(
        &'a self,
        q: MeanSeriesQuery,
    ) -> BoxFuture<'a, Result<Vec<MeanBucket>, StoreError>>;

    /// The newest **raw** reading for one station at or after `since`, whole.
    ///
    /// `since` rather than "the newest, ever", because the one caller's question is W1's
    /// recency rule: `snapshot:warm` wants the reading that would make `/latest` answer
    /// 200, which is one inside the last ten minutes. Pushing the bound into the statement
    /// is also what keeps it an index seek on a hypertable rather than a scan that finds a
    /// row from last March and has it discarded.
    ///
    /// `None` is a station with nothing in the window — a station that is offline, or one
    /// that has never reported. Not an error, for [`ReadingStore::totals`]'s reason: the
    /// absence of a reading is a fact about the station and not a failure of the query.
    ///
    /// **Raw, not aggregated.** A snapshot is rendered from a reading, and the 1-minute
    /// level holds `_sum`/`_count` pairs rather than values (**D-11**), so finalising one
    /// into a reading would mean serving a station's last minute as a mean of itself.
    fn latest<'a>(&'a self, q: LatestQuery) -> BoxFuture<'a, Result<Option<Reading>, StoreError>>;

    /// `/span`'s range: the materialised buckets plus the trailing partial one.
    ///
    /// **One method returning both halves is D-55.** The tail is aggregated from raw
    /// because every level is `materialized_only` — three of the four are the source of
    /// another, so none may serve a bucket the next one up has not seen — and a caller that
    /// had to remember to ask for it separately would eventually not. [`SpanQuery`] has how
    /// the 60-second prefix cache of **D-54** is expressed without a second method.
    ///
    /// `Err` for a column the level does not declare, which is a property of the
    /// *request*: a `(metric, agg)` the registry gives no column for is a declaration to
    /// fix, and `Fatal` because the next call asks the same thing.
    fn span<'a>(&'a self, q: SpanQuery) -> BoxFuture<'a, Result<Span, StoreError>>;

    /// One station's cached extremes, for `/almanac`.
    ///
    /// Every row the table holds, in no promised order: the frozen response's key order is
    /// the DTO's field order and the handler places each row by name, so an order here
    /// would be a second place that decided it.
    ///
    /// An empty `Vec` is a station with no cached extremes, which `/almanac` renders as
    /// `{}` — the shipped empty-cache behaviour, reproduced by the table's shape rather
    /// than by a branch (W1 assumption 12).
    fn almanac<'a>(
        &'a self,
        q: AlmanacQuery,
    ) -> BoxFuture<'a, Result<Vec<AlmanacRecord>, StoreError>>;
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

/// Which reading [`ReadingStore::latest`] should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestQuery {
    /// Whose.
    pub station: mw_types::StationKey,
    /// The oldest reading that would do, inclusive.
    ///
    /// W1 Decision 15's ten minutes, computed by the caller: the recency rule belongs to
    /// the contract and the store should not have a copy of it.
    pub since: chrono::DateTime<chrono::Utc>,
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

/// Which series [`ReadingStore::mean_series`] should return.
///
/// Carries an `until` where [`SeriesQuery`] does not, because its callers read *closed*
/// windows: the smoothing pass re-derives a trailing overlap and must not include buckets
/// a later pass will revise, and the snowfall rate reads a two-minute window around an
/// instant thirty minutes ago. A caller that had to trim the tail itself would be a caller
/// paging rows across the network to drop them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeanSeriesQuery {
    /// Whose.
    pub station: mw_types::StationKey,
    /// Which metric. Must declare `Strategy::Mean` at `level`.
    pub metric: mw_types::MetricKey,
    /// Which level's buckets.
    pub level: BucketLevel,
    /// The oldest bucket wanted, inclusive.
    pub since: chrono::DateTime<chrono::Utc>,
    /// The first bucket **not** wanted, or `None` for "everything from `since` onward".
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    /// How many to return at most. Bounded for [`BucketQuery::limit`]'s reason.
    pub limit: usize,
}

/// One bucket of a single metric's finalised `mean`, for [`ReadingStore::mean_series`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeanBucket {
    /// The bucket's start instant.
    pub bucket: chrono::DateTime<chrono::Utc>,
    /// `_sum / _count`, or `None` if no reading in the bucket carried the metric.
    pub mean: Option<f64>,
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

// ---------------------------------------------------------------------------------------
// `/span` — D-54 and D-55
// ---------------------------------------------------------------------------------------

/// One column of a `/span` row: a metric and how the bucket's state is combined into a
/// value.
///
/// `(MetricKey, Agg)` rather than `mw_types::Declaration`'s triple, because the window is
/// the bucket and the level already says how wide it is. The caller lists the columns the
/// frozen DTO needs; the store resolves each against the level's declared strategies, so a
/// column the registry does not carry is an error about the *request* rather than a
/// silently absent key ([`total_column`]'s reasoning in `mw_store::reader`, generalised).
pub type SpanColumn = (mw_types::MetricKey, mw_types::Agg);

/// Which buckets and which columns [`ReadingStore::span`] should return.
///
/// # `tail_start` is the whole of **D-54**
///
/// The materialised prefix is the buckets strictly **before** `tail_start`; the tail is
/// raw readings at or after it, aggregated into one bucket. The split is a parameter rather
/// than something the store computes, for the reason [`TotalsQuery`]'s pieces carry their
/// own level: only the caller knows where it put the boundary last time, which is what lets
/// it cache the prefix for 60 seconds and re-ask only for the tail.
///
/// **A cache hit is this query with `since == tail_start`.** The prefix range is then empty
/// by construction and the answer is the tail alone — so there is one method, the tail
/// always comes back with it (**D-55**: "no caller can forget it"), and the cache still
/// holds the prefix only (**D-54**).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanQuery {
    /// Whose.
    pub station: mw_types::StationKey,
    /// Which level's buckets. `/span`'s band selection is the caller's: `<= 6h` reads the
    /// 1-minute level, `<= 48h` the 5-minute, `<= 14d` the hour, and anything longer the
    /// day (`docs/legacy-api-contract.md` §2).
    pub level: BucketLevel,
    /// The oldest bucket wanted, inclusive.
    pub since: chrono::DateTime<chrono::Utc>,
    /// The first instant **not** in the materialised prefix, and the first instant that is
    /// in the tail. Bucket-aligned at `level`; see the type documentation.
    pub tail_start: chrono::DateTime<chrono::Utc>,
    /// The columns, answered in this order.
    pub columns: Vec<SpanColumn>,
    /// How many prefix buckets to return at most. Bounded for [`BucketQuery::limit`]'s
    /// reason — a year of 1-minute buckets is 525,600 rows, and the caller's own band
    /// selection is what keeps the number sane.
    pub limit: usize,
}

/// One `/span` bucket, finalised.
///
/// `values` is one entry per [`SpanQuery::columns`] member, in that order. `None` is a
/// column no reading in the bucket carried — **D-26** — and the frozen contract's zero for
/// it is produced one layer later, at the DTO.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanBucket {
    /// The bucket's start instant.
    pub bucket: chrono::DateTime<chrono::Utc>,
    /// One value per requested column, in request order.
    pub values: Vec<Option<f64>>,
}

/// What one [`ReadingStore::span`] answered: the materialised prefix, the closed buckets
/// the aggregate has not caught up to yet, and the trailing partial bucket.
///
/// **Three fields rather than one concatenated list**, because the caller caches exactly one
/// of them (**D-54**). Concatenating here would make the cacheable half unrecoverable
/// without the caller re-deriving the boundary it supplied.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    /// The closed, materialised buckets in `[since, tail_start)`, oldest first.
    ///
    /// A **gap is a missing bucket**, not a zero row: the continuous aggregate produces no
    /// row for a bucket no reading fell in and this does not gap-fill, which is the frozen
    /// contract's Q-9 and the reason a client may not assume a constant interval.
    ///
    /// **Cacheable, and complete** — the only half that is either. It ends at
    /// `tail_start - width` whenever the station has data there, which is what lets a cache
    /// hit ask for everything after it.
    pub prefix: Vec<SpanBucket>,
    /// Closed buckets the continuous aggregate has not materialised yet, aggregated from
    /// raw, oldest first.
    ///
    /// **Not cacheable**, and usually empty. Every level is `materialized_only = true`
    /// (W4's own note on why: TimescaleDB cannot build an aggregate on a real-time view, so
    /// the levels cannot be stacked otherwise), and each refresh policy runs on a schedule
    /// — one minute at the two lower levels, five minutes at `1h`, **thirty** at `1d`. So
    /// between a bucket closing and its policy running, that bucket is in the view's range
    /// and not in the view. Without this field it was in neither half of the answer and
    /// simply absent from the response: `/span/720h` dropped *yesterday* for the first half
    /// hour of every UTC day, and the Go server cannot reproduce that because its views are
    /// `materialized_only = false`.
    ///
    /// Bounded, because an unbounded raw fill is the scan the aggregates exist to avoid —
    /// see `mw_store`'s `MAX_BACKFILL_BUCKETS`.
    pub backfill: Vec<SpanBucket>,
    /// The still-filling bucket at `tail_start`, aggregated from raw.
    ///
    /// `None` when no raw reading has landed in it, which is the ordinary case for a
    /// station that has stopped reporting — and is why it is an `Option` rather than a row
    /// of `None`s: an empty bucket must not appear in the response at all.
    pub tail: Option<SpanBucket>,
}

impl Span {
    /// Every bucket as one sequence, oldest first — what a response body is rendered from.
    pub fn buckets(&self) -> impl Iterator<Item = &SpanBucket> {
        self.prefix
            .iter()
            .chain(self.backfill.iter())
            .chain(self.tail.iter())
    }

    /// The buckets after the cacheable prefix, oldest first.
    ///
    /// What a cache **hit** needs: the prefix came from the cache, and this is everything
    /// the store was asked for on top of it.
    pub fn after_prefix(&self) -> impl Iterator<Item = &SpanBucket> {
        self.backfill.iter().chain(self.tail.iter())
    }
}

/// Which station's cached extremes [`ReadingStore::almanac`] should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlmanacQuery {
    /// Whose.
    pub station: mw_types::StationKey,
}

/// One row of `almanac_cache`: the extreme, when it happened, and the wind direction at
/// the time where the record carries one.
///
/// `f32` rather than `f64` because the column is `real` and the contract serialises both
/// numbers as a Go `float32` — widening would change the digits Go emits for a value that
/// is not exactly representable in 24 bits of mantissa, which a frozen API cannot do (the
/// column's own comment in `migrations/`).
#[derive(Debug, Clone, PartialEq)]
pub struct AlmanacRecord {
    /// The frozen `/almanac` JSON key this row populates — `high_temp`, `low_barometer`.
    /// The table's `CHECK` is the permitted set.
    pub record: String,
    /// The extreme.
    pub value: f32,
    /// When it was observed.
    pub observed_at: chrono::DateTime<chrono::Utc>,
    /// The wind direction at the time, for the records that carry one. `None` is an absent
    /// key rather than a null (`AlmanacRecord.wind_dir` is `omitempty`).
    pub wind_dir: Option<f32>,
}

/// Aeris forecast rows — `Capability::ForecastStore`.
///
/// Its own capability rather than a method on [`ReadingStore`], because the design gives it
/// one (`docs/component-model.md` §2.1) and because the two degrade independently: `/span`
/// failing and `/forecast` failing are different rows of §3.2's table, and `api:legacy`
/// declares both `Soft` so that either can be absent without the other.
///
/// # The response is a byte string, and that is **D-39**
///
/// `/forecast` returns `{"lastUpdated": <string>, "data": <the stored jsonb>}` with the
/// `jsonb` written back **verbatim** — key order is jsonb-normalised (key length, then
/// bytewise) and the numeric text is whatever was stored, so the same contract contains both
/// `0` and `0.0` (Q-13). Nothing in this program parses or re-emits it, which is how
/// PostgreSQL's own whitespace and the missing trailing newline survive. So the handle
/// returns the text and not a parsed document.
pub trait ForecastStore: Send + Sync + 'static {
    /// The newest forecast for one span, optionally for one station.
    ///
    /// `None` is "no record matched", which the frozen handler turns into a `404` with an
    /// **empty body and no `Content-Type`** (Q-11) — not an error.
    fn forecast<'a>(
        &'a self,
        q: ForecastQuery,
    ) -> BoxFuture<'a, Result<Option<Forecast>, StoreError>>;

    /// Replace one `(station, span)` forecast with freshly fetched JSON, stamping
    /// `updated_at`.
    ///
    /// The write half, added by the `aeris:forecast:*` components (M6-3) — the item that
    /// knows the queries, which is what the `lib`-level note on handle traits means by "a
    /// handle trait is a query set". It shares `Capability::ForecastStore` with the read
    /// above for [`ReadingSink`]'s reason: it is one table seen from the other end, and
    /// `Access::Read` against `Access::Write` is what distinguishes the two consumers.
    /// `api:legacy` is handed this same trait object and **cannot** reach this method with
    /// anything to write, because the only thing it has is a [`ForecastQuery`].
    ///
    /// `data` is a JSON **array** as text. Two properties of that, both load-bearing:
    ///
    /// - **It is text and not a parsed document**, because nothing in this program parses
    ///   the forecast (**D-39**). The server casts it to `jsonb` on the way in, which is
    ///   where the key normalisation and the numeric text that `/forecast` serves verbatim
    ///   come from; a `serde_json::Value` round trip here would normalise it twice and the
    ///   second normalisation would be this program's.
    /// - **It must be an array**, which the column's own
    ///   `CHECK (jsonb_typeof(data) = 'array')` enforces: `/forecast?period=N` indexes it
    ///   and its out-of-range message names `len(data) - 1`, so an object would make that
    ///   branch unexpressible. A non-array is therefore `StoreError::rejected` — a
    ///   `Fatal`, because the next fetch produces the same shape.
    ///
    /// `updated_at` is set on both the insert and the update. The table's own comment has
    /// why that is spelled out rather than left to a default: the column has a `DEFAULT`
    /// and no trigger, so an upsert that forgets it leaves the contract-visible
    /// `lastUpdated` at the row's first-insert time — a forecast that refreshes hourly and
    /// reports the day it was first fetched — and **DEV-13** makes the same column the
    /// tiebreak for an unqualified `/forecast`, so a stale value changes which row is
    /// served as well as what it claims.
    fn store_forecast<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        span_hours: u32,
        data: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
}

impl CapabilityHandle for Arc<dyn ForecastStore> {
    const CAPABILITY: Capability = Capability::ForecastStore;
}

/// Which forecast [`ForecastStore::forecast`] should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastQuery {
    /// The station, when the request named one. `None` means "any station's forecast for
    /// this span", which is what the frozen handler does when neither `station_id` nor
    /// `station` is given — `First()` over a `forecast_span_hours` filter with no station
    /// predicate.
    pub station: Option<mw_types::StationKey>,
    /// `forecast_span_hours`: 24 is the hourly forecast and 240 the ten-day one. **Not a
    /// duration** — the path segment is a count of hours of total span (the endpoint's own
    /// grammar, `^\d{1,4}$`).
    pub span_hours: u32,
}

/// One stored forecast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forecast {
    /// `updated_at`, which the wrapper renders with Go's `time.Time.String()` layout.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// The `data` column's text, exactly as PostgreSQL rendered it. See the trait
    /// documentation: this is not re-encoded anywhere.
    pub data: String,
}

/// Aeris alert rows — `Capability::AlertStore`.
///
/// **Write-only, and that is not an oversight.** W2a §2.1 lists this capability beside
/// `ForecastStore` and `SnowCache`, and `db:postgres`'s spec has carried the note that it
/// is "still absent, because `/alerts` is not carried forward (**D-69**) and a capability
/// with no consumer is a handle trait nobody has checked against a query" since M4-2. It
/// arrives here because M6-3 brings the two components that *write* alerts, and
/// `docs/reconciliation.md` **A-M6-3-1** settles that those components are in scope while
/// the endpoint is not: the alert components exist so that "alerts are failing, forecasts
/// are fine" is expressible (W2a **D-33**), which needs the fetch and the store and not a
/// reader.
///
/// So the trait has exactly the two methods the two components need, and no `alerts(…)`
/// read. A read method here would be a query set written against no caller — which is the
/// thing the note above refused to do for a year — and adding one when a surface finally
/// wants alerts is an edit to this trait and its one implementation.
pub trait AlertStore: Send + Sync + 'static {
    /// Insert or revise one fetch's worth of alerts for one station.
    ///
    /// **Revise, not replace.** The Go controller upserts per alert id and never deletes on
    /// fetch, so an alert that drops out of the upstream's current response stays in the
    /// table until it expires and the sweep takes it. That is the behaviour, and it is the
    /// right one: an alert that has begun does not become untrue because the API stopped
    /// listing it, and the expiry the issuing office set is the only authority on when it
    /// stops mattering. A `DELETE`-then-`INSERT` here would make a single failed fetch
    /// erase every active warning.
    ///
    /// An **empty slice is a successful call that writes nothing** — the ordinary case,
    /// since most stations have no active alert most of the time. It is not an error and it
    /// is not a reason to delete anything, for the paragraph above's reason.
    ///
    /// `Ok(n)` is the number of rows written, for the component's log line and its metric.
    fn store_alerts<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        alerts: &'a [Alert],
    ) -> BoxFuture<'a, Result<u64, StoreError>>;

    /// Delete every alert that expired before `cutoff`, returning how many went.
    ///
    /// `aeris:alerts:gc`'s whole body. The cutoff is computed by the **caller** for
    /// [`LatestQuery::since`]'s reason: twenty-four hours past expiry is a retention
    /// decision and the store should not hold a copy of it.
    ///
    /// An alert with **no** expiry is never swept. `expires_at < $1` already has that
    /// property in SQL — a NULL comparison is NULL and a NULL predicate does not delete —
    /// and it is stated here because the obvious "sweep anything old" rewrite over
    /// `fetched_at` does not, and an alert the upstream declined to put an expiry on is the
    /// one alert an operator would least like to lose.
    fn sweep_expired_alerts<'a>(
        &'a self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> BoxFuture<'a, Result<u64, StoreError>>;
}

impl CapabilityHandle for Arc<dyn AlertStore> {
    const CAPABILITY: Capability = Capability::AlertStore;
}

/// One alert, as [`AlertStore::store_alerts`] takes it.
///
/// The four promoted strings and the three timestamps are columns; everything else the
/// upstream sent is `data`. The split is the Go row's and the reason is a list view that
/// does not parse `jsonb` per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// The upstream's own identifier, which is what makes a revision an update rather than
    /// a duplicate. Half of the natural key; the station is the other half.
    pub alert_id: String,
    /// When the issuing office published it. `None` for an upstream that omitted it, which
    /// is a NULL and not the epoch.
    pub issued_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the alert takes effect.
    pub begins_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When it stops applying. The sweep's column; `None` is an alert that is never swept.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `details.name` — "Winter Storm Warning".
    pub name: String,
    /// `details.color` — the upstream's RGB hex, without a `#`.
    pub color: String,
    /// `details.body` — the summary.
    pub body: String,
    /// `details.bodyFull` — the whole product text.
    pub body_full: String,
    /// The entire upstream alert object, as JSON **text**.
    ///
    /// Text and not a parsed document for [`ForecastStore::store_forecast`]'s reason, with
    /// one difference: it is an **object** here and an array there, because `/alerts` paged
    /// over a list of these and each row is one. The column's
    /// `CHECK (jsonb_typeof(data) = 'object')` is what makes a mistake a refusal.
    pub data: String,
}

/// The snow depth, event and smoothing cache rows — `Capability::SnowCache`.
///
/// One row per station and every column `NOT NULL DEFAULT 0`, because `/snow` emits all
/// seven numbers unconditionally and a configured-but-never-reporting gauge returns an
/// all-zero object with status 200 (fixture `snow-device-never-reported`). There is
/// deliberately **no recency filter**: an offline gauge returns its last known depth
/// indefinitely, which is part of the contract (fixture `snow-device-stale`).
pub trait SnowCache: Send + Sync + 'static {
    /// One station's cached snow figures, or `None` for a station with no row.
    ///
    /// `None` and an all-zero row are different facts and the handler treats them the same
    /// way, which is the contract's: both render as seven zeros. They are distinguished
    /// here anyway, because "the gauge has a row of zeros" and "nothing has ever written a
    /// row" are different operational states and collapsing them in the handle would make
    /// the second unobservable.
    fn totals<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
    ) -> BoxFuture<'a, Result<Option<SnowTotals>, StoreError>>;

    /// Write the columns one component owns, leaving every other column alone.
    ///
    /// **A patch and not a row, because four components write this table.**
    /// `docs/component-model.md` §4.3 gives `snow:depth`, `snow:events`, `snow:smoothing`
    /// and `snow:season` one row each, with different periods and independent failures —
    /// which is W2a **D-33** ("one component, one trigger") applied to the snow cache. If
    /// the handle took a whole [`SnowTotals`], each of the four would have to read the
    /// other three's columns and write them back, and a tick that raced another would
    /// silently revert it. A `None` field here is "not mine", and the implementation
    /// coalesces it against the stored value.
    ///
    /// Which component owns which column is [`SnowPatch`]'s documentation.
    ///
    /// `source_through` is the newest bucket the computation consumed, or `None` to leave
    /// it. It is **not** a recency filter and must never become one: `/snow` has none and
    /// must not acquire one (W1 Decision 16, fixture `snow-device-stale`), so this column
    /// is for the management UI's per-tier age and for nothing a response branches on.
    fn store_totals<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        patch: SnowPatch,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// The newest smoothed depth estimate for one station, if there is one.
    ///
    /// `None` is a station the smoother has never run for, which is what makes the next
    /// pass a **backfill from the season start** rather than an incremental update. The
    /// distinction is the whole of why this is its own method and not an
    /// [`SnowCache::estimate_window`] with a one-bucket range: the answer "there is nothing
    /// at all" selects a different algorithm, and a `max()` that returned the epoch would
    /// hide it.
    fn newest_estimate<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
    ) -> BoxFuture<'a, Result<Option<DepthEstimate>, StoreError>>;

    /// The newest estimate strictly **before** `at`.
    ///
    /// The rate limiter's carried state. An incremental pass re-derives a trailing overlap,
    /// so it needs the last estimate *outside* that overlap to limit the first one inside
    /// it against — without which every pass would restart the limiter at its own first
    /// smoothed value and a step the limiter had flattened would reappear at the seam.
    fn estimate_before<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        at: chrono::DateTime<chrono::Utc>,
    ) -> BoxFuture<'a, Result<Option<DepthEstimate>, StoreError>>;

    /// Replace the estimates in `[from, until)` with `rows`, in one transaction.
    ///
    /// Delete-then-insert rather than upsert, and the reason is that a pass can produce
    /// **fewer** rows than it replaces: the source query drops a bucket whose distance is
    /// within 2 mm of the base, so a gauge that goes bare mid-window leaves estimates
    /// behind that an upsert would never remove. One transaction, because the window is
    /// empty between the two statements and a reader in that gap would compute
    /// `max - min` over nothing.
    ///
    /// `until` is **exclusive** and `None` is "everything from `from` onward". It is not
    /// decoration: the smoother backfills in phases, each reading a different aggregate
    /// level over its own slice of history, and a coarse phase whose delete was unbounded
    /// would erase the finer estimates of every phase after it. `mw_pipeline::snow`'s
    /// `BACKFILL` has why the phases exist.
    ///
    /// `rows` must be sorted by bucket and must all lie inside the window; the
    /// implementation does not sort them, because the smoother's output is already in order
    /// and a re-sort here would hide a caller that had lost it.
    fn replace_estimates<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        from: chrono::DateTime<chrono::Utc>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        rows: &'a [DepthEstimate],
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// The first, smallest and largest estimate at or after `from`, in one statement.
    ///
    /// Four numbers because three different totals are three different combinations of
    /// them, and asking for them separately would be three round trips against a window
    /// that is advancing underneath:
    ///
    /// | Total | Expression |
    /// |---|---|
    /// | `snowlast24`, `snowlast72` | `max - min` over the window |
    /// | `snowseason` | `max - first` since the season's start |
    ///
    /// `None` is a window with no estimates in it, which the caller renders as zero
    /// *without writing* — see [`SnowPatch`]. An empty window and a flat one are different
    /// facts and collapsing them here would make the first unobservable.
    fn estimate_window<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        from: chrono::DateTime<chrono::Utc>,
    ) -> BoxFuture<'a, Result<Option<EstimateWindow>, StoreError>>;

    /// Replace one `(station, hours)` window's classified events, in one transaction.
    ///
    /// Wholesale for [`SnowCache::replace_estimates`]' reason — a pass can detect fewer
    /// segments than the last one — and transactional for the same reason: the website's
    /// accumulation bands read this table, and a reader between the delete and the insert
    /// would draw an empty chart.
    fn replace_events<'a>(
        &'a self,
        station: &'a mw_types::StationKey,
        hours: u32,
        events: &'a [SnowEvent],
    ) -> BoxFuture<'a, Result<(), StoreError>>;
}

impl CapabilityHandle for Arc<dyn SnowCache> {
    const CAPABILITY: Capability = Capability::SnowCache;
}

/// The columns of one `snow_totals_cache` row that one component owns.
///
/// Every field is `None` by default ([`SnowPatch::NOTHING`]) and a `Some` is a column this
/// write owns. Which component owns which, from `docs/component-model.md` §4.3 as
/// reconciled with `docs/service-architecture.md` §2a.7:
///
/// | Component | Trigger | Columns |
/// |---|---|---|
/// | `snow:depth` | `Every { 30 s }` | `depth`, `today`, `storm`, `fall_rate` |
/// | `snow:smoothing` | `Every { 15 min }` | `last24`, `last72` |
/// | `snow:season` | `Calendar { daily 00:10, Station }` | `season` |
/// | `snow:events` | `Every { 15 min }` | *none* — it writes `snow_events` |
///
/// `source_through` is **`snow:depth`'s alone**, and that is a choice rather than an
/// omission from the table: it is one column with one meaning — "the newest bucket this
/// row's numbers were computed from" — and two components whose newest buckets are at two
/// cadences would write two different answers to it every half-minute, which is worse than
/// one approximate answer. The fastest tier's is the one a staleness display wants.
///
/// **A `None` is "leave it", which is also how a failed sub-computation is reported.** The
/// Go cache refresh reads its own previous row and writes the old value back when a
/// calculation fails (`internal/snow/cache.go`'s three `keeping previous value` branches),
/// which needs a read, a decision and a write where this needs an absence. Same behaviour,
/// one fewer round trip, and no window in which the component holds a value it is about to
/// overwrite with itself.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SnowPatch {
    /// `snowdepth`, inches — the gauge's current depth.
    pub depth: Option<f32>,
    /// `snowtoday`, inches — new snow since the station's local midnight.
    pub today: Option<f32>,
    /// `snowlast24`, inches.
    pub last24: Option<f32>,
    /// `snowlast72`, inches.
    pub last72: Option<f32>,
    /// `snowseason`, inches.
    pub season: Option<f32>,
    /// `snowstorm`, inches.
    pub storm: Option<f32>,
    /// `snowfallrate`, inches per hour.
    pub fall_rate: Option<f32>,
    /// `source_through` — the newest bucket this computation consumed.
    pub source_through: Option<chrono::DateTime<chrono::Utc>>,
}

impl SnowPatch {
    /// A patch that writes no column — the base every caller spreads over.
    ///
    /// Named rather than written as `SnowPatch::default()` at a call site, because "every
    /// column I do not own is deliberately absent" is a statement and `default()` is not.
    ///
    /// **It is a base, not a message.** An earlier version of this comment claimed it
    /// stamps `computed_at` so that "a tick that found nothing to compute is
    /// distinguishable from a component that is not running"; it does not, because
    /// [`SnowPatch::is_empty`] is true of it and every caller skips the write. That
    /// distinction is the supervisor's — `mw_component_ticks_total{outcome="ok"}` and the
    /// component's `last_run` both move on a tick that computed nothing — and putting a
    /// second answer to it in a cache row would be the two-places defect this type exists
    /// to avoid.
    pub const NOTHING: Self = Self {
        depth: None,
        today: None,
        last24: None,
        last72: None,
        season: None,
        storm: None,
        fall_rate: None,
        source_through: None,
    };

    /// Whether this patch carries any column at all.
    ///
    /// `source_through` counts. It is a column like any other and a patch that carries only
    /// it is a tick that learned which bucket it had consumed and nothing else — which is
    /// exactly what the management UI's per-tier age reads, so dropping it would make
    /// [`SnowPatch::NOTHING`]'s documented purpose unreachable.
    pub fn is_empty(&self) -> bool {
        self.depth.is_none()
            && self.today.is_none()
            && self.last24.is_none()
            && self.last72.is_none()
            && self.season.is_none()
            && self.storm.is_none()
            && self.fall_rate.is_none()
            && self.source_through.is_none()
    }
}

/// One `snow_depth_estimates` row: a smoothed, rate-limited depth in **inches**.
///
/// Inches and not the millimetres `observations.snowdepth` carries, which is Q-4's unit
/// split arriving one layer earlier than the DTO: the rate limit is expressed in inches per
/// hour, so converting at the end would mean limiting in one unit and storing in another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthEstimate {
    /// The 5-minute bucket.
    pub bucket: chrono::DateTime<chrono::Utc>,
    /// The estimated depth, inches.
    pub depth_in: f32,
}

/// What [`SnowCache::estimate_window`] answers: a window's first, smallest and largest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EstimateWindow {
    /// The oldest bucket in the window.
    pub first_at: chrono::DateTime<chrono::Utc>,
    /// The depth at `first_at` — the season total's subtrahend.
    pub first_in: f32,
    /// The smallest depth in the window.
    pub min_in: f32,
    /// The largest depth in the window.
    pub max_in: f32,
}

/// One classified segment of the depth series — a `snow_events` row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnowEvent {
    /// When the segment starts.
    pub start: chrono::DateTime<chrono::Utc>,
    /// When it ends.
    pub end: chrono::DateTime<chrono::Utc>,
    /// What kind of segment it is.
    pub class: EventClass,
    /// The smoothed depth at the start, **millimetres**.
    pub start_depth_mm: f32,
    /// The smoothed depth at the end, millimetres.
    pub end_depth_mm: f32,
    /// How much snow it contributed, millimetres. `None` for the two classes that
    /// contribute none, which the table's own `CHECK` ties to `class`.
    pub accumulation_mm: Option<f32>,
}

/// What a segment of the depth series turned out to be.
///
/// The four classes of `internal/snow/pelt_computer.go`'s `classifySegments`, which are the
/// `snow_events.event_type` `CHECK`'s four values. A closed enum rather than a string for
/// [`Capability`]'s reason: which classes contribute snow is a decision, and a fifth one
/// would have to be made here, in the classifier, and in the migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventClass {
    /// The depth rose and stayed up. Contributes its **net** change.
    Accumulation,
    /// Neither a rise nor a fall worth the threshold. Contributes nothing.
    Plateau,
    /// The depth fell past the threshold — wind, melt or settling. Contributes nothing.
    Redistribution,
    /// The depth rose past the threshold and came most of the way back. Contributes its
    /// **maximum** rise, not its net: a drift that blew in and blew out still fell as snow.
    SpikeThenSettle,
}

impl EventClass {
    /// The `event_type` column's value, which is also the log field.
    pub const fn as_str(self) -> &'static str {
        match self {
            EventClass::Accumulation => "accumulation",
            EventClass::Plateau => "plateau",
            EventClass::Redistribution => "redistribution",
            EventClass::SpikeThenSettle => "spike_then_settle",
        }
    }

    /// Whether this class carries an accumulation, which the table ties to the column's
    /// nullability.
    pub const fn contributes(self) -> bool {
        matches!(self, EventClass::Accumulation | EventClass::SpikeThenSettle)
    }
}

/// One `snow_totals_cache` row.
///
/// **Inches**, which is the `/snow` contract's unit — not the millimetres `snowdepth`
/// carries on `/latest` (Q-4, and the table's own `COMMENT`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SnowTotals {
    /// `computed_at`: when the `snow:*` components last wrote this row.
    ///
    /// Not part of the frozen body -- `/snow` has no field for it -- and read because it is
    /// the identity of the bytes, which is what **DEV-1**'s additive `ETag` on this
    /// endpoint needs. `source_through` is the other staleness column and is deliberately
    /// **not** read: `/snow` has no recency filter and must not acquire one (fixture
    /// `snow-device-stale`), so a value nothing may act on is a value nothing should carry.
    pub computed_at: chrono::DateTime<chrono::Utc>,
    /// `snowdepth`, inches.
    pub depth: f32,
    /// `snowtoday`.
    pub today: f32,
    /// `snowlast24`.
    pub last24: f32,
    /// `snowlast72`.
    pub last72: f32,
    /// `snowseason`.
    pub season: f32,
    /// `snowstorm`.
    pub storm: f32,
    /// `snowfallrate`.
    pub fall_rate: f32,
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
