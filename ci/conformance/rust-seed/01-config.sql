-- The capture rig's configuration, in the NEW schema.
--
-- A translation of `ci/conformance/config-seed.sql` rather than a copy: that file is the Go
-- `devices` / `weather_websites` / `controller_configs` shape, and this one is
-- `migrations/config/`'s `stations` and `website`. The rows are the same rows -- W1 chose
-- seven devices and six websites so that every branch of the frozen contract is reachable,
-- and that choice is what the 70 fixtures were captured against.
--
-- Applied with sqlite3 to a database the daemon has already migrated, so the schema and the
-- seeded `listener` row exist. `cargo xtask conformance` applies it between the bootstrap
-- start and the real one.
--
-- **What the fixtures pin, and what is a reconstruction.** Load-bearing: the station ids
-- three fixtures address by number (`/latest?station_id=1`, `/snow?station_id=3`,
-- `/latest?station_id=4`), each station's name and configured type, each `enabled` flag,
-- which stations have coordinates, the six hostnames the `Host` headers send, and each
-- website's snow and air-quality bindings.
--
-- **Three more became load-bearing with implementation-plan.md M4-3**, which is the item
-- that renders a page: `apple_app_id` and `radar_token` are asserted as substrings of
-- `html-weather-station`, and `website.name` is the `StationName` the page renders four
-- times. The earlier note here said the radar token was not load-bearing "because the
-- runner compares JSON *shape* rather than values" -- which is true of the JSON endpoints
-- and was never true of the HTML fixtures' `required_substrings`. The page titles and the
-- exact coordinates are still reconstructions: no fixture reads either.

PRAGMA foreign_keys = ON;

BEGIN;

-- **The node row, and the reason it has to be here.**
--
-- The migration deliberately seeds none: `node_id` is a UUIDv7 a node mints once, and
-- `spool_path` has no default in the schema and no value in any design document, so the
-- boot sequence starts without one rather than deciding where a spool lives. A node with no
-- row has **no spool**, which means no reading fan-out -- and `snapshot:latest` is a
-- `Trigger::OnReading` component, so it is not planned, and it is the only provider of
-- `LatestSnapshot`. `api:legacy`'s hard need would then have nobody to satisfy it and the
-- daemon would refuse the whole configuration.
--
-- So the rig configures one, which is also the honest shape to measure: the Go rig's daemon
-- had two emulator-driven devices and a working ingest path, and a daemon that cannot
-- ingest is not the daemon production runs.
--
-- `spool_path` is **relative**, and resolved against the run's working directory --
-- `cargo xtask conformance` starts the daemon with `current_dir` at the repository root, so
-- the spool lands in `target/`. An absolute path would write outside the tree the gate was
-- invoked in.
DELETE FROM node;
INSERT INTO node (id, node_id, node_name, timezone, spool_path)
VALUES (1, x'0193f3c28b207c71ae3f10cc92d4b7aa', 'conformance', 'America/Denver',
        'target/conformance-rust/spool');

-- `website.primary_station_key` references `stations`, so the stations come first. Unlike
-- the Go schema there is no reference in the other direction -- a station does not name its
-- website -- so no deferral is needed.
DELETE FROM website;
DELETE FROM stations;

-- **Every station is `from_registration` with no transport**, which is this seed's one
-- deliberate departure from the Go rig and is worth stating.
--
-- In the Go rig two devices were driven by emulators over TCP and five were seeded straight
-- into the hypertable, with all seven carrying a `hostname` and `port` because its config
-- validation required one. Here a `transport_kind` is what makes a station a **component**
-- (W7 **D-44**: a node's position in a topology is the set of components its configuration
-- produces), so seven stations with transports would be seven station components dialling
-- ports nothing listens on for the length of the run. A station with no transport is
-- inventory -- one a peer forwards readings for -- which is exactly what these seven are:
-- their readings are inserted by `02-weather.sql` and no driver in this process
-- produces any.
--
-- The schema makes the pairing explicit: `CHECK ((transport_kind IS NULL) =
-- (identity_kind = 'from_registration'))`.
--
-- `owning_node_id` is a literal rather than this node's own id. It is not a foreign key --
-- a station may be two hops away, so its owning node is not necessarily a peer this node
-- has a row for (W7 §8.4) -- and nothing on the read path reads it.
--
-- `timezone` is set on every station, and that is load-bearing in two places: the boot
-- sequence REFUSES to start over a zone that does not resolve (`attach_registry`), and
-- `/latest`'s `sunrise` and `sunset` are formatted in it.
INSERT INTO stations
  (id, station_key, owning_node_id, name, type, enabled, latitude, longitude, altitude_m,
   timezone, snow_base_distance_mm, identity_kind)
VALUES
  -- Full sensor complement. `/latest?station_id=1` and the default station for `localhost`.
  (1, 'davis-full', x'4d57434f4e464f524d414e4345524947', 'davis-full', 'davis',
   1, 39.900, -105.500, 1800.0, 'America/Denver', NULL, 'from_registration'),
  -- A minimal complement, and the one `minimal.test` is bound to.
  (2, 'csi-minimal', x'4d57434f4e464f524d414e4345524947', 'csi-minimal', 'campbellscientific',
   1, 39.800, -105.400, 1750.0, 'America/Denver', NULL, 'from_registration'),
  -- The snow gauge. `/snow?station_id=3`, and `localhost`'s snow device.
  (3, 'snowgauge', x'4d57434f4e464f524d414e4345524947', 'snowgauge', 'snowgauge',
   0, 39.910, -105.510, 1800.0, 'America/Denver', 1500.0, 'from_registration'),
  -- The air-quality monitor. `/latest?station_id=4`, and `localhost`'s air-quality device.
  (4, 'aqmon', x'4d57434f4e464f524d414e4345524947', 'aqmon', 'airgradient',
   0, 39.905, -105.505, 1800.0, 'America/Denver', NULL, 'from_registration'),
  -- **No location and no readings**, which is three fixtures at once: `/latest` 404s,
  -- `/span` returns `[]`, `/almanac` returns `{}`, and `/api/stations` excludes it. The
  -- Go columns were `NOT NULL DEFAULT 0` so the rig wrote `0.000`; these are nullable, and
  -- `0, 0` is written rather than NULL so that the *same* predicate is being exercised --
  -- `latitude = 0 AND longitude = 0`, which is the one the Go handler applies.
  (5, 'nolocation', x'4d57434f4e464f524d414e4345524947', 'nolocation', 'campbellscientific',
   0, 0.0, 0.0, 0.0, 'America/Denver', NULL, 'from_registration'),
  -- A gauge configured and never physically present: every `/snow` field zero, still 200.
  (6, 'snowgauge-dead', x'4d57434f4e464f524d414e4345524947', 'snowgauge-dead', 'snowgauge',
   0, 39.920, -105.520, 1800.0, 'America/Denver', 1500.0, 'from_registration'),
  -- A gauge that went silent three days ago. `/snow` has no recency filter, so it keeps
  -- answering with its last known depth; `/latest` has one, so it 404s.
  (7, 'snowgauge-stale', x'4d57434f4e464f524d414e4345524947', 'snowgauge-stale', 'snowgauge',
   0, 39.930, -105.530, 1800.0, 'America/Denver', 1500.0, 'from_registration');

-- The six websites, by hostname. An unmatched `Host` resolves to the first by id (Q-18),
-- which no fixture reaches: all 70 send one of these six.
-- `apple_app_id` is written explicitly, and that is a difference from the Go rig rather
-- than a choice: the Go `weather_websites` column defaults to `6755874087` and
-- `ci/conformance/config-seed.sql` leaves it alone, while this schema defaults it to `''`.
-- `html-weather-station` asserts the value as a substring of the rendered
-- `<meta name="apple-itunes-app" content="app-id=...">` tag, so an empty one would both
-- fail the fixture and remove the tag -- which is what `{{- if .AppleAppID }}` does.
INSERT INTO website
  (id, name, hostname, page_title, about_station_html, primary_station_key,
   snow_enabled, snow_station_key, air_quality_enabled, air_quality_station_key,
   is_portal, apple_app_id, radar_enabled, radar_token, radar_registered_at)
VALUES
  -- The happy path: snow on, air quality on, radar on, about text set.
  (1, 'Suncrest Live Weather', 'localhost', 'Suncrest Live Weather',
   '<p>A Davis Vantage Pro2 on a ridge at 1800 m.</p>', 'davis-full',
   1, 'snowgauge', 1, 'aqmon', 0, '6755874087', 1, 'contract-radar-token', 1761667200),
  -- The `about_text` half of the DEV-10 hazard: no about text, so the Go server omits the
  -- key and the iOS decode of the whole object fails. This target emits it.
  (2, 'Minimal Station', 'minimal.test', 'Minimal Station', '', 'csi-minimal',
   0, NULL, 0, NULL, 0, '', 0, '', NULL),
  -- The `weather_device` half: a portal has no primary station, so Go omits BOTH keys.
  -- Also the only website `/api/stations` answers for.
  (3, 'Weather Portal', 'portal.test', 'Weather Management Portal', '', NULL,
   0, NULL, 0, NULL, 1, '', 0, '', NULL),
  -- `/snow` is a 404 for a website with snow disabled.
  (4, 'No Snow Station', 'nosnow.test', 'No Snow Station', '', 'nolocation',
   0, NULL, 0, NULL, 0, '', 0, '', NULL),
  -- The stale gauge, and the dead one.
  (5, 'Stale Snow Station', 'stalesnow.test', 'Stale Snow Station', '', 'nolocation',
   1, 'snowgauge-stale', 0, NULL, 0, '', 0, '', NULL),
  (6, 'Dead Snow Station', 'deadsnow.test', 'Dead Snow Station', '', 'nolocation',
   1, 'snowgauge-dead', 0, NULL, 0, '', 0, '', NULL);

-- **`aerisWeatherEnabled`, which is a row here and a column in the Go schema.**
--
-- The Go rig sets `devices.aeris_enabled = 1` on `davis-full`; this schema makes "which
-- stations get forecasts and alerts" a `station_service` pairing, which is what the
-- `website` table's own comment means by "`aerisWeatherEnabled` is a `station_service` row
-- with `service = 'aeris'`". The pairing carries no `uploader_id` -- an Aeris account is
-- not an uploader -- which the table's `CHECK ((service = 'aeris') = (uploader_id IS NULL))`
-- requires.
--
-- What it reaches: the rendered page's forecast band (`{{ if .AerisWeatherEnabled }}`) and
-- the ninth value of the inline configuration block. No fixture asserts either as a
-- substring, so a missing row would not fail the suite -- it would quietly render a
-- different page from the one W1 recorded, which is exactly what
-- `crates/mw-web/tests/byte_diff.rs` is for and why this row is here.
DELETE FROM station_service;
INSERT INTO station_service (station_key, service) VALUES ('davis-full', 'aeris');

COMMIT;
