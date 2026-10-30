-- Measurement 1b: the cost of the trailing partial bucket.
--
-- Every /span fixture W1 captured ends with the *current, incomplete* bucket —
-- span-1h's last ts is the minute that was 42 seconds old at capture,
-- span-72h-hourly-with-gap's is the hour that was 56 minutes in, span-720h-daily's is
-- today. A materialized_only continuous aggregate never emits that bucket, and all four
-- levels must be materialized_only because three of them are the source of another. So
-- the read path has to aggregate the tail from raw, and this is what that costs.
--
-- The window is one bucket wide, for one station: 30 raw rows at the minute level, 150 at
-- five minutes, 1,800 at the hour, 43,200 at the day. Measured against a compressed chunk
-- and an uncompressed one, because in production the newest chunk is uncompressed for
-- seven days (D-7's policy) and the tail is always in the newest chunk — so the
-- uncompressed number is the real one and the compressed number is the bound.

CREATE OR REPLACE FUNCTION mw_measure.time_tail(width interval, reps int DEFAULT 5)
RETURNS TABLE (bucket_width text, raw_rows bigint, first_ms numeric, median_ms numeric)
LANGUAGE plpgsql AS $$
DECLARE
    sql     text;
    hi      timestamptz;
    n       bigint;
    ms      numeric[] := '{}';
    i       int;
    started timestamptz;
BEGIN
    SELECT max(time) INTO hi FROM observations;
    -- The same expression set the 1-minute level computes from raw, over one bucket.
    sql := format($q$
        SELECT count(md5(q::text)) FROM (
            SELECT time_bucket(%L, time) AS bucket,
                   mw_mean(sum(outtemp::double precision), nullif(count(outtemp), 0)) AS outtemp,
                   min(outtemp) AS outtemp_min, max(outtemp) AS outtemp_max,
                   mw_mean(sum(barometer::double precision), nullif(count(barometer), 0)) AS barometer,
                   mw_mean(sum(outhumidity::double precision), nullif(count(outhumidity), 0)) AS outhumidity,
                   mw_mean(sum(windspeed::double precision), nullif(count(windspeed), 0)) AS windspeed,
                   max(windspeed) AS windspeed_max,
                   mw_circular_mean(sum(sind(winddir::double precision)),
                                    sum(cosd(winddir::double precision)),
                                    nullif(count(winddir), 0)) AS winddir,
                   sum(rainincremental::double precision) AS period_rain
              FROM observations
             WHERE station_id = $1 AND time >= $2 AND time < $3
             GROUP BY 1) q$q$, width);

    SELECT count(*) INTO n FROM observations
     WHERE station_id = 'station-1' AND time >= hi - width AND time < hi;

    FOR i IN 1..reps LOOP
        started := clock_timestamp();
        EXECUTE sql INTO n USING 'station-1', hi - width, hi;
        ms := ms || round((extract(epoch FROM clock_timestamp() - started) * 1000)::numeric, 2);
    END LOOP;

    RETURN QUERY
    SELECT width::text,
           (SELECT count(*) FROM observations
             WHERE station_id = 'station-1' AND time >= hi - width AND time < hi),
           ms[1],
           (SELECT round((percentile_cont(0.5) WITHIN GROUP (ORDER BY v))::numeric, 2)
              FROM unnest(ms) v);
END $$;

CREATE OR REPLACE FUNCTION mw_measure.tail_all(reps int DEFAULT 5)
RETURNS TABLE (bucket_width text, raw_rows bigint, first_ms numeric, median_ms numeric)
LANGUAGE plpgsql AS $$
DECLARE w interval;
BEGIN
    FOREACH w IN ARRAY ARRAY[INTERVAL '1 minute', INTERVAL '5 minutes',
                             INTERVAL '1 hour', INTERVAL '1 day'] LOOP
        RETURN QUERY SELECT * FROM mw_measure.time_tail(w, reps);
    END LOOP;
END $$;
