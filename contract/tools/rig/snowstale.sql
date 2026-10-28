DELETE FROM weather WHERE stationname = 'snowgauge-stale';
INSERT INTO weather (time, stationname, stationtype, snowdistance, outtemp, outhumidity, barometer)
SELECT t, 'snowgauge-stale', 'snowgauge',
       GREATEST(1430.0, 1500.0 - 70.0 * (EXTRACT(EPOCH FROM (t - (NOW() - INTERVAL '6 days'))) / EXTRACT(EPOCH FROM INTERVAL '3 days')))::real,
       (26.0 + 5.0 * sin(EXTRACT(EPOCH FROM t) / 7200.0))::real,
       (80.0)::real, 29.5::real
FROM generate_series(NOW() - INTERVAL '6 days', NOW() - INTERVAL '3 days', INTERVAL '15 minutes') AS t;
