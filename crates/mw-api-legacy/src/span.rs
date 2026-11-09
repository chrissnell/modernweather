//! `/span/{span}` -- the materialised prefix, the trailing bucket, and the 60-second cache.
//!
//! # D-54 and D-55, as code
//!
//! `ReadingStore::span` returns the closed buckets **plus** the still-filling one, in one
//! call, so no caller can forget the tail (**D-55**). The 60-second response cache holds
//! the **prefix only**, because the tail changes at the raw cadence and a cached one would
//! be a stale last point on every chart (**D-54**). The tail costs a measured 0.44-6.89 ms
//! (`ci/db/measure/35-trailing-bucket.sql`), which is what makes paying it per request the
//! right trade.
//!
//! **The cache key is the exact prefix range**, bucket-aligned at the level, rather than
//! `(station, span)`. A looser key would serve a prefix whose oldest and newest buckets are
//! not the ones the request asked for -- at the 1-minute level that is one bucket wrong at
//! each end within the TTL -- and the whole value of caching "the materialised prefix" is
//! that it is the same bytes. With an exact key the TTL bounds staleness of the
//! *materialisation* instead, which is the thing a continuous aggregate's refresh changes.
//!
//! # The band, and the one-year ceiling
//!
//! `<= 6h` reads the 1-minute level, `<= 48h` the 5-minute, `<= 14d` the hour, and anything
//! longer the day -- the Go `switch` verbatim, chosen to keep a response under ~300 points.
//! A span over a year is `400 time span exceeds maximum allowed duration of 1 year`, which
//! is also the reason `/span` could never have served the rollup history **D-70** records
//! as destroyed.
//!
//! # Where the window is measured from
//!
//! `now - span`, **floored to the level's bucket grid**. The Go query is
//! `bucket > NOW() - span` with no alignment, so this can include one additional oldest
//! bucket; **D-55** is the authority -- "`/span/24h` means 24 hours of 5-minute buckets" --
//! and an aligned window is what makes the cache key exact. The conformance suite compares
//! element *shape* rather than element count, so the difference is invisible to it; it is
//! recorded here because it is a byte difference a chart could see as one extra point at
//! the far left.
//!
//! # What this endpoint does not populate
//!
//! **Q-6**: `windgust`, `rainfall24h`, `rainfall48h`, `rainfall72h` and `rainfallstorm` are
//! absent from `transformSpanReadings` and have no `omitempty`, so every element carries
//! them as `0`. Only `/latest` computes them, and they are windows rather than buckets --
//! there is nothing in a 5-minute bucket that could answer "the last 24 hours". Left at the
//! contract's zero, deliberately.
//!
//! `sunrise`, `sunset`, the four lunar keys and the ten `extratext` keys are `omitempty` and
//! unset, so a `/span` element carries 72 keys where `/latest` carries 78.
//!
//! **`extrafloat1` is not overwritten with a smoothed snow depth** (Q-7). The Go handler
//! joins `snow_depth_est_5m`, a view the new schema does not have; the `snow:*` components
//! that would write it are M6-3's. So a snow station's `extrafloat1` carries its own
//! `extrafloat1` here, which is what a station that is not a snow gauge already got.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Response, StatusCode};
use chrono::{DateTime, Utc};
use mw_component::{BucketLevel, Span, SpanBucket, SpanColumn, SpanQuery};
use mw_types::{Agg, MetricKey, StationKey};

use crate::component::Handlers;
use crate::dto::{to_json_line, zf, LegacyWeatherReading};
use crate::params::Params;
use crate::reply::{self, Cache};

/// The `503` this endpoint gives when no website resolves: `/span`, which sets `Content-Type` first (`handlers.go:115`).
const NO_WEBSITE: reply::NoWebsite = reply::NO_WEBSITE_SPAN;

/// The ceiling, which is a `400` rather than a truncation.
const MAX_SPAN: Duration = Duration::from_secs(365 * 24 * 3600);

/// W3 D-40's TTL for the prefix.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// The default `max-age` when there are fewer than two buckets to measure an interval from.
const DEFAULT_MAX_AGE: u32 = 60;

/// How many prefixes the cache holds before the oldest expired entries are dropped.
///
/// A bound rather than a tuning knob: the key includes two instants, so a station charted
/// at four bands produces four live entries and a new set every bucket -- without a bound
/// the map grows for as long as the process runs. Sixty-four is an order of magnitude more
/// than the capture rig's seven stations times four bands can have live at once.
const CACHE_ENTRIES: usize = 64;

/// Serve `/span/{span}`.
pub async fn handle(h: &Handlers, host: &str, span: &str, params: &Params) -> Response<Body> {
    if let Err(response) = h.website(host, NO_WEBSITE) {
        return *response;
    }

    // **`station` is required here**, unlike on `/latest`: there is no primary-device
    // fallback and an absent parameter is a `400`. `station_id` is accepted by both clients
    // and **ignored** by this handler -- only `station` selects data, which fixture
    // `span-snow-device-with-station-id` pins by sending both.
    let Some(name) = params.non_empty("station") else {
        return reply::text(StatusCode::BAD_REQUEST, "station parameter is required");
    };
    let Some(station) = h.directory.station_by_name(name) else {
        return reply::text(StatusCode::NOT_FOUND, "station not found");
    };

    // Go's `time.ParseDuration`, which has no day unit -- `/span/30d` is a `400` and
    // fixture `span-day-unit-rejected` is why that matters.
    let Some(duration) = parse_go_duration(span) else {
        return reply::text(StatusCode::BAD_REQUEST, "error: invalid span duration");
    };
    if duration > MAX_SPAN {
        return reply::text(
            StatusCode::BAD_REQUEST,
            "time span exceeds maximum allowed duration of 1 year",
        );
    }

    let level = band(duration);
    let now = Utc::now();
    let tail_start = floor_to_bucket(now, level);
    let since = floor_to_bucket(
        now - chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::zero()),
        level,
    );

    let Some(store) = h.reading_store() else {
        // The database is down. An empty array is the response the contract already has
        // for a station with no data in the window (fixture `span-empty`), and it is not a
        // `503` -- **D-48** forbids one on this path and the frozen `max-age` still
        // applies.
        tracing::warn!(
            station = %station.key,
            "the reading store is unavailable, so /span answers with an empty array"
        );
        return body(&[], None, DEFAULT_MAX_AGE, &station.name, &station.kind);
    };

    let key = CacheKey {
        station: station.key.clone(),
        level,
        since,
        tail_start,
    };
    // The prefix: from the cache when the range is the same and the entry is inside the
    // TTL, and from the store otherwise. The tail is **never** cached.
    let cached = h.span_cache.get(&key);
    let mut query = SpanQuery {
        station: station.key.clone(),
        level,
        since,
        tail_start,
        columns: SPAN_COLUMNS.to_vec(),
        limit: bucket_limit(duration, level),
    };
    if cached.is_some() {
        // A hit: ask for the tail alone. `since == tail_start` makes the prefix range
        // empty by construction, which is how one method serves both halves (`SpanQuery`).
        query.since = tail_start;
    }
    let answer: Span = match store.span(query).await {
        Ok(answer) => answer,
        Err(e) => {
            tracing::warn!(
                station = %station.key,
                err = %mw_types::DisplayChain(&e),
                "the span query failed"
            );
            return reply::text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "error fetching weather data",
            );
        }
    };

    let prefix = match cached {
        Some(entry) => entry,
        None => {
            let entry = Prefix::render(&answer.prefix, &station.name, &station.kind);
            h.span_cache.put(key, entry.clone());
            entry
        }
    };

    let max_age = max_age_for(&prefix, answer.tail.as_ref(), level);
    body(
        &prefix.elements,
        answer.tail.as_ref(),
        max_age,
        &station.name,
        &station.kind,
    )
}

/// The response: a JSON array of the prefix's elements with the tail appended.
///
/// Assembled from the cached prefix text rather than re-serialised, which is what makes the
/// cache worth having: the prefix is up to 576 elements of 72 keys each.
fn body(
    elements: &[String],
    tail: Option<&SpanBucket>,
    max_age: u32,
    name: &str,
    kind: &str,
) -> Response<Body> {
    let tail_text = match tail {
        None => None,
        Some(bucket) => match render_one(bucket, name, kind) {
            Ok(text) => Some(text),
            Err(()) => {
                return reply::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "error fetching weather data",
                )
            }
        },
    };
    let mut out = String::with_capacity(elements.iter().map(|e| e.len() + 1).sum::<usize>() + 64);
    out.push('[');
    for (i, element) in elements.iter().chain(tail_text.iter()).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(element);
    }
    out.push(']');
    // Q-12: `json.NewEncoder(w).Encode` appends a newline, and this endpoint goes through
    // it. `/forecast` is the one that does not.
    out.push('\n');
    reply::json(StatusCode::OK, Cache::MaxAge(max_age), out.into_bytes())
}

/// `max-age`, computed the way the Go handler computes it.
///
/// The **interval between the first two buckets** minus one second, defaulting to 60 when
/// there are fewer than two. Not the level's bucket width, which would be the obvious
/// thing and is not what ships: across a gap the first two buckets are further apart than
/// one width, and the Go handler takes the difference it sees (Q-9's missing buckets are
/// what produce that). The runner compares `max-age=<n>` as a pattern, so the exact number
/// is not asserted -- it is reproduced because a client may cache on it.
fn max_age_for(prefix: &Prefix, tail: Option<&SpanBucket>, level: BucketLevel) -> u32 {
    let first_two = match (prefix.first_two, prefix.last, tail) {
        (Some(pair), _, _) => Some(pair),
        // One prefix bucket and a tail: the pair is the prefix's bucket and the tail's.
        (None, Some(first), Some(tail)) => Some((first, tail.bucket)),
        _ => None,
    };
    let Some((a, b)) = first_two else {
        return DEFAULT_MAX_AGE;
    };
    let seconds = b.signed_duration_since(a).num_seconds() - 1;
    match u32::try_from(seconds) {
        Ok(n) if n > 0 => n,
        // A non-positive or absurd interval -- two buckets at the same instant, which the
        // view's own primary key makes impossible -- falls back rather than wrapping.
        _ => {
            let _ = level;
            DEFAULT_MAX_AGE
        }
    }
}

/// Which level a span reads, which is the Go `switch` verbatim.
fn band(span: Duration) -> BucketLevel {
    const SIX_HOURS: Duration = Duration::from_secs(6 * 3600);
    const TWO_DAYS: Duration = Duration::from_secs(48 * 3600);
    const FOURTEEN_DAYS: Duration = Duration::from_secs(14 * 24 * 3600);
    if span <= SIX_HOURS {
        BucketLevel::Minute
    } else if span <= TWO_DAYS {
        BucketLevel::FiveMinute
    } else if span <= FOURTEEN_DAYS {
        BucketLevel::Hour
    } else {
        BucketLevel::Day
    }
}

/// An instant floored onto the level's bucket grid.
///
/// `time_bucket` without a zone is a **fixed grid from the epoch**, including at the day
/// level -- so this is integer division on the Unix timestamp and not a calendar
/// truncation. `BucketLevel::span`'s own documentation says so, and it is the reason a
/// station's local day is stitched from day buckets plus hour and minute edges elsewhere
/// rather than being one `date_trunc`.
fn floor_to_bucket(at: DateTime<Utc>, level: BucketLevel) -> DateTime<Utc> {
    let width = level.span().as_secs() as i64;
    let seconds = at.timestamp();
    DateTime::from_timestamp(seconds - seconds.rem_euclid(width), 0).unwrap_or(at)
}

/// How many prefix buckets to ask for.
///
/// The range's own bucket count plus two, which is a bound the request cannot exceed rather
/// than a page size: the widest band the endpoint serves is just under 48 hours of 5-minute
/// buckets, 576 rows. The store caps it again at its own `MAX_ROWS`.
fn bucket_limit(span: Duration, level: BucketLevel) -> usize {
    let width = level.span().as_secs().max(1);
    (span.as_secs() / width) as usize + 2
}

/// The rendered prefix of one span range.
#[derive(Debug, Clone)]
struct Prefix {
    /// One JSON object per bucket, in order.
    elements: Vec<String>,
    /// The first two buckets' instants, for `max-age`.
    first_two: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The first bucket's instant, when there is only one.
    last: Option<DateTime<Utc>>,
}

impl Prefix {
    /// Render a prefix once, so that a cache hit is a string concatenation.
    fn render(buckets: &[SpanBucket], name: &str, kind: &str) -> Self {
        let mut elements = Vec::with_capacity(buckets.len());
        for bucket in buckets {
            // A bucket that would not serialise is dropped rather than failing the
            // response: the alternative on a frozen route is a `500` for one element of an
            // array, and the case is unreachable for a struct of scalars.
            if let Ok(text) = render_one(bucket, name, kind) {
                elements.push(text);
            }
        }
        Self {
            elements,
            first_two: match buckets {
                [a, b, ..] => Some((a.bucket, b.bucket)),
                _ => None,
            },
            last: buckets.first().map(|b| b.bucket),
        }
    }
}

/// One bucket, as the frozen 72-key element.
///
/// `Err(())` rather than a response, because the two callers want different things from a
/// failure: [`body`] turns it into this endpoint's `500` and [`Prefix::render`] drops the
/// element. Unreachable either way -- the DTO is 72 scalars and two strings -- which is why
/// it is a unit rather than a reason.
fn render_one(bucket: &SpanBucket, name: &str, kind: &str) -> Result<String, ()> {
    let dto = element(bucket, name, kind);
    let bytes = to_json_line(&dto).map_err(|_| ())?;
    // The trailing newline the writer appends is not wanted inside an array, so it is
    // trimmed here rather than giving `to_json_line` a flag: one function with one
    // behaviour, and the array's own newline is added by `body`.
    String::from_utf8(bytes)
        .map(|s| s.trim_end_matches('\n').to_string())
        .map_err(|_| ())
}

/// One bucket's values, by `(metric, agg)` rather than by position.
///
/// A linear scan over at most sixty entries, which is what makes the mapping below readable:
/// the alternative is sixty index constants that have to stay in step with
/// [`SPAN_COLUMNS`], and a transposed pair there is a wrong number nothing would catch.
struct Row<'a> {
    values: &'a [Option<f64>],
}

impl Row<'_> {
    /// One column's value, zero-filled.
    fn at(&self, metric: MetricKey, agg: Agg) -> f32 {
        let value = SPAN_COLUMNS
            .iter()
            .position(|c| *c == (metric, agg))
            .and_then(|i| self.values.get(i).copied().flatten());
        zf(value.map(|v| v as f32))
    }

    /// One column's value, unfilled -- for the two keys whose branch needs to know whether
    /// the column was there.
    fn raw(&self, metric: MetricKey, agg: Agg) -> Option<f64> {
        SPAN_COLUMNS
            .iter()
            .position(|c| *c == (metric, agg))
            .and_then(|i| self.values.get(i).copied().flatten())
    }
}

/// Millimetres to inches, with the Go threshold and rounding.
///
/// Below a tenth of an inch is **zero**, and the result is rounded to tenths
/// (`types.go:199`). Both are the contract: the gauge's noise floor is why the threshold
/// exists, and a chart that drew 0.04 in of "snow" on a bare roof is what it prevents.
fn mm_to_inches(mm: f32) -> f32 {
    let inches = mm / 25.4;
    if inches < 0.1 {
        return 0.0;
    }
    (f64::from(inches) * 10.0).round() as f32 / 10.0
}

/// One `/span` element.
fn element(bucket: &SpanBucket, name: &str, kind: &str) -> LegacyWeatherReading {
    let row = Row {
        values: &bucket.values,
    };
    let mean = |m: MetricKey| row.at(m, Agg::Mean);
    let out_temp = mean(MetricKey::OutTemp);
    let wind_speed = mean(MetricKey::WindSpeed);
    let out_humidity = mean(MetricKey::OutHumidity);
    let wind_dir = row.at(MetricKey::WindDir, Agg::CircularMean);
    let day_rain = row.at(MetricKey::DayRain, Agg::Max);
    let period_rain = row.at(MetricKey::RainIncremental, Agg::Sum);

    LegacyWeatherReading {
        station_name: name.to_string(),
        station_type: kind.to_string(),
        // Q-3 again, over the BUCKET rather than the reading's own instant.
        reading_timestamp: bucket.bucket.timestamp_millis(),
        outside_temperature: out_temp,
        extra_temp1: mean(MetricKey::ExtraTemp1),
        extra_temp2: mean(MetricKey::ExtraTemp2),
        extra_temp3: mean(MetricKey::ExtraTemp3),
        extra_temp4: mean(MetricKey::ExtraTemp4),
        extra_temp5: mean(MetricKey::ExtraTemp5),
        extra_temp6: mean(MetricKey::ExtraTemp6),
        extra_temp7: mean(MetricKey::ExtraTemp7),
        soil_temp1: mean(MetricKey::SoilTemp1),
        soil_temp2: mean(MetricKey::SoilTemp2),
        soil_temp3: mean(MetricKey::SoilTemp3),
        soil_temp4: mean(MetricKey::SoilTemp4),
        leaf_temp1: mean(MetricKey::LeafTemp1),
        leaf_temp2: mean(MetricKey::LeafTemp2),
        leaf_temp3: mean(MetricKey::LeafTemp3),
        leaf_temp4: mean(MetricKey::LeafTemp4),
        out_humidity,
        extra_humidity1: mean(MetricKey::ExtraHumidity1),
        extra_humidity2: mean(MetricKey::ExtraHumidity2),
        extra_humidity3: mean(MetricKey::ExtraHumidity3),
        extra_humidity4: mean(MetricKey::ExtraHumidity4),
        extra_humidity5: mean(MetricKey::ExtraHumidity5),
        extra_humidity6: mean(MetricKey::ExtraHumidity6),
        extra_humidity7: mean(MetricKey::ExtraHumidity7),
        // **Q-1**: one value, two keys.
        outside_humidity: out_humidity,
        rain_rate: mean(MetricKey::RainRate),
        // The bucket's own increment total. The Go views carry it under `period_rain`
        // alone, and `rainincremental` is not a view column there at all -- so the Go
        // response has the total under one key and a zero under the other. Both carry it
        // here, which is the same number under a name the contract already has for it.
        rain_incremental: period_rain,
        period_rain,
        solar_watts: mean(MetricKey::SolarWatts),
        potential_solar_watts: mean(MetricKey::PotentialSolarWatts),
        // **DEV-7**: the bucket TOTAL rather than the mean of per-reading increments.
        solar_joules: row.at(MetricKey::SolarJoules, Agg::Sum),
        uv: mean(MetricKey::Uv),
        radiation: mean(MetricKey::Radiation),
        // The three station-reported running totals are `max` over the bucket, which is
        // the aggregate the Go views use for them and the only one that means anything for
        // a monotonic counter.
        storm_rain: row.at(MetricKey::StormRain, Agg::Max),
        day_rain,
        month_rain: row.at(MetricKey::MonthRain, Agg::Max),
        year_rain: row.at(MetricKey::YearRain, Agg::Max),
        barometer: mean(MetricKey::Barometer),
        wind_speed,
        wind_direction: wind_dir,
        cardinal_direction: cardinal(wind_dir).to_string(),
        // **Q-1**: `dayrain` again.
        rainfall_day: day_rain,
        // Recomputed from the bucket's means, which is **DEV-9**'s constraint: the
        // contract emits `windchill(mean(outtemp), mean(windspeed))` and not
        // `mean(windchill)`, and the two differ by 12 degrees for a two-reading bucket.
        wind_chill: zf(mw_pipeline::enrich::wind_chill(out_temp, wind_speed)),
        heat_index: zf(mw_pipeline::enrich::heat_index(out_temp, out_humidity)),
        inside_temperature: mean(MetricKey::InTemp),
        inside_humidity: mean(MetricKey::InHumidity),
        cons_battery_voltage: mean(MetricKey::ConsBatteryVoltage),
        station_battery_voltage: mean(MetricKey::StationBatteryVoltage),
        // **Q-4**: inches here, millimetres on `/latest`. The column is millimetres and
        // this is the conversion `transformSpanReadings` applies.
        snow_depth: mm_to_inches(mean(MetricKey::SnowDepth)),
        snow_distance: mean(MetricKey::SnowDistance),
        pm25: mean(MetricKey::Pm25),
        co2: mean(MetricKey::Co2),
        aqi_pm25_aqin: aqi(
            row.raw(MetricKey::AqiPm25Aqin, Agg::Mean),
            row.raw(MetricKey::Pm25, Agg::Mean),
            mw_pipeline::enrich::aqi_pm25,
        ),
        aqi_pm10_aqin: aqi(
            row.raw(MetricKey::AqiPm10Aqin, Agg::Mean),
            row.raw(MetricKey::Pm10InAqin, Agg::Mean),
            mw_pipeline::enrich::aqi_pm10,
        ),
        extra_float1: mean(MetricKey::ExtraFloat1),
        extra_float2: mean(MetricKey::ExtraFloat2),
        extra_float3: mean(MetricKey::ExtraFloat3),
        extra_float4: mean(MetricKey::ExtraFloat4),
        extra_float5: mean(MetricKey::ExtraFloat5),
        extra_float6: mean(MetricKey::ExtraFloat6),
        extra_float7: mean(MetricKey::ExtraFloat7),
        extra_float8: mean(MetricKey::ExtraFloat8),
        extra_float9: mean(MetricKey::ExtraFloat9),
        extra_float10: mean(MetricKey::ExtraFloat10),
        // Q-6: the five keys `/span` never populates, left at the contract's zero. And the
        // sixteen `omitempty` keys -- ten `extratext`, two sun times, four lunar -- left
        // unset, which is what makes a `/span` element 72 keys rather than 78.
        ..Default::default()
    }
}

/// `transform.go:209`'s AQI branch, per bucket.
///
/// The stored column when it is positive, the formula over the particulate mean otherwise.
/// Called per bucket in Go (`transform.go:80`), so a bucket whose AQIN device was offline
/// gets the computed value rather than a zero -- which is why a client was never charting a
/// flat zero here.
fn aqi(stored: Option<f64>, particulate: Option<f64>, compute: fn(f32) -> Option<i32>) -> f32 {
    if let Some(value) = stored.filter(|v| *v > 0.0) {
        return value as f32;
    }
    match particulate.filter(|v| *v > 0.0) {
        Some(value) => zf(compute(value as f32).map(|v| v as f32)),
        None => zf(None),
    }
}

/// Go's `headingToCardinalDirection`, shared with [`crate::render`] through a crate-private
/// re-export rather than copied: one table, one rounding rule.
fn cardinal(heading: f32) -> &'static str {
    crate::render::cardinal_for_span(heading)
}

/// Every `(metric, agg)` one `/span` element reads, in the order the store answers them.
///
/// The **order** is this list's own and is not the DTO's: [`Row::at`] looks a pair up by
/// name, so the two cannot be transposed. What the list has to be complete about is the
/// *set* -- a pair the DTO reads and this list omits is a key that is silently zero for
/// every bucket, which is the defect
/// `every_column_the_element_reads_is_declared` exists to catch.
pub const SPAN_COLUMNS: [SpanColumn; 59] = [
    (MetricKey::OutTemp, Agg::Mean),
    (MetricKey::ExtraTemp1, Agg::Mean),
    (MetricKey::ExtraTemp2, Agg::Mean),
    (MetricKey::ExtraTemp3, Agg::Mean),
    (MetricKey::ExtraTemp4, Agg::Mean),
    (MetricKey::ExtraTemp5, Agg::Mean),
    (MetricKey::ExtraTemp6, Agg::Mean),
    (MetricKey::ExtraTemp7, Agg::Mean),
    (MetricKey::SoilTemp1, Agg::Mean),
    (MetricKey::SoilTemp2, Agg::Mean),
    (MetricKey::SoilTemp3, Agg::Mean),
    (MetricKey::SoilTemp4, Agg::Mean),
    (MetricKey::LeafTemp1, Agg::Mean),
    (MetricKey::LeafTemp2, Agg::Mean),
    (MetricKey::LeafTemp3, Agg::Mean),
    (MetricKey::LeafTemp4, Agg::Mean),
    (MetricKey::OutHumidity, Agg::Mean),
    (MetricKey::ExtraHumidity1, Agg::Mean),
    (MetricKey::ExtraHumidity2, Agg::Mean),
    (MetricKey::ExtraHumidity3, Agg::Mean),
    (MetricKey::ExtraHumidity4, Agg::Mean),
    (MetricKey::ExtraHumidity5, Agg::Mean),
    (MetricKey::ExtraHumidity6, Agg::Mean),
    (MetricKey::ExtraHumidity7, Agg::Mean),
    (MetricKey::RainRate, Agg::Mean),
    (MetricKey::RainIncremental, Agg::Sum),
    (MetricKey::SolarWatts, Agg::Mean),
    (MetricKey::PotentialSolarWatts, Agg::Mean),
    (MetricKey::SolarJoules, Agg::Sum),
    (MetricKey::Uv, Agg::Mean),
    (MetricKey::Radiation, Agg::Mean),
    (MetricKey::StormRain, Agg::Max),
    (MetricKey::DayRain, Agg::Max),
    (MetricKey::MonthRain, Agg::Max),
    (MetricKey::YearRain, Agg::Max),
    (MetricKey::Barometer, Agg::Mean),
    (MetricKey::WindSpeed, Agg::Mean),
    (MetricKey::WindDir, Agg::CircularMean),
    (MetricKey::InTemp, Agg::Mean),
    (MetricKey::InHumidity, Agg::Mean),
    (MetricKey::ConsBatteryVoltage, Agg::Mean),
    (MetricKey::StationBatteryVoltage, Agg::Mean),
    (MetricKey::SnowDepth, Agg::Mean),
    (MetricKey::SnowDistance, Agg::Mean),
    (MetricKey::Pm25, Agg::Mean),
    (MetricKey::Co2, Agg::Mean),
    (MetricKey::AqiPm25Aqin, Agg::Mean),
    (MetricKey::AqiPm10Aqin, Agg::Mean),
    (MetricKey::Pm10InAqin, Agg::Mean),
    (MetricKey::ExtraFloat1, Agg::Mean),
    (MetricKey::ExtraFloat2, Agg::Mean),
    (MetricKey::ExtraFloat3, Agg::Mean),
    (MetricKey::ExtraFloat4, Agg::Mean),
    (MetricKey::ExtraFloat5, Agg::Mean),
    (MetricKey::ExtraFloat6, Agg::Mean),
    (MetricKey::ExtraFloat7, Agg::Mean),
    (MetricKey::ExtraFloat8, Agg::Mean),
    (MetricKey::ExtraFloat9, Agg::Mean),
    (MetricKey::ExtraFloat10, Agg::Mean),
];

/// Which prefix an entry caches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    station: StationKey,
    level: BucketLevel,
    since: DateTime<Utc>,
    tail_start: DateTime<Utc>,
}

/// `/span`'s 60-second prefix cache (**D-54**).
///
/// A `Mutex` rather than an `ArcSwap`, and the reason is that this is a *map with
/// insertion* rather than a value with replacement: every request may add an entry, so a
/// copy-on-write publish would copy the whole map per request. The critical section is a
/// hash lookup and there is no `await` inside it, so a request never holds the lock across
/// a query.
#[derive(Debug, Default)]
pub struct SpanCache {
    entries: Mutex<HashMap<CacheKey, (Instant, Prefix)>>,
}

impl SpanCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached prefix for an exact range, if it is inside the TTL.
    fn get(&self, key: &CacheKey) -> Option<Prefix> {
        let entries = self.entries.lock().ok()?;
        let (at, prefix) = entries.get(key)?;
        (at.elapsed() < CACHE_TTL).then(|| prefix.clone())
    }

    /// Cache a prefix, dropping expired entries when the map is at its bound.
    fn put(&self, key: CacheKey, prefix: Prefix) {
        let Ok(mut entries) = self.entries.lock() else {
            // A poisoned mutex means a previous holder panicked inside the critical
            // section, which is a hash insert. Serving without the cache is the right
            // answer on a frozen route; the alternative is propagating a panic.
            return;
        };
        if entries.len() >= CACHE_ENTRIES {
            entries.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            // Still full: every entry is live, so the oldest one goes. A map at the bound
            // with every entry live means more bands are being charted than the bound
            // allows for, and dropping the oldest is a cache miss rather than an error.
            if entries.len() >= CACHE_ENTRIES {
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(k, _)| k.clone())
                {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(key, (Instant::now(), prefix));
    }
}

/// Go's `time.ParseDuration`, which is the grammar `/span/{span}` is in.
///
/// A sequence of `[-+]?` then one or more `<number><unit>` pairs, where the number may have
/// a decimal point and the unit is one of `ns`, `us`, `µs`, `μs`, `ms`, `s`, `m`, `h`.
/// `"0"` on its own is valid.
///
/// **There is no day unit**, which is the whole reason this is not a loose parse:
/// `/span/30d` must be `400 error: invalid span duration` (fixture
/// `span-day-unit-rejected`), and a parser that accepted `d` would make a 30-day chart
/// work on the new server and fail on the old one.
///
/// A negative duration parses, as it does in Go. The window is then in the future and the
/// response is an empty array, which is what the Go query produces for it.
pub fn parse_go_duration(text: &str) -> Option<Duration> {
    let mut s = text;
    let negative = match s.as_bytes().first() {
        Some(b'-') => {
            s = &s[1..];
            true
        }
        Some(b'+') => {
            s = &s[1..];
            false
        }
        _ => false,
    };
    if s == "0" {
        return Some(Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let mut nanos = 0f64;
    while !s.is_empty() {
        // The number: digits, optionally a point, optionally more digits. At least one
        // digit somewhere, which is what rejects a bare unit like `"h"`.
        let digits = s
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count();
        if digits == 0 {
            return None;
        }
        let (number, rest) = s.split_at(digits);
        if number == "." || number.matches('.').count() > 1 {
            return None;
        }
        let value: f64 = number.parse().ok()?;
        // The unit, longest first so that `ms` is not read as `m` followed by `s`.
        let (unit_nanos, unit_len) = unit(rest)?;
        nanos += value * unit_nanos;
        s = &rest[unit_len..];
    }
    if negative {
        // Go returns a negative `Duration`; `std::time::Duration` is unsigned, so a
        // negative span becomes zero -- and a zero span reads the same empty window a
        // negative one does, which is the response Go gives for it.
        return Some(Duration::ZERO);
    }
    if !nanos.is_finite() || nanos < 0.0 {
        return None;
    }
    Some(Duration::from_nanos(nanos as u64))
}

/// One unit suffix: its length in nanoseconds and its length in bytes.
fn unit(s: &str) -> Option<(f64, usize)> {
    for (name, nanos) in [
        ("ns", 1.0),
        ("us", 1_000.0),
        ("µs", 1_000.0),
        ("μs", 1_000.0),
        ("ms", 1_000_000.0),
        ("s", 1_000_000_000.0),
        ("m", 60_000_000_000.0),
        ("h", 3_600_000_000_000.0),
    ] {
        if s.starts_with(name) {
            return Some((nanos, name.len()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's grammar, including the three cases the fixtures assert.
    #[test]
    fn the_duration_grammar_is_gos() {
        assert_eq!(parse_go_duration("24h"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_go_duration("1h"), Some(Duration::from_secs(3_600)));
        assert_eq!(parse_go_duration("90m"), Some(Duration::from_secs(5_400)));
        assert_eq!(parse_go_duration("1h30m"), Some(Duration::from_secs(5_400)));
        assert_eq!(
            parse_go_duration("720h"),
            Some(Duration::from_secs(2_592_000))
        );
        assert_eq!(parse_go_duration("1.5h"), Some(Duration::from_secs(5_400)));
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_go_duration("500ms"), Some(Duration::from_millis(500)));

        // **No day unit** -- fixture `span-day-unit-rejected`.
        assert_eq!(parse_go_duration("30d"), None);
        assert_eq!(parse_go_duration("1d"), None);
        // Fixture `span-invalid-duration`.
        assert_eq!(parse_go_duration("notaduration"), None);
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("h"), None);
        assert_eq!(parse_go_duration("24"), None, "a number with no unit");
        assert_eq!(parse_go_duration("24hh"), None);
        assert_eq!(parse_go_duration("1.2.3h"), None);
    }

    /// The ceiling is a `400` and `9000h` is over it -- fixture `span-exceeds-one-year`.
    #[test]
    fn nine_thousand_hours_is_over_the_one_year_ceiling() {
        let span = parse_go_duration("9000h").expect("a valid duration");
        assert!(span > MAX_SPAN);
        assert!(parse_go_duration("8760h").expect("a year") <= MAX_SPAN);
    }

    /// The Go `switch`, at each boundary.
    #[test]
    fn the_band_is_the_go_switch() {
        let at = |s: &str| band(parse_go_duration(s).expect(s));
        assert_eq!(at("1h"), BucketLevel::Minute);
        assert_eq!(at("6h"), BucketLevel::Minute, "6h inclusive");
        assert_eq!(at("6h1m"), BucketLevel::FiveMinute);
        assert_eq!(at("24h"), BucketLevel::FiveMinute);
        assert_eq!(at("48h"), BucketLevel::FiveMinute, "48h inclusive");
        assert_eq!(at("49h"), BucketLevel::Hour);
        assert_eq!(at("72h"), BucketLevel::Hour);
        assert_eq!(at("336h"), BucketLevel::Hour, "14 days inclusive");
        assert_eq!(at("337h"), BucketLevel::Day);
        assert_eq!(at("720h"), BucketLevel::Day);
    }

    /// **Every `(metric, agg)` the element reads has to be in [`SPAN_COLUMNS`]**, or the
    /// key is silently zero for every bucket.
    ///
    /// Driven by giving each declared column a distinct value and asserting that the
    /// element carries it: a pair the element reads and the list omits reads back as the
    /// zero-fill, which this catches by never producing one.
    #[test]
    fn every_column_the_element_reads_is_declared() {
        let values: Vec<Option<f64>> = (0..SPAN_COLUMNS.len())
            .map(|i| Some(i as f64 + 1.0))
            .collect();
        let bucket = SpanBucket {
            bucket: Utc::now(),
            values,
        };
        let dto = element(&bucket, "davis-full", "davis");
        let index = |metric: MetricKey, agg: Agg| -> f32 {
            SPAN_COLUMNS
                .iter()
                .position(|c| *c == (metric, agg))
                .map(|i| i as f32 + 1.0)
                .unwrap_or_else(|| panic!("{metric:?}/{agg:?} is not declared"))
        };
        assert_eq!(
            dto.outside_temperature,
            index(MetricKey::OutTemp, Agg::Mean)
        );
        assert_eq!(dto.soil_temp1, index(MetricKey::SoilTemp1, Agg::Mean));
        assert_eq!(dto.leaf_temp4, index(MetricKey::LeafTemp4, Agg::Mean));
        assert_eq!(
            dto.extra_humidity7,
            index(MetricKey::ExtraHumidity7, Agg::Mean)
        );
        assert_eq!(dto.barometer, index(MetricKey::Barometer, Agg::Mean));
        assert_eq!(
            dto.wind_direction,
            index(MetricKey::WindDir, Agg::CircularMean)
        );
        assert_eq!(dto.day_rain, index(MetricKey::DayRain, Agg::Max));
        assert_eq!(dto.rainfall_day, dto.day_rain, "Q-1");
        assert_eq!(dto.month_rain, index(MetricKey::MonthRain, Agg::Max));
        assert_eq!(dto.year_rain, index(MetricKey::YearRain, Agg::Max));
        assert_eq!(dto.storm_rain, index(MetricKey::StormRain, Agg::Max));
        assert_eq!(dto.period_rain, index(MetricKey::RainIncremental, Agg::Sum));
        assert_eq!(dto.rain_incremental, dto.period_rain);
        assert_eq!(dto.solar_joules, index(MetricKey::SolarJoules, Agg::Sum));
        assert_eq!(dto.extra_float10, index(MetricKey::ExtraFloat10, Agg::Mean));
        assert_eq!(dto.out_humidity, index(MetricKey::OutHumidity, Agg::Mean));
        assert_eq!(dto.outside_humidity, dto.out_humidity, "Q-1");
        assert_eq!(dto.inside_temperature, index(MetricKey::InTemp, Agg::Mean));
        assert_eq!(dto.pm25, index(MetricKey::Pm25, Agg::Mean));
        assert_eq!(dto.co2, index(MetricKey::Co2, Agg::Mean));
        assert_eq!(dto.snow_distance, index(MetricKey::SnowDistance, Agg::Mean));

        // **Q-6**: the five keys `/span` never populates stay at zero even when every
        // column carried a value.
        assert_eq!(dto.wind_gust, 0.0);
        assert_eq!(dto.rainfall24h, 0.0);
        assert_eq!(dto.rainfall48h, 0.0);
        assert_eq!(dto.rainfall72h, 0.0);
        assert_eq!(dto.rainfall_storm, 0.0);
    }

    /// A `/span` element is 72 keys: the 88 the DTO declares, minus the ten `extratext`,
    /// the two sun times and the four lunar keys, all of which are `omitempty` and unset.
    #[test]
    fn a_span_element_carries_seventy_two_keys() {
        let bucket = SpanBucket {
            bucket: Utc::now(),
            values: vec![Some(1.0); SPAN_COLUMNS.len()],
        };
        let text = render_one(&bucket, "davis-full", "davis").expect("renders");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let keys = value.as_object().expect("an object").len();
        assert_eq!(keys, 72, "{text}");
        assert!(!text.ends_with('\n'), "an array element carries no newline");
    }

    /// Q-4: inches, with the tenth-of-an-inch threshold and the tenths rounding.
    #[test]
    fn the_snow_depth_is_inches_with_the_go_threshold() {
        assert_eq!(mm_to_inches(0.0), 0.0);
        assert_eq!(mm_to_inches(2.0), 0.0, "0.079 in is below the threshold");
        assert_eq!(mm_to_inches(25.4), 1.0);
        assert_eq!(mm_to_inches(119.38), 4.7);
        assert_eq!(mm_to_inches(2.54), 0.1, "exactly the threshold");
    }

    /// `max-age` is the first two buckets' interval minus one second, which is what the Go
    /// handler computes and what three fixtures recorded (`299`, `3599`, `86399`).
    #[test]
    fn the_cache_control_is_the_bucket_interval_minus_a_second() {
        let at = |secs: i64| DateTime::from_timestamp(secs, 0).expect("a real instant");
        let prefix = Prefix {
            elements: vec![String::new(), String::new()],
            first_two: Some((at(0), at(300))),
            last: Some(at(0)),
        };
        assert_eq!(max_age_for(&prefix, None, BucketLevel::FiveMinute), 299);

        let hourly = Prefix {
            elements: vec![String::new(), String::new()],
            first_two: Some((at(0), at(3600))),
            last: Some(at(0)),
        };
        assert_eq!(max_age_for(&hourly, None, BucketLevel::Hour), 3599);

        // Fewer than two buckets: the default, which is what `span-empty` recorded.
        let empty = Prefix {
            elements: Vec::new(),
            first_two: None,
            last: None,
        };
        assert_eq!(
            max_age_for(&empty, None, BucketLevel::FiveMinute),
            DEFAULT_MAX_AGE
        );

        // One prefix bucket and a tail: the pair is those two.
        let one = Prefix {
            elements: vec![String::new()],
            first_two: None,
            last: Some(at(0)),
        };
        let tail = SpanBucket {
            bucket: at(60),
            values: Vec::new(),
        };
        assert_eq!(max_age_for(&one, Some(&tail), BucketLevel::Minute), 59);
    }

    /// The bucket grid is a fixed interval from the epoch, including at the day level.
    #[test]
    fn an_instant_floors_onto_the_levels_grid() {
        let at = |secs: i64| DateTime::from_timestamp(secs, 0).expect("a real instant");
        assert_eq!(
            floor_to_bucket(at(1_793_200_123), BucketLevel::Minute),
            at(1_793_200_080)
        );
        assert_eq!(
            floor_to_bucket(at(1_793_200_123), BucketLevel::FiveMinute),
            at(1_793_199_900)
        );
        assert_eq!(
            floor_to_bucket(at(1_793_200_123), BucketLevel::Hour),
            at(1_793_199_600)
        );
        assert_eq!(
            floor_to_bucket(at(1_793_200_123), BucketLevel::Day),
            at(1_793_145_600)
        );
        // Already aligned stays put.
        assert_eq!(
            floor_to_bucket(at(1_793_145_600), BucketLevel::Day),
            at(1_793_145_600)
        );
    }

    /// The cache is keyed on the exact range, so a different range is a miss rather than a
    /// wrong answer -- and an entry past the TTL is a miss too.
    #[test]
    fn the_cache_is_keyed_on_the_exact_prefix_range() {
        let cache = SpanCache::new();
        let at = |secs: i64| DateTime::from_timestamp(secs, 0).expect("a real instant");
        let key = |since: i64| CacheKey {
            station: StationKey::new("davis-full"),
            level: BucketLevel::FiveMinute,
            since: at(since),
            tail_start: at(1_000_000),
        };
        let prefix = Prefix {
            elements: vec!["{}".into()],
            first_two: None,
            last: Some(at(0)),
        };
        assert!(cache.get(&key(0)).is_none(), "empty");
        cache.put(key(0), prefix.clone());
        assert!(cache.get(&key(0)).is_some(), "the same range hits");
        assert!(cache.get(&key(300)).is_none(), "a different range misses");

        // The bound holds: more distinct ranges than `CACHE_ENTRIES` does not grow without
        // limit.
        for i in 0..(CACHE_ENTRIES as i64 * 3) {
            cache.put(key(i * 300), prefix.clone());
        }
        let len = cache.entries.lock().expect("not poisoned").len();
        assert!(len <= CACHE_ENTRIES, "{len} entries");
    }

    /// The limit covers the widest band the endpoint serves.
    #[test]
    fn the_bucket_limit_covers_the_range() {
        let at = |s: &str| {
            let span = parse_go_duration(s).expect(s);
            bucket_limit(span, band(span))
        };
        assert_eq!(at("6h"), 362, "six hours of minutes");
        assert_eq!(at("48h"), 578, "forty-eight hours of five-minute buckets");
        assert_eq!(at("336h"), 338, "fourteen days of hours");
        assert_eq!(at("8760h"), 367, "a year of days");
    }
}
