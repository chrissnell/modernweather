-- Measurement 2: how long after a minute boundary is that minute's bucket readable from
-- observations_1m?
--
-- W2a assumption 14 and W3 assumption 21 give this a hard number: `window:rollup` has a
-- 10-second deadline from the minute boundary, and it reads 1-minute buckets. If the
-- bucket is not there in time, `window:rollup`'s period has to grow and W2a's
-- Phase::Aligned needs revisiting — which is a finding, not something to work around.
--
-- Two sessions. mw_measure.live_insert() writes 4 stations at a 2-second cadence at
-- now(), which is what the writer does; mw_measure.watch_buckets() polls the aggregate
-- five times a second and records the wall-clock instant each new bucket first became
-- visible. The lag is then (first_seen - bucket_end), measured, not inferred from the
-- job's schedule.
--
-- The refresh policy under test is the one the W4 migration installs. It is created here
-- rather than assumed so that this file measures a policy and not a hope.

CREATE TABLE IF NOT EXISTS mw_measure.bucket_sightings (
    bucket     timestamptz PRIMARY KEY,
    first_seen timestamptz NOT NULL
);

-- Writes at the real cadence into the real table. Deliberately one INSERT per tick with
-- its own COMMIT: a batched load would hide the invalidation traffic that a live writer
-- actually generates, and it is that traffic the refresh has to keep up with.
CREATE OR REPLACE PROCEDURE mw_measure.live_insert(run_for interval, n_stations int DEFAULT 4)
LANGUAGE plpgsql AS $$
DECLARE
    deadline timestamptz := clock_timestamp() + run_for;
BEGIN
    WHILE clock_timestamp() < deadline LOOP
        INSERT INTO observations (time, station_id, outtemp, barometer, windspeed,
                                  outhumidity, winddir, rainincremental)
        SELECT date_trunc('second', clock_timestamp()), 'station-' || i,
               round((60 + 5 * sin(extract(epoch FROM clock_timestamp()) / 600))::numeric, 1),
               round((29 + sin(extract(epoch FROM clock_timestamp()) / 900))::numeric, 2),
               round((7 + 3 * sin(extract(epoch FROM clock_timestamp()) / 120))::numeric, 1),
               round((55 + 10 * sin(extract(epoch FROM clock_timestamp()) / 700))::numeric, 1),
               round((180 + 90 * sin(extract(epoch FROM clock_timestamp()) / 300))::numeric, 1),
               0.0
          FROM generate_series(1, n_stations) i;
        COMMIT;
        PERFORM pg_sleep(2);
    END LOOP;
END $$;

-- Polls the aggregate and records when each bucket first appears. 0.2 s granularity is
-- fifty times finer than the 10-second deadline being tested.
CREATE OR REPLACE PROCEDURE mw_measure.watch_buckets(run_for interval)
LANGUAGE plpgsql AS $$
DECLARE
    deadline timestamptz := clock_timestamp() + run_for;
    newest   timestamptz;
BEGIN
    WHILE clock_timestamp() < deadline LOOP
        SELECT max(bucket) INTO newest FROM observations_1m
         WHERE bucket > clock_timestamp() - INTERVAL '30 minutes';
        IF newest IS NOT NULL THEN
            INSERT INTO mw_measure.bucket_sightings (bucket, first_seen)
            VALUES (newest, clock_timestamp())
            ON CONFLICT (bucket) DO NOTHING;
            COMMIT;
        END IF;
        PERFORM pg_sleep(0.2);
    END LOOP;
END $$;

-- (bucket + 1 minute) is when the bucket closed; first_seen is when it became readable.
CREATE OR REPLACE VIEW mw_measure.refresh_lag AS
SELECT bucket,
       first_seen,
       round(extract(epoch FROM first_seen - (bucket + INTERVAL '1 minute'))::numeric, 2)
           AS lag_seconds
  FROM mw_measure.bucket_sightings
 ORDER BY bucket;
