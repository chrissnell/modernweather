-- Measurement 5: size on disk per level, and the raw table, at a stated row count. Read
-- straight out of TimescaleDB's own size functions rather than from pg_total_relation_size
-- on the view, which reports nothing useful for a continuous aggregate.
SELECT 'observations' AS relation,
       (SELECT count(*) FROM observations) AS rows,
       pg_size_pretty(hypertable_size('observations')) AS total,
       NULL::text AS compression
UNION ALL
SELECT ca.view_name,
       (SELECT approximate_row_count(format('%I.%I', ca.materialization_hypertable_schema,
                                             ca.materialization_hypertable_name)::regclass)),
       pg_size_pretty(hypertable_size(format('%I.%I', ca.materialization_hypertable_schema,
                                                      ca.materialization_hypertable_name)::regclass)),
       ca.compression_enabled::text
FROM timescaledb_information.continuous_aggregates ca
ORDER BY 1;
