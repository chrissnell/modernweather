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

use mw_component::{
    BoxFuture, Forecast, ForecastQuery, ForecastStore, SnowCache, SnowTotals, StoreError,
};
use mw_types::StationKey;
use sqlx::{PgPool, Row};

use crate::reader::classify;

/// `ForecastStore` and `SnowCache` over one pool.
#[derive(Debug)]
pub struct FrozenReads {
    pool: PgPool,
}

impl FrozenReads {
    /// Both handles over one pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The same object under both of its capabilities.
    ///
    /// One call rather than two upcasts at the call site, so that `db:postgres` cannot
    /// publish two different instances under two capabilities that are meant to be one
    /// pool — which would be invisible until somebody wondered why `/forecast` saw a
    /// connection the probe loop had closed.
    pub fn handles(self: Arc<Self>) -> (Arc<dyn ForecastStore>, Arc<dyn SnowCache>) {
        (
            Arc::clone(&self) as Arc<dyn ForecastStore>,
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
            // `span_hours` is bound as an `i32`: the column is `integer` and the grammar
            // caps the value at four digits, so the conversion cannot be lossy — and
            // binding an `i64` against an `integer` column is a planner cast that defeats
            // the index.
            let span = i32::try_from(q.span_hours).map_err(|_| {
                StoreError::rejected(std::io::Error::other(format!(
                    "a forecast span of {} hours is outside the frozen endpoint's \
                     four-digit grammar, so no row can exist for it",
                    q.span_hours
                )))
            })?;
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
                // No station named. The frozen handler answers with *a* row for the span
                // and the Go one takes gorm's `First()`, which orders by the surrogate
                // `id` this table does not have. Ordered by station key instead, so the
                // answer is at least deterministic across runs; nothing in the contract
                // says which station a stationless request gets, and a configuration with
                // two stations' forecasts for one span and no `?station=` is ambiguous at
                // the request rather than here.
                None => sqlx::query(
                    "SELECT updated_at, data::text FROM forecasts \
                     WHERE span_hours = $1 ORDER BY station_id LIMIT 1",
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
}
