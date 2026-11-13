-- The weather database the frozen endpoints read, seeded.
--
-- `ci/conformance/README.md`'s table said of the Rust target's weather data: *"nothing yet;
-- seeding readings for the five frozen endpoints is M4-2's, which is what will make a
-- *green* replay mean something"*. This is that, and it is a **translation** of W1's rig
-- (`contract/tools/rig/`) rather than a copy: that rig writes the Go `weather` table and the
-- Go continuous aggregates, and this writes `observations` and refreshes W4's four levels.
--
-- # What each block is for, and which fixtures it serves
--
-- The runner compares **shape**, not values (`contract/conformance/src/lib.rs`), so what
-- matters here is which keys can be present and which responses are non-empty -- not the
-- numbers. Where a number does change a key's presence it is called out:
--
--   * `stationtype` is `omitempty`, so the column must be non-NULL or `/latest` and
--     `/span` lose the key. It carries the DRIVER's self-reported type (`campbell`), which
--     is **not** the configured type `/stationinfo` returns (`campbellscientific`):
--     `docs/legacy-api-contract.md` §3.1 is explicit that the two sets must not be unified.
--     Non-NULL here because that is what the migrated history has; the rewrite's own ingest
--     path does not stamp it yet, which is a known deviation recorded against M7-4 rather
--     than something this rig may decide.
--   * **`stationname` is deliberately NULL on every row below, and that is the point.**
--     It used to be seeded, which is how this suite reported green for `/latest` while the
--     preview served `{"stationname":""}`: the renderer read the column, and the column is
--     `NULL` for every row the rewrite's ingest path writes -- no codec stamps it (see
--     `mw_codec::wll`) -- while the rig wrote it by hand. The rig now reproduces what
--     production actually holds for a station this daemon ingested, and the fixtures pass
--     because `/latest` takes the name from `config.stations` (W4 assumption 24), the way
--     `/span` and `/snow` already did. Seeding it again would re-open the hole.
--   * `extratext1`..`extratext10` are `omitempty` and stay **NULL**, because all six
--     `/latest` fixtures recorded 78 keys and not 88.
--   * a reading inside the last ten minutes is what makes `/latest` a 200 (W1 Decision 15),
--     so `snowgauge-stale` gets nothing newer than three days and `nolocation` gets nothing
--     at all.
--
-- # Why the aggregates are refreshed by hand
--
-- Every level is `materialized_only` -- three of the four are the source of another, so
-- none may serve a bucket the next one up has not seen -- and the refresh policies run on
-- their own schedule. A replay that started before the first refresh would read empty
-- views and `/span` would be `[]` for every station. `03-refresh.sql` is that refresh, as a
-- separate file so that the order is visible in the harness rather than implied.

BEGIN;

-- The read-side projection. `writer:timescale` maintains it (**D-56**) and nothing writes
-- readings in this run, so it is seeded: `almanac_cache`, `snow_totals_cache` and
-- `forecasts` all reference it.
--
-- `altitude_ft` rather than the configuration's `altitude_m`, which is the one unit
-- conversion between the two sides and is in both column names so it cannot be done twice.
DELETE FROM stations;
INSERT INTO stations
  (station_id, name, type, timezone, latitude, longitude, altitude_ft,
   snow_base_distance_mm, first_seen, last_seen)
VALUES
  ('davis-full',      'davis-full',      'davis',              'America/Denver',  39.900, -105.500, 5905.5, NULL,   now() - interval '31 days', now()),
  ('csi-minimal',     'csi-minimal',     'campbellscientific', 'America/Denver',  39.800, -105.400, 5741.5, NULL,   now() - interval '31 days', now()),
  ('snowgauge',       'snowgauge',       'snowgauge',          'America/Denver',  39.910, -105.510, 5905.5, 1500.0, now() - interval '31 days', now()),
  ('aqmon',           'aqmon',           'airgradient',        'America/Denver',  39.905, -105.505, 5905.5, NULL,   now() - interval '31 days', now()),
  ('nolocation',      'nolocation',      'campbellscientific', 'America/Denver',  0.0,    0.0,      0.0,    NULL,   now() - interval '31 days', NULL),
  ('snowgauge-dead',  'snowgauge-dead',  'snowgauge',          'America/Denver',  39.920, -105.520, 5905.5, 1500.0, now() - interval '31 days', NULL),
  ('snowgauge-stale', 'snowgauge-stale', 'snowgauge',          'America/Denver',  39.930, -105.530, 5905.5, 1500.0, now() - interval '31 days', now() - interval '3 days');

DELETE FROM observations;

-- `davis-full`: a full sensor complement over 31 days at five-minute spacing, which covers
-- every `/span` band -- `/span/24h` reads the 5-minute level and `/span/720h` the day
-- level. The last row is at `now()`, so `/latest` is a 200.
--
-- The values are a smooth synthetic series rather than captured data. That is not a
-- shortcut: the runner asserts the integer-or-decimal property of each literal against the
-- *value*, so what a seed has to provide is values of both kinds -- which a sine does, and
-- a table of constants would not.
INSERT INTO observations (
    time, station_id, stationname, stationtype,
    outtemp, outhumidity, barometer, windspeed, windspeed10, winddir,
    rainrate, rainincremental, dayrain, monthrain, yearrain, stormrain,
    solarwatts, potentialsolarwatts, solarjoules, uv, radiation,
    intemp, inhumidity, consbatteryvoltage, stationbatteryvoltage,
    extratemp1, extratemp2, soiltemp1, soiltemp2, leaftemp1, leaftemp2,
    extrahumidity1, extrahumidity2,
    extrafloat1, extrafloat2, extrafloat3,
    windchill, heatindex)
SELECT
    t, 'davis-full', NULL, 'davis',
    (55 + 12 * sin(extract(epoch FROM t) / 43200.0))::float4,
    (47 + 20 * cos(extract(epoch FROM t) / 43200.0))::float4,
    (29.981 + 0.2 * sin(extract(epoch FROM t) / 86400.0))::float4,
    (4 + 6 * abs(sin(extract(epoch FROM t) / 3600.0)))::float4,
    (4 + 5 * abs(sin(extract(epoch FROM t) / 3600.0)))::float4,
    ((extract(epoch FROM t) / 60)::int % 360)::float4,
    0::float4,
    (CASE WHEN (extract(epoch FROM t)::bigint / 300) % 97 = 0 THEN 0.01 ELSE 0 END)::float4,
    0::float4, 0::float4, 0::float4, 0::float4,
    greatest(0, 478 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
    greatest(0, 503 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
    greatest(0, 1.7 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
    greatest(0, 4 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
    greatest(0, 478 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
    68::float4, 31::float4, 4.72::float4, 3.98::float4,
    (54 + 3 * sin(extract(epoch FROM t) / 43200.0))::float4, 51::float4,
    48::float4, 49::float4, 52::float4, 53::float4,
    61::float4, 62::float4,
    1.5::float4, 2::float4, 3.25::float4,
    -- Stored where the formula applies and NULL where it does not, which is **DEV-9**:
    -- the read path recomputes both anyway, and a stored `windchill = outtemp` would be
    -- indistinguishable from a real one.
    NULL, NULL
FROM generate_series(now() - interval '31 days', now(), interval '5 minutes') AS t;

-- The last ninety minutes at one-minute spacing, so that `/span/1h` reads a 1-minute level
-- with more than one bucket in it. `ON CONFLICT` because the five-minute series above
-- already covers every fifth minute, and the raw table's uniqueness on
-- `(station_id, time)` is what makes a replayed prefix idempotent (W3 assumption 22) --
-- the same property this relies on.
INSERT INTO observations (time, station_id, stationname, stationtype,
                          outtemp, outhumidity, barometer, windspeed, winddir,
                          rainincremental, solarwatts, intemp, inhumidity)
SELECT t, 'davis-full', NULL, 'davis',
       (55 + 12 * sin(extract(epoch FROM t) / 43200.0))::float4,
       (47 + 20 * cos(extract(epoch FROM t) / 43200.0))::float4,
       29.981::float4,
       (4 + 6 * abs(sin(extract(epoch FROM t) / 3600.0)))::float4,
       ((extract(epoch FROM t) / 60)::int % 360)::float4,
       0::float4,
       greatest(0, 478 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
       68::float4, 31::float4
FROM generate_series(now() - interval '90 minutes', now(), interval '1 minute') AS t
ON CONFLICT (station_id, time) DO NOTHING;

-- `csi-minimal`: a Campbell logger, which is what `latest-minimal-complement` and
-- `span-1h-one-minute-buckets` read. **`stationtype` is `campbell`**, the driver's name,
-- not the configured `campbellscientific` -- the two sets must not be unified
-- (`docs/legacy-api-contract.md` §3.1).
--
-- Two series, exactly as `davis-full` has, and the reason is a trap worth recording: the
-- first version of this block generated one 1-minute series and filtered it with
-- `extract(epoch FROM t)::bigint % 300 = 0`, which inserted **nothing**. `now()` is not on
-- a minute boundary, so no member of a series stepping from it is ever congruent to zero.
-- `generate_series` with the spacing it wants is the fix; the per-station assertion in
-- `xtask/src/conformance.rs` is what would have caught it.
INSERT INTO observations (time, station_id, stationname, stationtype,
                          outtemp, outhumidity, barometer, windspeed, winddir,
                          rainincremental, solarwatts, solarjoules, radiation)
SELECT t, 'csi-minimal', NULL, 'campbell',
       (48 + 9 * sin(extract(epoch FROM t) / 43200.0))::float4,
       (55 + 15 * cos(extract(epoch FROM t) / 43200.0))::float4,
       (29.7 + 0.15 * sin(extract(epoch FROM t) / 86400.0))::float4,
       (2 + 4 * abs(cos(extract(epoch FROM t) / 3600.0)))::float4,
       ((extract(epoch FROM t) / 90)::int % 360)::float4,
       0::float4,
       greatest(0, 455 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
       greatest(0, 1.6 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4,
       greatest(0, 455 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4
FROM generate_series(now() - interval '31 days', now(), interval '5 minutes') AS t;

-- The last two hours at one-minute spacing, which is what makes `/span/1h` -- the only
-- fixture that reads the 1-minute level -- an array with more than one element.
INSERT INTO observations (time, station_id, stationname, stationtype,
                          outtemp, outhumidity, barometer, windspeed, winddir,
                          rainincremental, solarwatts)
SELECT t, 'csi-minimal', NULL, 'campbell',
       (48 + 9 * sin(extract(epoch FROM t) / 43200.0))::float4,
       (55 + 15 * cos(extract(epoch FROM t) / 43200.0))::float4,
       29.7::float4,
       (2 + 4 * abs(cos(extract(epoch FROM t) / 3600.0)))::float4,
       ((extract(epoch FROM t) / 90)::int % 360)::float4,
       0::float4,
       greatest(0, 455 * sin(pi() * (extract(hour FROM t) - 6) / 12.0))::float4
FROM generate_series(now() - interval '2 hours', now(), interval '1 minute') AS t
ON CONFLICT (station_id, time) DO NOTHING;

-- `snowgauge`: hourly over 31 days, **with a gap**. `span-72h-hourly-with-gap` is the
-- fixture that records Q-9 -- a gap in `/span` is simply missing buckets, because the
-- continuous aggregate produces no row for a bucket no reading fell in and the handler does
-- not gap-fill. The `WHERE` is that gap: a six-hour hole thirty hours ago.
--
-- `snowdepth` is a stored column here (**DEV-9**), in **millimetres**: `/latest` serves it
-- unconverted and `/span` and `/snow` convert to inches (Q-4).
INSERT INTO observations (time, station_id, stationname, stationtype,
                          outtemp, outhumidity, snowdistance, snowdepth, extrafloat1)
SELECT t, 'snowgauge', NULL, 'snowgauge',
       (30 + 8 * sin(extract(epoch FROM t) / 43200.0))::float4,
       (78 + 10 * cos(extract(epoch FROM t) / 43200.0))::float4,
       (1500 - 119.38 - 20 * sin(extract(epoch FROM t) / 86400.0))::float4,
       (119.38 + 20 * sin(extract(epoch FROM t) / 86400.0))::float4,
       4.7::float4
FROM generate_series(now() - interval '31 days', now(), interval '10 minutes') AS t
WHERE NOT (t BETWEEN now() - interval '36 hours' AND now() - interval '30 hours');

-- `aqmon`: an AirGradient, which `latest-air-quality-device` reads. Only the last two hours,
-- because nothing on the frozen surface asks it for a span.
--
-- `aqi_pm25_aqin` is left **NULL**, which exercises the branch the contract actually has:
-- `transform.go:209` reads the column only when it is positive and computes from `pm25`
-- otherwise, so a client was never charting a flat zero here.
INSERT INTO observations (time, station_id, stationname, stationtype,
                          outtemp, outhumidity, pm25, pm25_in_aqin, pm10_in_aqin, co2,
                          pm_in_temp_aqin, pm_in_humidity_aqin)
SELECT t, 'aqmon', NULL, 'airgradient',
       (52 + 4 * sin(extract(epoch FROM t) / 43200.0))::float4,
       (44 + 8 * cos(extract(epoch FROM t) / 43200.0))::float4,
       (6.2 + 3 * abs(sin(extract(epoch FROM t) / 1800.0)))::float4,
       (5.1 + 2 * abs(sin(extract(epoch FROM t) / 1800.0)))::float4,
       (9.4 + 4 * abs(cos(extract(epoch FROM t) / 1800.0)))::float4,
       (612 + 80 * sin(extract(epoch FROM t) / 3600.0))::float4,
       71::float4, 38
FROM generate_series(now() - interval '2 hours', now(), interval '2 minutes') AS t;

-- `snowgauge-stale`: silent for three days. `/snow` keeps answering from its cache row and
-- `/latest` is a 404, which is the pair `snow-device-stale` and
-- `latest-snow-device-offline` record.
INSERT INTO observations (time, station_id, stationname, stationtype,
                          outtemp, snowdistance, snowdepth)
SELECT t, 'snowgauge-stale', NULL, 'snowgauge',
       28::float4, (1500 - 71.12)::float4, 71.12::float4
FROM generate_series(now() - interval '10 days', now() - interval '3 days', interval '1 hour') AS t;

-- `nolocation` and `snowgauge-dead` get **nothing**, which is three fixtures:
-- `latest-no-recent-data`, `span-empty`, `almanac-empty` and `snow-device-never-reported`.

COMMIT;
