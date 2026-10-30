-- Measurement 5, second half: enable compression on all four aggregate levels and on the
-- raw hypertable, compress every chunk, and report the ratio next to the hold rate that
-- explains it (D-10). Then 30-span-latency.sql is re-run against the compressed levels,
-- which is the measurement OQ-4 made its recommendation conditional on.
--
-- segmentby and orderby are stated rather than left to TimescaleDB's defaults, for the
-- same reason D-7 states them on the raw table: /span always filters by one station and
-- orders by bucket, so segmenting by station_id is what lets a compressed read skip every
-- other station's batches instead of decompressing them.

ALTER MATERIALIZED VIEW observations_1m SET (
    timescaledb.compress = true,
    timescaledb.compress_segmentby = 'station_id',
    timescaledb.compress_orderby   = 'bucket DESC');
ALTER MATERIALIZED VIEW observations_5m SET (
    timescaledb.compress = true,
    timescaledb.compress_segmentby = 'station_id',
    timescaledb.compress_orderby   = 'bucket DESC');
ALTER MATERIALIZED VIEW observations_1h SET (
    timescaledb.compress = true,
    timescaledb.compress_segmentby = 'station_id',
    timescaledb.compress_orderby   = 'bucket DESC');
ALTER MATERIALIZED VIEW observations_1d SET (
    timescaledb.compress = true,
    timescaledb.compress_segmentby = 'station_id',
    timescaledb.compress_orderby   = 'bucket DESC');

\timing on
\echo '--- compressing observations_1m'
SELECT count(compress_chunk(c)) FROM show_chunks('observations_1m') c;
\echo '--- compressing observations_5m'
SELECT count(compress_chunk(c)) FROM show_chunks('observations_5m') c;
\echo '--- compressing observations_1h'
SELECT count(compress_chunk(c)) FROM show_chunks('observations_1h') c;
\echo '--- compressing observations_1d'
SELECT count(compress_chunk(c)) FROM show_chunks('observations_1d') c;
\echo '--- compressing observations (raw)'
SELECT count(compress_chunk(c)) FROM show_chunks('observations') c;
\timing off
