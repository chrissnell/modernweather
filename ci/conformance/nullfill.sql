-- The NULL-fill contract/tools/README.md leaves as a comment, as runnable SQL.
--
-- The Go daemon inserts its entire reading struct on every write, so a row it produced has no
-- NULL in any numeric column. Rows seeded straight into the hypertable by the rig do, and that
-- difference is not cosmetic: a NULL winddir makes /span return 500 (W1-BUG-1, recorded in
-- docs/legacy-api-contract.md §10). Matching the daemon's shape is what makes the seeded
-- stations answerable at all.
--
-- The column list is derived rather than written out because `weather` carries 197 numeric
-- columns (127 real, 69 integer, 1 bigint, and no numeric/decimal) and a hand-maintained list
-- would rot on the next migration.
DO $$
DECLARE
    assignments text;
BEGIN
    SELECT string_agg(format('%I = COALESCE(%I, 0)', column_name, column_name), ', ')
      INTO assignments
      FROM information_schema.columns
     WHERE table_schema = 'public'
       AND table_name = 'weather'
       AND data_type IN ('real', 'double precision', 'integer', 'bigint', 'smallint');

    IF assignments IS NULL THEN
        RAISE EXCEPTION 'weather has no numeric columns; is the daemon''s schema provisioned?';
    END IF;

    EXECUTE format(
        'UPDATE weather SET %s WHERE stationname IN (%L, %L, %L, %L)',
        assignments, 'snowgauge', 'snowgauge-stale', 'snowgauge-dead', 'aqmon');
END $$;
