-- Run immediately before capture.py.
--
-- /latest only returns a reading from the last ten minutes, and the Go Davis driver stops
-- polling after one successful 20-packet LOOP batch (davis/station.go:200), so the two
-- emulator-driven stations need their newest row cloned forward to NOW(). Cloning the whole
-- row keeps the set of populated columns exactly what the Go driver wrote; only `time` moves.
INSERT INTO weather SELECT (d).* FROM (
  SELECT to_jsonb(s) || jsonb_build_object('time', NOW()) AS j
  FROM weather s WHERE s.stationname = 'davis-full' ORDER BY s.time DESC LIMIT 1
) q, jsonb_populate_record(NULL::weather, q.j) d;

INSERT INTO weather SELECT (d).* FROM (
  SELECT to_jsonb(s) || jsonb_build_object('time', NOW()) AS j
  FROM weather s WHERE s.stationname = 'csi-minimal' ORDER BY s.time DESC LIMIT 1
) q, jsonb_populate_record(NULL::weather, q.j) d;

-- The two directly-seeded live stations.
INSERT INTO weather (time, stationname, stationtype, snowdistance, outtemp, outhumidity, barometer)
VALUES (NOW(), 'snowgauge', 'snowgauge', 1380.0, 29.3, 86.0, 29.42);

INSERT INTO weather (time, stationname, stationtype, outtemp, outhumidity, pm25, pm10_in_aqin, co2,
                     extrafloat1, extrafloat2, extrafloat3, extrafloat4)
VALUES (NOW(), 'aqmon', 'airgradient', 67.8, 39.0, 15.2, 23.4, 702.0, 8.1, 21.5, 0.37, 1.9);

-- The Go daemon inserts its entire reading struct, so a row it writes has no NULLs. Match that
-- for the directly-seeded stations: leaving winddir NULL makes /span return 500 (W1-BUG-1).
-- Generate the column list with:
--   SELECT string_agg(format('%I = COALESCE(%I, 0)', column_name, column_name), ', ')
--     FROM information_schema.columns
--    WHERE table_name = 'weather'
--      AND data_type IN ('real','double precision','integer','bigint','smallint');
-- then: UPDATE weather SET <that list> WHERE stationname IN ('snowgauge','aqmon','snowgauge-stale');
