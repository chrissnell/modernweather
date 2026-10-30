-- The seed fixture ci/db/migrate-twice.sh asks W4 for.
--
-- Its job is to make path B — previous release's schema, then rows, then the migrations at
-- HEAD — mean something. Without rows the upgrade path runs against empty tables, and a
-- migration that rewrites a hypertable, backfills a column or rebuilds an aggregate
-- behaves differently when there is something in it. That is the case production is in and
-- the only case worth testing.
--
-- Applied against the PREVIOUS release's schema, so it must only name objects that existed
-- before the migrations under test. Today that means `observations` and nothing else: the
-- station dimension, the caches and `extra` all arrive in the W4 migration. When a later
-- release moves that boundary, the rows that depend on the newer objects move into
-- ci/db/seed-post.sql — which does not exist yet and should not be invented early.
--
-- The timestamps are anchored to *yesterday at noon*, not to a fixed date, and that is
-- not cosmetic. Three of the four aggregate levels carry retention (W4 decision 5), so a
-- fixture dated 2026-03-01 would be dropped by a retention job that happened to fire
-- during the gate — a flake that would look like data loss. Noon yesterday is inside every
-- retention window and late enough that the DAY bucket is closed, which matters because
-- `refresh_continuous_aggregate` materialises only complete buckets: a fixture written
-- today would never appear in observations_1d and every assertion that reads that level
-- would pass vacuously.
--
-- Deliberately small and deliberately awkward. Three stations, three minutes, and every
-- shape that has broken something:
--
--   * uneven bucket counts, which is the condition under which avg(avg(x)) is wrong
--     (D-12 measured it wrong in all 56 uneven pairs) and under which a rolled-up
--     sum/count has to be bit-identical to a direct scan;
--   * a station reporting no winddir at all, ever — W1-BUG-1's trigger;
--   * a station reporting a winddir that straddles north, where a scalar mean gives 180
--     and a vector mean gives 0;
--   * a bucket with exactly one reading, and two periods in which a station is silent
--     altogether, so that nullif(count, 0) and "a gap is a missing element, not a row of
--     zeroes" are both exercised by the same fixture.

-- The anchor. ci/db/verify-seed.sql recovers it with min(time) over these rows rather
-- than recomputing this expression, so the two files cannot disagree across midnight.
-- No DDL here, ever: path A would not have it and the schema comparison would fail on a
-- difference the fixture introduced.

-- Minute 0 — 'even' reports every 20 s (3 readings), 'uneven' reports once, 'nowind'
-- twice.
INSERT INTO observations (time, station_id, outtemp, barometer, winddir, rainincremental)
SELECT base + offs, station_id, outtemp, barometer, winddir, rainincremental
  FROM (SELECT date_trunc('day', now()) - INTERVAL '12 hours' AS base) a,
       (VALUES
    (INTERVAL  '0 sec', 'seed-even',   10.0, 29.90,  10.0, 0.01),
    (INTERVAL '20 sec', 'seed-even',   20.0, 29.91,  20.0, 0.01),
    (INTERVAL '40 sec', 'seed-even',   30.0, 29.92,  30.0, 0.01),
    (INTERVAL  '0 sec', 'seed-uneven', 99.0, 30.00,  90.0, 0.50),
    (INTERVAL  '0 sec', 'seed-nowind',  1.0, 28.00,  NULL, 0.00),
    (INTERVAL '30 sec', 'seed-nowind',  3.0, 28.10,  NULL, 0.00),

-- Minute 1 — 'even' keeps its cadence, 'uneven' reports twice, 'nowind' is silent. The
-- straddling directions are here: 350 and 10 degrees, whose vector mean is 0 and whose
-- scalar mean is 180.
    (INTERVAL  '60 sec', 'seed-even',   40.0, 29.93, 350.0, 0.02),
    (INTERVAL  '80 sec', 'seed-even',   50.0, 29.94,  10.0, 0.02),
    (INTERVAL '100 sec', 'seed-even',   60.0, 29.95, 350.0, 0.02),
    (INTERVAL  '60 sec', 'seed-uneven',  1.0, 30.01, 180.0, 0.10),
    (INTERVAL  '90 sec', 'seed-uneven',  3.0, 30.02, 180.0, 0.10),

-- Minute 2 — only 'even', and only once, so this bucket's count is 1 and the other two
-- stations have no bucket at all rather than a bucket of zeroes.
    (INTERVAL '120 sec', 'seed-even',   70.0, 29.96, 180.0, 0.04)
       ) AS v(offs, station_id, outtemp, barometer, winddir, rainincremental);
