-- Run by ci/db/migrate-twice.sh on path B after the migrations at HEAD have been applied
-- over ci/db/seed.sql's rows. A schema comparison passes happily over a migration that
-- silently dropped every row; this is the part that does not.
--
-- It refreshes the aggregates first, because the migrations create them WITH NO DATA
-- (D-15) and an unrefreshed aggregate is empty rather than wrong — which would make every
-- assertion below vacuously true.
--
-- This file is where D-12, D-13 and D-14 stop being spike results and become assertions
-- the build runs. Each one is bit-identical or it fails; none of them is a tolerance.

\set ON_ERROR_STOP on

-- Not inside a transaction: refresh_continuous_aggregate cannot be, which is also why
-- the rebuild is a script and not a migration.
SET synchronous_commit = off;
CALL refresh_continuous_aggregate('observations_1m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_5m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1h', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1d', NULL, NULL);
RESET synchronous_commit;

-- ---------------------------------------------------------------------------
-- V-1. The rows are still there.
-- ---------------------------------------------------------------------------
DO $$
DECLARE n bigint;
BEGIN
    SELECT count(*) INTO n FROM observations WHERE station_id LIKE 'seed-%';
    IF n <> 12 THEN
        RAISE EXCEPTION 'expected 12 seeded rows, found % — the upgrade path lost data', n;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- V-2. The mean reconstructed from the TOP of the hierarchy is bit-identical to
--      a direct scan of raw.
--
-- This is D-12, D-13 and D-14 in one assertion, and the seed's uneven bucket
-- counts are what give it teeth: 'seed-even' has 3, 3 and 1 readings in its three
-- minutes, and averaging the three per-minute averages gives 40 while the direct
-- mean is 40 — equal by luck — whereas 'seed-uneven' has 1 and 2, where
-- avg(avg(x)) gives 51.5 and the true mean is 34.333... The comparison is `=`,
-- not a tolerance: D-14 widened the accumulator to double precision specifically
-- so that the hierarchy and a direct scan agree exactly.
-- ---------------------------------------------------------------------------
DO $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT d.station_id,
               d.direct,
               mw_mean(a.outtemp_sum, a.outtemp_count) AS rolled
          FROM (SELECT station_id,
                       sum(outtemp::double precision) / count(outtemp) AS direct
                  FROM observations
                 WHERE station_id LIKE 'seed-%' AND outtemp IS NOT NULL
                 GROUP BY station_id) d
          JOIN (SELECT station_id, sum(outtemp_sum) AS outtemp_sum,
                       sum(outtemp_count)::bigint AS outtemp_count
                  FROM observations_1d
                 WHERE station_id LIKE 'seed-%'
                 GROUP BY station_id) a USING (station_id)
    LOOP
        IF r.rolled IS DISTINCT FROM r.direct THEN
            RAISE EXCEPTION 'station %: mean through the hierarchy is %, direct from raw is % '
                            '— D-13/D-14 broken', r.station_id, r.rolled, r.direct;
        END IF;
    END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- V-3. The circular mean composes through every level, and it is a VECTOR mean.
--
-- 'seed-even' reports 10, 20, 30, 350, 10, 350, 180. A scalar average of those is
-- 135.7; the vector mean is 9.07. The assertion is against a direct computation
-- from raw rather than against a constant, so it tests composition rather than
-- arithmetic — but the two differ enough that a scalar mean sneaking in anywhere
-- fails rather than passes within a tolerance.
-- ---------------------------------------------------------------------------
DO $$
DECLARE rolled double precision; direct double precision;
BEGIN
    SELECT mw_circular_mean(sum(winddir_sin_sum), sum(winddir_cos_sum),
                            sum(winddir_dir_count)::bigint)
      INTO rolled FROM observations_1d WHERE station_id = 'seed-even';
    SELECT mw_circular_mean(sum(sind(winddir::double precision)),
                            sum(cosd(winddir::double precision)),
                            count(winddir))
      INTO direct FROM observations WHERE station_id = 'seed-even';
    IF abs(rolled - direct) > 1e-9 THEN
        RAISE EXCEPTION 'circular mean through the hierarchy is %, direct from raw is %',
                        rolled, direct;
    END IF;
    IF abs(rolled - (SELECT avg(winddir) FROM observations WHERE station_id = 'seed-even')) < 1 THEN
        RAISE EXCEPTION 'the circular mean (%) agrees with a scalar average, which on this '
                        'fixture it must not — a scalar avg() has replaced the vector mean',
                        rolled;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- V-4. W1-BUG-1 against real rows, not just against the finaliser.
--
-- 'seed-nowind' reports temperature and pressure and never a winddir, which is
-- exactly a snow gauge or an air-quality monitor. Its buckets exist, its
-- winddir_dir_count is NULL, and the finalised direction must be 0 — the value the
-- Go daemon emits because it writes zeros, and the value a non-optional Swift
-- Double can decode. NULL here would fail the decode of the whole /span response.
-- ---------------------------------------------------------------------------
DO $$
DECLARE r record;
BEGIN
    FOR r IN SELECT 'observations_1m' AS lvl UNION ALL SELECT 'observations_5m'
             UNION ALL SELECT 'observations_1h' UNION ALL SELECT 'observations_1d'
    LOOP
        EXECUTE format($q$
            SELECT count(*) FROM %I
             WHERE station_id = 'seed-nowind'
               AND (winddir_dir_count IS NOT NULL
                    OR mw_circular_mean(winddir_sin_sum, winddir_cos_sum,
                                        winddir_dir_count) IS DISTINCT FROM 0)$q$, r.lvl)
        INTO STRICT r;
        IF r.count <> 0 THEN
            RAISE EXCEPTION '%: a station with no winddir does not finalise to 0 — W1-BUG-1', r.lvl;
        END IF;
    END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- V-5. A bucket with no readings produces no row (not a row of zeroes), and a
--      bucket with one reading produces a count of 1.
--
-- The first half is contract: "a gap in the data is missing array elements, not
-- nulls" (docs/legacy-api-contract.md §2.3, fixture span-72h-hourly-with-gap).
-- 'seed-nowind' reports twice inside the first minute and is silent for the other
-- two, so it must have exactly ONE 1-minute bucket out of the fixture's three. An
-- implementation that emitted a bucket per period with zeroes in the empty ones
-- would have three, and every client would read two fabricated readings.
-- ---------------------------------------------------------------------------
DO $$
DECLARE n bigint;
BEGIN
    SELECT count(*) INTO n FROM observations_1m WHERE station_id = 'seed-nowind';
    IF n <> 1 THEN
        RAISE EXCEPTION 'seed-nowind has % 1-minute buckets, expected 1 — a silent bucket '
                        'produced a row', n;
    END IF;
    -- The anchor is recovered from the rows, not recomputed, so this cannot disagree
    -- with ci/db/seed.sql across a midnight boundary.
    SELECT outtemp_count INTO n FROM observations_1m
     WHERE station_id = 'seed-even'
       AND bucket = (SELECT time_bucket(INTERVAL '1 minute', min(time)) + INTERVAL '2 minutes'
                       FROM observations WHERE station_id LIKE 'seed-%');
    IF n <> 1 THEN
        RAISE EXCEPTION 'the single-reading bucket has count %, expected 1', n;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- V-6. `period_rain` survives as an aggregate-level column with no raw
--      counterpart (W1 assumption 11).
--
-- The raw column is `rainincremental`; the frozen /span key is `period_rain`, and
-- it names rainincremental's bucket TOTAL, not its mean — the one legacy key that
-- names an aggregate rather than a value. The total must also roll up to the same
-- number at every level, which a mean would not.
-- ---------------------------------------------------------------------------
DO $$
DECLARE lvl text; v double precision; direct double precision;
BEGIN
    SELECT sum(rainincremental::double precision) INTO direct
      FROM observations WHERE station_id = 'seed-even';
    FOREACH lvl IN ARRAY ARRAY['observations_1m','observations_5m',
                               'observations_1h','observations_1d'] LOOP
        EXECUTE format('SELECT sum(rainincremental_total) FROM %I WHERE station_id = %L',
                       lvl, 'seed-even') INTO v;
        IF abs(v - direct) > 1e-9 THEN
            RAISE EXCEPTION '%: period_rain totals % against % direct from raw', lvl, v, direct;
        END IF;
    END LOOP;
END $$;

\echo 'seed verification: all passed'
