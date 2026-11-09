-- The three tables the frozen contract reads **directly**, and nothing else reads.
--
-- W1 assumption 12: `/almanac` and `/snow` read `almanac_cache` and `snow_totals_cache`
-- "and nothing else", and `/almanac` returns `{}` rather than computing anything when its
-- cache is empty. They are not an optimisation that can be skipped here -- they are the
-- endpoints' only data source, so a seed that omitted them would make four fixtures fail on
-- an empty body rather than on the contract.
--
-- The third is `forecasts`, which `/forecast` reads verbatim. The rows below are W1's own
-- captured Aeris payloads, carried over from `contract/tools/rig/aeris.sql`.

BEGIN;

DELETE FROM almanac_cache;
DELETE FROM snow_totals_cache;
DELETE FROM forecasts;

-- `/almanac?station=davis-full` -- the eight records `almanac-populated` recorded.
--
-- **Exactly eight, and no air-quality records**, which is a property of the rig rather than
-- an omission: `/almanac` reads the particulate extremes from the WEBSITE's air-quality
-- device, so seeding `aqmon`'s rows would take this response from 8 keys to 14 -- for every
-- station asked about, which is what `ci/conformance/README.md` records as having been
-- watched rather than assumed.
--
-- Only `high_wind_speed` carries a `wind_dir`. The column is nullable and the JSON field is
-- `omitempty` on a pointer, so an absent direction is an absent KEY and never a null --
-- which is the difference between a client reading "no direction was recorded" and one
-- failing to decode.
INSERT INTO almanac_cache (station_id, record, value, observed_at, wind_dir)
VALUES
  ('davis-full', 'high_temp',       66.8,     now() - interval '18 hours', NULL),
  ('davis-full', 'low_temp',        62.9,     now() - interval '18 hours', NULL),
  ('davis-full', 'high_wind_speed', 22,       now() - interval '18 hours', 35.99653),
  ('davis-full', 'max_rain_hour',   0,        now() - interval '2 hours',  NULL),
  ('davis-full', 'max_rain_day',    0,        now() - interval '18 hours', NULL),
  ('davis-full', 'low_barometer',   29.981,   now() - interval '18 hours', NULL),
  ('davis-full', 'low_humidity',    47,       now() - interval '18 hours', NULL),
  ('davis-full', 'high_solar',      478,      now() - interval '90 minutes', NULL);

-- `/snow`. Three gauges and three states:
--
--   `snowgauge`        reporting -- what `localhost` and `?station_id=3` read.
--   `snowgauge-stale`  silent for three days and still answering, because there is no
--                      recency filter on this endpoint and there must not be one.
--   `snowgauge-dead`   **no row at all**, which is `snow-device-never-reported`: every
--                      field zero with status 200, produced by the absence of data rather
--                      than by a branch.
--
-- `davis-full` has no row either, which is `snow-station-param-overrides-depth-only`:
-- `?station=davis-full` redirects the DEPTH lookup only (Q-8), so the response carries that
-- station's absent depth under the snow gauge's name with the snow gauge's totals.
--
-- Inches, which is this endpoint's unit -- not the millimetres `snowdepth` carries on
-- `/latest` (Q-4).
INSERT INTO snow_totals_cache
  (station_id, snowdepth, snowtoday, snowlast24, snowlast72, snowseason, snowstorm,
   snowfallrate, computed_at, source_through)
VALUES
  ('snowgauge',       4.7, 0, 0.6, 2.7, 4.0, 0.6, 0, now(), now()),
  ('snowgauge-stale', 2.8, 0, 0,   0,   2.3, 0,   0, now() - interval '3 days',
                                                     now() - interval '3 days');

-- `/forecast`. W1's captured Aeris payloads, with the station id resolved to a station key.
--
-- Three rows, which is four fixtures: span 240 for `davis-full` (ten daily periods),
-- span 24 for `davis-full` (twenty-four hourly ones), and span 240 for `csi-minimal` with
-- an **empty array**, which is `forecast-empty-data`.
--
-- `updated_at` is what `lastUpdated` renders, and `davis-full`'s is the newer of the two
-- 240-hour rows on purpose: `/forecast/240` with no station answers with the most recently
-- fetched forecast for that span (`mw_store::frozen`), and `forecast-240-ten-daily-periods`
-- sends no station.
INSERT INTO forecasts (station_id, span_hours, data, updated_at) VALUES
('davis-full', 240, $json$[
{"dateTimeISO":"2026-10-28T00:00:00-06:00","maxTempF":48,"minTempF":24,"avgTempF":36,"pop":10,"precipIN":0.0,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":45,"minFeelslikeF":19,"windSpeedMPH":7,"windSpeedMaxMPH":14,"windDir":"NW","windDirDEG":315,"weather":"Mostly Sunny","weatherPrimaryCoded":":FW:","weatherIcon":"fair.png","compactWeather":"Mostly Sunny"},
{"dateTimeISO":"2026-10-29T00:00:00-06:00","maxTempF":47,"minTempF":23,"avgTempF":35,"pop":20,"precipIN":0.0,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":44,"minFeelslikeF":18,"windSpeedMPH":8,"windSpeedMaxMPH":15,"windDir":"N","windDirDEG":0,"weather":"Partly Cloudy","weatherPrimaryCoded":":SC:","weatherIcon":"pcloudy.png","compactWeather":"Partly Cloudy"},
{"dateTimeISO":"2026-10-30T00:00:00-06:00","maxTempF":46,"minTempF":22,"avgTempF":34,"pop":70,"precipIN":0.31,"iceaccumIN":0.0,"snowIN":3.5,"maxFeelslikeF":43,"minFeelslikeF":17,"windSpeedMPH":9,"windSpeedMaxMPH":16,"windDir":"NE","windDirDEG":45,"weather":"Snow Showers","weatherPrimaryCoded":"S:R:S","weatherIcon":"snow.png","compactWeather":"Snow Showers"},
{"dateTimeISO":"2026-10-31T00:00:00-06:00","maxTempF":45,"minTempF":21,"avgTempF":33,"pop":30,"precipIN":0.02,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":42,"minFeelslikeF":16,"windSpeedMPH":10,"windSpeedMaxMPH":17,"windDir":"E","windDirDEG":90,"weather":"Cloudy","weatherPrimaryCoded":":OV:","weatherIcon":"cloudy.png","compactWeather":"Cloudy"},
{"dateTimeISO":"2026-11-01T00:00:00-06:00","maxTempF":44,"minTempF":20,"avgTempF":32,"pop":60,"precipIN":0.46,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":41,"minFeelslikeF":15,"windSpeedMPH":11,"windSpeedMaxMPH":18,"windDir":"SE","windDirDEG":135,"weather":"Rain Showers","weatherPrimaryCoded":"RW:R:R","weatherIcon":"rain.png","compactWeather":"Rain Showers"},
{"dateTimeISO":"2026-11-02T00:00:00-06:00","maxTempF":43,"minTempF":19,"avgTempF":31,"pop":15,"precipIN":0.0,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":40,"minFeelslikeF":14,"windSpeedMPH":12,"windSpeedMaxMPH":19,"windDir":"S","windDirDEG":180,"weather":"Mostly Cloudy","weatherPrimaryCoded":":BK:","weatherIcon":"mcloudy.png","compactWeather":"Mostly Cloudy"},
{"dateTimeISO":"2026-11-03T00:00:00-06:00","maxTempF":42,"minTempF":18,"avgTempF":30,"pop":0,"precipIN":0.0,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":39,"minFeelslikeF":13,"windSpeedMPH":13,"windSpeedMaxMPH":20,"windDir":"SW","windDirDEG":225,"weather":"Sunny","weatherPrimaryCoded":":CL:","weatherIcon":"sunny.png","compactWeather":"Sunny"},
{"dateTimeISO":"2026-11-04T00:00:00-06:00","maxTempF":41,"minTempF":17,"avgTempF":29,"pop":55,"precipIN":0.12,"iceaccumIN":0.0,"snowIN":1.2,"maxFeelslikeF":38,"minFeelslikeF":12,"windSpeedMPH":14,"windSpeedMaxMPH":21,"windDir":"W","windDirDEG":270,"weather":"Light Snow","weatherPrimaryCoded":"S:L:S","weatherIcon":"snow.png","compactWeather":"Light Snow"},
{"dateTimeISO":"2026-11-05T00:00:00-06:00","maxTempF":40,"minTempF":16,"avgTempF":28,"pop":45,"precipIN":0.08,"iceaccumIN":0.04,"snowIN":0.0,"maxFeelslikeF":37,"minFeelslikeF":11,"windSpeedMPH":15,"windSpeedMaxMPH":22,"windDir":"WNW","windDirDEG":292,"weather":"Freezing Rain","weatherPrimaryCoded":"ZR:L:RS","weatherIcon":"sleet.png","compactWeather":"Freezing Rain"},
{"dateTimeISO":"2026-11-06T00:00:00-06:00","maxTempF":39,"minTempF":15,"avgTempF":27,"pop":5,"precipIN":0.0,"iceaccumIN":0.0,"snowIN":0.0,"maxFeelslikeF":36,"minFeelslikeF":10,"windSpeedMPH":16,"windSpeedMaxMPH":23,"windDir":"NNE","windDirDEG":22,"weather":"Mostly Sunny","weatherPrimaryCoded":":FW:","weatherIcon":"fair.png","compactWeather":"Mostly Sunny"}
]$json$::jsonb, TIMESTAMPTZ '2026-10-28 15:45:12.482913+00');

INSERT INTO forecasts (station_id, span_hours, data, updated_at)
SELECT 'davis-full', 24,
       jsonb_agg(jsonb_build_object(
           'dateTimeISO',         to_char(timestamp '2026-10-28 00:00:00' + (h || ' hours')::interval, 'YYYY-MM-DD"T"HH24:MI:SS') || '-06:00',
           'maxTempF',            38 + (h % 7),
           'minTempF',            22 + (h % 5),
           'avgTempF',            30 + (h % 9),
           'pop',                 CASE WHEN h BETWEEN 8 AND 12 THEN 70 ELSE 10 END,
           'precipIN',            CASE WHEN h BETWEEN 8 AND 12 THEN 0.05 ELSE 0.0 END,
           'iceaccumIN',          0.0,
           'snowIN',              CASE WHEN h BETWEEN 8 AND 12 THEN 0.6 ELSE 0.0 END,
           'maxFeelslikeF',       36 + (h % 7),
           'minFeelslikeF',       18 + (h % 5),
           'windSpeedMPH',        4 + (h % 11),
           'windSpeedMaxMPH',     9 + (h % 15),
           'windDir',             'N',
           'windDirDEG',          (h * 15) % 360,
           'weather',             CASE WHEN h BETWEEN 8 AND 12 THEN 'Light Snow' ELSE 'Mostly Clear' END,
           'weatherPrimaryCoded', CASE WHEN h BETWEEN 8 AND 12 THEN 'S:L:S' ELSE ':FW:' END,
           'weatherIcon',         CASE WHEN h BETWEEN 8 AND 12 THEN 'snow.png' ELSE 'clear.png' END,
           'compactWeather',      CASE WHEN h BETWEEN 8 AND 12 THEN 'Light Snow' ELSE 'Mostly Clear' END
       ) ORDER BY h),
       TIMESTAMPTZ '2026-10-28 15:45:12.482913+00'
FROM generate_series(0, 23) AS h;

-- The empty one. `[]` is an array, which the table's own `CHECK` requires -- the `?period=`
-- branch indexes the stored document and a stored object would make it unexpressible.
--
-- An hour older than `davis-full`'s, so that `/forecast/240` with no station answers with
-- `davis-full`'s ten periods.
INSERT INTO forecasts (station_id, span_hours, data, updated_at)
VALUES ('csi-minimal', 240, '[]'::jsonb, TIMESTAMPTZ '2026-10-28 14:45:12.482913+00');

COMMIT;
