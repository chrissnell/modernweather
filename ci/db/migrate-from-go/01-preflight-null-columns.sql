-- P-8 in full: which of the old table's columns have never held a value.
--
-- Separate from 00-preflight-source.sql because it is the one preflight query that is not
-- instant — it is a single pass over a bounded sample of the raw table rather than a
-- catalogue read. Read-only: it creates temporary tables in its own session and touches
-- nothing else.
--
-- Three outcomes, and they want different responses:
--   present_and_populated  nothing to do.
--   present_but_all_null   the column exists and the Go daemon has never written it in the
--                          sampled window. Worth knowing before a [[metric]] entry is
--                          defended on the grounds that production uses it.
--   absent                 the ~72-column gap (OQ-6). `migrate-from-go.sh gap` lists those;
--                          this query cannot see a column that is not there.
--
-- The window is the last 30 days, which separates a dead column from a live one and is
-- cheap enough to run on production. Edit the interval below to widen it.
\pset footer off

CREATE TEMP TABLE w9_sample AS
    SELECT * FROM weather WHERE time >= (SELECT max(time) FROM weather) - INTERVAL '30 days';

CREATE TEMP TABLE w9_null_scan (column_name text, non_null bigint);

DO $$
DECLARE
    r record;
    q text := '';
    n int := 0;
BEGIN
    FOR r IN SELECT a.attname
               FROM pg_attribute a
              WHERE a.attrelid = 'weather'::regclass AND a.attnum > 0 AND NOT a.attisdropped
              ORDER BY a.attnum
    LOOP
        q := q || format('%s SELECT %L, count(%I) FROM w9_sample',
                         CASE WHEN n = 0 THEN '' ELSE ' UNION ALL' END, r.attname, r.attname);
        n := n + 1;
    END LOOP;
    EXECUTE 'INSERT INTO w9_null_scan ' || q;
    RAISE NOTICE 'scanned % columns over % sampled rows', n, (SELECT count(*) FROM w9_sample);
END $$;

SELECT column_name, non_null,
       CASE WHEN non_null > 0 THEN 'present_and_populated' ELSE 'present_but_all_null' END AS verdict
  FROM w9_null_scan
 ORDER BY (non_null > 0), column_name;

SELECT count(*) FILTER (WHERE non_null > 0) AS populated,
       count(*) FILTER (WHERE non_null = 0) AS always_null,
       count(*)                             AS columns_present
  FROM w9_null_scan;

DROP TABLE w9_null_scan;
DROP TABLE w9_sample;
