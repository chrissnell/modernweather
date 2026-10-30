-- Measurement 1: /span query latency per band, against each aggregate level, with the
-- level's chunks compressed and uncompressed. This is the measurement DESIGN.md OQ-4
-- made its own recommendation conditional on.
--
-- The query shape is built from the view's own columns rather than transcribed, and it
-- finalises EVERY metric the level carries — 194 metrics, 333 output expressions. The
-- frozen contract's /span element has 133 keys (docs/legacy-api-contract.md §3), so this
-- is a strict upper bound on the real read, not an approximation of it. md5(q::text)
-- forces every output column to be constructed; without it the planner is free to prune
-- projections the caller never looks at.

CREATE OR REPLACE FUNCTION mw_measure.span_select(view_name text)
RETURNS text LANGUAGE plpgsql STABLE AS $$
DECLARE
    cols text[];
    out  text[] := ARRAY['bucket AS ts'];
    c    text;
    base text;
BEGIN
    SELECT array_agg(a.attname::text ORDER BY a.attnum) INTO cols
      FROM pg_attribute a
     WHERE a.attrelid = view_name::regclass AND a.attnum > 0 AND NOT a.attisdropped;

    FOREACH c IN ARRAY cols LOOP
        IF c LIKE '%!_sin!_sum' ESCAPE '!' THEN
            base := left(c, length(c) - 8);
            out := out || format('mw_circular_mean(%I, %I, %I) AS %I',
                                 c, base || '_cos_sum', base || '_dir_count', base);
        ELSIF c LIKE '%!_cos!_sum' ESCAPE '!' THEN
            CONTINUE;
        ELSIF c LIKE '%!_sum' ESCAPE '!' THEN
            base := left(c, length(c) - 4);
            out := out || format('mw_mean(%I, %I) AS %I', c, base || '_count', base);
        ELSIF c LIKE '%!_min' ESCAPE '!' OR c LIKE '%!_max' ESCAPE '!'
              OR c LIKE '%!_total' ESCAPE '!' THEN
            out := out || format('%I', c);
        END IF;
    END LOOP;

    RETURN format('SELECT %s FROM %I WHERE station_id = $1 AND bucket >= $2 AND bucket < $3 '
                  'ORDER BY bucket', array_to_string(out, ', '), view_name);
END $$;

-- One band, `reps` times, reporting the median. The first run is reported separately
-- because it is the only one not reading a warm cache, and both numbers matter: /span is
-- 0.9% of the measured request mix (W3 §2a.8), so a cold read is the common one.
CREATE OR REPLACE FUNCTION mw_measure.time_span(
    view_name text, station text, span interval, reps int DEFAULT 5
) RETURNS TABLE (level text, span_label text, out_rows bigint, first_ms numeric, median_ms numeric)
LANGUAGE plpgsql AS $$
DECLARE
    sql     text;
    t0      timestamptz;
    hi      timestamptz;
    n       bigint;
    ms      numeric[] := '{}';
    i       int;
    started timestamptz;
BEGIN
    SELECT max(time) INTO hi FROM observations;
    t0 := hi - span;
    sql := format('SELECT count(md5(q::text)) FROM (%s) q', mw_measure.span_select(view_name));
    FOR i IN 1..reps LOOP
        started := clock_timestamp();
        EXECUTE sql INTO n USING station, t0, hi;
        ms := ms || round((extract(epoch FROM clock_timestamp() - started) * 1000)::numeric, 2);
    END LOOP;
    RETURN QUERY SELECT view_name, span::text, n, ms[1],
                        (SELECT round((percentile_cont(0.5) WITHIN GROUP (ORDER BY v))::numeric, 2)
                           FROM unnest(ms) v);
END $$;

-- The four bands the frozen contract defines (docs/legacy-api-contract.md §2.3). The
-- 24-hour row is the one that matters most: it is the website's default view and it falls
-- in the 5-minute band.
CREATE OR REPLACE VIEW mw_measure.span_bands AS
SELECT * FROM (VALUES
    (1, 'observations_1m', INTERVAL '6 hours'),
    (2, 'observations_5m', INTERVAL '24 hours'),
    (3, 'observations_5m', INTERVAL '48 hours'),
    (4, 'observations_1h', INTERVAL '14 days'),
    (5, 'observations_1d', INTERVAL '200 days')
) AS t(ord, view_name, span);

CREATE OR REPLACE FUNCTION mw_measure.span_all(station text DEFAULT 'station-1', reps int DEFAULT 5)
RETURNS TABLE (level text, span_label text, out_rows bigint, first_ms numeric, median_ms numeric)
LANGUAGE plpgsql AS $$
DECLARE b record;
BEGIN
    FOR b IN SELECT * FROM mw_measure.span_bands ORDER BY ord LOOP
        RETURN QUERY SELECT * FROM mw_measure.time_span(b.view_name, station, b.span, reps);
    END LOOP;
END $$;
