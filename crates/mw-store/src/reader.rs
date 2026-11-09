//! The read half of the pool: `window:rollup`'s three reads, `snapshot:warm`'s one, and
//! the frozen REST surface's two.
//!
//! Six methods, and the module doc on [`mw_component::ReadingStore`] has the table of what
//! each one answers and why it cannot be one of the others. `buckets`, `totals` and
//! `series` feed `WindowedView`; `latest` seeds the snapshot after a restart; `span` and
//! `almanac` serve `/span` and `/almanac`, and arrived with the item that wrote their
//! queries. Nothing here is on the `/latest` path -- `api:legacy` is handed
//! `LatestSnapshot` and never this trait, which is what makes that handler's zero queries
//! structural rather than a rule (**D-35**).
//!
//! # The column list is the registry's, and the casts are the reason it is one statement
//!
//! `observations_1m` has 624 aggregate columns in four SQL types — `double precision` for
//! the `_sum`s and the two `_total`s, `bigint` for the `_count`s, `float4` and `int4` for
//! the `_min`s and `_max`s. [`TimescaleReader::buckets`] casts every one of them to
//! `float8` in the `SELECT`, so there is one decode path instead of four and a 624-column
//! read is a loop rather than a 624-branch `match`.
//!
//! **That cast loses nothing and it is worth saying why**, because "cast everything to a
//! double" is usually how precision goes missing. `float4` and `int4` are exactly
//! representable in `float8`; `double precision` is a no-op; and a `bigint` count that is
//! not exactly representable is a count above 2^53 **in one minute**. The arithmetic the
//! consumer then does is in double precision because **D-14** requires it to be: `sum(real)`
//! returns `real`, float4 addition is not associative, and a hierarchy that added in the
//! column's own type was measured wrong in 55 of 56 uneven-bucket pairs.
//!
//! # Why the caller names the level
//!
//! [`TimescaleReader::totals`] answers one sum per [`Piece`], and a piece carries its own
//! [`BucketLevel`]. That looks like it hands the caller a decision the store should make,
//! and it is the opposite: only `window:rollup` knows that a range's ends are bucket-aligned
//! at a given level, because only it knows the station's zone. A local month that begins at
//! `2026-11-01T00:00` in `America/Denver` begins at `07:00Z`, which is on the 1-hour grid
//! and not on the 1-day one — so the exact decomposition is *hour edges around day
//! buckets*, and a store that guessed would guess wrong for `Asia/Kathmandu`, whose offset
//! is `+05:45`.
//!
//! What makes the stitching exact rather than approximate is **D-13**: a 1-day bucket's
//! `rainincremental_total` is the sum of its 24 hourly totals, which are the sums of their
//! 1-minute totals, all accumulated in double precision. Replacing sixty 1-minute buckets
//! with the hour that contains them changes no value. The Go implementation stitches the
//! same three levels (`rainfall_calc.go`, `update_rainfall_summary`) and gets it *nearly*
//! right: `last_5min` is computed as `date_trunc('hour', NOW()) + (minute/5*5) minutes`,
//! so the 5-minute bucket in progress is counted by neither the aggregate term nor the raw
//! term and up to five minutes of rain is dropped from every figure.
//!
//! # No `COALESCE(..., 0)`, anywhere
//!
//! `sum()` over no rows is `NULL`, and this module returns that as `None`. The Go
//! equivalents all write `COALESCE(SUM(period_rain), 0)`, which is what makes an offline
//! gauge report a dry day and a station that has never reported rain report zero rainfall
//! for the year. **D-26** is that an unreported metric is `NULL`, never `0.0`, and the
//! frozen contract's zeros are produced one layer later at the DTO (§1.7), by the one
//! zero-filling function `ci/layers.toml`'s `one-zero-fill` rule permits — in
//! `mw-api-legacy` and nowhere else.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use mw_component::{
    AlmanacQuery, AlmanacRecord, BoxFuture, Bucket, BucketLevel, BucketQuery, Bucketed,
    LatestQuery, Piece, ReadingStore, SeriesQuery, Span, SpanBucket, SpanColumn, SpanQuery,
    StoreError, TotalsQuery,
};
use mw_types::{
    Agg, AggregateColumn, MetricKey, MetricValue, Reading, SqlType, Strategy, LEVELS, METRICS,
};
use sqlx::{PgPool, Row};

/// The widest bucket read this module will issue, in rows.
///
/// Not a tuning knob: it is the bound that stops a daemon which has been down for a week
/// asking for a week of buckets in one statement. `window:rollup` refreshes incrementally
/// and asks for an hour at most, so this is only ever reached by a caller that has
/// forgotten its own ring size — and a `Vec` sized by how long the process was stopped is
/// the shape that turns an outage into an allocation failure.
const MAX_ROWS: usize = 10_000;

/// How many closed-but-unmaterialised buckets `span` will aggregate from raw.
///
/// **A bound on the raw scan, not a tuning knob.** The buckets it covers are the ones a
/// refresh policy has not reached yet, and the slowest policy in `migrations/` runs every
/// **thirty minutes** (`observations_1d`; the others are one and five). Four buckets is
/// therefore more than any level needs: two hours at the `1h` level and four days at `1d`.
///
/// The bound matters because the unbounded case is real and is the scan the continuous
/// aggregates exist to prevent -- a station with no materialised rows at all over a
/// 720-hour window would otherwise aggregate a month of raw rows per request. Past the
/// bound a bucket stays absent until its policy runs, which is where this started; the
/// difference is that it is now absent for seconds at the levels anyone watches rather than
/// for half an hour at the level nobody notices.
const MAX_BACKFILL_BUCKETS: i32 = 4;

/// The raw hypertable, which [`TimescaleReader::latest`] and `span`'s trailing buckets are
/// the only reads of.
const OBSERVATIONS: &str = "observations";

/// The four stored dimension columns of `observations`, in the order the writer declares
/// them -- so a `SELECT` built here and the `INSERT` built there name the same columns in
/// the same order.
///
/// Duplicated from `crate::writer` rather than shared, and the duplication is the point:
/// the writer *asserts* this list against `mw_types::DIMENSIONS` at build time and refuses
/// to construct if they disagree, which is the check that keeps both copies honest. Sharing
/// one constant would make the assertion cover a list nobody could get wrong.
const DIMENSION_COLUMNS: [&str; 4] = ["time", "station_id", "stationname", "stationtype"];

/// The read half of the pool (W3 §2a.1's `ReadingStore`).
pub struct TimescaleReader {
    pool: PgPool,
    /// Built once, because it is 624 cast column expressions.
    buckets: String,
    /// Built once, because it is 216 column names.
    latest: String,
}

impl std::fmt::Debug for TimescaleReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimescaleReader")
            .field("bucket_columns", &minute_columns().len())
            .finish_non_exhaustive()
    }
}

impl TimescaleReader {
    /// The reader over one pool.
    ///
    /// Infallible: everything it needs is in `mw_types::LEVELS`, which is generated, and a
    /// level the registry does not have is a compile error in [`BucketLevel::as_str`]'s
    /// match rather than a runtime disagreement. The per-metric check — "does this metric
    /// declare a total at this level" — is per call, because it is a property of the
    /// *request* and not of the build.
    pub fn new(pool: PgPool) -> Self {
        Self {
            buckets: buckets_statement(),
            latest: latest_statement(),
            pool,
        }
    }

    /// How many columns one bucket carries, for the log line `db:postgres` prints.
    pub fn bucket_columns(&self) -> usize {
        minute_columns().len()
    }
}

impl ReadingStore for TimescaleReader {
    fn buckets<'a>(&'a self, q: BucketQuery) -> BoxFuture<'a, Result<Vec<Bucket>, StoreError>> {
        Box::pin(async move {
            let limit = i64::try_from(q.limit.min(MAX_ROWS)).unwrap_or(i64::from(i32::MAX));
            let rows = sqlx::query(&self.buckets)
                .bind(q.station.as_str())
                .bind(q.since)
                .bind(limit)
                .fetch_all(&self.pool)
                .await
                .map_err(classify)?;
            let width = minute_columns().len();
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let bucket: DateTime<Utc> = row.try_get(0).map_err(StoreError::rejected)?;
                let mut values = Vec::with_capacity(width);
                for i in 0..width {
                    // The `SELECT` casts every column to float8, so one decode covers all
                    // four of the level's SQL types -- see the module documentation for
                    // why that is lossless.
                    values.push(
                        row.try_get::<Option<f64>, _>(i + 1)
                            .map_err(StoreError::rejected)?,
                    );
                }
                out.push(Bucket { bucket, values });
            }
            Ok(out)
        })
    }

    fn totals<'a>(&'a self, q: TotalsQuery) -> BoxFuture<'a, Result<Vec<Option<f64>>, StoreError>> {
        Box::pin(async move {
            // One answer per piece, in the order asked. Pieces are grouped by level so
            // that one statement serves every piece at that level; the index each piece
            // keeps is how its answer finds its way back.
            let mut answers: Vec<Option<f64>> = vec![None; q.pieces.len()];
            for level in BucketLevel::ALL {
                let at_level: Vec<(usize, &Piece)> = q
                    .pieces
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.level == *level)
                    .collect();
                if at_level.is_empty() {
                    continue;
                }
                let column = total_column(q.metric, *level)?;
                let (sql, lower, upper) = totals_statement(*level, column, &at_level);
                let mut query = sqlx::query(&sql)
                    .bind(q.station.as_str())
                    .bind(lower)
                    .bind(upper);
                for (_, piece) in &at_level {
                    query = query.bind(piece.from).bind(piece.until);
                }
                let row = query.fetch_one(&self.pool).await.map_err(classify)?;
                for (slot, (index, _)) in at_level.iter().enumerate() {
                    answers[*index] = row.try_get(slot).map_err(StoreError::rejected)?;
                }
            }
            Ok(answers)
        })
    }

    fn latest<'a>(&'a self, q: LatestQuery) -> BoxFuture<'a, Result<Option<Reading>, StoreError>> {
        Box::pin(async move {
            let Some(row) = sqlx::query(&self.latest)
                .bind(q.station.as_str())
                .bind(q.since)
                .fetch_optional(&self.pool)
                .await
                .map_err(classify)?
            else {
                // A station with nothing in the window. `snapshot:warm` leaves its key
                // unpopulated and `/latest` 404s until a reading arrives, which is the
                // answer the contract already has bytes for.
                return Ok(None);
            };
            let mut reading = Reading {
                time: row.try_get(0).map_err(StoreError::rejected)?,
                station_id: row.try_get(1).map_err(StoreError::rejected)?,
                station_name: row.try_get(2).map_err(StoreError::rejected)?,
                station_type: row.try_get(3).map_err(StoreError::rejected)?,
                ..Reading::default()
            };
            for (i, meta) in METRICS.iter().enumerate() {
                let at = DIMENSION_COLUMNS.len() + i;
                // Decoded in the column's own type and placed through the generated
                // setter, which is `Reading::value`'s inverse over the same list -- so a
                // metric added to `metrics/registry.toml` is read back with no edit here.
                // A NULL column is a `set` that never happens, and the field stays `None`
                // (**D-26**).
                let placed = match meta.sql_type {
                    SqlType::Float4 => row
                        .try_get::<Option<f32>, _>(at)
                        .map_err(StoreError::rejected)?
                        .map(|v| reading.set(meta.key, MetricValue::Float(v))),
                    SqlType::Int4 => row
                        .try_get::<Option<i32>, _>(at)
                        .map_err(StoreError::rejected)?
                        .map(|v| reading.set(meta.key, MetricValue::Int(v))),
                    SqlType::Int8 => row
                        .try_get::<Option<i64>, _>(at)
                        .map_err(StoreError::rejected)?
                        .map(|v| reading.set(meta.key, MetricValue::BigInt(v))),
                    SqlType::Text => row
                        .try_get::<Option<String>, _>(at)
                        .map_err(StoreError::rejected)?
                        .map(|v| reading.set(meta.key, MetricValue::Text(&v))),
                    SqlType::Timestamptz => row
                        .try_get::<Option<DateTime<Utc>>, _>(at)
                        .map_err(StoreError::rejected)?
                        .map(|v| reading.set(meta.key, MetricValue::Time(v))),
                };
                if placed == Some(false) {
                    // Unreachable while this match and the generator agree, and named
                    // rather than ignored because the failure it guards is a metric that
                    // silently reads back as NULL for one SQL type.
                    return Err(StoreError::rejected(std::io::Error::other(format!(
                        "`{}` decoded as {} and `Reading::set` would not take that type. \
                         The generated setter and this decode disagree about the column's \
                         type, which means `metrics/registry.toml` changed and \
                         `cargo xtask codegen` has not been re-run.",
                        meta.column,
                        meta.sql_type.ddl(),
                    ))));
                }
            }
            Ok(Some(reading))
        })
    }

    fn series<'a>(&'a self, q: SeriesQuery) -> BoxFuture<'a, Result<Vec<Bucketed>, StoreError>> {
        Box::pin(async move {
            let column = total_column(q.metric, q.level)?;
            let sql = format!(
                "SELECT bucket, {column} FROM {view} \
                 WHERE station_id = $1 AND bucket >= $2 ORDER BY bucket LIMIT $3",
                column = column.column,
                view = view(q.level),
            );
            let limit = i64::try_from(q.limit.min(MAX_ROWS)).unwrap_or(i64::from(i32::MAX));
            let rows = sqlx::query(&sql)
                .bind(q.station.as_str())
                .bind(q.since)
                .bind(limit)
                .fetch_all(&self.pool)
                .await
                .map_err(classify)?;
            rows.into_iter()
                .map(|row| {
                    Ok(Bucketed {
                        bucket: row.try_get(0).map_err(StoreError::rejected)?,
                        total: row.try_get(1).map_err(StoreError::rejected)?,
                    })
                })
                .collect()
        })
    }

    fn span<'a>(&'a self, q: SpanQuery) -> BoxFuture<'a, Result<Span, StoreError>> {
        Box::pin(async move {
            // Resolved before either statement runs, so a column the level does not
            // declare is one error naming the column rather than two half-run queries.
            let resolved = q
                .columns
                .iter()
                .map(|c| resolve_span_column(q.level, *c))
                .collect::<Result<Vec<_>, _>>()?;

            // The materialised prefix. Empty by construction when the caller passed
            // `since == tail_start`, which is how **D-54**'s 60-second cache hit asks for
            // the tail alone (`SpanQuery`'s own documentation).
            let mut prefix = Vec::new();
            if q.since < q.tail_start {
                let sql = prefix_statement(q.level, &resolved);
                let limit = i64::try_from(q.limit.min(MAX_ROWS)).unwrap_or(i64::from(i32::MAX));
                let rows = sqlx::query(&sql)
                    .bind(q.station.as_str())
                    .bind(q.since)
                    .bind(q.tail_start)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(classify)?;
                prefix.reserve(rows.len());
                for row in rows {
                    prefix.push(SpanBucket {
                        bucket: row.try_get(0).map_err(StoreError::rejected)?,
                        values: read_values(&row, 1, resolved.len())?,
                    });
                }
            }

            // From raw: every bucket after the materialised prefix, bounded above by the
            // trailing bucket's own extent rather than left open -- a station whose clock is
            // ahead of this one would otherwise pull a reading from the *next* bucket into
            // this one, and a frozen response is not the place to discover a clock skew.
            let width = chrono::Duration::from_std(q.level.span())
                .map_err(|e| StoreError::rejected(std::io::Error::other(e.to_string())))?;
            let tail_end = q.tail_start + width;

            // **Where the raw aggregation starts, and why it is not always `tail_start`.**
            //
            // Every level is `materialized_only = true` and each refresh policy runs on a
            // schedule, so a bucket that has closed may not be in the view yet: the prefix's
            // range covers it and the view has no row for it. Reading the tail from
            // `tail_start` alone put that bucket in neither half, and `/span/720h` dropped
            // yesterday for the first half hour of every UTC day.
            //
            // So the boundary comes from the DATA rather than the clock: the first bucket
            // after the newest one the prefix actually holds. Bounded by
            // `MAX_BACKFILL_BUCKETS`, because the unbounded case -- a station with no
            // materialised rows at all over a 720-hour window -- is the raw scan the
            // aggregates exist to avoid, and no refresh policy here is more than thirty
            // minutes behind.
            let from = if q.since >= q.tail_start {
                // The caller holds a cached prefix and asked for the rest. The cache only
                // stores a prefix that ends at `tail_start - width` (`mw_api_legacy::span`),
                // so there is nothing between it and the tail.
                q.tail_start
            } else {
                let after_prefix = prefix.last().map(|b| b.bucket + width);
                let floor = q.tail_start - width * MAX_BACKFILL_BUCKETS;
                after_prefix
                    .unwrap_or(q.since)
                    .max(floor.max(q.since))
                    .min(q.tail_start)
            };

            let sql = tail_statement(&resolved, q.level);
            let rows = sqlx::query(&sql)
                .bind(q.station.as_str())
                .bind(from)
                .bind(tail_end)
                .fetch_all(&self.pool)
                .await
                .map_err(classify)?;

            // Column 0 is the bucket and column 1 is `count(*)`: an aggregate over no rows
            // still returns one row with every value NULL -- indistinguishable from a bucket
            // whose every metric was unreported. The frozen contract needs the difference,
            // because an empty bucket must not appear in the array at all. With a
            // `GROUP BY` there is no row for an empty bucket, so the count is belt and
            // braces rather than the only guard it was.
            let mut backfill = Vec::new();
            let mut tail = None;
            for row in rows {
                let bucket: chrono::DateTime<chrono::Utc> =
                    row.try_get(0).map_err(StoreError::rejected)?;
                let count: i64 = row.try_get(1).map_err(StoreError::rejected)?;
                if count == 0 {
                    continue;
                }
                let values = read_values(&row, 2, resolved.len())?;
                if bucket >= q.tail_start {
                    tail = Some(SpanBucket { bucket, values });
                } else {
                    backfill.push(SpanBucket { bucket, values });
                }
            }

            Ok(Span {
                prefix,
                backfill,
                tail,
            })
        })
    }

    fn almanac<'a>(
        &'a self,
        q: AlmanacQuery,
    ) -> BoxFuture<'a, Result<Vec<AlmanacRecord>, StoreError>> {
        Box::pin(async move {
            let rows = sqlx::query(
                "SELECT record, value, observed_at, wind_dir FROM almanac_cache \
                 WHERE station_id = $1",
            )
            .bind(q.station.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(classify)?;
            rows.into_iter()
                .map(|row| {
                    Ok(AlmanacRecord {
                        record: row.try_get(0).map_err(StoreError::rejected)?,
                        value: row.try_get(1).map_err(StoreError::rejected)?,
                        observed_at: row.try_get(2).map_err(StoreError::rejected)?,
                        wind_dir: row.try_get(3).map_err(StoreError::rejected)?,
                    })
                })
                .collect()
        })
    }
}

/// `n` consecutive `float8` columns from `at`, as the values of one [`SpanBucket`].
///
/// One decode path for every strategy, which is [`buckets_statement`]'s property for
/// [`prefix_statement`]'s reason: both statements cast, and the module documentation has
/// why that is lossless.
fn read_values(
    row: &sqlx::postgres::PgRow,
    at: usize,
    n: usize,
) -> Result<Vec<Option<f64>>, StoreError> {
    let mut values = Vec::with_capacity(n);
    for i in 0..n {
        values.push(
            row.try_get::<Option<f64>, _>(at + i)
                .map_err(StoreError::rejected)?,
        );
    }
    Ok(values)
}

/// One `/span` column, as the two expressions that compute it.
///
/// Two, because the prefix and the tail are read from different relations with different
/// contents: the view holds composable state and is finalised by `mw_mean` /
/// `mw_circular_mean`, and the raw table holds values and is aggregated *into* that state
/// and finalised in the same expression. Resolving both together is what keeps them from
/// drifting -- a column that is a mean on one side and a maximum on the other would produce
/// a response whose last element means something different from the rest of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpanExpr {
    /// Over `LEVELS[level].view`.
    prefix: String,
    /// Over `observations`, grouped into one bucket by the statement's `WHERE`.
    tail: String,
}

/// Which registry strategy an [`Agg`] reads.
///
/// `Agg::Sum` maps onto the registry's `total` -- the same strategy under two names
/// (**D-50**), kept because `Sum` is what a protocol specification says and `total` is what
/// the column is called.
const fn strategy_of(agg: Agg) -> Strategy {
    match agg {
        Agg::Mean => Strategy::Mean,
        Agg::Min => Strategy::Min,
        Agg::Max => Strategy::Max,
        Agg::Sum => Strategy::Total,
        Agg::CircularMean => Strategy::CircularMean,
    }
}

/// Resolve one `(metric, agg)` against a level, into the prefix and tail expressions.
///
/// The aggregate column names are matched by **suffix** rather than by position within the
/// metric's group. Both are a coupling to the generator; this one fails loudly and names
/// what is missing, where a positional read of a two-column group would silently swap a
/// mean's numerator and denominator if the generator ever emitted them the other way round.
/// `ci/db/schema-assertions.sql` A-1 and A-2 pin the same four suffixes against a real
/// server, so the coupling is checked at both ends.
fn resolve_span_column(level: BucketLevel, column: SpanColumn) -> Result<SpanExpr, StoreError> {
    let (metric, agg) = column;
    let strategy = strategy_of(agg);
    let declared: Vec<&AggregateColumn> = LEVELS
        .iter()
        .find(|l| l.name == level.as_str())
        .map(|l| l.columns)
        .unwrap_or(&[])
        .iter()
        .filter(|c| c.metric == metric && c.strategy == strategy)
        .collect();
    let find = |suffix: &str| -> Result<&'static str, StoreError> {
        declared
            .iter()
            .map(|c| c.column)
            .find(|name| name.ends_with(suffix))
            .ok_or_else(|| missing_column(metric, agg, level, suffix))
    };
    // The raw column, for the tail. Every metric has exactly one, and a metric whose key
    // is not in `METRICS` cannot be constructed -- `MetricKey` is generated.
    let raw = METRICS
        .iter()
        .find(|m| m.key == metric)
        .map(|m| m.column)
        .ok_or_else(|| missing_column(metric, agg, level, "a raw column"))?;
    Ok(match strategy {
        Strategy::Mean => SpanExpr {
            prefix: format!("mw_mean({}, {})", find("_sum")?, find("_count")?),
            tail: format!("mw_mean(sum({raw}::double precision), nullif(count({raw}), 0))"),
        },
        Strategy::Min => SpanExpr {
            prefix: format!("{}::float8", find("_min")?),
            tail: format!("min({raw})::float8"),
        },
        Strategy::Max => SpanExpr {
            prefix: format!("{}::float8", find("_max")?),
            tail: format!("max({raw})::float8"),
        },
        Strategy::Total => SpanExpr {
            prefix: format!("{}::float8", find("_total")?),
            tail: format!("sum({raw}::double precision)"),
        },
        Strategy::CircularMean => SpanExpr {
            prefix: format!(
                "mw_circular_mean({}, {}, {})",
                find("_sin_sum")?,
                find("_cos_sum")?,
                find("_dir_count")?
            ),
            tail: format!(
                "mw_circular_mean(sum(sind({raw}::double precision)), \
                 sum(cosd({raw}::double precision)), nullif(count({raw}), 0))"
            ),
        },
    })
}

/// The error a `(metric, agg)` the registry does not carry at this level produces.
///
/// `Fatal` for [`total_column`]'s reason: it is a property of the request, and the next
/// call asks the same thing.
fn missing_column(metric: MetricKey, agg: Agg, level: BucketLevel, suffix: &str) -> StoreError {
    StoreError::rejected(std::io::Error::other(format!(
        "metrics/registry.toml declares no `{suffix}` column for {metric:?} with \
         {agg:?} at the {level} level, so there is nothing to read. Add the strategy to \
         that metric's `aggregates` and re-run `cargo xtask codegen`, or ask for one it \
         already declares."
    )))
}

/// `SELECT bucket, <finalised columns> FROM <view> WHERE station_id = $1 AND bucket >= $2
/// AND bucket < $3 ORDER BY bucket LIMIT $4`.
///
/// `bucket < $3` is **D-54**'s boundary: the prefix stops where the still-filling bucket
/// starts, so the materialised half of the answer is a function of `(since, tail_start)`
/// alone and is therefore cacheable for as long as the caller's TTL says.
///
/// The `LIMIT` orders **ascending**, so a truncated answer loses the newest buckets rather
/// than the oldest. That is the wrong half to lose for this caller and it is a bound it
/// cannot reach: the widest band the frozen endpoint serves is just under 48 hours of
/// 5-minute buckets, 576 rows, against a [`MAX_ROWS`] of 10,000. The bound is here for the
/// reason [`BucketQuery::limit`]'s is -- a `Vec` sized by a caller's arithmetic mistake.
fn prefix_statement(level: BucketLevel, columns: &[SpanExpr]) -> String {
    let mut sql = String::with_capacity(96 + columns.len() * 64);
    sql.push_str("SELECT bucket");
    for c in columns {
        let _ = write!(sql, ", {}", c.prefix);
    }
    let _ = write!(
        sql,
        " FROM {view} WHERE station_id = $1 AND bucket >= $2 AND bucket < $3 \
         ORDER BY bucket LIMIT $4",
        view = view(level),
    );
    sql
}

/// `SELECT count(*), <aggregated-and-finalised columns> FROM observations WHERE
/// station_id = $1 AND time >= $2 AND time < $3`.
///
/// No `GROUP BY`: the `WHERE` **is** the bucket, one bucket wide, so the aggregate is over
/// the whole result and returns exactly one row. `count(*)` is what distinguishes that one
/// row from a bucket that really has no readings in it -- see [`ReadingStore::span`].
///
/// This is the expression set the 1-minute level's own view computes from raw (the
/// generated SQL for it, which `ci/layers.toml` forbids this crate from naming by path),
/// which is not a coincidence and is the only shape that can be right: the tail has to be
/// the bucket the 1-minute level *would* have materialised had it refreshed, and a
/// different expression here would make the last element of a `/span` response discontinuous
/// with the ones before it. `ci/db/measure/35-trailing-bucket.sql` measured this statement's
/// cost at 0.44-6.89 ms, which is what **D-54** cites.
fn tail_statement(columns: &[SpanExpr], level: BucketLevel) -> String {
    let mut sql = String::with_capacity(128 + columns.len() * 96);
    // `time_bucket` with the level's own width, so the raw rows are grouped onto exactly the
    // grid the view uses -- which is what makes a backfilled bucket identical to the one the
    // policy will later materialise. The width is a literal from `BucketLevel` and not from
    // the request.
    let _ = write!(
        sql,
        "SELECT time_bucket(INTERVAL '{}', time) AS bucket, count(*)",
        bucket_interval(level)
    );
    for c in columns {
        let _ = write!(sql, ", {}", c.tail);
    }
    let _ = write!(
        sql,
        " FROM {OBSERVATIONS} WHERE station_id = $1 AND time >= $2 AND time < $3 \
         GROUP BY bucket ORDER BY bucket"
    );
    sql
}

/// The level's bucket width as a PostgreSQL interval literal.
///
/// A `match` rather than a formatted `Duration`, so the string is a constant the statement
/// builder cannot get wrong and `clippy` can see is exhaustive.
const fn bucket_interval(level: BucketLevel) -> &'static str {
    match level {
        BucketLevel::Minute => "1 minute",
        BucketLevel::FiveMinute => "5 minutes",
        BucketLevel::Hour => "1 hour",
        BucketLevel::Day => "1 day",
    }
}

/// The 1-minute level's declared columns, in the order a [`Bucket`]'s values are in.
///
/// A function rather than a constant because `mw_types::LEVELS` is a static array and the
/// level is found by name: `BucketLevel::Minute.as_str()` is `"1m"`, which is the registry's
/// own key. The lookup cannot fail — a generated level set without `1m` would not have
/// generated the view either — and the `expect` says which invariant it is asserting rather
/// than being a shrug.
fn minute_columns() -> &'static [AggregateColumn] {
    LEVELS
        .iter()
        .find(|l| l.name == BucketLevel::Minute.as_str())
        .map(|l| l.columns)
        .expect("metrics/registry.toml declares the 1-minute level; the generator emits it first")
}

/// The view one level is read from.
fn view(level: BucketLevel) -> &'static str {
    LEVELS
        .iter()
        .find(|l| l.name == level.as_str())
        .map(|l| l.view)
        .expect("every BucketLevel is one of metrics/registry.toml's four levels")
}

/// The `total`-strategy column one metric declares at one level.
///
/// `Err` rather than a panic, because this is a property of the **request**: an output that
/// declares `(OutTemp, Day, Sum)` has asked for a total of a metric the registry gives
/// `mean, min, max`, and that is a declaration to fix rather than a build that is broken.
/// `Fatal`, because the next call asks the same thing.
fn total_column(
    metric: MetricKey,
    level: BucketLevel,
) -> Result<&'static AggregateColumn, StoreError> {
    LEVELS
        .iter()
        .find(|l| l.name == level.as_str())
        .and_then(|l| {
            l.columns
                .iter()
                .find(|c| c.metric == metric && c.strategy == Strategy::Total)
        })
        .ok_or_else(|| {
            StoreError::rejected(std::io::Error::other(format!(
                "metrics/registry.toml does not declare a `total` for {metric:?} at the \
                 {level} level, so there is no column to sum. Add `total` to that metric's \
                 `aggregates` and re-run `cargo xtask codegen`, or ask for a strategy it \
                 already declares."
            )))
        })
}

/// `SELECT <216 columns> FROM observations WHERE station_id = $1 AND time >= $2 ORDER BY
/// time DESC LIMIT 1`.
///
/// No casts, unlike [`buckets_statement`]: the aggregate read wants one decode path over
/// four SQL types because it indexes 624 columns positionally, and this one wants the
/// column's own type because it is rebuilding a [`Reading`] whose fields are those types.
/// A cast to `float8` here would turn the eleven `text` metrics and the four
/// `timestamptz` ones into NULL.
///
/// `ORDER BY time DESC LIMIT 1` with the `time >= $2` bound is an index seek: `observations`
/// is a hypertable partitioned on `time` with `(station_id, time)` unique, so the planner
/// reads the newest chunk the bound allows and stops. Without the bound it would be the
/// same seek on a station that is reporting and a scan back through every chunk on one that
/// is not -- which is exactly the station `snapshot:warm` asks about.
fn latest_statement() -> String {
    let mut sql = String::with_capacity(64 + (DIMENSION_COLUMNS.len() + METRICS.len()) * 20);
    sql.push_str("SELECT ");
    for (i, column) in DIMENSION_COLUMNS
        .iter()
        .copied()
        .chain(METRICS.iter().map(|m| m.column))
        .enumerate()
    {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(column);
    }
    let _ = write!(
        sql,
        " FROM {OBSERVATIONS} WHERE station_id = $1 AND time >= $2 ORDER BY time DESC LIMIT 1"
    );
    sql
}

/// `SELECT bucket, <624 cast columns> FROM observations_1m WHERE ... ORDER BY bucket LIMIT`.
///
/// Built once per reader. The `ORDER BY bucket` is ascending and the `LIMIT` therefore
/// takes the **oldest** rows, which is correct for an incremental refresh — the caller asks
/// for everything after the newest bucket it holds, so the oldest of those is the next one
/// it needs and a truncated answer leaves no hole.
fn buckets_statement() -> String {
    let columns = minute_columns();
    // 624 columns at ~40 characters of `name::float8 AS name`.
    let mut sql = String::with_capacity(64 + columns.len() * 48);
    sql.push_str("SELECT bucket");
    for c in columns {
        // The alias is the column's own name, so a row can also be read by name when a
        // test wants to be explicit about which column it is asserting.
        let _ = write!(sql, ", {name}::float8 AS {name}", name = c.column);
    }
    let _ = write!(
        sql,
        " FROM {view} WHERE station_id = $1 AND bucket >= $2 ORDER BY bucket LIMIT $3",
        view = view(BucketLevel::Minute),
    );
    sql
}

/// One statement summing `column` over each piece, plus the bounds that let the scan stop.
///
/// Returns the SQL and the `(lower, upper)` envelope of every piece, which is bound as
/// `$2`/`$3`: the `FILTER`s decide which rows land in which sum, and the `WHERE` is what
/// stops the planner reading the whole hypertable to apply them. Without it a one-hour
/// piece at the 1-day level would scan every chunk the station has.
fn totals_statement(
    level: BucketLevel,
    column: &AggregateColumn,
    pieces: &[(usize, &Piece)],
) -> (String, DateTime<Utc>, DateTime<Utc>) {
    let lower = pieces
        .iter()
        .map(|(_, p)| p.from)
        .min()
        .unwrap_or_else(Utc::now);
    let upper = pieces
        .iter()
        .map(|(_, p)| p.until)
        .max()
        .unwrap_or_else(Utc::now);
    let mut sql = String::with_capacity(64 + pieces.len() * 96);
    sql.push_str("SELECT ");
    for (slot, _) in pieces.iter().enumerate() {
        if slot > 0 {
            sql.push_str(", ");
        }
        // $4/$5 for the first piece, $6/$7 for the second, and so on -- $1 is the station
        // and $2/$3 are the envelope.
        let from = 4 + slot * 2;
        let _ = write!(
            sql,
            "sum({column}) FILTER (WHERE bucket >= ${from} AND bucket < ${until}) AS p{slot}",
            column = column.column,
            until = from + 1,
        );
    }
    let _ = write!(
        sql,
        " FROM {view} WHERE station_id = $1 AND bucket >= $2 AND bucket < $3",
        view = view(level),
    );
    (sql, lower, upper)
}

/// A `sqlx` failure, classified the way [`StoreError`]'s own documentation requires.
///
/// The same split the writer makes, and for the same reason: a connection that died is a
/// read that succeeds on the next tick, and a statement the server rejected is a build that
/// disagrees with its schema and will be rejected identically forever. `window:rollup`
/// keeps its last good state either way (W3 §2a.6) — what the class buys is the log line.
pub(crate) fn classify(e: sqlx::Error) -> StoreError {
    match &e {
        sqlx::Error::Database(_)
        | sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnDecode { .. } => StoreError::rejected(e),
        _ => StoreError::unavailable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::BucketLevel;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    /// **The `latest` statement's column order is the decode's index arithmetic**, and
    /// without a database nothing else checks that they agree.
    ///
    /// `ReadingStore::latest`'s loop reads column `DIMENSION_COLUMNS.len() + i` for
    /// `METRICS[i]`, so a statement that dropped a column, reordered one, or grew a
    /// sixth dimension would shift every metric after the change — and decode a `float4`
    /// out of an `int4` column, or a timestamp out of a text one. The only other coverage
    /// is the `#[ignore]`d database test, which does not run in an ordinary
    /// `cargo test`. This is the same assertion
    /// [`the_bucket_statement_is_the_registrys_column_list_in_order`] makes ten lines down,
    /// for the same reason.
    #[test]
    fn the_latest_statement_is_the_dimensions_then_the_registrys_metrics_in_order() {
        let sql = latest_statement();
        assert!(
            sql.starts_with("SELECT time, station_id, stationname, stationtype, "),
            "the four dimensions come first, in the writer's order: {}",
            &sql[..80]
        );
        // Every metric column, in `METRICS` order, each one after the last.
        let mut cursor = 0usize;
        for m in METRICS {
            let needle = format!(", {}", m.column);
            let found = sql[cursor..]
                .find(&needle)
                .unwrap_or_else(|| panic!("{} is missing or out of order", m.column));
            cursor += found + needle.len();
        }
        // Exactly as many comma-separated selections as declared columns, so nothing rode
        // along: a `SELECT *` or a stray column would shift the decode's indices.
        let selections = sql
            .trim_start_matches("SELECT ")
            .split(" FROM ")
            .next()
            .expect("a FROM clause")
            .split(',')
            .count();
        assert_eq!(
            selections,
            DIMENSION_COLUMNS.len() + METRICS.len(),
            "216 columns today, and the number is the registry's rather than a literal"
        );
        // **No casts**, unlike the bucket statement: a `::float8` here would decode the
        // eleven text and four timestamptz metrics as NULL.
        assert!(!sql.contains("::"), "{sql}");
        // The bound and the ordering are what make this an index seek rather than a scan
        // back through every chunk a non-reporting station has.
        assert!(
            sql.contains("WHERE station_id = $1 AND time >= $2"),
            "{sql}"
        );
        assert!(sql.ends_with("ORDER BY time DESC LIMIT 1"), "{sql}");
    }

    /// The dimension list this module decodes by position is the one the writer asserts
    /// against the registry.
    ///
    /// `crate::writer` refuses to construct if `DIMENSIONS`' stored set and its own
    /// `DIMENSION_COLUMNS` disagree, and `TimescaleStore::new` always builds both halves —
    /// so that refusal gates this module too. It gates it through a link that is not
    /// visible from here, though, and `TimescaleReader::new` alone is infallible, so the
    /// agreement is asserted here as well rather than inherited silently.
    #[test]
    fn the_dimension_columns_are_the_registrys_stored_set() {
        let declared: Vec<&str> = mw_types::DIMENSIONS
            .iter()
            .filter(|d| d.stored)
            .map(|d| d.column)
            .collect();
        assert_eq!(declared, DIMENSION_COLUMNS);
    }

    /// The bucket statement names **every** column the 1-minute level declares, in order,
    /// and nothing else. That is the property a consumer's indexing depends on: `Bucket`'s
    /// `values[i]` is `LEVELS[0].columns[i]`, and a statement that skipped a column or
    /// reordered one would silently shift every value after it.
    #[test]
    fn the_bucket_statement_is_the_registrys_column_list_in_order() {
        let sql = buckets_statement();
        let columns = minute_columns();
        assert!(sql.starts_with("SELECT bucket, "), "{}", &sql[..40]);
        let mut cursor = 0usize;
        for c in columns {
            let needle = format!(", {name}::float8 AS {name}", name = c.column);
            let found = sql[cursor..]
                .find(&needle)
                .unwrap_or_else(|| panic!("{} is missing or out of order", c.column));
            cursor += found + needle.len();
        }
        // Exactly as many cast expressions as declared columns, so nothing extra rode
        // along -- a `SELECT *` would pass the loop above and fail this.
        assert_eq!(sql.matches("::float8 AS ").count(), columns.len());
        assert!(sql.ends_with(
            "FROM observations_1m WHERE station_id = $1 AND bucket >= $2 ORDER BY bucket LIMIT $3"
        ));
    }

    /// The `FILTER` placeholders are allocated from `$4` upwards in piece order, because
    /// `$1` is the station and `$2`/`$3` are the envelope the scan is bounded by. Off by
    /// one here would bind one piece's range to its neighbour's sum, which is a wrong
    /// number rather than an error.
    #[test]
    fn each_piece_gets_its_own_pair_of_placeholders_and_the_scan_is_bounded() {
        let column = total_column(MetricKey::RainIncremental, BucketLevel::Minute)
            .expect("rain_incremental declares a total");
        let a = Piece {
            level: BucketLevel::Minute,
            from: at("2026-11-06T07:00:00Z"),
            until: at("2026-11-06T15:00:00Z"),
        };
        let b = Piece {
            level: BucketLevel::Minute,
            from: at("2026-11-05T15:00:00Z"),
            until: at("2026-11-06T15:00:00Z"),
        };
        let (sql, lower, upper) =
            totals_statement(BucketLevel::Minute, column, &[(3, &a), (7, &b)]);
        assert_eq!(
            sql,
            "SELECT sum(rainincremental_total) FILTER (WHERE bucket >= $4 AND bucket < $5) AS p0, \
             sum(rainincremental_total) FILTER (WHERE bucket >= $6 AND bucket < $7) AS p1 \
             FROM observations_1m WHERE station_id = $1 AND bucket >= $2 AND bucket < $3"
        );
        assert_eq!(lower, b.from, "the envelope is the widest piece's start");
        assert_eq!(upper, a.until);
        assert!(
            !sql.contains("COALESCE"),
            "a window with no buckets is NULL, not zero (D-26)"
        );
    }

    /// The level a piece names is the view it reads, for all four — so a day-level piece
    /// cannot accidentally be answered from the minute view with the same column name.
    #[test]
    fn every_level_reads_its_own_view() {
        let views: Vec<&str> = BucketLevel::ALL.iter().copied().map(view).collect();
        assert_eq!(
            views,
            vec![
                "observations_1m",
                "observations_5m",
                "observations_1h",
                "observations_1d"
            ]
        );
        assert_eq!(BucketLevel::Day.span().as_secs(), 86_400);
    }

    /// Asking for a total of a metric the registry gives `mean, min, max` names the metric,
    /// the level and the fix. It is `Fatal`, because the declaration is what is wrong.
    #[test]
    fn a_metric_with_no_declared_total_is_a_named_refusal() {
        use mw_types::{Class, Classify};
        let e = total_column(MetricKey::OutTemp, BucketLevel::Minute)
            .expect_err("out_temp declares mean, min and max and no total");
        let chain = mw_types::DisplayChain(&e).to_string();
        assert!(chain.contains("OutTemp"), "{chain}");
        assert!(chain.contains("1m"), "{chain}");
        assert!(chain.contains("cargo xtask codegen"), "{chain}");
        assert_eq!(e.class(), Class::Fatal);

        // And the one metric this item actually sums is declared at every level, which is
        // what makes the local-month stitch across three of them possible at all.
        for level in BucketLevel::ALL {
            let c = total_column(MetricKey::RainIncremental, *level)
                .unwrap_or_else(|e| panic!("rain_incremental at {level}: {e}"));
            assert_eq!(c.column, "rainincremental_total");
            assert_eq!(c.strategy, Strategy::Total);
        }
    }

    /// `/span`'s four strategies resolve to the finaliser calls the generated views are
    /// built to be read with, and the tail's expressions are the ones the 1-minute level
    /// computes from raw.
    ///
    /// Asserted as strings because that is what is actually at risk: the SQL is assembled
    /// here and executed against a server the unit tests do not have, so a `_sum` paired
    /// with the wrong `_count` would be caught by nothing until `gate conformance`.
    /// `ci/db/schema-assertions.sql` A-1 and A-2 are the other end of the same check.
    #[test]
    fn a_span_column_resolves_to_the_levels_finaliser_and_the_raw_aggregate() {
        let mean = resolve_span_column(BucketLevel::FiveMinute, (MetricKey::OutTemp, Agg::Mean))
            .expect("out_temp declares a mean at every level");
        assert_eq!(mean.prefix, "mw_mean(outtemp_sum, outtemp_count)");
        assert_eq!(
            mean.tail,
            "mw_mean(sum(outtemp::double precision), nullif(count(outtemp), 0))"
        );

        let dir = resolve_span_column(BucketLevel::Hour, (MetricKey::WindDir, Agg::CircularMean))
            .expect("wind_dir declares a circular mean");
        assert_eq!(
            dir.prefix,
            "mw_circular_mean(winddir_sin_sum, winddir_cos_sum, winddir_dir_count)"
        );
        assert!(
            dir.tail.contains("sind(winddir::double precision)"),
            "{}",
            dir.tail
        );
        assert!(
            dir.tail.contains("cosd(winddir::double precision)"),
            "{}",
            dir.tail
        );

        let total = resolve_span_column(BucketLevel::Day, (MetricKey::RainIncremental, Agg::Sum))
            .expect("rain_incremental declares a total at every level");
        assert_eq!(total.prefix, "rainincremental_total::float8");
        assert_eq!(total.tail, "sum(rainincremental::double precision)");

        let max = resolve_span_column(BucketLevel::Minute, (MetricKey::WindSpeed, Agg::Max))
            .expect("wind_speed declares a max");
        assert_eq!(max.prefix, "windspeed_max::float8");
        assert_eq!(max.tail, "max(windspeed)::float8");
    }

    /// A `(metric, agg)` the registry does not carry is a named refusal rather than a
    /// column expression that fails at the server. `windspeed` declares `mean` and `max`
    /// and no `min`, which is the real case this protects.
    #[test]
    fn a_span_column_the_registry_does_not_carry_names_what_is_missing() {
        use mw_types::{Class, Classify};
        let e = resolve_span_column(BucketLevel::Minute, (MetricKey::WindSpeed, Agg::Min))
            .expect_err("wind_speed declares mean and max, not min");
        let chain = mw_types::DisplayChain(&e).to_string();
        assert!(chain.contains("WindSpeed"), "{chain}");
        assert!(chain.contains("_min"), "{chain}");
        assert!(chain.contains("1m"), "{chain}");
        assert_eq!(e.class(), Class::Fatal);
    }

    /// The two statements, so that the prefix's exclusive upper bound and the tail's
    /// `count(*)` are pinned: the first is **D-54**'s boundary and the second is the only
    /// thing that distinguishes an empty trailing bucket from one whose every metric was
    /// unreported.
    #[test]
    fn the_prefix_stops_where_the_tail_starts_and_the_tail_counts_its_rows() {
        let columns =
            vec![
                resolve_span_column(BucketLevel::FiveMinute, (MetricKey::OutTemp, Agg::Mean))
                    .expect("declared"),
            ];
        let prefix = prefix_statement(BucketLevel::FiveMinute, &columns);
        assert_eq!(
            prefix,
            "SELECT bucket, mw_mean(outtemp_sum, outtemp_count) FROM observations_5m \
             WHERE station_id = $1 AND bucket >= $2 AND bucket < $3 ORDER BY bucket LIMIT $4"
        );
        let tail = tail_statement(&columns, BucketLevel::FiveMinute);
        // **Grouped on the level's own grid**, which is what makes a bucket aggregated from
        // raw identical to the one the refresh policy will later materialise -- and what
        // lets one statement serve both the still-filling bucket and the closed ones the
        // policy has not reached. The interval is a literal from `BucketLevel`, never from
        // the request.
        assert_eq!(
            tail,
            "SELECT time_bucket(INTERVAL '5 minutes', time) AS bucket, count(*), \
             mw_mean(sum(outtemp::double precision), nullif(count(outtemp), 0)) FROM \
             observations \
             WHERE station_id = $1 AND time >= $2 AND time < $3 GROUP BY bucket ORDER BY bucket"
        );
        // One literal per level, and no level reaches the raw table with any other width.
        for (level, interval) in [
            (BucketLevel::Minute, "1 minute"),
            (BucketLevel::FiveMinute, "5 minutes"),
            (BucketLevel::Hour, "1 hour"),
            (BucketLevel::Day, "1 day"),
        ] {
            assert!(
                tail_statement(&[], level).contains(&format!("INTERVAL '{interval}'")),
                "{level:?}"
            );
        }
    }

    /// `Agg::Sum` is the registry's `total` and there is no sixth strategy (**D-50**).
    #[test]
    fn every_agg_maps_onto_one_registry_strategy() {
        assert_eq!(strategy_of(Agg::Mean), Strategy::Mean);
        assert_eq!(strategy_of(Agg::Min), Strategy::Min);
        assert_eq!(strategy_of(Agg::Max), Strategy::Max);
        assert_eq!(strategy_of(Agg::Sum), Strategy::Total);
        assert_eq!(strategy_of(Agg::CircularMean), Strategy::CircularMean);
    }

    /// A dead connection is retried and a rejected statement is not, which is the split
    /// `StoreError`'s own documentation promises and the only thing a reader's error class
    /// is read for.
    #[test]
    fn a_transport_failure_is_retryable_and_a_bad_column_is_not() {
        use mw_types::{Class, Classify};
        assert_eq!(classify(sqlx::Error::PoolClosed).class(), Class::RETRYABLE);
        assert_eq!(
            classify(sqlx::Error::ColumnNotFound("rainincremental_total".into())).class(),
            Class::Fatal
        );
    }
}
