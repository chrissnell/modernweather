-- HAND-WRITTEN. Not emitted by `cargo xtask migrate`, and that is the point of it.
--
-- The migration immediately before this one changes `solarjoules` from a mean to a bucket
-- total (N-9). An aggregation-strategy change rewrites the column set of every level that
-- carries the metric, and aggregates are disposable derived data, so the supported change
-- is drop, regenerate, refresh (D-15). The generated migration therefore runs
--
--     DROP MATERIALIZED VIEW IF EXISTS observations_1d CASCADE;   -- and 1h, 5m, 1m
--
-- and CASCADE takes everything hanging off those views with them:
--
--   * the four composite (station_id, bucket DESC) indexes
--   * the three retention policies on 1m, 5m and 1h
--   * all four continuous-aggregate refresh policies
--
-- all of which were created by 20261029234500_w4_station_dimension_caches_indexes_and_
-- policies.sql. The generator does not know they exist: refresh policy, compression and
-- retention are deliberately not generated (generated/sql/0*_observations_*.sql says so
-- in every header, OQ-4), so nothing in `migrate` puts them back.
--
-- The consequence of not re-asserting them is not an error. It is the 1-minute level
-- growing without bound at 24 MB/day of uncompressible data, and `window:rollup` missing
-- its 10-second deadline because no job is materialising the minute bucket any more —
-- both silent. `ci/db/schema-assertions.sql` A-8 is what makes it loud, and it is why
-- this file exists rather than a note in a PR.
--
-- This is a recurring cost, not a one-off: any future registry change that rebuilds a
-- level needs the same follow-up. The structural fix is to make W4's policy block
-- re-assertable the way the finalisers already are — `migrate` emits
-- `CREATE OR REPLACE FUNCTION mw_mean(...)` into every migration for exactly this reason —
-- and that belongs with whoever owns the policy parameters, not with this work item.
--
-- EVERY STATEMENT BELOW IS BYTE-IDENTICAL TO THE ONE IN 20261029234500. That migration is
-- the reasoning; this one is only the re-assertion, and if the two ever disagree that one
-- is right.
--
-- No `IF NOT EXISTS` and no `if_not_exists => true`, deliberately. None of these objects
-- can already exist when this migration runs — sqlx applies migrations in filename order
-- and the one immediately before this drops all four views CASCADE — so a guard would buy
-- nothing and would make these statements differ from the ones CI has already proved apply
-- against a real TimescaleDB.

-- ---------------------------------------------------------------------------
-- The composite indexes. /span's predicate is (station_id, bucket range) and the index
-- TimescaleDB creates for the group-by column is not composite.
-- ---------------------------------------------------------------------------
CREATE INDEX observations_1m_station_bucket_idx ON observations_1m (station_id, bucket DESC);
CREATE INDEX observations_5m_station_bucket_idx ON observations_5m (station_id, bucket DESC);
CREATE INDEX observations_1h_station_bucket_idx ON observations_1h (station_id, bucket DESC);
CREATE INDEX observations_1d_station_bucket_idx ON observations_1d (station_id, bucket DESC);

-- ---------------------------------------------------------------------------
-- Retention. Three levels, and observations_1d unbounded on purpose. No retention policy
-- on `observations` itself, ever (D-4) — nothing here adds one and A-8 fails the build if
-- anything does.
-- ---------------------------------------------------------------------------
SELECT add_retention_policy('observations_1m', drop_after => INTERVAL '7 days');
SELECT add_retention_policy('observations_5m', drop_after => INTERVAL '14 days');
SELECT add_retention_policy('observations_1h', drop_after => INTERVAL '90 days');

-- ---------------------------------------------------------------------------
-- Refresh policies. The minute level's phase is the load-bearing part: `window:rollup`
-- has 10 seconds from the minute boundary to read that minute's bucket, and `initial_start`
-- plus the default fixed schedule is what makes the phase deterministic rather than
-- whatever it happened to be when the policy was created. The upper levels are staggered
-- so that two levels never refresh in the same instant, since each reads the one below it.
-- ---------------------------------------------------------------------------
SELECT add_continuous_aggregate_policy('observations_1m',
    start_offset      => INTERVAL '30 minutes',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '1 minute',
    initial_start     => date_trunc('minute', now()) + INTERVAL '1 minute 3 seconds');

SELECT add_continuous_aggregate_policy('observations_5m',
    start_offset      => INTERVAL '2 hours',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '1 minute',
    initial_start     => date_trunc('minute', now()) + INTERVAL '1 minute 20 seconds');

SELECT add_continuous_aggregate_policy('observations_1h',
    start_offset      => INTERVAL '2 days',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '5 minutes',
    initial_start     => date_trunc('hour', now()) + INTERVAL '5 minutes 30 seconds');

SELECT add_continuous_aggregate_policy('observations_1d',
    start_offset      => INTERVAL '14 days',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '30 minutes',
    initial_start     => date_trunc('hour', now()) + INTERVAL '10 minutes');

-- The views are back WITH NO DATA. Materialising them is ci/db/rebuild-aggregates.sh,
-- which cannot run inside a transaction and must relax commit durability (D-17). A deploy
-- that applies these migrations and skips it leaves /span empty for every band, with no
-- error — run `ci/db/rebuild-aggregates.sh --check-watermarks` afterwards.
