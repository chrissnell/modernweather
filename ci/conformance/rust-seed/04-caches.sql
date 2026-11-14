-- The three tables the frozen contract reads **directly**, and nothing else reads -- two of
-- which this file seeds.
--
-- W1 assumption 12: `/almanac` and `/snow` read `almanac_cache` and `snow_totals_cache`
-- "and nothing else", and `/almanac` returns `{}` rather than computing anything when its
-- cache is empty. They are not an optimisation that can be skipped here -- they are the
-- endpoints' only data source, so a seed that omitted them would make four fixtures fail on
-- an empty body rather than on the contract. **`almanac_cache` is the exception as of
-- A-M7-6-1**: it has a writer now, so seeding it would hide whether that writer works.
--
-- The third is `forecasts`, which `/forecast` reads verbatim. The 240-hour row is W1's own
-- captured Aeris payload, carried over from `contract/tools/rig/aeris.sql`; the 24-hour row is
-- rebuilt here with `jsonb_build_object` and is shape-identical rather than byte-identical --
-- its `windDir` is `'N'` for all 24 periods where the capture cycles the compass (its own
-- `windDirDEG` does cycle). The fixtures that read it assert shape and numeric convention, so
-- the difference is invisible to them; it is recorded here so nobody reads the row as a capture.

BEGIN;

-- `almanac_cache` is emptied and not re-seeded; see the note below.
DELETE FROM almanac_cache;
DELETE FROM snow_totals_cache;
DELETE FROM forecasts;

-- `/almanac` is **not seeded here, deliberately**, and this is the one table in this file
-- that is left empty on purpose.
--
-- It used to be seeded with the eight records `almanac-populated` recorded, and that seed
-- was a lie this suite told itself for two milestones. `/almanac` reads `almanac_cache` and
-- nothing else and renders no rows as `{}` with a `200` -- which is the frozen contract's
-- behaviour for an empty cache -- so a seeded rig compared against a recording of a seeded
-- rig says nothing at all about whether the daemon can produce those rows. It could not:
-- nothing in the program wrote that table, `/almanac` served `{}` in the real deployment,
-- and this gate was green throughout (`docs/reconciliation.md` **A-M7-6-1**).
--
-- `almanac:records` now computes the records from each station's own history on its first
-- occurrence, and `xtask::conformance::almanac_is_populated` asserts the rows exist
-- **after the daemon has started**. So the two `/almanac` fixtures are now a measurement of
-- the component rather than of this file, and removing the component fails the run.
--
-- Two properties of the rig that the fixtures depend on, recorded here because they are not
-- obvious and they are what keeps `almanac-populated` at eight keys:
--
--   * `aqmon` is `enabled = 0` in `01-config.sql`, and `almanac:records` plans only enabled
--     stations -- so the website's air-quality device has no records and the four
--     particulate keys are absent. Enabling it takes this response from 8 keys to 14 for
--     every station asked about, which `ci/conformance/README.md` records as having been
--     watched rather than assumed.
--   * `nolocation` has no readings, so no record can be computed for it and
--     `/almanac?station=nolocation` is `{}` -- which is `almanac-empty`, now produced by the
--     absence of data rather than by the absence of a seed.

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
