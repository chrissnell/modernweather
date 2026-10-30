-- W9 lab harness: a faithful copy of the OLD Go schema, seeded, so that every
-- transform and every reconciliation query in docs/data-migration.md has been
-- executed against something before an operator runs it on production.
--
-- Production is unreachable from this runtime (W0 OQ-1), so the lab is the only
-- place these can be proved to run at all. The value model is lifted from
-- ci/db/measure/00-harness.sql for the same reason W4 gives: a generator using
-- random() measures the entropy ceiling and reports a compression ratio no
-- production instance will ever see.
--
-- The lab reproduces the *shape* of the owner's database, not its volume:
--   * a raw window shortened by an active 365-day retention policy, and
--   * a weather_1d / weather_1h rollup that reaches further back than raw does,
--     created by the same mechanism production used — materialise, then let
--     retention drop the raw chunks underneath.
-- Row counts and sizes are therefore lab figures. docs/data-migration.md §8 says
-- which numbers are load-dependent and which are not.

CREATE SCHEMA IF NOT EXISTS w9_lab;

-- The populated column subset, in attribute order. Production carries ~143 of the
-- 215 columns createTableSQL declares (OQ-6); the lab populates a Davis-shaped
-- subset and leaves the rest NULL, because a declared-but-NULL column costs one
-- bit in the null bitmap (D-3) and populating all 215 would overstate every size.
CREATE OR REPLACE FUNCTION w9_lab.populated_columns(populated int)
RETURNS TABLE (attnum int, colname text, typname text)
LANGUAGE sql STABLE AS $$
    SELECT row_number() OVER (ORDER BY a.attnum)::int, a.attname::text, t.typname::text
    FROM pg_attribute a
    JOIN pg_type t ON t.oid = a.atttypid
    WHERE a.attrelid = 'weather'::regclass
      AND a.attnum > 0 AND NOT a.attisdropped
      AND a.attname NOT IN ('time', 'stationname', 'stationtype')
      AND t.typname IN ('float4', 'int4', 'int8')
    ORDER BY a.attnum
    LIMIT populated
$$;

CREATE OR REPLACE FUNCTION w9_lab.value_expr(i int, colname text, typname text)
RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE typname
        WHEN 'float4' THEN format(
            'round((%s + %s * sin(2*pi()*(extract(epoch from g.t) + %s * 37) / %s))::numeric, 1)::float4',
            20 + (i % 7) * 11, 8 + (i % 5) * 2, i, 2700 + (i % 11) * 300)
        ELSE format(
            'round(%s + %s * sin(2*pi()*(extract(epoch from g.t) + %s * 37) / %s))::%s',
            40 + (i % 9) * 5, 10 + (i % 4) * 3, i, 3300 + (i % 7) * 420, typname)
    END
$$;

-- Fill `weather`. `winddir` is overridden to straddle north, because a scalar mean
-- and a vector mean differ by 180 degrees there and that is the one difference a
-- tolerance-based reconciliation would not catch (ci/db/verify-seed.sql V-3).
--
-- `stations` is the lab's list of (stationname, stationtype) pairs; the gap station
-- reports nothing for a whole day every 37 days, so that a bucket with no readings
-- and a day with no readings both exist in the source.
CREATE OR REPLACE PROCEDURE w9_lab.fill(
    t0              timestamptz,
    t1              timestamptz,
    cadence_seconds int DEFAULT 30,
    populated       int DEFAULT 140,
    slice           interval DEFAULT INTERVAL '7 days'
) LANGUAGE plpgsql AS $$
DECLARE
    cols  text;
    exprs text;
    s     timestamptz := t0;
    e     timestamptz;
    sql   text;
BEGIN
    SELECT string_agg(quote_ident(colname), ', ' ORDER BY attnum),
           string_agg(
               CASE colname
                   WHEN 'winddir' THEN
                       '((350 + 20 * sin(2*pi()*extract(epoch from g.t)/5400))::numeric % 360)::float4'
                   ELSE w9_lab.value_expr(attnum, colname, typname)
               END, ', ' ORDER BY attnum)
      INTO cols, exprs
      FROM w9_lab.populated_columns(populated);

    WHILE s < t1 LOOP
        e := least(s + slice, t1);
        sql := format($f$
            INSERT INTO weather (time, stationname, stationtype, %s)
            SELECT g.t, st.name, st.type, %s
            FROM generate_series(%L::timestamptz, %L::timestamptz - INTERVAL '%s seconds',
                                 INTERVAL '%s seconds') AS g(t)
            CROSS JOIN (VALUES ('davis-full','davis'),
                               ('snowgauge','snowgauge'),
                               ('ambient-aq','ambient-customized')) AS st(name, type)
            WHERE NOT (st.name = 'snowgauge'
                       AND (extract(epoch from date_trunc('day', g.t))::bigint / 86400) %% 37 = 0)
        $f$, cols, exprs, s, e, cadence_seconds, cadence_seconds);
        EXECUTE sql;
        COMMIT;
        s := e;
    END LOOP;
END $$;

-- The hold rate, quoted next to every compression figure (W0 Q3).
CREATE OR REPLACE FUNCTION w9_lab.hold_rate(sample interval DEFAULT INTERVAL '7 days')
RETURNS numeric LANGUAGE sql STABLE AS $$
    SELECT round(sum((outtemp = prev)::int)::numeric / nullif(count(*), 0), 4)
    FROM (SELECT outtemp, lag(outtemp) OVER (PARTITION BY stationname ORDER BY time) AS prev
          FROM weather
          WHERE time >= (SELECT max(time) FROM weather) - sample) z
    WHERE prev IS NOT NULL
$$;
