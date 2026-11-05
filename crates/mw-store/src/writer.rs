//! `writer:timescale`'s half of the pool: the batch `INSERT`, and **D-56**'s projections.
//!
//! # One statement per batch, and why it is `unnest` rather than a multi-row `VALUES`
//!
//! W3 §2.4 sizes the durable cursor's read-ahead at **256 envelopes** against PostgreSQL's
//! 65,535-bind-parameter limit: "at the registry's 216 allocated columns the hard ceiling
//! is 303 rows per multi-row `INSERT`; 256 is the power of two below it". That is correct
//! arithmetic for a `VALUES` list, and it is a ceiling this code does not need to live
//! under. `INSERT ... SELECT * FROM unnest($1::timestamptz[], $2::text[], …)` binds **one
//! array per column** — 216 parameters for any batch size — so the parameter limit stops
//! being a function of the batch at all. 256 stays the batch size because it is also the
//! single-round-trip figure, but the writer is no longer the thing that constrains it, and
//! a 217th column is no longer three rows off the ceiling.
//!
//! # The column list is the registry's, not a literal
//!
//! Every column name here comes from `mw_types::METRICS` and `mw_types::DIMENSIONS`, and
//! every value from `Reading::value`. A metric added to `metrics/registry.toml` is written
//! by this code with no edit, which is the property the Go implementation's 591 lines of
//! hand-written mapping did not have. The one thing that *is* asserted rather than derived
//! is the dimension set: four stored dimensions, named, because a fifth would otherwise be
//! a column this writer silently never fills — see [`DIMENSION_COLUMNS`].
//!
//! # D-56, and the one place this item departs from it
//!
//! **D-56** reads: *`Ingress::admit` upserts `stations` on identity resolution;
//! `writer:timescale` maintains `stations.last_seen` and `station_metrics`.* The second
//! half is here. The first half is here **too**, and that is a departure rather than a
//! deferral:
//!
//! - `admit` may do no I/O but the spool append (W3 **D-87**), which is not a rule the
//!   admission code follows but a property of its signatures — `StationRegistry::resolve`
//!   is synchronous and there is no store handle on that path.
//! - `ci/layers.toml` forbids `mw-pipeline` both `sqlx` and `mw-store`, so "upsert a
//!   PostgreSQL row" is not an expression that crate can contain, and `cargo xtask gate
//!   layout` rejects the dependency that would make it one.
//!
//! So the whole of D-56 lands on the writer, which also makes it **one transaction**: the
//! projection row, the readings it describes and the metric inventory commit together, so
//! there is no window in which `observations` has rows for a station `stations` does not
//! have — and `station_metrics`' foreign key to `stations` is satisfiable without ordering
//! two components. Filed as a proposed amendment to D-56 in `docs/reconciliation.md`'s
//! *Proposed amendments* section rather than decided here.
//!
//! What it costs is one thing, stated: the projection is refreshed when a station
//! **reports**, not when it is **configured**. A station added to the configuration and
//! not yet reporting has no `stations` row, where D-56's reading would have given it one
//! at the first frame — which is the same instant, because a frame is what produces a
//! reading. The case that genuinely differs is a station whose coordinates are *edited*
//! while it is not reporting: the projection catches up on its next reading rather than at
//! the edit. `/span`'s join is the only reader, and it reads rows that have readings.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mw_component::{
    BoxFuture, ReadingSink, StationProjection, StationRegistry, StoreError, Stored,
};
use mw_types::{DimensionMeta, Envelope, MetricValue, SqlType, StationKey, DIMENSIONS, METRICS};
use sqlx::{PgPool, Postgres, Transaction};
use tracing::warn;

/// The raw hypertable.
const OBSERVATIONS: &str = "observations";

/// The four stored dimension columns, in the order `observations` declares them.
///
/// Asserted against `mw_types::DIMENSIONS` by [`TimescaleWriter::new`] rather than derived
/// from it, and the difference matters. The *metrics* are derived, so a new one is written
/// for free; a new stored **dimension** is not something a writer can place, because
/// `Reading`'s dimension fields are named rather than reachable through `Reading::value` —
/// so deriving the list would produce an `INSERT` that binds a column it has no value for.
/// Asserting it turns that into a refusal to build, with the column named, instead of a
/// silently unfilled column nobody notices until a query returns NULL.
const DIMENSION_COLUMNS: [&str; 4] = ["time", "station_id", "stationname", "stationtype"];

/// Feet per metre. `config.stations.altitude_m` holds metres and the weather schema's
/// `stations.altitude_ft` projection holds feet — two columns, two units, both with the
/// unit in the name precisely so that the conversion is visible here instead of being a
/// factor of 3.28 nobody notices on a chart.
const FEET_PER_METRE: f64 = 1.0 / 0.3048;

/// The write half of the pool (W3 §2a.1's `TimescaleSink`).
pub struct TimescaleWriter {
    pool: PgPool,
    registry: Arc<dyn StationRegistry>,
    /// Built once, because it is 216 column names and a 214-assignment `DO UPDATE` list.
    observations: String,
}

impl std::fmt::Debug for TimescaleWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimescaleWriter")
            .field("columns", &(DIMENSION_COLUMNS.len() + METRICS.len()))
            .finish_non_exhaustive()
    }
}

/// A stored dimension the registry declares and this writer cannot place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "metrics/registry.toml declares the stored dimensions [{declared}], and this writer \
     places [{known}]. A stored dimension is a column of `observations`, so the difference \
     would be a column nothing ever writes: give it a line in mw_store::writer's INSERT, \
     or make it `role = \"metadata\"`, which is the role for a dimension the database has \
     no reason to store per row (N-14)."
)]
pub struct UnplaceableDimensions {
    /// What the registry says.
    pub declared: String,
    /// What this writer knows how to bind.
    pub known: String,
}

impl TimescaleWriter {
    /// The writer over one pool and the station registry the projections are copied from.
    ///
    /// Fails only on the dimension-set assertion, which is a build-time disagreement
    /// between this file and the registry rather than anything about the database.
    pub fn new(
        pool: PgPool,
        registry: Arc<dyn StationRegistry>,
    ) -> Result<Self, UnplaceableDimensions> {
        let declared: Vec<&str> = DIMENSIONS
            .iter()
            .filter(|d| d.stored)
            .map(|d| d.column)
            .collect();
        if declared != DIMENSION_COLUMNS {
            return Err(UnplaceableDimensions {
                declared: declared.join(", "),
                known: DIMENSION_COLUMNS.join(", "),
            });
        }
        Ok(Self {
            observations: observations_statement(),
            pool,
            registry,
        })
    }

    /// How many columns the batch statement writes, for a log line and for a test.
    pub fn columns(&self) -> usize {
        DIMENSION_COLUMNS.len() + METRICS.len()
    }

    /// The `INSERT` this writer runs, for a test that wants to read it.
    pub fn statement(&self) -> &str {
        &self.observations
    }

    async fn write(&self, batch: &[Envelope]) -> Result<Stored, StoreError> {
        if batch.is_empty() {
            return Ok(Stored::default());
        }
        let rows = Rows::collapse(batch);
        let mut tx = self.pool.begin().await.map_err(classify)?;
        // Order is load-bearing: `station_metrics` references `stations`, and a reading
        // whose station has no projection row "is addressable by nothing" (W4 §2.1).
        self.upsert_stations(&mut tx, &rows).await?;
        let written = self.insert_observations(&mut tx, &rows).await?;
        self.upsert_station_metrics(&mut tx, &rows).await?;
        tx.commit().await.map_err(classify)?;
        Ok(Stored {
            // The whole batch, not `written`: two envelopes that collapsed are both
            // durably handled, and a cursor that stopped short of the second would replay
            // it forever.
            stored: batch.len(),
            rows: written,
        })
    }

    /// **D-56**'s projection, and `stations.last_seen`.
    ///
    /// `name` and `type` are `NOT NULL` in the schema and the configuration's are
    /// authoritative (W4 §2.1: "`config:sqlite`'s StationRegistry is authoritative for a
    /// station's existence"). For a station the registry does not know — a peer's claim,
    /// until M6-4's resolution check lands — the envelope's own `stationname` and
    /// `stationtype` are the fallback, and the station key is the last resort, because the
    /// alternative is refusing to store a reading over a projection row.
    async fn upsert_stations(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rows: &Rows,
    ) -> Result<(), StoreError> {
        let p = projection_columns(rows, self.registry.as_ref());

        sqlx::query(
            "INSERT INTO stations (station_id, name, type, timezone, latitude, longitude, \
                                   altitude_ft, snow_base_distance_mm, first_seen, last_seen) \
             SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], \
                                  $5::float8[], $6::float8[], $7::float8[], $8::float8[], \
                                  $9::timestamptz[], $10::timestamptz[]) \
             ON CONFLICT (station_id) DO UPDATE SET \
                 name = EXCLUDED.name, \
                 type = EXCLUDED.type, \
                 timezone = EXCLUDED.timezone, \
                 latitude = EXCLUDED.latitude, \
                 longitude = EXCLUDED.longitude, \
                 altitude_ft = EXCLUDED.altitude_ft, \
                 snow_base_distance_mm = EXCLUDED.snow_base_distance_mm, \
                 first_seen = least(stations.first_seen, EXCLUDED.first_seen), \
                 last_seen = greatest(stations.last_seen, EXCLUDED.last_seen)",
        )
        .bind(&p.ids)
        .bind(&p.names)
        .bind(&p.kinds)
        .bind(&p.zones)
        .bind(&p.latitudes)
        .bind(&p.longitudes)
        .bind(&p.altitudes_ft)
        .bind(&p.bases)
        .bind(&p.first_seen)
        .bind(&p.last_seen)
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
        Ok(())
    }

    async fn insert_observations(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rows: &Rows,
    ) -> Result<u64, StoreError> {
        let mut q = sqlx::query(&self.observations)
            .bind(&rows.time)
            .bind(&rows.station_id)
            .bind(&rows.station_name)
            .bind(&rows.station_type);
        for column in &rows.metrics {
            q = match column {
                Column::Float(v) => q.bind(v),
                Column::Int(v) => q.bind(v),
                Column::BigInt(v) => q.bind(v),
                Column::Text(v) => q.bind(v),
                Column::Time(v) => q.bind(v),
            };
        }
        Ok(q.execute(&mut **tx)
            .await
            .map_err(classify)?
            .rows_affected())
    }

    /// Which metrics a station actually emits (W4 §2.2, **D-56**).
    ///
    /// Keyed by the registry's stable `key` and not by the PostgreSQL column name, which
    /// the column's own `COMMENT` states and W9's import relies on. One row per
    /// `(station, metric)` the batch carried a non-NULL value for, so the table answers
    /// "what can this station report" without the per-request scan of every chunk the Go
    /// `TODO` at `grpcreceiver/station.go:64` was avoiding.
    async fn upsert_station_metrics(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rows: &Rows,
    ) -> Result<(), StoreError> {
        if rows.metric_seen.is_empty() {
            return Ok(());
        }
        let mut stations = Vec::with_capacity(rows.metric_seen.len());
        let mut keys = Vec::with_capacity(rows.metric_seen.len());
        let mut first_seen = Vec::with_capacity(rows.metric_seen.len());
        let mut last_seen = Vec::with_capacity(rows.metric_seen.len());
        for ((station, key), span) in &rows.metric_seen {
            stations.push(station.clone());
            keys.push((*key).to_string());
            first_seen.push(span.first);
            last_seen.push(span.last);
        }
        sqlx::query(
            "INSERT INTO station_metrics (station_id, metric_key, first_seen, last_seen) \
             SELECT * FROM unnest($1::text[], $2::text[], $3::timestamptz[], $4::timestamptz[]) \
             ON CONFLICT (station_id, metric_key) DO UPDATE SET \
                 first_seen = least(station_metrics.first_seen, EXCLUDED.first_seen), \
                 last_seen = greatest(station_metrics.last_seen, EXCLUDED.last_seen)",
        )
        .bind(&stations)
        .bind(&keys)
        .bind(&first_seen)
        .bind(&last_seen)
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
        Ok(())
    }
}

impl ReadingSink for TimescaleWriter {
    fn store<'a>(&'a self, batch: &'a [Envelope]) -> BoxFuture<'a, Result<Stored, StoreError>> {
        Box::pin(self.write(batch))
    }
}

/// What the station reported about itself, where the registry has nothing.
#[derive(Debug, Default, Clone)]
struct Reported {
    name: Option<String>,
    kind: Option<String>,
}

/// One batch, transposed into one array per column.
///
/// Transposed because the statement binds arrays, and **collapsed** because it does not:
/// see [`Rows::collapse`].
struct Rows {
    time: Vec<DateTime<Utc>>,
    station_id: Vec<String>,
    station_name: Vec<Option<String>>,
    station_type: Vec<Option<String>>,
    metrics: Vec<Column>,
    /// The distinct stations in the batch, with the span of observation instants for each.
    stations: BTreeMap<String, Span>,
    /// What each station said its name and type were, for the projection's fallback.
    reported: BTreeMap<String, Reported>,
    /// `(station, metric key)` to the span of instants the batch carried a value at.
    metric_seen: BTreeMap<(String, &'static str), Span>,
}

/// The oldest and newest observation instant something was seen at in one batch.
///
/// Both halves are needed and a single instant will not do: `first_seen` and `last_seen`
/// are `least()`/`greatest()`ed against the stored row, so binding one value to both makes
/// a station's `first_seen` the *newest* instant in its first batch. Measured rather than
/// reasoned about -- a two-reading batch two seconds apart stored `first_seen` two seconds
/// after the earlier reading, which is the kind of wrong that nothing downstream notices.
#[derive(Debug, Clone, Copy)]
struct Span {
    first: DateTime<Utc>,
    last: DateTime<Utc>,
}

impl Span {
    fn at(instant: DateTime<Utc>) -> Self {
        Span {
            first: instant,
            last: instant,
        }
    }

    fn extend(&mut self, instant: DateTime<Utc>) {
        self.first = self.first.min(instant);
        self.last = self.last.max(instant);
    }
}

/// One column's values, in the type its `sql_type` declares.
enum Column {
    Float(Vec<Option<f32>>),
    Int(Vec<Option<i32>>),
    BigInt(Vec<Option<i64>>),
    Text(Vec<Option<String>>),
    Time(Vec<Option<DateTime<Utc>>>),
}

impl Column {
    fn empty(sql_type: SqlType, capacity: usize) -> Self {
        match sql_type {
            SqlType::Float4 => Column::Float(Vec::with_capacity(capacity)),
            SqlType::Int4 => Column::Int(Vec::with_capacity(capacity)),
            SqlType::Int8 => Column::BigInt(Vec::with_capacity(capacity)),
            SqlType::Text => Column::Text(Vec::with_capacity(capacity)),
            SqlType::Timestamptz => Column::Time(Vec::with_capacity(capacity)),
        }
    }

    /// Append one reading's value for this column.
    ///
    /// A value whose variant disagrees with the column's type is pushed as `NULL` rather
    /// than panicking or mis-binding: both come from the same generated table, so a
    /// disagreement is impossible unless the generator is broken, and the right failure
    /// for "impossible" on the write path is a NULL and a count rather than a process that
    /// exits mid-batch. The mismatch is counted by the caller.
    fn push(&mut self, value: Option<MetricValue<'_>>) -> bool {
        match (self, value) {
            (Column::Float(v), Some(MetricValue::Float(x))) => v.push(Some(x)),
            (Column::Int(v), Some(MetricValue::Int(x))) => v.push(Some(x)),
            (Column::BigInt(v), Some(MetricValue::BigInt(x))) => v.push(Some(x)),
            (Column::Text(v), Some(MetricValue::Text(x))) => v.push(Some(x.to_string())),
            (Column::Time(v), Some(MetricValue::Time(x))) => v.push(Some(x)),
            // The ordinary case: the station did not report this metric, so the column is
            // `NULL` and **never** `0.0` (**D-26**).
            (slot, None) => {
                slot.push_null();
                return true;
            }
            (slot, Some(_)) => {
                slot.push_null();
                return false;
            }
        }
        true
    }

    fn push_null(&mut self) {
        match self {
            Column::Float(v) => v.push(None),
            Column::Int(v) => v.push(None),
            Column::BigInt(v) => v.push(None),
            Column::Text(v) => v.push(None),
            Column::Time(v) => v.push(None),
        }
    }
}

impl Rows {
    /// Transpose a batch, **collapsing envelopes that share a `(station_id, time)`**.
    ///
    /// Not an optimisation — the statement is wrong without it. `observations` has a unique
    /// index on `(station_id, time)` and `ON CONFLICT DO UPDATE` may not touch the same row
    /// twice in one statement: PostgreSQL raises *"ON CONFLICT DO UPDATE command cannot
    /// affect row a second time"* and the whole batch fails.
    ///
    /// A batch containing two such envelopes is not a corner case. `Ingress::admit`
    /// substitutes the frame's arrival instant for every `PartialReading` whose frame
    /// carried no timestamp, so a Davis `LOOP 4` response — four observations in one frame
    /// — produces four readings with **identical** `(station_id, time)`. Sequential
    /// upserts would have left the last one standing; this does the same thing in one
    /// statement, keeping the last, which is also what a replay of the same prefix
    /// produces. [`Stored::rows`] is how many rows survived, so the collapsing is a number
    /// an operator can see rather than an invisible loss.
    fn collapse(batch: &[Envelope]) -> Rows {
        // `(station, time)` to the index in `batch` that wins. `BTreeMap` rather than a
        // hash map: the iteration order is then the row order, which makes the statement's
        // parameters a deterministic function of the batch and a failing test
        // reproducible.
        let mut winner: BTreeMap<(&str, DateTime<Utc>), usize> = BTreeMap::new();
        for (i, envelope) in batch.iter().enumerate() {
            winner.insert((envelope.station.as_str(), envelope.observed_at), i);
        }

        let n = winner.len();
        let mut rows = Rows {
            time: Vec::with_capacity(n),
            station_id: Vec::with_capacity(n),
            station_name: Vec::with_capacity(n),
            station_type: Vec::with_capacity(n),
            metrics: METRICS
                .iter()
                .map(|m| Column::empty(m.sql_type, n))
                .collect(),
            stations: BTreeMap::new(),
            reported: BTreeMap::new(),
            metric_seen: BTreeMap::new(),
        };

        let mut mismatches = 0usize;
        for (&(station, observed_at), &i) in &winner {
            let reading = &batch[i].reading;
            rows.time.push(observed_at);
            rows.station_id.push(station.to_string());
            rows.station_name.push(reading.station_name.clone());
            rows.station_type.push(reading.station_type.clone());
            for (column, meta) in rows.metrics.iter_mut().zip(METRICS.iter()) {
                let value = reading.value(meta.key);
                if value.is_some() {
                    rows.metric_seen
                        .entry((station.to_string(), meta.name))
                        .and_modify(|span| span.extend(observed_at))
                        .or_insert_with(|| Span::at(observed_at));
                }
                if !column.push(value) {
                    mismatches += 1;
                }
            }
            rows.stations
                .entry(station.to_string())
                .and_modify(|span| span.extend(observed_at))
                .or_insert_with(|| Span::at(observed_at));
            let reported = rows.reported.entry(station.to_string()).or_default();
            if reading.station_name.is_some() {
                reported.name = reading.station_name.clone();
            }
            if reading.station_type.is_some() {
                reported.kind = reading.station_type.clone();
            }
        }
        if mismatches > 0 {
            // Unreachable while the adopted types are the generator's output, which the
            // `codegen-drift` gate is what guarantees: `MetricMeta::sql_type` and
            // `Reading::value` come from the same registry entry. Logged rather than
            // asserted because the cost of being wrong about "unreachable" on the write
            // path is a process that will not store anything at all.
            warn!(
                mismatches,
                "a metric's value did not match the type metrics/registry.toml declares \
                 for its column, so it was stored as NULL. This cannot happen while the \
                 adopted types match the registry -- run `cargo xtask codegen --check`."
            );
        }
        rows
    }
}

/// `stations`' ten columns for one batch, transposed.
///
/// A named type rather than ten locals, because the whole point of extracting this is that
/// it is **pure** -- a function of the batch and the registry snapshot and nothing else --
/// and therefore testable without a database. The precedence rules below are the ones that
/// decide what `/span` joins to, and they were otherwise only observable by reading a row
/// back out of PostgreSQL.
#[derive(Debug, Default)]
struct ProjectionColumns {
    ids: Vec<String>,
    names: Vec<String>,
    kinds: Vec<String>,
    zones: Vec<String>,
    latitudes: Vec<Option<f64>>,
    longitudes: Vec<Option<f64>>,
    altitudes_ft: Vec<Option<f64>>,
    bases: Vec<Option<f64>>,
    first_seen: Vec<DateTime<Utc>>,
    last_seen: Vec<DateTime<Utc>>,
}

/// Build `stations`' columns for every station the batch carried.
///
/// **The configuration wins, the reading is the fallback, the key is the last resort.**
/// W4 §2.1 makes `config:sqlite`'s registry authoritative for a station's existence and
/// configuration, so a configured name beats the one the driver reports about itself --
/// which also means an operator renaming a station in configuration is reflected on
/// `/span` without touching the readings. The reading is next because a station this node
/// has no row for still has to get a projection row (**D-30**: readings are the thing that
/// must not be lost). The key is last because the two columns are `NOT NULL`.
fn projection_columns(rows: &Rows, registry: &dyn StationRegistry) -> ProjectionColumns {
    let mut p = ProjectionColumns::default();
    for (station, span) in &rows.stations {
        let configured = registry.projection(&StationKey::new(station.as_str()));
        if configured.is_none() {
            // Once per batch per unknown station, at WARN rather than silently: a
            // projection row carrying the station key as its display name is what `/span`
            // will join to, and "the name on the chart is the key" is the observable
            // symptom of a station this node has not been told about.
            warn!(
                station = %station,
                "no configuration row for this station, so its read-side projection is \
                 filled from the reading itself. Until M6-4 resolves a peer's station key \
                 this is the expected state for a received station."
            );
        }
        let fallback = rows.reported.get(station);
        p.names.push(
            configured
                .as_ref()
                .map(|c| c.name.clone())
                .or_else(|| fallback.and_then(|r| r.name.clone()))
                .unwrap_or_else(|| station.clone()),
        );
        p.kinds.push(
            configured
                .as_ref()
                .map(|c| c.kind.clone())
                .or_else(|| fallback.and_then(|r| r.kind.clone()))
                // `'unknown'` is what W9's import writes for the same gap
                // (`populate_stations`'s `coalesce(..., 'unknown')`), so the two agree
                // rather than each inventing a placeholder.
                .unwrap_or_else(|| "unknown".to_string()),
        );
        // Destructured explicitly rather than through a default, because `None` here is a
        // decision and not a fallback: a station with no configuration row has no known
        // zone and no known position, and a NULL coordinate is what the projection's own
        // nullable columns are for.
        let (timezone, latitude, longitude, altitude_m, base) = match configured {
            Some(StationProjection {
                timezone,
                latitude,
                longitude,
                altitude_m,
                snow_base_distance_mm,
                ..
            }) => (
                timezone,
                latitude,
                longitude,
                altitude_m,
                snow_base_distance_mm,
            ),
            None => (None, None, None, None, None),
        };
        p.ids.push(station.clone());
        // The schema's own `DEFAULT 'UTC'`, repeated because the column is `NOT NULL` and
        // this is an `INSERT` with an explicit column list. A station with no configured
        // zone rolls over in UTC, which is what the column said before this row existed.
        p.zones.push(timezone.unwrap_or_else(|| "UTC".to_string()));
        p.latitudes.push(latitude);
        p.longitudes.push(longitude);
        p.altitudes_ft.push(altitude_m.map(|m| m * FEET_PER_METRE));
        p.bases.push(base);
        p.first_seen.push(span.first);
        p.last_seen.push(span.last);
    }
    p
}

/// The 216-column `INSERT`, built once.
fn observations_statement() -> String {
    let dimensions: Vec<&DimensionMeta> = DIMENSIONS.iter().filter(|d| d.stored).collect();
    let columns: Vec<&str> = dimensions
        .iter()
        .map(|d| d.column)
        .chain(METRICS.iter().map(|m| m.column))
        .collect();
    let casts: Vec<String> = dimensions
        .iter()
        .map(|d| d.sql_type.ddl())
        .chain(METRICS.iter().map(|m| m.sql_type.ddl()))
        .enumerate()
        .map(|(i, ddl)| format!("${}::{ddl}[]", i + 1))
        .collect();
    // Every column, including the dimensions: an envelope replayed from the spool carries
    // the same `stationname` it carried the first time, so assigning it again is the same
    // value, and a *corrected* reading for the same instant should correct every column it
    // names. `station_id` and `time` are the conflict target and cannot be assigned.
    let updates: Vec<String> = columns
        .iter()
        .filter(|c| **c != "station_id" && **c != "time")
        .map(|c| format!("{c} = EXCLUDED.{c}"))
        .collect();
    format!(
        "INSERT INTO {OBSERVATIONS} ({}) SELECT * FROM unnest({}) \
         ON CONFLICT (station_id, time) DO UPDATE SET {}",
        columns.join(", "),
        casts.join(", "),
        updates.join(", ")
    )
}

/// Which side of **D-27** a `sqlx` failure falls on.
///
/// The same split `mw_store::version` makes and for the same reason: the socket and the
/// session are transient, and everything else — a statement the server rejected, a column
/// that is not there, a value that will not encode — is this build disagreeing with the
/// schema it was built against, which no retry fixes.
fn classify(e: sqlx::Error) -> StoreError {
    match &e {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => StoreError::unavailable(e),
        // A `Database` error that is a connection failure in disguise: PostgreSQL's
        // `57P01` (admin_shutdown) and `57P03` (cannot_connect_now) arrive as server
        // errors and mean "come back", which is the opposite of what a rejected statement
        // means.
        sqlx::Error::Database(db)
            if matches!(db.code().as_deref(), Some("57P01" | "57P03" | "53300")) =>
        {
            StoreError::unavailable(e)
        }
        _ => StoreError::rejected(e),
    }
}

/// Every `(station, metric)` pair, so a test can assert the inventory without a database.
///
/// Public for `tests/db.rs`, which compares it against what the table holds: the point of
/// `station_metrics` is that it answers "what can this station report", and a test that
/// only read the table back would not notice the writer computing the wrong set.
#[must_use]
pub fn metrics_in(batch: &[Envelope]) -> BTreeSet<(String, &'static str)> {
    Rows::collapse(batch)
        .metric_seen
        .into_keys()
        .collect::<BTreeSet<_>>()
}

/// How many rows a batch will write — its distinct `(station_id, time)` pairs.
///
/// Public for the same reason: `Stored::rows` is only meaningful against an expectation,
/// and deriving the expectation from the batch is what makes the collapsing assertable.
#[must_use]
pub fn rows_in(batch: &[Envelope]) -> usize {
    batch
        .iter()
        .map(|e| (e.station.as_str(), e.observed_at))
        .collect::<BTreeSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statement is derived, so what is worth asserting is the shape the derivation
    /// has to have: one placeholder per column, the conflict target excluded from the
    /// assignment list, and no literal column name in this file beyond the four dimensions.
    #[test]
    fn the_statement_binds_one_array_per_column_and_assigns_every_one_but_the_key() {
        let sql = observations_statement();
        let columns = DIMENSION_COLUMNS.len() + METRICS.len();
        assert_eq!(
            sql.matches("::").count(),
            columns,
            "one cast per column in the unnest"
        );
        assert!(
            sql.contains(&format!("${columns}::")),
            "the last placeholder is ${columns}: {sql}"
        );
        assert!(
            !sql.contains(&format!("${}::", columns + 1)),
            "and there is no {}th",
            columns + 1
        );
        assert!(sql.contains("ON CONFLICT (station_id, time) DO UPDATE"));
        assert_eq!(
            sql.matches(" = EXCLUDED.").count(),
            columns - 2,
            "every column but the two in the conflict target is assigned"
        );
        assert!(
            !sql.contains("station_id = EXCLUDED.station_id"),
            "the conflict target is not assignable"
        );
        assert!(!sql.contains("time = EXCLUDED.time"));
    }

    /// The parameter count is what W3 §2.4 sized the batch against. An array-per-column
    /// statement is 216 parameters for a batch of 1 and for a batch of 10,000, which is
    /// the claim the module doc makes.
    #[test]
    fn the_parameter_count_does_not_depend_on_the_batch_size() {
        let sql = observations_statement();
        let placeholders = sql.matches('$').count();
        assert_eq!(placeholders, DIMENSION_COLUMNS.len() + METRICS.len());
        assert!(
            placeholders < 65_535,
            "PostgreSQL's bind-parameter limit, which this statement is now nowhere near"
        );
    }

    #[test]
    fn the_dimension_assertion_names_the_column_it_cannot_place() {
        let e = UnplaceableDimensions {
            declared: "time, station_id, stationname, stationtype, elevation".into(),
            known: DIMENSION_COLUMNS.join(", "),
        };
        assert!(e.to_string().contains("elevation"), "{e}");
        // The shipped registry agrees with this file, which is the assertion that matters.
        let declared: Vec<&str> = DIMENSIONS
            .iter()
            .filter(|d| d.stored)
            .map(|d| d.column)
            .collect();
        assert_eq!(declared, DIMENSION_COLUMNS);
    }

    fn envelope(station: &str, at: DateTime<Utc>, barometer: Option<f32>) -> Envelope {
        Envelope {
            station: StationKey::new(station),
            observed_at: at,
            received_at: at,
            provenance: mw_types::Provenance::admitted(
                mw_types::OriginId::new([7; 16]),
                mw_types::Seq::FIRST,
            ),
            reading: Arc::new(mw_types::Reading {
                time: at,
                station_id: station.to_string(),
                barometer,
                ..mw_types::Reading::default()
            }),
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    /// The collapse, which is the thing that keeps `ON CONFLICT DO UPDATE` legal. Four
    /// observations from one Davis `LOOP 4` frame share an instant, and the last wins.
    #[test]
    fn envelopes_sharing_a_station_and_instant_collapse_to_the_last() {
        let batch = vec![
            envelope("backyard", at(100), Some(29.90)),
            envelope("backyard", at(100), Some(29.91)),
            envelope("backyard", at(100), Some(29.92)),
            envelope("backyard", at(101), Some(30.00)),
            envelope("roof", at(100), Some(28.00)),
        ];
        let rows = Rows::collapse(&batch);
        assert_eq!(rows.time.len(), 3, "three distinct (station, time) pairs");
        assert_eq!(rows_in(&batch), 3);
        let Column::Float(barometer) = &rows.metrics[0] else {
            panic!("barometer is the registry's first metric and is a float4")
        };
        assert_eq!(METRICS[0].column, "barometer");
        // Ordered by (station, time): backyard@100, backyard@101, roof@100.
        assert_eq!(
            barometer,
            &[Some(29.92), Some(30.00), Some(28.00)],
            "the last envelope for an instant is the one stored"
        );
        assert_eq!(rows.stations.len(), 2);
        assert_eq!(
            rows.stations["backyard"].first,
            at(100),
            "the oldest instant"
        );
        assert_eq!(rows.stations["backyard"].last, at(101), "and the newest");
    }

    /// **D-26**, in the shape it reaches the database: an unreported metric is a `None` in
    /// the bound array and therefore a SQL `NULL`, not a zero.
    #[test]
    fn an_unreported_metric_is_null_and_not_zero() {
        let batch = vec![envelope("backyard", at(100), None)];
        let rows = Rows::collapse(&batch);
        for (column, meta) in rows.metrics.iter().zip(METRICS.iter()) {
            let is_null = match column {
                Column::Float(v) => v[0].is_none(),
                Column::Int(v) => v[0].is_none(),
                Column::BigInt(v) => v[0].is_none(),
                Column::Text(v) => v[0].is_none(),
                Column::Time(v) => v[0].is_none(),
            };
            assert!(is_null, "{} should be NULL", meta.column);
        }
        assert!(
            metrics_in(&batch).is_empty(),
            "a reading with no metric contributes nothing to station_metrics"
        );
    }

    /// Which side of **D-27** each `sqlx` failure falls on. This is the decision that
    /// decides whether a batch replays or whether the component stops asking, and it is
    /// made over the error enum rather than over a string -- so it is assertable without a
    /// database and is asserted here.
    #[test]
    fn a_dead_socket_replays_and_a_rejected_statement_does_not() {
        use mw_types::{Class, Classify};
        assert_eq!(
            classify(sqlx::Error::PoolTimedOut).class(),
            Class::RETRYABLE,
            "a pool that could not hand out a connection is the database being away"
        );
        assert_eq!(classify(sqlx::Error::PoolClosed).class(), Class::RETRYABLE);
        assert_eq!(
            classify(sqlx::Error::WorkerCrashed).class(),
            Class::RETRYABLE
        );
        assert_eq!(
            classify(sqlx::Error::Io(std::io::Error::other("connection reset"))).class(),
            Class::RETRYABLE
        );
        // Everything else is this build disagreeing with the schema it was built against,
        // which no retry fixes: a column that is not there is not there next time either.
        assert_eq!(
            classify(sqlx::Error::ColumnNotFound("snowdepth".into())).class(),
            Class::Fatal
        );
        assert_eq!(classify(sqlx::Error::RowNotFound).class(), Class::Fatal);
    }

    /// A failed write keeps its chain, because that is the only thing that distinguishes
    /// the two classifications for the person reading the log (§5.9).
    #[test]
    fn a_store_failure_keeps_the_sqlx_error_under_it() {
        let e = classify(sqlx::Error::Io(std::io::Error::other("no route to host")));
        let chain = mw_types::DisplayChain(&e).to_string();
        assert!(chain.contains("could not be reached"), "{chain}");
        assert!(chain.contains("no route to host"), "{chain}");
    }

    /// A fake registry: one station configured, everything else unknown.
    #[derive(Debug)]
    struct OneConfigured(StationProjection);

    impl StationRegistry for OneConfigured {
        fn resolve(
            &self,
            _meta: &mw_component::FrameMeta,
        ) -> Result<StationKey, mw_component::Reject> {
            Err(mw_component::Reject::UnknownStation)
        }

        fn metadata(&self, _station: &StationKey) -> mw_component::StationMeta {
            mw_component::StationMeta::EMPTY
        }

        fn projection(&self, station: &StationKey) -> Option<StationProjection> {
            (station.as_str() == "configured").then(|| self.0.clone())
        }
    }

    fn configured() -> OneConfigured {
        OneConfigured(StationProjection {
            name: "The Back Yard".into(),
            kind: "weatherlink-live".into(),
            timezone: Some("America/Denver".into()),
            latitude: Some(39.7392),
            longitude: Some(-104.9903),
            altitude_m: Some(1609.0),
            snow_base_distance_mm: Some(1500.0),
        })
    }

    fn named(station: &str, at_secs: i64, name: Option<&str>, kind: Option<&str>) -> Envelope {
        let when = at(at_secs);
        Envelope {
            reading: Arc::new(mw_types::Reading {
                time: when,
                station_id: station.to_string(),
                station_name: name.map(str::to_string),
                station_type: kind.map(str::to_string),
                barometer: Some(29.92),
                ..mw_types::Reading::default()
            }),
            ..envelope(station, when, None)
        }
    }

    /// **D-56**'s projection, as a pure function of the batch and the snapshot: the
    /// configured name and type win over the ones the station reports about itself.
    #[test]
    fn the_configuration_is_authoritative_for_the_projections_name_and_type() {
        let batch = vec![named(
            "configured",
            100,
            Some("whatever the driver calls itself"),
            Some("davis"),
        )];
        let p = projection_columns(&Rows::collapse(&batch), &configured());
        assert_eq!(p.ids, vec!["configured"]);
        assert_eq!(p.names, vec!["The Back Yard"]);
        assert_eq!(p.kinds, vec!["weatherlink-live"]);
        assert_eq!(p.zones, vec!["America/Denver"]);
        assert_eq!(p.latitudes, vec![Some(39.7392)]);
        assert_eq!(p.bases, vec![Some(1500.0)]);
        // Metres in, feet out. The two column names carry the two units precisely so that
        // this conversion is visible rather than being a factor of 3.28 on a chart.
        let feet = p.altitudes_ft[0].expect("projected");
        assert!(
            (feet - 1609.0 / 0.3048).abs() < 1e-9,
            "1609 m should be ~5279.5 ft, got {feet}"
        );
    }

    /// A station the registry does not know -- a peer's, until M6-4 -- is projected from
    /// the reading, with `NULL` where nothing knows the answer.
    #[test]
    fn an_unknown_station_is_projected_from_the_reading_and_never_from_a_guess() {
        let batch = vec![named(
            "received",
            100,
            Some("a satellite's station"),
            Some("snowgauge"),
        )];
        let p = projection_columns(&Rows::collapse(&batch), &configured());
        assert_eq!(p.names, vec!["a satellite's station"]);
        assert_eq!(p.kinds, vec!["snowgauge"]);
        assert_eq!(
            p.zones,
            vec!["UTC"],
            "the column's own DEFAULT, because it is NOT NULL"
        );
        assert_eq!(
            p.latitudes,
            vec![None],
            "a guess would be worse than a NULL"
        );
        assert_eq!(p.longitudes, vec![None]);
        assert_eq!(p.altitudes_ft, vec![None]);
        assert_eq!(p.bases, vec![None]);
    }

    /// Nothing knows the name: the key is the last resort, because the column is
    /// `NOT NULL` and the alternative is refusing to store a reading.
    #[test]
    fn a_station_nothing_can_name_is_projected_under_its_own_key() {
        let batch = vec![named("anonymous", 100, None, None)];
        let p = projection_columns(&Rows::collapse(&batch), &configured());
        assert_eq!(p.names, vec!["anonymous"]);
        assert_eq!(
            p.kinds,
            vec!["unknown"],
            "the same placeholder W9's import writes for the same gap"
        );
    }

    /// `first_seen` is the batch's **oldest** instant and `last_seen` its newest. One
    /// instant bound to both columns is the obvious implementation and it is wrong: with
    /// `least`/`greatest` against the stored row, a station's `first_seen` would be the
    /// newest reading in its first batch.
    #[test]
    fn the_projections_span_is_the_batchs_oldest_and_newest_instant() {
        let batch = vec![
            named("configured", 300, None, None),
            named("configured", 100, None, None),
            named("configured", 200, None, None),
        ];
        let p = projection_columns(&Rows::collapse(&batch), &configured());
        assert_eq!(p.first_seen, vec![at(100)]);
        assert_eq!(p.last_seen, vec![at(300)]);
    }

    /// Two stations in one batch produce two rows, in a deterministic order -- which is
    /// what makes the statement's parameters a function of the batch and a failing test
    /// reproducible.
    #[test]
    fn two_stations_in_one_batch_are_two_rows_in_key_order() {
        let batch = vec![
            named("received", 100, Some("b"), Some("davis")),
            named("configured", 100, None, None),
        ];
        let p = projection_columns(&Rows::collapse(&batch), &configured());
        assert_eq!(p.ids, vec!["configured", "received"]);
        assert_eq!(p.names, vec!["The Back Yard", "b"]);
    }

    /// `station_metrics` is keyed by the registry's stable key, not by the column name.
    /// The two differ for most metrics (`out_temp` against `outtemp`), and the column's
    /// own `COMMENT` says which one belongs there.
    #[test]
    fn the_metric_inventory_is_keyed_by_the_registry_key() {
        let at100 = at(100);
        let batch = vec![Envelope {
            reading: Arc::new(mw_types::Reading {
                time: at100,
                station_id: "backyard".into(),
                out_temp: Some(41.0),
                extra_text1: Some("a note".into()),
                ..mw_types::Reading::default()
            }),
            ..envelope("backyard", at100, None)
        }];
        let seen = metrics_in(&batch);
        assert!(seen.contains(&("backyard".to_string(), "out_temp")));
        assert!(
            seen.contains(&("backyard".to_string(), "extra_text1")),
            "a text metric is inventory too -- `Reading::get` would have missed it"
        );
        assert!(
            !seen.iter().any(|(_, k)| *k == "outtemp"),
            "the column name is the other namespace"
        );
        assert_eq!(seen.len(), 2);
    }
}
