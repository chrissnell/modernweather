-- Measurement 4: is a unique constraint, or an ON CONFLICT target, permitted on the
-- compressed hypertable, and at what cost?
--
-- W3 assumption 22 requires the raw table to be idempotent over a replayed prefix: §2.3's
-- cancel-safe delivery replays part of a batch after a cancellation, so the writer must be
-- able to re-send rows it has already sent. That needs either a unique constraint with
-- ON CONFLICT or an equivalent, and "equivalent" is doing a lot of work in that sentence.
-- What TimescaleDB 2.31 actually permits is the thing to establish before the writer is
-- designed around it — earlier versions did not allow the compressed case at all.
--
-- Four questions, in order:
--   A. can a UNIQUE index be created on (station_id, time) with chunks compressed?
--   B. does INSERT ... ON CONFLICT DO UPDATE work against an uncompressed chunk?
--   C. does it work against a COMPRESSED chunk, and does the chunk stay compressed?
--   D. what does the index cost at insert time?
--
-- Assumes 00-harness.sql and a filled `observations`. Idempotent: safe to re-run.

\set ON_ERROR_STOP on
\timing off
DROP INDEX IF EXISTS observations_station_id_time_key;

\echo '=== A. CREATE UNIQUE INDEX with chunks compressed'
SELECT count(*) FILTER (WHERE is_compressed) AS compressed_chunks, count(*) AS chunks
  FROM timescaledb_information.chunks WHERE hypertable_name = 'observations';
\timing on
CREATE UNIQUE INDEX observations_station_id_time_key ON observations (station_id, time DESC);
\timing off

-- Index size, with every chunk compressed. This is the number that matters least and is
-- the easiest to misread: a compressed chunk's index is built over the compressed rows —
-- one per segment per batch — so it is almost free. The index's real cost is on the
-- uncompressed window, which is measured below after a decompress_chunk.
CREATE OR REPLACE VIEW mw_measure.index_sizes AS
SELECT CASE WHEN c.relname LIKE '%station_id_time_key%' THEN 'unique (station_id, time)'
            ELSE 'hypertable time index' END AS index_name,
       pg_size_pretty(sum(pg_relation_size(i.indexrelid))) AS total,
       count(*) AS per_chunk_indexes
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
 WHERE c.relname LIKE '%station_id_time_key%' OR c.relname LIKE '%observations_time_idx%'
 GROUP BY 1;

\echo '--- index sizes, all chunks compressed'
SELECT * FROM mw_measure.index_sizes;

-- One minute of one station at each end of the dataset. The bounds are resolved to
-- literals first because a correlated subquery would change what is being timed.
SELECT min(time)              AS lo,
       min(time) + INTERVAL '1 minute' AS lo_end,
       max(time) - INTERVAL '1 minute' AS hi_start,
       max(time)              AS hi
  FROM observations \gset

\echo '=== B. ON CONFLICT DO UPDATE against an UNCOMPRESSED chunk (newest)'
-- Decompressed explicitly rather than assumed: in production the newest chunk is
-- uncompressed for seven days (D-7), which is exactly the window a cancel-safe replay
-- lands in, so this is the common case and it has to actually be uncompressed to measure.
-- Ordered by range_start, not by the chunk's name: show_chunks() returns regclasses and
-- ordering those sorts _hyper_1_9_chunk after _hyper_1_100_chunk.
SELECT decompress_chunk(r, if_compressed => true) IS NOT NULL AS decompressed
  FROM (SELECT format('%I.%I', chunk_schema, chunk_name)::regclass AS r
          FROM timescaledb_information.chunks
         WHERE hypertable_name = 'observations'
         ORDER BY range_start DESC LIMIT 1) q;
SELECT chunk_name, is_compressed FROM timescaledb_information.chunks
 WHERE hypertable_name = 'observations' ORDER BY range_start DESC LIMIT 1;

\echo '--- index sizes with the newest chunk uncompressed — this is the real cost'
SELECT * FROM mw_measure.index_sizes;
\timing on
INSERT INTO observations (time, station_id, outtemp)
SELECT time, station_id, 99.5 FROM observations
 WHERE station_id = 'station-1' AND time >= :'hi_start' AND time < :'hi'
ON CONFLICT (station_id, time) DO UPDATE SET outtemp = EXCLUDED.outtemp;
\timing off
SELECT count(*) AS rows_updated_to_99_5 FROM observations
 WHERE station_id = 'station-1' AND outtemp = 99.5;

\echo '=== C. the same insert against a COMPRESSED chunk (oldest)'
SELECT chunk_name, is_compressed FROM timescaledb_information.chunks
 WHERE hypertable_name = 'observations' ORDER BY range_start LIMIT 1;
\timing on
INSERT INTO observations (time, station_id, outtemp)
SELECT time, station_id, 98.5 FROM observations
 WHERE station_id = 'station-1' AND time >= :'lo' AND time < :'lo_end'
ON CONFLICT (station_id, time) DO UPDATE SET outtemp = EXCLUDED.outtemp;
\timing off
SELECT count(*) AS rows_updated_to_98_5 FROM observations
 WHERE station_id = 'station-1' AND outtemp = 98.5;

-- The question the measurement exists for: did the conflicting rows force the chunk out
-- of compression? If they did, a replay after a cancellation is an expensive operation
-- rather than a cheap one.
\echo '--- is the oldest chunk still compressed?'
SELECT chunk_name, is_compressed FROM timescaledb_information.chunks
 WHERE hypertable_name = 'observations' ORDER BY range_start LIMIT 1;

\echo '=== D. insert throughput with and without the index, 43,200 rows each'
-- A time range outside the filled block, so neither run conflicts with existing rows.
\timing on
CALL mw_measure.fill(4, '2026-12-01 00:00:00+00', '2026-12-01 06:00:00+00');
\timing off
DROP INDEX observations_station_id_time_key;
\timing on
CALL mw_measure.fill(4, '2026-12-02 00:00:00+00', '2026-12-02 06:00:00+00');
\timing off
CREATE UNIQUE INDEX observations_station_id_time_key ON observations (station_id, time DESC);

-- Leave the dataset as it was found.
DELETE FROM observations WHERE time >= '2026-12-01 00:00:00+00';
