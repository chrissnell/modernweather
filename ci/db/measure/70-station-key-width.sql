-- Measurement 6b: what a text station_id costs against a surrogate int4.
--
-- The W4 prompt asks for a surrogate `station_id int` and this schema keys readings by the
-- text id that arrives on the wire (protobuf field 221). That is a decision against the
-- prompt, so it needs a number rather than an argument.
--
-- Two hypertables, identical but for the partition column's type, the same rows in each,
-- measured uncompressed and compressed. The compressed figure is the one that decides it:
-- station_id is the compress_segmentby key, so a compressed batch stores it once rather
-- than once per row, and whatever the uncompressed delta is, it applies only for as long
-- as a chunk stays uncompressed — seven days under D-7's policy.

DROP TABLE IF EXISTS mw_measure.key_text CASCADE;
DROP TABLE IF EXISTS mw_measure.key_int  CASCADE;

CREATE TABLE mw_measure.key_text (
    time       timestamptz NOT NULL,
    station_id text        NOT NULL,
    outtemp    float4, barometer float4, outhumidity float4, windspeed float4,
    winddir    float4, rainincremental float4, intemp float4, inhumidity float4
);
CREATE TABLE mw_measure.key_int (
    time       timestamptz NOT NULL,
    station_id int4        NOT NULL,
    outtemp    float4, barometer float4, outhumidity float4, windspeed float4,
    winddir    float4, rainincremental float4, intemp float4, inhumidity float4
);

SELECT create_hypertable('mw_measure.key_text', 'time', chunk_time_interval => INTERVAL '7 days');
SELECT create_hypertable('mw_measure.key_int',  'time', chunk_time_interval => INTERVAL '7 days');

ALTER TABLE mw_measure.key_text SET (timescaledb.compress,
    timescaledb.compress_segmentby = 'station_id', timescaledb.compress_orderby = 'time DESC');
ALTER TABLE mw_measure.key_int  SET (timescaledb.compress,
    timescaledb.compress_segmentby = 'station_id', timescaledb.compress_orderby = 'time DESC');

-- One day, four stations, the production cadence: 172,800 rows into each.
INSERT INTO mw_measure.key_text
SELECT g.t, 'station-' || i,
       outtemp, barometer, outhumidity, windspeed, winddir, rainincremental, intemp, inhumidity
  FROM generate_series('2026-01-01'::timestamptz,
                       '2026-01-02'::timestamptz - INTERVAL '2 sec', INTERVAL '2 sec') g(t)
 CROSS JOIN generate_series(1, 4) i
 CROSS JOIN LATERAL (SELECT
       round((60 + 12 * sin(2*pi()*extract(epoch from g.t)/3000))::numeric, 1)::float4 outtemp,
       round((29 +  1 * sin(2*pi()*extract(epoch from g.t)/4200))::numeric, 2)::float4 barometer,
       round((55 + 14 * sin(2*pi()*extract(epoch from g.t)/3600))::numeric, 1)::float4 outhumidity,
       round(( 7 +  5 * sin(2*pi()*extract(epoch from g.t)/2700))::numeric, 1)::float4 windspeed,
       round((180 + 90 * sin(2*pi()*extract(epoch from g.t)/5100))::numeric, 1)::float4 winddir,
       0.0::float4 rainincremental,
       round((68 +  4 * sin(2*pi()*extract(epoch from g.t)/4800))::numeric, 1)::float4 intemp,
       round((40 +  8 * sin(2*pi()*extract(epoch from g.t)/3900))::numeric, 1)::float4 inhumidity) v;

INSERT INTO mw_measure.key_int
SELECT time, replace(station_id, 'station-', '')::int4,
       outtemp, barometer, outhumidity, windspeed, winddir, rainincremental, intemp, inhumidity
  FROM mw_measure.key_text;

ANALYZE mw_measure.key_text;
ANALYZE mw_measure.key_int;

\echo '--- uncompressed'
SELECT 'text' AS key_type, hypertable_size('mw_measure.key_text') AS bytes,
       round(hypertable_size('mw_measure.key_text') / 172800.0, 1) AS bytes_per_row
UNION ALL
SELECT 'int4', hypertable_size('mw_measure.key_int'),
       round(hypertable_size('mw_measure.key_int') / 172800.0, 1);

SELECT count(compress_chunk(c)) FROM show_chunks('mw_measure.key_text') c;
SELECT count(compress_chunk(c)) FROM show_chunks('mw_measure.key_int')  c;

\echo '--- compressed'
SELECT 'text' AS key_type, hypertable_size('mw_measure.key_text') AS bytes,
       round(hypertable_size('mw_measure.key_text') / 172800.0, 1) AS bytes_per_row
UNION ALL
SELECT 'int4', hypertable_size('mw_measure.key_int'),
       round(hypertable_size('mw_measure.key_int') / 172800.0, 1);

DROP TABLE mw_measure.key_text CASCADE;
DROP TABLE mw_measure.key_int  CASCADE;
