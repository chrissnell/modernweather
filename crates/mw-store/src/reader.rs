//! `window:rollup`'s half of the pool: the three reads that feed `WindowedView`.
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
    BoxFuture, Bucket, BucketLevel, BucketQuery, Bucketed, LatestQuery, Piece, ReadingStore,
    SeriesQuery, StoreError, TotalsQuery,
};
use mw_types::{
    AggregateColumn, MetricKey, MetricValue, Reading, SqlType, Strategy, LEVELS, METRICS,
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

/// The raw hypertable, which [`TimescaleReader::latest`] is the only read of.
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
fn classify(e: sqlx::Error) -> StoreError {
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
