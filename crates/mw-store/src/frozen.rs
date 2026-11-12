//! The two caches the frozen REST surface reads directly, and the forecast table.
//!
//! `ForecastStore` and `SnowCache` are their own capabilities rather than methods on
//! [`mw_component::ReadingStore`] because `docs/component-model.md` §2.1 gives them their
//! own, and because §3.2's table degrades them independently: `api:legacy` declares all
//! three `Soft`, so `/forecast` failing and `/span` failing are different states and
//! either can be absent without the other. One pool, three handles, one provider — the
//! same arrangement [`crate::TimescaleStore`] has for the two halves of the reading store,
//! and the capability bus keys a slot per `(provider, capability)` so three publications
//! from one component is the ordinary case rather than a clash.
//!
//! # These are the endpoints' only data source, not an optimisation
//!
//! W1 assumption 12: `/almanac` and `/snow` read `almanac_cache` and `snow_totals_cache`
//! "and nothing else", and `/almanac` returns `{}` rather than computing anything when its
//! cache is empty. So there is no fallback query here and there must not be one — a
//! handler that recomputed on a cache miss would serve a different number from the one the
//! cache will serve a minute later, on an endpoint whose whole point is that it is cheap.
//! `/almanac`'s half lives on [`mw_component::ReadingStore`] (`crate::reader`) because it
//! is keyed the same way the aggregate reads are; `/snow`'s is here because W2a gives the
//! snow cache a capability of its own.
//!
//! # No `COALESCE(..., 0)`, for `crate::reader`'s reason
//!
//! A station with no `snow_totals_cache` row is `None` and a station with a row of zeros is
//! `Some(SnowTotals::default())`. The frozen response is seven zeros either way, which is
//! why it would be tempting to collapse them — and they are different operational states
//! ("nothing has ever written a row" against "the gauge has reported no snow"), so the
//! collapse happens at the DTO, where every other one of the contract's zeros is produced.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mw_component::{
    Alert, AlertStore, BoxFuture, DepthEstimate, EstimateWindow, Forecast, ForecastQuery,
    ForecastStore, SnowCache, SnowEvent, SnowPatch, SnowTotals, StoreError,
};
use mw_types::StationKey;
use sqlx::{PgPool, Row};

use crate::reader::classify;

/// `ForecastStore`, `AlertStore` and `SnowCache` over one pool.
#[derive(Debug)]
pub struct FrozenReads {
    pool: PgPool,
}

impl FrozenReads {
    /// Every handle over one pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The same object under each of its capabilities.
    ///
    /// One call rather than three upcasts at the call site, so that `db:postgres` cannot
    /// publish three different instances under capabilities that are meant to be one
    /// pool — which would be invisible until somebody wondered why `/forecast` saw a
    /// connection the probe loop had closed.
    pub fn handles(
        self: Arc<Self>,
    ) -> (
        Arc<dyn ForecastStore>,
        Arc<dyn AlertStore>,
        Arc<dyn SnowCache>,
    ) {
        (
            Arc::clone(&self) as Arc<dyn ForecastStore>,
            Arc::clone(&self) as Arc<dyn AlertStore>,
            self as Arc<dyn SnowCache>,
        )
    }
}

impl ForecastStore for FrozenReads {
    fn forecast<'a>(
        &'a self,
        q: ForecastQuery,
    ) -> BoxFuture<'a, Result<Option<Forecast>, StoreError>> {
        Box::pin(async move {
            // `data::text`, which is the whole of **D-39** on this side: PostgreSQL renders
            // the `jsonb` and this hands the bytes on untouched, so jsonb's own key order
            // (length, then bytewise) and its own numeric text survive into the response.
            // Decoding into `serde_json::Value` and re-encoding would normalise both, which
            // is a change to a frozen body that no test comparing *shape* could see.
            //
            // `span_hours` is bound as an `i32` -- see [`span`] for why.
            let span = span(q.span_hours)?;
            let row = match &q.station {
                Some(station) => sqlx::query(
                    "SELECT updated_at, data::text FROM forecasts \
                     WHERE span_hours = $1 AND station_id = $2",
                )
                .bind(span)
                .bind(station.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(classify)?,
                // No station named. The Go handler takes gorm's `First()`, which orders by
                // the surrogate `id` this table does not have and therefore means
                // "whichever was inserted first". Insertion order is not recoverable from a
                // `(station_id, span_hours)` primary key, so *some* deterministic order has
                // to be chosen, and "most recently updated" is the better of the two
                // available answers for a cache: a stale row cannot win. `station_id` breaks
                // a tie.
                //
                // **It is a byte difference on a response, so it is DEV-13** in
                // `docs/reconciliation.md` §3 rather than a judgement recorded in a comment.
                // Worth knowing while reading the fixture that covers it:
                // `ci/conformance/rust-seed/04-caches.sql` backdates `csi-minimal`'s row so
                // that `forecast-240-ten-daily-periods` selects `davis-full` deterministically
                // under this ordering -- the Go rig gives both rows the same timestamp, so
                // that fixture passes because the seed was shaped to the ordering and not
                // because the two orderings agree.
                None => sqlx::query(
                    "SELECT updated_at, data::text FROM forecasts \
                     WHERE span_hours = $1 ORDER BY updated_at DESC, station_id LIMIT 1",
                )
                .bind(span)
                .fetch_optional(&self.pool)
                .await
                .map_err(classify)?,
            };
            let Some(row) = row else {
                return Ok(None);
            };
            Ok(Some(Forecast {
                updated_at: row.try_get(0).map_err(StoreError::rejected)?,
                data: row.try_get(1).map_err(StoreError::rejected)?,
            }))
        })
    }

    fn store_forecast<'a>(
        &'a self,
        station: &'a StationKey,
        span_hours: u32,
        data: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let span = span(span_hours)?;
            // `$3::jsonb` and not a parse on this side. The cast is where the column's
            // `CHECK (jsonb_typeof(data) = 'array')` and PostgreSQL's own key
            // normalisation happen, which is the half of **D-39** that makes
            // `/forecast`'s body reproducible: this program never renders that JSON, so
            // it never gets a chance to render it differently from the server.
            //
            // `updated_at = now()` on **both** arms. The table's comment has the whole
            // note; the short version is that the column has a `DEFAULT` and no trigger,
            // so an upsert that omits it from the `DO UPDATE` leaves `lastUpdated` at the
            // row's first-insert time -- and **DEV-13** makes the same column the
            // tiebreak for an unqualified `/forecast`, so the staleness would pick the
            // served row as well as date it.
            sqlx::query(
                "INSERT INTO forecasts (station_id, span_hours, data, updated_at) \
                 VALUES ($1, $2, $3::jsonb, now()) \
                 ON CONFLICT (station_id, span_hours) \
                 DO UPDATE SET data = EXCLUDED.data, updated_at = now()",
            )
            .bind(station.as_str())
            .bind(span)
            .bind(data)
            .execute(&self.pool)
            .await
            .map_err(classify)?;
            Ok(())
        })
    }
}

/// `span_hours` as the `integer` the column is.
///
/// Shared by the read and the write so that the two cannot disagree about which spans are
/// representable: the endpoint's grammar is `^\d{1,4}$`, so four digits is the widest value
/// any request can carry and a row outside that range is one nothing can reach. Binding an
/// `i64` against an `integer` column would also be a planner cast that defeats the index.
fn span(span_hours: u32) -> Result<i32, StoreError> {
    i32::try_from(span_hours).map_err(|_| {
        StoreError::rejected(std::io::Error::other(format!(
            "a forecast span of {span_hours} hours is outside the frozen endpoint's \
             four-digit grammar, so no row can exist for it"
        )))
    })
}

impl AlertStore for FrozenReads {
    fn store_alerts<'a>(
        &'a self,
        station: &'a StationKey,
        alerts: &'a [Alert],
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            if alerts.is_empty() {
                // The ordinary case, and deliberately not a `DELETE`: the trait's own
                // documentation has why a fetch that lists nothing must not erase an
                // active warning.
                return Ok(0);
            }
            // `UNNEST` of nine arrays rather than a statement per alert. The same shape
            // `crate::writer`'s batch insert takes, for the same reason: one round trip,
            // one plan, and an upsert that is atomic across the whole fetch -- a
            // per-alert loop that failed halfway would leave the station holding a
            // mixture of two fetches.
            let (
                mut ids,
                mut issued,
                mut begins,
                mut expires,
                mut names,
                mut colors,
                mut bodies,
                mut fulls,
                mut data,
            ) = (
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
                Vec::with_capacity(alerts.len()),
            );
            for alert in alerts {
                ids.push(alert.alert_id.as_str());
                issued.push(alert.issued_at);
                begins.push(alert.begins_at);
                expires.push(alert.expires_at);
                names.push(alert.name.as_str());
                colors.push(alert.color.as_str());
                bodies.push(alert.body.as_str());
                fulls.push(alert.body_full.as_str());
                data.push(alert.data.as_str());
            }
            let affected = sqlx::query(
                "INSERT INTO alerts (station_id, alert_id, issued_at, begins_at, expires_at, \
                                     name, color, body, body_full, data, fetched_at, updated_at) \
                 SELECT $1, a.alert_id, a.issued_at, a.begins_at, a.expires_at, \
                        a.name, a.color, a.body, a.body_full, a.data::jsonb, now(), now() \
                 FROM UNNEST($2::text[], $3::timestamptz[], $4::timestamptz[], \
                             $5::timestamptz[], $6::text[], $7::text[], $8::text[], \
                             $9::text[], $10::text[]) \
                      AS a(alert_id, issued_at, begins_at, expires_at, \
                           name, color, body, body_full, data) \
                 ON CONFLICT (station_id, alert_id) DO UPDATE SET \
                     issued_at = EXCLUDED.issued_at, \
                     begins_at = EXCLUDED.begins_at, \
                     expires_at = EXCLUDED.expires_at, \
                     name = EXCLUDED.name, \
                     color = EXCLUDED.color, \
                     body = EXCLUDED.body, \
                     body_full = EXCLUDED.body_full, \
                     data = EXCLUDED.data, \
                     updated_at = now()",
            )
            .bind(station.as_str())
            .bind(&ids)
            .bind(&issued)
            .bind(&begins)
            .bind(&expires)
            .bind(&names)
            .bind(&colors)
            .bind(&bodies)
            .bind(&fulls)
            .bind(&data)
            .execute(&self.pool)
            .await
            .map_err(classify)?
            .rows_affected();
            Ok(affected)
        })
    }

    fn sweep_expired_alerts<'a>(
        &'a self,
        cutoff: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            // `expires_at < $1` and nothing else. An alert with a NULL expiry is not
            // matched -- a NULL comparison is NULL and a NULL predicate deletes nothing --
            // which is the behaviour the trait promises and the reason this is not written
            // over `fetched_at`.
            let gone = sqlx::query("DELETE FROM alerts WHERE expires_at < $1")
                .bind(cutoff)
                .execute(&self.pool)
                .await
                .map_err(classify)?
                .rows_affected();
            Ok(gone)
        })
    }
}

impl SnowCache for FrozenReads {
    fn totals<'a>(
        &'a self,
        station: &'a StationKey,
    ) -> BoxFuture<'a, Result<Option<SnowTotals>, StoreError>> {
        Box::pin(async move {
            let Some(row) = sqlx::query(
                "SELECT snowdepth, snowtoday, snowlast24, snowlast72, snowseason, \
                 snowstorm, snowfallrate, computed_at FROM snow_totals_cache \
                 WHERE station_id = $1",
            )
            .bind(station.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?
            else {
                return Ok(None);
            };
            Ok(Some(SnowTotals {
                depth: row.try_get(0).map_err(StoreError::rejected)?,
                today: row.try_get(1).map_err(StoreError::rejected)?,
                last24: row.try_get(2).map_err(StoreError::rejected)?,
                last72: row.try_get(3).map_err(StoreError::rejected)?,
                season: row.try_get(4).map_err(StoreError::rejected)?,
                storm: row.try_get(5).map_err(StoreError::rejected)?,
                fall_rate: row.try_get(6).map_err(StoreError::rejected)?,
                computed_at: row.try_get(7).map_err(StoreError::rejected)?,
            }))
        })
    }

    fn store_totals<'a>(
        &'a self,
        station: &'a StationKey,
        patch: SnowPatch,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            // One statement, and every column is `COALESCE($n, <the stored value>)`. The
            // trait's documentation has why a patch rather than a row: four components
            // write this table on four schedules, so a whole-row write would make each one
            // revert the other three. Doing it in the `DO UPDATE` rather than with a read
            // first is also what makes two components ticking at the same instant safe --
            // the coalesce reads the row under the same lock the update takes.
            //
            // Every placeholder is cast. With a patch whose fields are all `None` the
            // server has nothing to infer the parameter types from, and an uncast NULL in
            // a `COALESCE` against a `real` column is a type-resolution failure rather
            // than a no-op.
            //
            // The `INSERT` arm's `COALESCE($n, 0)` is the table's own `NOT NULL DEFAULT 0`
            // restated, and it is restated rather than left to the default because a
            // partial `INSERT` column list would have to be built per patch shape -- which
            // is a statement per combination, in a cache that has seven numbers.
            sqlx::query(
                "INSERT INTO snow_totals_cache \
                     (station_id, snowdepth, snowtoday, snowlast24, snowlast72, snowseason, \
                      snowstorm, snowfallrate, computed_at, source_through) \
                 VALUES ($1, COALESCE($2::real, 0), COALESCE($3::real, 0), \
                         COALESCE($4::real, 0), COALESCE($5::real, 0), COALESCE($6::real, 0), \
                         COALESCE($7::real, 0), COALESCE($8::real, 0), now(), \
                         $9::timestamptz) \
                 ON CONFLICT (station_id) DO UPDATE SET \
                     snowdepth      = COALESCE($2::real, snow_totals_cache.snowdepth), \
                     snowtoday      = COALESCE($3::real, snow_totals_cache.snowtoday), \
                     snowlast24     = COALESCE($4::real, snow_totals_cache.snowlast24), \
                     snowlast72     = COALESCE($5::real, snow_totals_cache.snowlast72), \
                     snowseason     = COALESCE($6::real, snow_totals_cache.snowseason), \
                     snowstorm      = COALESCE($7::real, snow_totals_cache.snowstorm), \
                     snowfallrate   = COALESCE($8::real, snow_totals_cache.snowfallrate), \
                     computed_at    = now(), \
                     source_through = COALESCE($9::timestamptz, \
                                               snow_totals_cache.source_through)",
            )
            .bind(station.as_str())
            .bind(patch.depth)
            .bind(patch.today)
            .bind(patch.last24)
            .bind(patch.last72)
            .bind(patch.season)
            .bind(patch.storm)
            .bind(patch.fall_rate)
            .bind(patch.source_through)
            .execute(&self.pool)
            .await
            .map_err(classify)?;
            Ok(())
        })
    }

    fn newest_estimate<'a>(
        &'a self,
        station: &'a StationKey,
    ) -> BoxFuture<'a, Result<Option<DepthEstimate>, StoreError>> {
        Box::pin(async move {
            // The `(station_id, bucket DESC)` index's whole purpose, and the reason this is
            // not `max(bucket)`: the depth at that bucket is wanted too, and a `max()` plus
            // a lookup is two statements against a table a writer is replacing rows in.
            let row = sqlx::query(
                "SELECT bucket, depth_in FROM snow_depth_estimates \
                 WHERE station_id = $1 ORDER BY bucket DESC LIMIT 1",
            )
            .bind(station.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
            estimate(row)
        })
    }

    fn estimate_before<'a>(
        &'a self,
        station: &'a StationKey,
        at: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<Option<DepthEstimate>, StoreError>> {
        Box::pin(async move {
            let row = sqlx::query(
                "SELECT bucket, depth_in FROM snow_depth_estimates \
                 WHERE station_id = $1 AND bucket < $2 ORDER BY bucket DESC LIMIT 1",
            )
            .bind(station.as_str())
            .bind(at)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
            estimate(row)
        })
    }

    fn replace_estimates<'a>(
        &'a self,
        station: &'a StationKey,
        from: DateTime<Utc>,
        rows: &'a [DepthEstimate],
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut tx = self.pool.begin().await.map_err(classify)?;
            sqlx::query("DELETE FROM snow_depth_estimates WHERE station_id = $1 AND bucket >= $2")
                .bind(station.as_str())
                .bind(from)
                .execute(&mut *tx)
                .await
                .map_err(classify)?;
            if !rows.is_empty() {
                let buckets: Vec<DateTime<Utc>> = rows.iter().map(|r| r.bucket).collect();
                let depths: Vec<f32> = rows.iter().map(|r| r.depth_in).collect();
                sqlx::query(
                    "INSERT INTO snow_depth_estimates (station_id, bucket, depth_in) \
                     SELECT $1, e.bucket, e.depth_in \
                     FROM UNNEST($2::timestamptz[], $3::real[]) AS e(bucket, depth_in)",
                )
                .bind(station.as_str())
                .bind(&buckets)
                .bind(&depths)
                .execute(&mut *tx)
                .await
                .map_err(classify)?;
            }
            tx.commit().await.map_err(classify)?;
            Ok(())
        })
    }

    fn estimate_window<'a>(
        &'a self,
        station: &'a StationKey,
        from: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<Option<EstimateWindow>, StoreError>> {
        Box::pin(async move {
            // The first depth is a correlated `ORDER BY bucket LIMIT 1` rather than
            // `min(depth_in)`: the season total is "the maximum, less the depth we started
            // the season at", and on a season that began with snow on the ground those two
            // are different numbers -- taking the minimum would count the existing pack as
            // having fallen this season.
            let row = sqlx::query(
                "SELECT min(bucket), min(depth_in), max(depth_in), \
                        (SELECT depth_in FROM snow_depth_estimates \
                         WHERE station_id = $1 AND bucket >= $2 ORDER BY bucket LIMIT 1) \
                 FROM snow_depth_estimates WHERE station_id = $1 AND bucket >= $2",
            )
            .bind(station.as_str())
            .bind(from)
            .fetch_one(&self.pool)
            .await
            .map_err(classify)?;
            // An aggregate over no rows is one row of NULLs, so the emptiness is read off
            // `min(bucket)` rather than off `fetch_optional`.
            let Some(first_at): Option<DateTime<Utc>> =
                row.try_get(0).map_err(StoreError::rejected)?
            else {
                return Ok(None);
            };
            let min_in: Option<f32> = row.try_get(1).map_err(StoreError::rejected)?;
            let max_in: Option<f32> = row.try_get(2).map_err(StoreError::rejected)?;
            let first_in: Option<f32> = row.try_get(3).map_err(StoreError::rejected)?;
            // `depth_in` is `NOT NULL`, so a bucket exists and these do not: the three
            // are `Some` whenever the first is, and a `None` here means this build and its
            // schema disagree about the column's nullability.
            match (min_in, max_in, first_in) {
                (Some(min_in), Some(max_in), Some(first_in)) => Ok(Some(EstimateWindow {
                    first_at,
                    first_in,
                    min_in,
                    max_in,
                })),
                _ => Err(StoreError::rejected(std::io::Error::other(
                    "snow_depth_estimates.depth_in answered NULL for a bucket that exists, \
                     so this build and the schema disagree about that column being NOT NULL",
                ))),
            }
        })
    }

    fn replace_events<'a>(
        &'a self,
        station: &'a StationKey,
        hours: u32,
        events: &'a [SnowEvent],
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let hours = i32::try_from(hours).map_err(|_| {
                StoreError::rejected(std::io::Error::other(format!(
                    "a chart range of {hours} hours is not one of the four \
                     `snow_events.hours` permits"
                )))
            })?;
            let mut tx = self.pool.begin().await.map_err(classify)?;
            sqlx::query("DELETE FROM snow_events WHERE station_id = $1 AND hours = $2")
                .bind(station.as_str())
                .bind(hours)
                .execute(&mut *tx)
                .await
                .map_err(classify)?;
            if !events.is_empty() {
                let starts: Vec<DateTime<Utc>> = events.iter().map(|e| e.start).collect();
                let ends: Vec<DateTime<Utc>> = events.iter().map(|e| e.end).collect();
                let classes: Vec<&str> = events.iter().map(|e| e.class.as_str()).collect();
                let from_mm: Vec<f32> = events.iter().map(|e| e.start_depth_mm).collect();
                let to_mm: Vec<f32> = events.iter().map(|e| e.end_depth_mm).collect();
                let accum: Vec<Option<f32>> = events.iter().map(|e| e.accumulation_mm).collect();
                sqlx::query(
                    "INSERT INTO snow_events (station_id, hours, start_time, end_time, \
                                              event_type, start_depth_mm, end_depth_mm, \
                                              accumulation_mm, computed_at) \
                     SELECT $1, $2, e.start_time, e.end_time, e.event_type, \
                            e.start_depth_mm, e.end_depth_mm, e.accumulation_mm, now() \
                     FROM UNNEST($3::timestamptz[], $4::timestamptz[], $5::text[], \
                                 $6::real[], $7::real[], $8::real[]) \
                          AS e(start_time, end_time, event_type, start_depth_mm, \
                               end_depth_mm, accumulation_mm)",
                )
                .bind(station.as_str())
                .bind(hours)
                .bind(&starts)
                .bind(&ends)
                .bind(&classes)
                .bind(&from_mm)
                .bind(&to_mm)
                .bind(&accum)
                .execute(&mut *tx)
                .await
                .map_err(classify)?;
            }
            tx.commit().await.map_err(classify)?;
            Ok(())
        })
    }
}

/// One `(bucket, depth_in)` row as a [`DepthEstimate`].
fn estimate(row: Option<sqlx::postgres::PgRow>) -> Result<Option<DepthEstimate>, StoreError> {
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(DepthEstimate {
        bucket: row.try_get(0).map_err(StoreError::rejected)?,
        depth_in: row.try_get(1).map_err(StoreError::rejected)?,
    }))
}
