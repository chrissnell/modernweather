-- Back-fill 30 h of history for the two emulator-driven stations by replicating the
-- column-population pattern of their real emulator rows at 5-minute spacing.
-- Keeps the real populated-column set exactly; only `time` and a little rain differ.
CREATE TEMP TABLE davis_src AS
  SELECT *, row_number() OVER (ORDER BY time) AS rn FROM weather WHERE stationname = 'davis-full';
CREATE TEMP TABLE csi_src AS
  SELECT *, row_number() OVER (ORDER BY time) AS rn FROM weather WHERE stationname = 'csi-minimal';

DO $$
DECLARE
  n_davis int;
  n_csi   int;
BEGIN
  SELECT count(*) INTO n_davis FROM davis_src;
  SELECT count(*) INTO n_csi   FROM csi_src;

  -- Davis: 30 h back, 5-minute spacing, rain added in two bursts so the rainfall
  -- and storm functions have something to find.
  EXECUTE format($f$
    INSERT INTO weather
    SELECT (d).* FROM (
      SELECT (SELECT to_jsonb(s) FROM davis_src s
              WHERE s.rn = 1 + (g %% %s)) || jsonb_build_object(
                'time', t,
                'rainincremental',
                  CASE WHEN t BETWEEN NOW() - INTERVAL '26 hours' AND NOW() - INTERVAL '25 hours' THEN 0.01
                       WHEN t BETWEEN NOW() - INTERVAL '5 hours'  AND NOW() - INTERVAL '4 hours'  THEN 0.02
                       ELSE 0 END
              ) AS j
      FROM generate_series(NOW() - INTERVAL '30 hours', NOW() - INTERVAL '10 minutes', INTERVAL '5 minutes')
           WITH ORDINALITY AS gs(t, g)
    ) q, jsonb_populate_record(NULL::weather, q.j - 'rn') d;
  $f$, n_davis);

  -- Campbell: 30 h back, 5-minute spacing. No rain sensor on this station.
  EXECUTE format($f$
    INSERT INTO weather
    SELECT (d).* FROM (
      SELECT (SELECT to_jsonb(s) FROM csi_src s
              WHERE s.rn = 1 + (g %% %s)) || jsonb_build_object('time', t) AS j
      FROM generate_series(NOW() - INTERVAL '30 hours', NOW() - INTERVAL '10 minutes', INTERVAL '5 minutes')
           WITH ORDINALITY AS gs(t, g)
    ) q, jsonb_populate_record(NULL::weather, q.j - 'rn') d;
  $f$, n_csi);
END $$;
