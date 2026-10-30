-- Does the NEW hierarchy reproduce the OLD daily view's numbers?
--
-- The digests in 30-reconcile.sql compare raw against raw, which proves the readings moved.
-- This compares `observations_1d` — built bottom-up through observations_1m, _5m and _1h —
-- against the old `weather_1d`, which was built in one step directly from raw. So it is the
-- check that catches the class of defect D-11 through D-14 exist for: a mean of means, a
-- float accumulator that is no wider than its column, a count that arrived as 0 instead of
-- NULL. Those are all invisible to a row-count reconciliation and all visible here.
--
-- It runs on the TARGET, against w9.weather_1d_archive, which is the old view's own rows
-- carried over by `migrate-from-go.sh rollup`. That matters: comparing the new hierarchy
-- against a *recomputation* would compare it against this schema's own arithmetic twice.
--
-- MOST OF THIS IS AN EXACT COMPARISON, AND TWO ROWS CANNOT BE
--
-- Twelve of the fourteen statistics below must agree **bit for bit**, and they do: the old
-- view computed them in one operation at double precision (`avg(real)` accumulates in
-- float8 and returns float8) or at the column's own width (`min`, `max`).
--
-- Two cannot, and the reason is in the Go schema rather than in this one:
--
--   period_rain  is `sum(rainincremental)`, and `sum(real)` returns `real`. The old view
--                therefore accumulated a day's worth of additions in a 24-bit mantissa.
--   winddir      is `circular_avg(winddir)`, whose Go state type declares `sin_sum real,
--                cos_sum real, accum real` — so the sine and cosine sums accumulate in
--                float4 as well, and the finaliser divides and takes atan2d in `real` too.
--
-- Both are exactly the defect D-14 widened the new accumulator to avoid, so the NEW value
-- is the accurate one and the check bounds how wrong the OLD one is allowed to be:
-- n * 2^-24 relative, the standard worst-case bound for n additions in that width. These
-- two rows carry tol_model = 'float4_accumulated'; every other row gets zero tolerance.
--
-- Measured in the lab at 2,160 readings per daily bucket: period_rain differs by up to
-- 0.58 in 185,877 (3.1e-6 relative) and winddir by up to 6.1e-5 degrees (1.7e-7). Both
-- round to a *different float32*, in 100% of buckets — which matters for what the frozen
-- API serialises and is recorded as a deviation in docs/data-migration.md section 7.
\pset footer off

TRUNCATE w9.rollup_recon;

-- Only the window where both sides have something to say: buckets that have raw behind them
-- in the imported data AND a row in the old archive.
INSERT INTO w9.rollup_recon (station_id, bucket, metric, stat, old_value, new_value,
                             n_readings, tol_model)
SELECT m.station_id, n.bucket, s.metric, s.stat, s.old_value, s.new_value,
       -- The additions the old accumulator made. The largest count the bucket carries
       -- bounds it: a station that reports nothing has no count at all, and a station that
       -- reports anything reports it as often as it reports everything else.
       greatest(coalesce(n.outtemp_count, 0), coalesce(n.winddir_dir_count, 0),
                coalesce(n.barometer_count, 0), coalesce(n.snowdepth_count, 0), 1),
       s.tol_model
  FROM observations_1d n
  JOIN w9.station_map m ON m.station_id = n.station_id
  JOIN w9.weather_1d_archive a
    ON a.stationname = m.stationname AND a.bucket = n.bucket
 CROSS JOIN LATERAL (VALUES
    -- A mean, finalised from the new level's sum and count, against the old view's avg().
    ('out_temp',   'mean', a.outtemp::double precision,
                           mw_mean(n.outtemp_sum, n.outtemp_count), 'exact'),
    ('out_temp',   'max',  a.max_outtemp::double precision, n.outtemp_max::double precision, 'exact'),
    ('out_temp',   'min',  a.min_outtemp::double precision, n.outtemp_min::double precision, 'exact'),
    ('barometer',  'mean', a.barometer::double precision,
                           mw_mean(n.barometer_sum, n.barometer_count), 'exact'),
    ('barometer',  'max',  a.max_barometer::double precision, n.barometer_max::double precision, 'exact'),
    ('barometer',  'min',  a.min_barometer::double precision, n.barometer_min::double precision, 'exact'),
    ('out_humidity','mean', a.outhumidity::double precision,
                           mw_mean(n.outhumidity_sum, n.outhumidity_count), 'exact'),
    ('wind_speed', 'mean', a.windspeed::double precision,
                           mw_mean(n.windspeed_sum, n.windspeed_count), 'exact'),
    ('wind_speed', 'max',  a.max_windspeed::double precision, n.windspeed_max::double precision, 'exact'),
    -- The angle. The old side is circular_avg(); the new side is three composable columns
    -- finalised by mw_circular_mean. A scalar mean would differ by up to 180 degrees here,
    -- which is why the seed data straddles north (ci/db/verify-seed.sql V-3).
    ('wind_dir',   'circular_mean', a.winddir::double precision,
                           mw_circular_mean(n.winddir_sin_sum, n.winddir_cos_sum, n.winddir_dir_count),
                           'float4_accumulated'),
    -- period_rain: the one legacy key that names a bucket total rather than a value
    -- (W1 assumption 11). A mean would roll up to a different number at every level and a
    -- total must roll up to the same one.
    ('rain_incremental', 'total', a.period_rain::double precision, n.rainincremental_total,
                           'float4_accumulated'),
    ('day_rain',   'max',  a.dayrain::double precision, n.dayrain_max::double precision, 'exact'),
    ('in_temp',    'mean', a.intemp::double precision,
                           mw_mean(n.intemp_sum, n.intemp_count), 'exact'),
    ('snow_depth', 'mean', a.snowdepth::double precision,
                           mw_mean(n.snowdepth_sum, n.snowdepth_count), 'exact')
 ) AS s(metric, stat, old_value, new_value, tol_model);

\echo
\echo '=== R-4  the new hierarchy against the old daily view, per statistic'
SELECT metric, stat, tol_model, verdict, count(*) AS buckets,
       max(abs(old_value - new_value)) AS worst_abs_diff,
       max(abs(old_value - new_value)
           / greatest(abs(old_value), abs(new_value), 1)) AS worst_rel_diff
  FROM w9.rollup_report GROUP BY 1, 2, 3, 4 ORDER BY 4 DESC, 1, 2;

\echo
\echo '=== R-4b every disagreement, with the worst relative error'
SELECT station_id, bucket::date, metric, stat, tol_model, n_readings,
       old_value, new_value, tolerance,
       abs(old_value - new_value) / greatest(abs(old_value), abs(new_value), 1) AS rel_err
  FROM w9.rollup_report
 WHERE verdict <> 'ok'
 ORDER BY rel_err DESC NULLS LAST
 LIMIT 20;

\echo
\echo '=== R-4d  the contract-visible question: after serialisation to float32, which of'
\echo '===       these values would a /span response actually report differently?'
SELECT metric, stat, count(*) AS buckets,
       count(*) FILTER (WHERE old_value::real IS DISTINCT FROM new_value::real)
         AS differ_as_float32
  FROM w9.rollup_report
 GROUP BY 1, 2 HAVING count(*) FILTER (WHERE old_value::real IS DISTINCT FROM new_value::real) > 0
 ORDER BY 1, 2;

\echo
\echo '=== R-4c  and the control: a scalar mean of the wind direction must DISAGREE with the'
\echo '===       vector mean where the directions straddle north. If this returns 0 rows the'
\echo '===       check above passed for the wrong reason — a scalar mean sneaking in would'
\echo '===       also be within 1e-9 of itself.'
SELECT count(*) AS buckets_where_scalar_and_vector_differ
  FROM observations_1d n
 WHERE n.winddir_dir_count IS NOT NULL
   AND abs(mw_circular_mean(n.winddir_sin_sum, n.winddir_cos_sum, n.winddir_dir_count)
           - mw_mean(n.winddir_sin_sum, n.winddir_dir_count)) > 1;

\echo
\echo '=== R-5  the legacy archive import, against the archive it came from. The mean the old'
\echo '===       view stored must come back out of mw_mean(sum, count) unchanged, and the'
\echo '===       angle must come back out of mw_circular_mean unchanged.'
SELECT count(*) AS legacy_rows,
       count(*) FILTER (WHERE mean_ok)     AS mean_exact,
       count(*) FILTER (WHERE angle_ok)    AS angle_exact,
       count(*) FILTER (WHERE extrema_ok)  AS extrema_exact,
       count(*) FILTER (WHERE total_ok)    AS total_exact
  FROM (
    SELECT mw_mean(l.outtemp_sum, l.outtemp_count) IS NOT DISTINCT FROM a.outtemp::double precision
             AS mean_ok,
           abs(coalesce(mw_circular_mean(l.winddir_sin_sum, l.winddir_cos_sum, l.winddir_dir_count), 0)
               - coalesce(a.winddir::double precision, 0)) < 1e-9 AS angle_ok,
           (l.outtemp_max IS NOT DISTINCT FROM a.max_outtemp
            AND l.outtemp_min IS NOT DISTINCT FROM a.min_outtemp) AS extrema_ok,
           l.rainincremental_total IS NOT DISTINCT FROM a.period_rain::double precision
             AS total_ok
      FROM observations_1d_legacy l
      JOIN w9.station_map m ON m.station_id = l.station_id
      JOIN w9.weather_1d_archive a ON a.stationname = m.stationname AND a.bucket = l.bucket
  ) z;
