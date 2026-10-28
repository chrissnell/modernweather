-- Snow gauge: 5-minute readings over the last 80 hours, base distance 1500 mm.
-- Deliberate 6-hour gap between now-40h and now-34h so a /span response covers a gap.
INSERT INTO weather (time, stationname, stationtype, snowdistance, outtemp, outhumidity, barometer)
SELECT t,
       'snowgauge', 'snowgauge',
       -- 1500 mm clear ground, settling to 1380 mm (≈4.7 in of snow) by now-6h
       GREATEST(1380.0, 1500.0 - 120.0 * LEAST(1.0, (EXTRACT(EPOCH FROM (t - (NOW() - INTERVAL '80 hours'))) / EXTRACT(EPOCH FROM INTERVAL '74 hours'))))::real,
       (28.0 + 6.0 * sin(EXTRACT(EPOCH FROM t) / 7200.0))::real,
       (82.0 + 8.0 * cos(EXTRACT(EPOCH FROM t) / 5400.0))::real,
       29.42::real
FROM generate_series(NOW() - INTERVAL '80 hours', NOW(), INTERVAL '5 minutes') AS t
WHERE t < NOW() - INTERVAL '40 hours' OR t > NOW() - INTERVAL '34 hours';

-- Air quality monitor: 5-minute readings over the last 50 hours.
INSERT INTO weather (time, stationname, stationtype, outtemp, outhumidity,
                     pm25, pm10_in_aqin, co2, extrafloat1, extrafloat2, extrafloat3, extrafloat4)
SELECT t, 'aqmon', 'airgradient',
       (66.0 + 4.0 * sin(EXTRACT(EPOCH FROM t) / 9000.0))::real,
       (41.0 + 5.0 * cos(EXTRACT(EPOCH FROM t) / 9000.0))::real,
       (12.4 + 7.0 * sin(EXTRACT(EPOCH FROM t) / 3600.0))::real,   -- pm25
       (19.8 + 9.0 * cos(EXTRACT(EPOCH FROM t) / 3600.0))::real,   -- pm10_in_aqin
       (612.0 + 90.0 * sin(EXTRACT(EPOCH FROM t) / 4800.0))::real, -- co2
       (8.1)::real,    -- extrafloat1 = PM1.0
       (21.5)::real,   -- extrafloat2 = PM10
       (0.37)::real,   -- extrafloat3 = TVOC
       (1.9)::real     -- extrafloat4 = NOx
FROM generate_series(NOW() - INTERVAL '50 hours', NOW(), INTERVAL '5 minutes') AS t;
