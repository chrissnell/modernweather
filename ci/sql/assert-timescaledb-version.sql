-- DESIGN.md D-42: the required database version is asserted in CI and at startup.
--
-- Three of the storage design's load-bearing spike answers are version-gated, so a server
-- that silently predates them turns a design premise into a runtime surprise:
--
--   * nullable ADD COLUMN on compressed chunks without decompression (TimescaleDB 2.11+,
--     extended since) — the entire "adding a metric is cheap" premise;
--   * a continuous aggregate built on another continuous aggregate (2.9+) — the 1m/1h/1d
--     hierarchy;
--   * stats_agg / rollup, which need timescaledb_toolkit and are absent from the plain
--     timescale/timescaledb images.
--
-- Run with: psql --set ON_ERROR_STOP=1 -f ci/sql/assert-timescaledb-version.sql
\set ON_ERROR_STOP on

DO $$
DECLARE
    required_pg      int  := 17;
    required_ts      text := '2.31.0';
    actual_pg        int;
    actual_ts        text;
    toolkit_present  bool;
BEGIN
    SELECT current_setting('server_version_num')::int / 10000 INTO actual_pg;
    SELECT extversion FROM pg_extension WHERE extname = 'timescaledb' INTO actual_ts;
    SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb_toolkit')
        INTO toolkit_present;

    IF actual_ts IS NULL THEN
        RAISE EXCEPTION 'the timescaledb extension is not installed';
    END IF;

    IF actual_pg < required_pg THEN
        RAISE EXCEPTION 'PostgreSQL % is older than the required %', actual_pg, required_pg;
    END IF;

    -- string_to_array comparison rather than a text comparison, so that 2.9.0 does not
    -- sort above 2.31.0.
    IF string_to_array(actual_ts, '.')::int[] < string_to_array(required_ts, '.')::int[] THEN
        RAISE EXCEPTION 'timescaledb % is older than the required %', actual_ts, required_ts;
    END IF;

    IF NOT toolkit_present THEN
        -- A warning, not an error: W4 owns the sum-and-count versus stats_agg decision
        -- (DESIGN.md OQ-3), and both are correct. This fails only once W4 records that
        -- the schema uses stats_agg, which it does by adding the extension to a migration
        -- — at which point the migration itself is the assertion.
        RAISE WARNING 'timescaledb_toolkit is not installed; stats_agg and rollup are unavailable';
    END IF;

    RAISE NOTICE 'PostgreSQL % / timescaledb % / toolkit % — ok',
        actual_pg, actual_ts, toolkit_present;
END
$$;
