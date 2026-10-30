-- Measurement 6: what the unique index and a foreign key cost at insert time.
--
-- Both feed a decision — the index is W3 assumption 22's idempotency (decision 11) and the
-- foreign key is the one decision 15 declines — so a single sample is not enough. Three
-- repetitions per configuration, each 43,200 wide rows into a fresh time range so nothing
-- conflicts, reported as a median. The node is shared, and the spread between
-- configurations turns out to be smaller than the spread within one.

\set ON_ERROR_STOP on
\timing off

CREATE TABLE IF NOT EXISTS mw_measure.insert_cost (
    config text NOT NULL, rep int NOT NULL, ms numeric NOT NULL,
    PRIMARY KEY (config, rep));
TRUNCATE mw_measure.insert_cost;

CREATE TABLE IF NOT EXISTS mw_measure.stations_probe (station_id text PRIMARY KEY);
INSERT INTO mw_measure.stations_probe SELECT 'station-' || i FROM generate_series(1, 4) i
  ON CONFLICT DO NOTHING;

-- One configuration, `reps` times. The fill ranges start at `from_day` and step a day, so
-- no repetition ever inserts into a range another one used.
CREATE OR REPLACE PROCEDURE mw_measure.time_inserts(cfg text, from_day date, reps int DEFAULT 3)
LANGUAGE plpgsql AS $$
DECLARE i int; t0 timestamptz; d timestamptz;
BEGIN
    FOR i IN 1..reps LOOP
        d := from_day + (i - 1);
        t0 := clock_timestamp();
        CALL mw_measure.fill(4, d, d + INTERVAL '6 hours');
        INSERT INTO mw_measure.insert_cost (config, rep, ms)
        VALUES (cfg, i, round((extract(epoch FROM clock_timestamp() - t0) * 1000)::numeric, 1));
        COMMIT;
        DELETE FROM observations WHERE time >= d AND time < d + INTERVAL '6 hours';
        COMMIT;
    END LOOP;
END $$;

DROP INDEX IF EXISTS observations_station_id_time_key;
ALTER TABLE observations DROP CONSTRAINT IF EXISTS observations_station_fk;
CALL mw_measure.time_inserts('1. no index, no fk', '2026-12-01');

CREATE UNIQUE INDEX observations_station_id_time_key ON observations (station_id, time DESC);
CALL mw_measure.time_inserts('2. unique index', '2026-12-11');

-- Note what this ALTER itself proves: a foreign key is accepted on a hypertable with
-- compressed chunks. That was the part in doubt, not the insert cost.
\timing on
ALTER TABLE observations ADD CONSTRAINT observations_station_fk
    FOREIGN KEY (station_id) REFERENCES mw_measure.stations_probe (station_id);
\timing off
CALL mw_measure.time_inserts('3. unique index + fk', '2026-12-21');

ALTER TABLE observations DROP CONSTRAINT observations_station_fk;

SELECT config,
       min(ms) AS min_ms,
       round(percentile_cont(0.5) WITHIN GROUP (ORDER BY ms)::numeric, 1) AS median_ms,
       max(ms) AS max_ms,
       round(43200 / (percentile_cont(0.5) WITHIN GROUP (ORDER BY ms) / 1000)) AS rows_per_sec
  FROM mw_measure.insert_cost GROUP BY config ORDER BY config;

-- Leave the dataset as it was found.
DELETE FROM observations WHERE time >= '2026-12-01';
DROP TABLE mw_measure.stations_probe;
