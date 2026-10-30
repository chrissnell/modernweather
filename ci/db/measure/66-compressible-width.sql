-- Measurement 5, third part — and the one that changed a decision.
--
-- `compress_chunk` on observations_1m fails: "row is too big: size 8912, maximum size
-- 8160". DESIGN.md D-19 anticipated this shape of failure but put it at the far end of the
-- column budget: "a near-ceiling aggregate cannot be compressed at all ... the
-- 1600-column view the spike built was rejected for compression with an estimated row
-- size of 28,852 bytes". The generated levels are 629 columns, a third of the ceiling, and
-- they are rejected too. So the question is not whether a 1600-column view compresses. It
-- is where the real threshold is, because that is the number that decides whether OQ-4's
-- recommendation — compress the minute level, keep it unbounded — is available at all.
--
-- A compressed chunk holds one row per segment per batch, and in that row every one of the
-- source relation's columns becomes a compressed array. Those arrays are TOASTable, but
-- the pointers are not: ~14 bytes of them per column have to fit in an 8160-byte heap
-- tuple regardless of how much data they point at. The threshold is therefore a function
-- of column count alone, and it is found by bisection rather than reasoned about.

-- Split in two because refresh_continuous_aggregate() cannot run inside a function:
-- probe_ddl() renders the view, the caller refreshes it at top level, probe_compress()
-- reports what compression said. 66-compressible-width.sh drives the three steps.
CREATE OR REPLACE FUNCTION mw_measure.probe_ddl(n_cols int)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    cols   text;
    msg    text;
    cutoff timestamptz;
BEGIN
    -- A continuous aggregate's query may not contain a subquery, so the bound is
    -- resolved here and interpolated as a literal.
    SELECT min(time) + INTERVAL '2 days' INTO cutoff FROM observations;
    EXECUTE 'DROP MATERIALIZED VIEW IF EXISTS mw_measure_probe CASCADE';
    SELECT string_agg(format('sum(%I::double precision) AS c%s', colname, attnum), ', ')
      INTO cols
      FROM mw_measure.populated_columns(least(n_cols, 140));
    -- Past 140 distinct source columns, reuse them with distinct output names: the
    -- threshold is about the compressed row's column count, not about which column.
    IF n_cols > 140 THEN
        SELECT cols || ', ' || string_agg(format('sum(%I::double precision) AS d%s',
                                                 colname, attnum), ', ')
          INTO cols
          FROM mw_measure.populated_columns(n_cols - 140);
    END IF;
    IF n_cols > 280 THEN
        SELECT cols || ', ' || string_agg(format('min(%I) AS e%s', colname, attnum), ', ')
          INTO cols
          FROM mw_measure.populated_columns(n_cols - 280);
    END IF;
    IF n_cols > 420 THEN
        SELECT cols || ', ' || string_agg(format('max(%I) AS f%s', colname, attnum), ', ')
          INTO cols
          FROM mw_measure.populated_columns(n_cols - 420);
    END IF;
    IF n_cols > 560 THEN
        SELECT cols || ', ' || string_agg(format('count(%I) AS g%s', colname, attnum), ', ')
          INTO cols
          FROM mw_measure.populated_columns(n_cols - 560);
    END IF;

    RETURN format($q$
        CREATE MATERIALIZED VIEW mw_measure_probe
        WITH (timescaledb.continuous, timescaledb.materialized_only = true) AS
        SELECT time_bucket(INTERVAL '1 hour', time) AS bucket, station_id, %s
        FROM observations
        WHERE time < %L
        GROUP BY 1, 2 WITH NO DATA$q$, cols, cutoff);
END $$;

CREATE OR REPLACE FUNCTION mw_measure.probe_compress()
RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    PERFORM compress_chunk(c) FROM show_chunks('mw_measure_probe') c;
    RETURN 'COMPRESSED';
EXCEPTION WHEN OTHERS THEN
    RETURN 'REFUSED: ' || SQLERRM;
END $$;
