-- Preflight, run against the OLD database. Every statement here is a SELECT.
--
-- None of this could be measured from the agent runtime: production is not reachable from
-- the homelab cluster (W0 OQ-1), so these are the owner's to run. Each one answers a
-- question docs/data-migration.md records as open, and the output is meant to be pasted
-- back into the document.
\pset footer off
\echo
\echo '=== P-1  Versions. Settles OQ-1. Below TimescaleDB 2.9 the W4 hierarchy cannot be'
\echo '===      built at all, which makes a server upgrade part of this migration rather'
\echo '===      than an option. docs/database-schema.md 5.2 lists what else moves.'
SELECT version() AS postgresql;
SELECT extname, extversion FROM pg_extension
 WHERE extname IN ('timescaledb', 'timescaledb_toolkit') ORDER BY extname;

\echo
\echo '=== P-2  Is raw retention still firing? This is the number that is still growing.'
\echo '===      A row here means every day converts one more day of raw readings into'
\echo '===      rollup-only history. `next_start` says when the next one goes.'
SELECT job_id, application_name, schedule_interval, next_start,
       config ->> 'drop_after' AS drop_after,
       config ->> 'hypertable_id' AS hypertable_id
  FROM timescaledb_information.jobs
 WHERE proc_name = 'policy_retention'
 ORDER BY job_id;

\echo
\echo '=== P-2b Every policy job, with its last run. A disabled refresh policy on'
\echo '===      weather_1d is the most likely reason the rollup-only history still exists'
\echo '===      at all: on TimescaleDB 2.31 an active one destroys it (see P-3).'
SELECT j.job_id, j.proc_name, j.hypertable_name, j.scheduled, j.config,
       s.last_run_status, s.last_successful_finish
  FROM timescaledb_information.jobs j
  LEFT JOIN timescaledb_information.job_stats s USING (job_id)
 WHERE j.proc_name LIKE 'policy_%'
 ORDER BY j.proc_name, j.job_id;

\echo
\echo '=== P-3  !! THE DANGEROUS ONE !! Pending invalidations over the rollup-only window.'
\echo '===      A continuous aggregate re-materialises an invalidated range from raw. Where'
\echo '===      retention has already dropped the raw, that means DELETING the buckets.'
\echo '===      Measured on TimescaleDB 2.31.0: one refresh policy run removed exactly the'
\echo '===      buckets whose raw had gone, 41 of 83, with no error. If this query returns'
\echo '===      rows whose range overlaps the window that has no raw behind it, the history'
\echo '===      is one policy run from being destroyed and the archive copy is urgent.'
-- The log stores its open ends as INT64 min/max and as the internal -infinity
-- sentinel, none of which to_timestamp() can represent — so a bare to_timestamp() here
-- dies with `timestamp out of range` and reports nothing. Clamped to NULL instead, and
-- NULL means "unbounded in that direction", which for the low end means ALL of history.
SELECT l.materialization_id,
       c.user_view_name,
       CASE WHEN l.lowest_modified_value  BETWEEN -210866803200000000 AND 185331695999999999
            THEN _timescaledb_functions.to_timestamp(l.lowest_modified_value) END  AS invalid_from,
       CASE WHEN l.greatest_modified_value BETWEEN -210866803200000000 AND 185331695999999999
            THEN _timescaledb_functions.to_timestamp(l.greatest_modified_value) END AS invalid_to,
       l.lowest_modified_value   AS raw_lo,
       l.greatest_modified_value AS raw_hi
  FROM _timescaledb_catalog.continuous_aggs_materialization_invalidation_log l
  JOIN _timescaledb_catalog.continuous_agg c ON c.mat_hypertable_id = l.materialization_id
 ORDER BY 1, 5;

\echo
\echo '=== P-3b The same question in the form that decides something: for each level, does a'
\echo '===      pending invalidation reach into the window that has no raw behind it? A'
\echo '===      non-zero count on weather_1d or weather_1h means the next refresh over that'
\echo '===      range — policy or manual — deletes those buckets. Archive FIRST.'
SELECT c.user_view_name,
       r.raw_from::date AS raw_begins,
       count(*)         AS invalidations_reaching_below_raw
  FROM _timescaledb_catalog.continuous_aggs_materialization_invalidation_log l
  JOIN _timescaledb_catalog.continuous_agg c ON c.mat_hypertable_id = l.materialization_id
 CROSS JOIN (SELECT min(time) AS raw_from FROM weather) r
 WHERE l.lowest_modified_value < (extract(epoch from r.raw_from) * 1000000)::bigint
 GROUP BY 1, 2
 ORDER BY 1;

\echo
\echo '=== P-4  Extents. The numbers in docs/data-migration.md are a 2026-10-27 snapshot'
\echo '===      and the raw window moves every day; re-measure before quoting any of them.'
SELECT 'weather' AS relation, count(*) AS rows,
       min(time)::date AS lo, max(time)::date AS hi,
       (max(time)::date - min(time)::date) AS days,
       pg_size_pretty(hypertable_size('weather')) AS size
  FROM weather;
SELECT 'weather_1m' AS relation, count(*) AS rows, min(bucket)::date AS lo,
       max(bucket)::date AS hi, (max(bucket)::date - min(bucket)::date) AS days
  FROM weather_1m
UNION ALL SELECT 'weather_5m', count(*), min(bucket)::date, max(bucket)::date,
       (max(bucket)::date - min(bucket)::date) FROM weather_5m
UNION ALL SELECT 'weather_1h', count(*), min(bucket)::date, max(bucket)::date,
       (max(bucket)::date - min(bucket)::date) FROM weather_1h
UNION ALL SELECT 'weather_1d', count(*), min(bucket)::date, max(bucket)::date,
       (max(bucket)::date - min(bucket)::date) FROM weather_1d;

\echo
\echo '=== P-4b The loss window, per station: daily buckets with no raw behind them. This is'
\echo '===      the ~294-day number, measured rather than remembered.'
SELECT d.stationname,
       count(*) FILTER (WHERE d.bucket < r.lo) AS rollup_only_days,
       min(d.bucket)::date AS rollup_from,
       r.lo::date          AS raw_from,
       max(d.bucket)::date AS rollup_to
  FROM weather_1d d
  JOIN (SELECT stationname, min(time) AS lo FROM weather GROUP BY 1) r USING (stationname)
 GROUP BY d.stationname, r.lo
 ORDER BY d.stationname;

\echo
\echo '=== P-5  The materialised size of the aggregates, which pg_total_relation_size on the'
\echo '===      view cannot see: the rows live in an internal materialisation hypertable.'
\echo '===      This is also why pg_dump -t weather_1d captures a view definition and no data.'
-- hypertable_size, not pg_total_relation_size: the materialisation is itself a
-- hypertable, so the parent relation is empty and pg_total_relation_size reports the
-- 32 kB of catalogue rather than the data.
SELECT view_name,
       pg_size_pretty(hypertable_size(
           format('%I.%I', materialization_hypertable_schema,
                           materialization_hypertable_name)::regclass)) AS materialised_size,
       materialized_only, compression_enabled
  FROM timescaledb_information.continuous_aggregates
 ORDER BY view_name;

\echo
\echo '=== P-6  The hold rate. Settles OQ-2 — it is the single variable that predicts the'
\echo '===      compression ratio, which ranges 1.89x to 13.74x on the same schema (D-10).'
\echo '===      Anything above ~85% means the "16 GB for ten years" figure in the planning'
\echo '===      documents stands.'
SELECT round(100.0 * sum((outtemp = prev)::int) / nullif(count(*), 0), 1) AS pct_unchanged,
       count(*) AS pairs_sampled
  FROM (SELECT outtemp, lag(outtemp) OVER (PARTITION BY stationname ORDER BY time) AS prev
          FROM weather
         WHERE time >= (SELECT max(time) FROM weather) - INTERVAL '7 days') z
 WHERE prev IS NOT NULL;

\echo
\echo '=== P-7  The column gap, source side. Settles the production half of OQ-6; the'
\echo '===      comparison against the registry is `migrate-from-go.sh gap` on the target.'
SELECT count(*) AS columns_in_production_weather
  FROM information_schema.columns
 WHERE table_schema = 'public' AND table_name = 'weather';
SELECT count(*) AS columns_in_weather_1d
  FROM information_schema.columns
 WHERE table_schema = 'public' AND table_name = 'weather_1d';

\echo
\echo '=== P-8  Columns that exist and have never held a value. A declared-but-always-NULL'
\echo '===      column is a different problem from an absent one: the Go daemon has a code'
\echo '===      path writing it that is not working.'
\echo '===      (Run on a sample; a full scan of 19 GB per column is not the point.)'
SELECT 'run ci/db/migrate-from-go/01-preflight-null-columns.sql for this one' AS note;

\echo
\echo '=== P-9  Chunk layout, which sets the import slice size.'
SELECT count(*) AS chunks,
       min(range_start)::date AS lo, max(range_end)::date AS hi,
       (SELECT (range_end - range_start) FROM timescaledb_information.chunks
         WHERE hypertable_name = 'weather' LIMIT 1) AS chunk_interval
  FROM timescaledb_information.chunks WHERE hypertable_name = 'weather';
SELECT count(*) AS compressed_chunks FROM timescaledb_information.chunks
 WHERE hypertable_name = 'weather' AND is_compressed;
