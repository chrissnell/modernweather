//! `almanac:records` — the component that was missing, and the cache it fills.
//!
//! ```text
//!   observations_1d ──┐                              ┌─ high_temp, low_temp
//!   observations_1h ──┤                              ├─ high_wind_speed (+ wind_dir)
//!   observations     ─┤   almanac:records            ├─ max_rain_hour, max_rain_day
//!   almanac_cache   ──┘   Every { 1 hour }  ───────▶ ├─ low_barometer, low_humidity
//!   (the previous records)                           ├─ high_solar
//!                                                    └─ high_pm25, high_pm10_in,
//!                            │                          high_co2, high_tvoc
//!                            ▼
//!                      almanac_cache ──▶ /almanac
//! ```
//!
//! # What this fixes
//!
//! `/almanac` on the rewrite served `{}` — HTTP 200, an empty object — against a Go server
//! answering with fourteen records. Nothing was wrong with the handler, the route or the
//! station lookup: `almanac_cache` was **empty and nothing in the program ever wrote to
//! it**. W1 assumption 12 names `almanac_cache` and `snow_totals_cache` as the two
//! precomputed caches; W3 §2a.7 gives the second one four `snow:*` components and leaves
//! the first with a reader and no writer. That is a gap in the plan rather than a defect in
//! a component, and `docs/reconciliation.md` **A-M7-6-1** records it as one.
//!
//! The migration is **not** the other half of the answer, and this was checked rather than
//! assumed: `ci/db/migrate-from-go/migrate-from-go.sh` names `almanac_cache` in exactly two
//! places — the cascade that empties it during `reset-target`, and the row count it reports
//! afterwards — and there is no step that reads the Go table. It was never imported.
//! `docs/data-migration.md` and `docs/implementation-plan.md` §5.0 both say so for a
//! stronger reason than an oversight: the Go cache holds records from **2025-08-19**, which
//! is earlier than the first imported reading, so importing them would have seeded the
//! table with values no longer derivable from the data underneath it — and they would have
//! been overwritten by the first refresh anyway.
//!
//! # One component, not twelve
//!
//! **D-33** splits a cache into components when they have "independent periods, failures
//! and rate budgets", which is why the snow cache is four. These twelve records have none
//! of the three: one period (Go registers `refresh_almanac_cache` with
//! `add_job(..., '1 hour')` and this keeps it), one failure mode — the weather database not
//! answering — and no rate budget at all, because every read is local. Twelve components
//! would be twelve `last_run`s for one question.
//!
//! The split that *is* here is the station loop, which is `window:rollup`'s shape: one
//! component iterating every station inside one tick, so adding a station does not add a
//! component.
//!
//! # Why it is a component with a schedule and not a backfill
//!
//! Go computes these in PL/pgSQL and schedules the function as a TimescaleDB background
//! job. A migration that imported the rows once would make the records freeze at migration
//! time and quietly rot — which is `sun_times` over again (**A-M5-4-1**), and the shape the
//! done-criteria of the item that wrote this file ruled out in advance.
//!
//! # No SQL here, which is `ci/layers.toml`
//!
//! Every extreme is read through [`mw_component::ReadingStore`]'s `extreme` and
//! `raw_extreme` and written through [`mw_component::AlmanacCache`]. Go evaluates the same
//! twelve `ORDER BY ... LIMIT 1`s as one PL/pgSQL function reachable only through a live
//! TimescaleDB, so none of it had a test; `records_for` is the table those twelve
//! statements become, and it is a unit test over a fake store.

mod plan;
#[cfg(test)]
mod tests;

pub use plan::{register, AlmanacConfig, AlmanacPlanError, AlmanacStation};

use std::sync::Arc;
use std::time::Duration;

use mw_component::{
    AlmanacCache, AlmanacQuery, AlmanacRecord, BoxFuture, BucketLevel, ComponentError, Cx,
    ExtremeQuery, Extremum, RawExtremeQuery, ReadingStore, SpanColumn, StoreError, Tick,
};
use mw_types::{Agg, MetricKey};
use tracing::{debug, warn};

/// The component's id.
pub const ID: &str = "almanac:records";

/// Its period — Go's `add_job('refresh_almanac_cache', '1 hour')`, kept.
///
/// An hour is the right order of magnitude for an all-time extreme and it is also the
/// endpoint's own `Cache-Control: max-age=3600`, so a client cannot observe a fresher
/// answer than this produces.
pub const PERIOD: Duration = Duration::from_secs(3600);

/// Its per-occurrence deadline.
///
/// Thirteen statements per station: twelve `ORDER BY ... LIMIT 1`s and one raw lookup
/// inside a single day. Every one of them is one row out of either a day-level view with
/// one row per station per day — 365 a year — or one day of raw. Two minutes is three
/// orders of magnitude of slack on that, and a miss means the database is not answering
/// rather than that the work grew.
pub const DEADLINE: Duration = Duration::from_secs(120);

/// `almanac:records`' body: every station, and the two handles.
struct Records {
    stations: Arc<[AlmanacStation]>,
    store: Arc<dyn ReadingStore>,
    cache: Arc<dyn AlmanacCache>,
}

impl std::fmt::Debug for Records {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Records")
            .field("stations", &self.stations)
            .finish_non_exhaustive()
    }
}

impl Tick for Records {
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            for station in self.stations.iter() {
                // Cancellation between stations rather than only between ticks: a node with
                // several stations would otherwise keep reading for as long as the whole
                // loop takes after the deadline has already cancelled it.
                if cx.is_cancelled() {
                    return Err(ComponentError::Cancelled);
                }
                let records = self.compute(station).await.map_err(|e| {
                    // The station is in the message and not only in a log field: one
                    // component serves every station, and an operator reading `last_error`
                    // has no other way to learn which one failed.
                    ComponentError::tick(
                        format!("compute station `{}`'s almanac records", station.key),
                        e,
                    )
                })?;
                if records.is_empty() {
                    // **Not an error, and not a write.** A station that has never reported
                    // has no extremes, which is a fact about the station; `/almanac` serves
                    // `{}` for it, which is what the Go server does too. `warn` rather than
                    // `debug` because the other way to reach this is a weather database
                    // whose aggregate levels have been rebuilt and not materialised, and
                    // that is worth a line.
                    warn!(
                        station = %station.key,
                        "no almanac record could be computed for this station, so its \
                         cached records are left as they are and /almanac answers from \
                         them. A station that has never reported is the ordinary cause; \
                         the other is an aggregate level that has been rebuilt without \
                         being materialised (ci/db/rebuild-aggregates.sh)."
                    );
                    continue;
                }
                let written = self
                    .cache
                    .replace_almanac(&station.key, &records)
                    .await
                    .map_err(|e| {
                        ComponentError::tick(
                            format!("write station `{}`'s almanac records", station.key),
                            e,
                        )
                    })?;
                debug!(
                    station = %station.key,
                    records = written,
                    "the almanac records were recomputed"
                );
            }
            Ok(())
        })
    }
}

impl Records {
    /// One station's records, in the order [`records_for`] declares them.
    ///
    /// A failed read aborts the whole station and writes nothing, which is Go's transaction
    /// boundary and the right one: a partial set would replace twelve records with however
    /// many were read before the database stopped answering, and the cache is the only
    /// thing `/almanac` has.
    async fn compute(&self, station: &AlmanacStation) -> Result<Vec<AlmanacRecord>, StoreError> {
        let specs = records_for(&station.kind);
        // The records as they stand, read **once** per station rather than once per
        // retained record. Only [`RecordSpec::retained`] records consult it, so a station
        // whose every record comes from an unbounded level does not read it at all.
        let previous = if specs.iter().any(|s| s.retained) {
            self.store
                .almanac(AlmanacQuery {
                    station: station.key.clone(),
                })
                .await?
        } else {
            Vec::new()
        };
        let mut out = Vec::with_capacity(RECORD_COUNT);
        // **Whether the HISTORY answered, which is not the same question as whether `out`
        // is empty**, and conflating the two was a defect this function had: a station
        // whose every level read returns nothing still produces a carried-forward
        // `max_rain_hour`, so `out` was non-empty, the caller's "write nothing" branch did
        // not fire, and the replace took the other seven standing records with it --
        // permanently, since each later tick found the same nothing and rewrote the same one
        // row. The carry-forward exists to keep a record that the data can no longer see;
        // it must not be the reason the rest are deleted.
        let mut computed = 0usize;
        for spec in &specs {
            let fresh = self.fresh(station, spec).await?;
            computed += usize::from(fresh.is_some());
            let record = if spec.retained {
                let held = previous.iter().find(|r| r.record == spec.name).cloned();
                more_extreme(spec.direction, fresh, held)
            } else {
                fresh
            };
            if let Some(record) = record {
                out.push(record);
            }
        }
        if computed == 0 {
            return Ok(Vec::new());
        }
        Ok(out)
    }

    /// One record as the data in the database has it now, or `None` when no bucket
    /// qualifies.
    ///
    /// `None` is the ordinary case for most records on most stations: a Campbell logger has
    /// no `pm25` column and an AirGradient has no `outtemp`, so the extreme does not exist
    /// and the key is absent from the response. That is what Go's `INSERT ... SELECT ...
    /// LIMIT 1` over no rows produces, and it is what makes `/almanac` answer eight keys
    /// for one station and fourteen for another.
    async fn fresh(
        &self,
        station: &AlmanacStation,
        spec: &RecordSpec,
    ) -> Result<Option<AlmanacRecord>, StoreError> {
        let Some(found) = self
            .store
            .extreme(ExtremeQuery {
                station: station.key.clone(),
                level: spec.level,
                column: spec.column,
                direction: spec.direction,
                positive_only: spec.positive_only,
                carry: spec.carry,
            })
            .await?
        else {
            return Ok(None);
        };
        let (value, observed_at) = match spec.instant {
            // The bucket's own start, which is the timestamp Go's `INSERT ... bucket`
            // stores. Midnight UTC for a day-level record: the contract renders it in the
            // station's offset, which is why the live Go server's `high_temp` reads
            // `2025-08-19T18:00:00-06:00` -- that is 2025-08-20T00:00:00Z, the bucket.
            Instant::Bucket => (found.value, found.bucket),
            Instant::RawWithinBucket { metric } => {
                let until = found.bucket + bucket_width(spec.level);
                let Some(peak) = self
                    .store
                    .raw_extreme(RawExtremeQuery {
                        station: station.key.clone(),
                        metric,
                        since: found.bucket,
                        until,
                        direction: spec.direction,
                        positive_only: spec.positive_only,
                    })
                    .await?
                else {
                    // Unreachable through any state this program creates: the day bucket
                    // exists *because* raw rows fell in it, and `observations` has no
                    // retention policy and never will (**D-4**). Answered rather than
                    // asserted, because a component body may not panic over a row count,
                    // and the `warn` is what makes the impossible case visible if the raw
                    // table is ever pruned by hand.
                    warn!(
                        station = %station.key,
                        record = spec.name,
                        bucket = %found.bucket,
                        "the extreme bucket for this record holds no raw reading, so the \
                         record is left out. `observations` has no retention policy (D-4), \
                         so this means rows were removed from it by hand."
                    );
                    return Ok(None);
                };
                (peak.value, peak.time)
            }
        };
        Ok(Some(AlmanacRecord {
            record: spec.name.to_string(),
            // `real` in the column and a Go `float32` on the wire, so the narrowing happens
            // here and once -- which is also what makes [`more_extreme`] compare the value
            // that will actually be served.
            value: value as f32,
            observed_at,
            // `None` is an absent `wind_dir` key and never a null, which falls out of the
            // DTO's `Option` rather than out of a branch here.
            wind_dir: found.carried.map(|d| d as f32),
        }))
    }
}

/// Whichever of two readings of the same record is further in `direction`.
///
/// The carry-forward [`RecordSpec::retained`] exists for, and a total function over the
/// four cases so that "there is no cached record yet" and "the level can no longer see the
/// record" are handled by the same line.
///
/// A **tie keeps the stored record**, which is not arbitrary: the stored one is the earlier
/// observation of the same extreme, and `/almanac`'s `timestamp` is part of the answer. An
/// hour that merely equals the standing record has not broken it.
fn more_extreme(
    direction: Extremum,
    fresh: Option<AlmanacRecord>,
    held: Option<AlmanacRecord>,
) -> Option<AlmanacRecord> {
    match (fresh, held) {
        (Some(fresh), Some(held)) => {
            let beats = match direction {
                Extremum::Greatest => fresh.value > held.value,
                Extremum::Least => fresh.value < held.value,
            };
            Some(if beats { fresh } else { held })
        }
        (Some(fresh), None) => Some(fresh),
        (None, held) => held,
    }
}

/// Which instant a record's timestamp comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Instant {
    /// The extreme bucket's start.
    Bucket,
    /// The extreme **reading** inside the extreme bucket.
    ///
    /// `high_solar`'s, and only `high_solar`'s. Go reads it from the raw table outright
    /// ("from the raw weather table (aggregates keep only a daily average)") because the
    /// timestamp the contract serves is a reading's — the live server answers
    /// `2026-05-18T13:04:28.657385-06:00`, which is not a bucket boundary. Reading the day
    /// level first and the raw table only inside the day it names is what keeps that
    /// bounded: `observations` holds every reading the station has ever produced and no
    /// index covers a metric column, so Go's unbounded `ORDER BY solarwatts DESC` is a full
    /// scan of all history every hour.
    RawWithinBucket {
        /// The raw column, which is the same metric the level's column aggregates.
        metric: MetricKey,
    },
}

/// One record of the twelve: where it comes from and which way it is extreme.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordSpec {
    /// The `record` value, which is the frozen `/almanac` JSON key. The table's `CHECK`
    /// constrains it and a typo would be a constraint violation rather than a dropped
    /// field.
    name: &'static str,
    /// Which aggregate level the extreme is searched at.
    level: BucketLevel,
    /// The column searched.
    column: SpanColumn,
    /// Greatest or least.
    direction: Extremum,
    /// Whether a non-positive value is excluded — Go's `> 0` against its `IS NOT NULL`.
    positive_only: bool,
    /// A second column read from the same bucket. `high_wind_speed` only.
    carry: Option<SpanColumn>,
    /// Where the timestamp comes from.
    instant: Instant,
    /// Whether this record's **source level has a retention horizon**, so a previously
    /// cached record may be greater than anything the level can still see.
    ///
    /// True for `max_rain_hour` and nothing else, and it is a correctness requirement
    /// rather than a nicety. `migrations/20261102042200_...sql` keeps `observations_1h` for
    /// **90 days** and leaves `observations_1d` unbounded on purpose, so eleven of the
    /// twelve records are computed over all history and the twelfth is computed over a
    /// rolling quarter. Without this flag `max_rain_hour` would *fall* as the hour that set
    /// it aged out of the level — an all-time record that un-sets itself, which is the
    /// rotting the item that wrote this file existed to prevent.
    ///
    /// What it costs, stated: `max_rain_hour` is the only record that is not a pure
    /// function of the data in the database. It is seeded from the newest 90 days the first
    /// time this component runs and is exact for every hour after that. The Go server's own
    /// value may therefore be higher — its hourly view retains years — and
    /// `ci/diff/README.md` is where such a difference is recorded rather than explained
    /// away.
    retained: bool,
}

/// How many records this component can produce for one station.
const RECORD_COUNT: usize = 12;

/// The twelve records, as `migrations/weather/030_almanac_airgradient_pm10.up.sql` writes
/// them.
///
/// That function is the authority for every row of this table: the level each extreme is
/// read at, which direction it is, and which of `IS NOT NULL` and `> 0` filters it. The
/// filters are not interchangeable and the difference is Go's: a barometer reporting `0` is
/// a dead sensor and `low_barometer` must not be it, while a `low_temp` of 0 °F is a
/// January morning.
///
/// # The two device-specific records, which is why this is a function
///
/// `high_tvoc` and `high_pm10_in` are read from different columns depending on the device,
/// and migration 030 is the one that made them so:
///
/// - **`high_pm10_in`** is `pm10_in_aqin` on an Ambient AQIN and `extrafloat2` on an
///   AirGradient, "so PM10 (and the derived AQI PM10) were always blank for AirGradient
///   devices" until that migration sourced it per station type.
/// - **`high_tvoc`** is `extrafloat3` and **AirGradient only**: Go's statement carries
///   `AND stationtype = 'airgradient'`.
///
/// **Keyed on the configured type, not the reported one.** Go tests the `stationtype`
/// column in the weather table, which is what the driver reported; this reads
/// `config.stations.type`, which is what the operator configured. They are "not the same
/// set and must not be unified" (`docs/legacy-api-contract.md` §3.1) — and for these two
/// records the configured one is the better key, because it is the column the registrar
/// planned the station's driver from, so a device that has not reported yet is still
/// classified correctly.
///
/// # The three snow records are absent, deliberately
///
/// `deepest_snow`, `max_snow_hour` and `max_snow_day` are permitted by the table's `CHECK`
/// and are **never assigned** in Go: `handlers.go:1416` carries "Snow metrics temporarily
/// disabled due to performance issues" (**Q-19**) and the refresh function's own closing
/// comment says they are "calculated separately". Writing them here would add three keys to
/// a frozen response. The handler already reads the table rather than filtering it, so they
/// appear if and when something writes them, and nothing does.
fn records_for(kind: &str) -> Vec<RecordSpec> {
    let base = |name: &'static str,
                level: BucketLevel,
                column: SpanColumn,
                direction: Extremum,
                positive_only: bool| RecordSpec {
        name,
        level,
        column,
        direction,
        positive_only,
        carry: None,
        instant: Instant::Bucket,
        retained: false,
    };
    let mut out = vec![
        base(
            "high_temp",
            BucketLevel::Day,
            (MetricKey::OutTemp, Agg::Max),
            Extremum::Greatest,
            false,
        ),
        base(
            "low_temp",
            BucketLevel::Day,
            (MetricKey::OutTemp, Agg::Min),
            Extremum::Least,
            false,
        ),
        RecordSpec {
            // The one record with a second column, and the direction it carries is the
            // **day's** mean and not the direction at the instant of the gust: Go selects
            // `max_windspeed, bucket, winddir` from one `weather_1d` row, so `winddir` there
            // is `mw_circular_mean` over the whole bucket. Reproducing the contract means
            // reading it the same way.
            carry: Some((MetricKey::WindDir, Agg::CircularMean)),
            ..base(
                "high_wind_speed",
                BucketLevel::Day,
                (MetricKey::WindSpeed, Agg::Max),
                Extremum::Greatest,
                false,
            )
        },
        RecordSpec {
            retained: true,
            ..base(
                "max_rain_hour",
                BucketLevel::Hour,
                (MetricKey::RainIncremental, Agg::Sum),
                Extremum::Greatest,
                false,
            )
        },
        base(
            // The same column as `max_rain_hour` at a different level, which is the whole
            // of the difference between the two records.
            "max_rain_day",
            BucketLevel::Day,
            (MetricKey::RainIncremental, Agg::Sum),
            Extremum::Greatest,
            false,
        ),
        base(
            "low_barometer",
            BucketLevel::Day,
            (MetricKey::Barometer, Agg::Min),
            Extremum::Least,
            true,
        ),
        base(
            "low_humidity",
            BucketLevel::Day,
            (MetricKey::OutHumidity, Agg::Min),
            Extremum::Least,
            true,
        ),
        RecordSpec {
            instant: Instant::RawWithinBucket {
                metric: MetricKey::SolarWatts,
            },
            ..base(
                "high_solar",
                BucketLevel::Day,
                (MetricKey::SolarWatts, Agg::Max),
                Extremum::Greatest,
                true,
            )
        },
        base(
            "high_pm25",
            BucketLevel::Day,
            (MetricKey::Pm25, Agg::Max),
            Extremum::Greatest,
            true,
        ),
        base(
            "high_pm10_in",
            BucketLevel::Day,
            if kind == AIRGRADIENT {
                (MetricKey::ExtraFloat2, Agg::Max)
            } else {
                (MetricKey::Pm10InAqin, Agg::Max)
            },
            Extremum::Greatest,
            true,
        ),
        base(
            "high_co2",
            BucketLevel::Day,
            (MetricKey::Co2, Agg::Max),
            Extremum::Greatest,
            true,
        ),
    ];
    if kind == AIRGRADIENT {
        out.push(base(
            "high_tvoc",
            BucketLevel::Day,
            (MetricKey::ExtraFloat3, Agg::Max),
            Extremum::Greatest,
            true,
        ));
    }
    out
}

/// `config.stations.type` for an AirGradient air-quality monitor.
///
/// The same string `mw_station::airgradient::STATION_TYPE` is, and **not** that constant:
/// this crate may not name `mw-station`, because they are in the same layer
/// (`ci/layers.toml`). Public so that the binary -- the one crate that may name both --
/// can assert the two agree; `crates/modernweather/src/boot.rs` is where it does.
pub const AIRGRADIENT: &str = "airgradient";

/// One bucket's width, for the raw window `high_solar` is looked up in.
fn bucket_width(level: BucketLevel) -> chrono::Duration {
    match level {
        BucketLevel::Minute => chrono::Duration::minutes(1),
        BucketLevel::FiveMinute => chrono::Duration::minutes(5),
        BucketLevel::Hour => chrono::Duration::hours(1),
        BucketLevel::Day => chrono::Duration::days(1),
    }
}
