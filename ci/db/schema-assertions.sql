-- Schema assertions that need a real TimescaleDB, run by ci/db/migrate-twice.sh against
-- both migration paths. Every one of these is a property that reading the generated SQL
-- does not reveal, and three of them are here because they were violated.
--
-- Raises on the first failure, so psql's ON_ERROR_STOP turns any of them into a red gate.

\set ON_ERROR_STOP on

-- ---------------------------------------------------------------------------
-- A-1. Every aggregate level has the column types generated/rust/aggregates.rs
--      says it has.
--
-- This one caught a live defect. sum(bigint) returns numeric, so
-- `nullif(sum(c_count), 0)` without a cast gives every level above the first a
-- *numeric* count while the generated Rust declares bigint. mw_mean is
-- (double precision, bigint) and numeric does not implicitly narrow, so
-- `mw_mean(c_sum, c_count)` failed outright on three of /span's four bands and
-- sqlx would have decoded the column into the wrong Rust type on all three.
-- The generator now casts; this is the assertion that keeps it cast.
-- ---------------------------------------------------------------------------
DO $$
DECLARE bad text;
BEGIN
    SELECT string_agg(format('%s.%s is %s', c.relname, a.attname,
                             format_type(a.atttypid, a.atttypmod)), ', ')
      INTO bad
      FROM pg_attribute a
      JOIN pg_class c ON c.oid = a.attrelid
      JOIN timescaledb_information.continuous_aggregates ca
        ON ca.view_name = c.relname AND ca.view_schema = 'public'
     WHERE a.attnum > 0 AND NOT a.attisdropped
       AND (
             ((a.attname LIKE '%\_count' OR a.attname LIKE '%\_dir\_count')
               AND format_type(a.atttypid, a.atttypmod) <> 'bigint')
          OR ((a.attname LIKE '%\_sum' OR a.attname LIKE '%\_total')
               AND format_type(a.atttypid, a.atttypmod) <> 'double precision')
           );
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'aggregate column types drifted from the generator: %', bad;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-2. The finalisers are callable on every level, with the level's own column
--      types and no cast at the call site.
--
-- A-1 compares types; this one actually resolves the function, which is what
-- /span does. A signature change that A-1 would miss fails here.
-- ---------------------------------------------------------------------------
DO $$
DECLARE lvl text; n bigint;
BEGIN
    FOREACH lvl IN ARRAY ARRAY['observations_1m','observations_5m',
                               'observations_1h','observations_1d'] LOOP
        EXECUTE format(
            'SELECT count(*) FROM (SELECT mw_mean(outtemp_sum, outtemp_count),
                                          mw_circular_mean(winddir_sin_sum, winddir_cos_sum,
                                                           winddir_dir_count)
                                     FROM %I LIMIT 1) q', lvl) INTO n;
    END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- A-3. W1-BUG-1 stays dead. A bucket whose winddir was never non-NULL must
--      finalise to 0, not to NULL and not to a division-by-zero error.
--
-- OQ-W1-2 option (c), W3 assumption 18. `windd` is a non-optional Swift Double
-- and a missing key fails the decode of the entire /span response, so NULL here
-- is a contract break and an exception here is a 500 on every /span for any
-- station without a wind vane.
-- ---------------------------------------------------------------------------
DO $$
DECLARE v double precision;
BEGIN
    SELECT mw_circular_mean(NULL, NULL, NULL) INTO v;
    IF v IS DISTINCT FROM 0 THEN
        RAISE EXCEPTION 'mw_circular_mean(NULL,NULL,NULL) = % — W1-BUG-1 is live again', v;
    END IF;
    SELECT mw_circular_mean(0, 0, 0) INTO v;
    IF v IS DISTINCT FROM 0 THEN
        RAISE EXCEPTION 'mw_circular_mean(0,0,0) = % — W1-BUG-1 is live again', v;
    END IF;
    -- And the opposite guard: a real direction must not be flattened to 0.
    SELECT round(mw_circular_mean(sind(90.0) * 3, cosd(90.0) * 3, 3)::numeric, 6) INTO v;
    IF v IS DISTINCT FROM 90 THEN
        RAISE EXCEPTION 'mw_circular_mean lost a real direction: expected 90, got %', v;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-4. mw_mean returns NULL for an empty bucket rather than 0.
--
-- The asymmetry with A-3 is deliberate and is the whole of OQ-W1-2: a missing
-- temperature is absent, a missing direction is 0, because only the direction
-- has a non-optional client-side type whose decode fails on absence.
-- ---------------------------------------------------------------------------
DO $$
DECLARE v double precision;
BEGIN
    SELECT mw_mean(NULL, NULL) INTO v;
    IF v IS NOT NULL THEN
        RAISE EXCEPTION 'mw_mean(NULL,NULL) = % — expected NULL', v;
    END IF;
    SELECT mw_mean(12.0, 0) INTO v;
    IF v IS NOT NULL THEN
        RAISE EXCEPTION 'mw_mean(12,0) = % — expected NULL, not a division by zero', v;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-5. The aggregate hierarchy is contiguous and each level reads the one below
--      it, never raw (D-11). A level that silently started reading raw would
--      still be correct and would quietly discard the 52x rebuild advantage the
--      hierarchy exists for.
-- ---------------------------------------------------------------------------
DO $$
DECLARE bad text;
BEGIN
    SELECT string_agg(format('%s reads %s', view_name, src), ', ') INTO bad
      FROM (
        SELECT ca.view_name,
               CASE WHEN ca.view_definition LIKE '%FROM observations_1h%' THEN 'observations_1h'
                    WHEN ca.view_definition LIKE '%FROM observations_5m%' THEN 'observations_5m'
                    WHEN ca.view_definition LIKE '%FROM observations_1m%' THEN 'observations_1m'
                    WHEN ca.view_definition LIKE '%FROM observations%'    THEN 'observations'
               END AS src
          FROM timescaledb_information.continuous_aggregates ca
         WHERE ca.view_schema = 'public'
      ) q
     WHERE (view_name, src) NOT IN (
        ('observations_1m', 'observations'),
        ('observations_5m', 'observations_1m'),
        ('observations_1h', 'observations_5m'),
        ('observations_1d', 'observations_1h'));
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'the aggregate hierarchy is not the one D-11 and W4 decision 1 define: %', bad;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-6. Every level that is the source of another is materialized_only.
--      TimescaleDB cannot build an aggregate on a real-time aggregate, so this
--      would fail at CREATE time — but it fails with a message about hypertables
--      rather than about this rule, so it is worth asserting by name.
-- ---------------------------------------------------------------------------
DO $$
DECLARE bad text;
BEGIN
    SELECT string_agg(view_name, ', ') INTO bad
      FROM timescaledb_information.continuous_aggregates
     WHERE view_schema = 'public'
       AND view_name IN ('observations_1m','observations_5m','observations_1h')
       AND NOT materialized_only;
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'these levels feed another level and must be materialized_only: %', bad;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-7. The raw table is idempotent over a replayed prefix (W3 assumption 22):
--      there is a unique constraint on (station_id, time) for ON CONFLICT to
--      name. Asserted on the constraint, not on an INSERT, so it holds on an
--      empty database too.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
          JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'observations'::regclass
           AND i.indisunique
           AND (SELECT array_agg(a.attname::text ORDER BY a.attname::text)
                  FROM pg_attribute a
                 WHERE a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey))
               = ARRAY['station_id','time']::text[]
    ) THEN
        RAISE EXCEPTION 'observations has no unique index on (station_id, time); '
                        'the writer cannot be idempotent over a replayed prefix';
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-8. No retention policy on the RAW hypertable, ever (D-4) — and one on each
--      of the three lower aggregate levels (W4 decision 5).
--
-- The two halves are the same decision seen from both ends. Raw is permanent,
-- which is what makes an aggregate a cache; because an aggregate is a cache, the
-- levels that are only ever read over a bounded window must not keep history they
-- cannot compress and nobody queries. The failure mode of getting either half
-- wrong is silence for exactly as long as the policy's interval.
-- ---------------------------------------------------------------------------
DO $$
DECLARE bad text; missing text;
BEGIN
    SELECT string_agg(j.hypertable_name, ', ') INTO bad
      FROM timescaledb_information.jobs j
     WHERE j.proc_name = 'policy_retention'
       AND j.hypertable_name = 'observations';
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'a retention policy exists on the raw hypertable, which D-4 forbids';
    END IF;

    -- timescaledb_information.jobs reports a continuous aggregate's policy against the
    -- *view* name rather than the materialisation hypertable, so this matches by name.
    SELECT string_agg(want, ', ') INTO missing
      FROM (VALUES ('observations_1m'), ('observations_5m'), ('observations_1h')) v(want)
     WHERE NOT EXISTS (
        SELECT 1 FROM timescaledb_information.jobs j
         WHERE j.proc_name = 'policy_retention' AND j.hypertable_name = v.want);
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'these aggregate levels have no retention policy: %. '
                        'They cannot be compressed (439-column ceiling) and are read only '
                        'over a bounded window, so unbounded they are uncompressible data '
                        'nothing queries.', missing;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- A-9. Every aggregate level has a refresh policy, and the 1-minute level's is
--      phase-locked to the minute boundary.
--
-- W2a assumption 14 and W3 assumption 21 give `window:rollup` 10 seconds from the
-- boundary to read that minute's bucket. A policy created without `initial_start`
-- gets whatever phase the deploy happened at, which meets the deadline by luck;
-- measured lag with the phase locked is 3.06-3.22 s. A missing `fixed_schedule`
-- is therefore a latent timing failure that nothing else would report.
-- ---------------------------------------------------------------------------
DO $$
DECLARE missing text; pol record;
BEGIN
    SELECT string_agg(want, ', ') INTO missing
      FROM (VALUES ('observations_1m'), ('observations_5m'),
                   ('observations_1h'), ('observations_1d')) v(want)
     WHERE NOT EXISTS (
        SELECT 1 FROM timescaledb_information.jobs j
         WHERE j.proc_name = 'policy_refresh_continuous_aggregate'
           AND j.hypertable_name = v.want);
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'these aggregate levels have no refresh policy: %', missing;
    END IF;

    SELECT j2.fixed_schedule, j2.schedule_interval INTO pol
      FROM timescaledb_information.jobs j2
     WHERE j2.proc_name = 'policy_refresh_continuous_aggregate'
       AND j2.hypertable_name = 'observations_1m';
    IF NOT pol.fixed_schedule THEN
        RAISE EXCEPTION 'observations_1m''s refresh policy is not on a fixed schedule, so '
                        'its phase within the minute is whatever the deploy happened at. '
                        'window:rollup has a 10-second deadline from the boundary.';
    END IF;
    IF pol.schedule_interval <> INTERVAL '1 minute' THEN
        RAISE EXCEPTION 'observations_1m refreshes every %, not every minute', pol.schedule_interval;
    END IF;
END $$;

\echo 'schema assertions: all passed'
