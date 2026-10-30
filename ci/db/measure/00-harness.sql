-- W4 measurement harness. Synthetic raw readings with a *held-sensor* value model.
--
-- Everything docs/database-schema.md §6 reports was produced by this file and the
-- numbered files beside it, against timescale/timescaledb-ha:pg17 — the same image the
-- `database` job uses. It is not wired into any gate: it writes tens of gigabytes and
-- takes tens of minutes. It is committed so the numbers can be reproduced or disputed.
--
-- The value model matters more than the row count. DESIGN.md D-10 measured compression
-- between 1.89x and 13.74x on the same schema and row count, with the whole spread
-- explained by the fraction of consecutive readings identical to their predecessor —
-- which is what TimescaleDB's float encoding exploits. A generator using random() would
-- measure the entropy ceiling and report a number no production instance will ever see.
-- So each column is a slow sinusoid quantised to a sensor's actual resolution, which is
-- what makes consecutive 2-second readings repeat. mw_measure_hold_rate() reports the
-- rate achieved, and every compression figure is quoted next to it.

CREATE SCHEMA IF NOT EXISTS mw_measure;

-- The populated column set, in attribute order. Production carries ~143 of the 216
-- columns the registry declares (the ~72-column gap is OQ-6, W9's to reconcile), so the
-- harness populates that many and leaves the rest NULL — a declared-but-NULL column is
-- one bit in the row's null bitmap (D-3), and measuring with all 216 populated would
-- overstate every size in this document.
CREATE OR REPLACE FUNCTION mw_measure.populated_columns(populated int)
RETURNS TABLE (attnum int, colname text, typname text)
LANGUAGE sql STABLE AS $$
    SELECT row_number() OVER (ORDER BY a.attnum)::int, a.attname::text, t.typname::text
    FROM pg_attribute a
    JOIN pg_type t ON t.oid = a.atttypid
    WHERE a.attrelid = 'observations'::regclass
      AND a.attnum > 0 AND NOT a.attisdropped
      AND a.attname NOT IN ('time', 'station_id', 'stationname', 'stationtype')
      AND t.typname IN ('float4', 'int4', 'int8')
    ORDER BY a.attnum
    LIMIT populated
$$;

-- One column's value expression. `i` is the column's ordinal, which seeds its period,
-- phase and amplitude so that no two columns are correlated and none is constant.
--
-- The quantisation is the point: amp * sin(2*pi*t/period) has a maximum slope of
-- 2*pi*amp/period per second, so with amp = 12 and period >= 2700 s the value moves less
-- than 0.06 per 2-second step and rounding to one decimal makes most consecutive pairs
-- identical. That is a held sensor, and it is what production's float columns look like.
CREATE OR REPLACE FUNCTION mw_measure.value_expr(i int, colname text, typname text)
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

-- Fill `observations` one time slice at a time. Slices keep the WAL and the dead-tuple
-- footprint of a single statement bounded; they are not a performance trick and the
-- slice size does not change the result.
CREATE OR REPLACE PROCEDURE mw_measure.fill(
    n_stations      int,
    t0              timestamptz,
    t1              timestamptz,
    cadence_seconds int DEFAULT 2,
    populated       int DEFAULT 140,
    slice           interval DEFAULT INTERVAL '12 hours'
) LANGUAGE plpgsql AS $$
DECLARE
    cols  text;
    exprs text;
    s     timestamptz := t0;
    e     timestamptz;
    sql   text;
BEGIN
    SELECT string_agg(quote_ident(colname), ', ' ORDER BY attnum),
           string_agg(mw_measure.value_expr(attnum, colname, typname), ', ' ORDER BY attnum)
      INTO cols, exprs
      FROM mw_measure.populated_columns(populated);

    WHILE s < t1 LOOP
        e := least(s + slice, t1);
        sql := format($f$
            INSERT INTO observations (time, station_id, %s)
            SELECT g.t, st.id, %s
            FROM generate_series(%L::timestamptz, %L::timestamptz - INTERVAL '%s seconds',
                                 INTERVAL '%s seconds') AS g(t)
            CROSS JOIN (SELECT 'station-' || i AS id FROM generate_series(1, %s) i) AS st
        $f$, cols, exprs, s, e, cadence_seconds, cadence_seconds, n_stations);
        EXECUTE sql;
        COMMIT;
        s := e;
    END LOOP;
END $$;

-- The fraction of consecutive readings, per station, whose value is identical to its
-- predecessor. DESIGN.md D-10 and docs/spike-findings.md Q3 make this the one number
-- that explains a compression ratio, so it is reported with every ratio below.
CREATE OR REPLACE FUNCTION mw_measure.hold_rate(sample_rows int DEFAULT 200000)
RETURNS numeric LANGUAGE sql STABLE AS $$
    WITH s AS (
        SELECT station_id, time, outtemp, barometer, windspeed, outhumidity,
               lag(outtemp)     OVER w AS p_outtemp,
               lag(barometer)   OVER w AS p_barometer,
               lag(windspeed)   OVER w AS p_windspeed,
               lag(outhumidity) OVER w AS p_outhumidity
        FROM (SELECT * FROM observations ORDER BY station_id, time LIMIT sample_rows) q
        WINDOW w AS (PARTITION BY station_id ORDER BY time)
    ), f AS (
        SELECT unnest(ARRAY[
            (outtemp     IS NOT DISTINCT FROM p_outtemp)::int,
            (barometer   IS NOT DISTINCT FROM p_barometer)::int,
            (windspeed   IS NOT DISTINCT FROM p_windspeed)::int,
            (outhumidity IS NOT DISTINCT FROM p_outhumidity)::int]) AS same
        FROM s WHERE p_outtemp IS NOT NULL
    )
    SELECT round(avg(same)::numeric, 4) FROM f
$$;
