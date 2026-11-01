-- The W1 capture rig's configuration database, as data rather than as prose.
--
-- contract/tools/README.md "Configuration" describes the devices and websites W1 chose so that
-- every branch of the frozen contract is reachable, but it describes them in a table a human was
-- expected to retype. (Its prose says "five devices" above a table listing seven; seven is
-- right, and the fixtures prove it.)
--
-- The fixtures pin the parts that are not free: the station ids three of them address by number
-- (`/latest?station_id=1`, `/snow?station_id=3`, `/latest?station_id=4`), the id/name/type and
-- location of every device /api/stations returns, each `enabled` flag, the six hostnames the
-- Host headers send, and the strings the HTML and weather-app.js fixtures require as substrings.
--
-- What a green replay does NOT check, because contract/conformance compares JSON *shape* rather
-- than values: the page titles, the exact coordinates, every string in remote_stations, the
-- unused ports below, and the gRPC settings. Those are reconstructions of the rig, chosen to be
-- consistent with it, and a change to one of them will not fail the suite.
--
-- Applied with sqlite3 to a database `remoteweather -config` has already bootstrapped, so the
-- schema (pkg/config/provider_sqlite.go) and the default `configs` row already exist.

PRAGMA foreign_keys = ON;

BEGIN;

-- devices.website_id and weather_websites.device_id reference each other, so neither table can
-- be populated first under immediate enforcement. Defer to COMMIT, which still rejects a
-- dangling reference -- it just stops caring about the order the rows arrive in.
PRAGMA defer_foreign_keys = ON;

DELETE FROM devices;
DELETE FROM weather_websites;
DELETE FROM remote_stations;
DELETE FROM controller_configs WHERE controller_type = 'rest';

-- Websites first, because devices.website_id references them.
--
-- The ids themselves are not load-bearing. No fixture reads a website id, and the REST server's
-- fallback for an unmatched Host is not "the first row" either: GetWeatherWebsites returns them
-- ORDER BY name, so DefaultWebsite ends up being `Dead Snow Station`. That is unreachable here
-- anyway -- websiteMiddleware matches Host exactly and all 70 fixtures send one of the six
-- hostnames below -- but it is worth not believing the opposite.
--
-- apple_app_id is left at its schema default (6755874087), which html-weather-station asserts.
INSERT INTO weather_websites
  (id, config_id, name, device_id, hostname, page_title, about_station_html,
   snow_enabled, snow_device_name, air_quality_enabled, air_quality_device_name,
   is_portal, radar_enabled, radar_token)
VALUES
  -- The happy path: snow on, air quality on, radar on, about text set.
  (1, 1, 'Suncrest Live Weather', 1, 'localhost', 'Suncrest Live Weather',
   '<p>A Davis Vantage Pro2 on a ridge at 1800 m.</p>',
   1, 'snowgauge', 1, 'aqmon', 0, 1, 'contract-radar-token'),
  -- The about_text hazard: about_station_html empty, so /stationinfo omits about_text and the
  -- iOS client's non-optional String fails to decode. legacy-api-contract.md §6.
  (2, 1, 'Minimal Station', 2, 'minimal.test', 'Minimal Station', '',
   0, '', 0, '', 0, 0, ''),
  -- The weather_device hazard: a portal has no device_id, so /stationinfo omits BOTH
  -- weather_device and about_text. Also the only website /api/stations answers.
  --
  -- The page_title is cosmetic. html-portal and html-portal-root assert the substring
  -- "Weather Management Portal", but that comes from restserver/assets/portal.html.tmpl, which
  -- consumes no template variables at all; page_title reaches the wire only through
  -- /api/stations, where only its type is asserted.
  (3, 1, 'Weather Portal', NULL, 'portal.test', 'Weather Management Portal', '',
   0, '', 0, '', 1, 0, ''),
  -- The three websites below carry device_id 5 (`nolocation`). That is a reconstruction, not
  -- something contract/tools/README.md recorded: its website table names no device for these
  -- three. A non-portal website needs a resolvable primary device or `/` 500s, and pointing all
  -- three at the device with no location keeps them invisible to /api/stations, which excludes
  -- devices without coordinates. Three websites sharing one device is a state the management
  -- API's own uniqueness check would refuse, so do not copy this into a real configuration.
  --
  -- /snow 404 for a website with snow disabled.
  (4, 1, 'No Snow Station', 5, 'nosnow.test', 'No Snow Station', '',
   0, '', 0, '', 0, 0, ''),
  -- /snow has no recency filter, so a gauge that went silent three days ago keeps answering.
  (5, 1, 'Stale Snow Station', 5, 'stalesnow.test', 'Stale Snow Station', '',
   1, 'snowgauge-stale', 0, '', 0, 0, ''),
  -- A gauge configured but never physically present: every field zero, still 200.
  (6, 1, 'Dead Snow Station', 5, 'deadsnow.test', 'Dead Snow Station', '',
   1, 'snowgauge-dead', 0, '', 0, 0, '');

-- Devices. The ids are load-bearing: three fixtures address stations by id, and
-- asset-weather-app-js asserts `airQualityDeviceID: 4`.
--
-- Only the two emulator-driven devices are enabled. The rest are seeded straight into the
-- hypertable because no emulator exists for them, and an enabled device with no station behind
-- it would have the daemon dialling a dead port for the length of the run. They still carry a
-- hostname and port: config validation requires every device to name a transport, enabled or
-- not, so the seeded five point at ports nothing listens on.
INSERT INTO devices
  (id, config_id, name, type, enabled, hostname, port, base_snow_distance,
   website_id, latitude, longitude, altitude, aeris_enabled)
VALUES
  -- Full sensor complement; aeris_enabled is what registers /forecast and /alerts. No client id
  -- or secret is written, so both are NULL: the Aeris controller only polls the upstream API
  -- when both are non-empty, so the routes exist and contract/tools/rig/aeris.sql stays the only
  -- source. (They are read through sql.NullString, so NULL is safe here -- unlike
  -- remote_stations below.)
  (1, 1, 'davis-full',      'davis',              1, '127.0.0.1', '22222',    0, 1, 39.900, -105.500, 1800, 1),
  (2, 1, 'csi-minimal',     'campbellscientific', 1, '127.0.0.1', '8123',     0, 2, 39.800, -105.400, 1750, 0),
  (3, 1, 'snowgauge',       'snowgauge',          0, '127.0.0.1', '19001', 1500, NULL, 39.910, -105.510, 1800, 0),
  (4, 1, 'aqmon',           'airgradient',        0, '127.0.0.1', '19002',    0, NULL, 39.905, -105.505, 1800, 0),
  -- A configured device with no location and no readings: /latest 404s, /span returns [],
  -- /almanac returns {}, and /api/stations excludes it for having no location.
  (5, 1, 'nolocation',      'campbellscientific', 0, '127.0.0.1', '19003',    0, NULL,  0.000,    0.000,    0, 0),
  (6, 1, 'snowgauge-dead',  'snowgauge',          0, '127.0.0.1', '19004', 1500, NULL, 39.920, -105.520, 1800, 0),
  (7, 1, 'snowgauge-stale', 'snowgauge',          0, '127.0.0.1', '19005', 1500, NULL, 39.930, -105.530, 1800, 0);

-- /api/remote-stations. No shipped client reads it; the fixture exists because the endpoint is
-- on the frozen surface. Both rows are stale on purpose, which is the `status: offline` branch.
--
-- Every text column is written as '' rather than left NULL: GetRemoteStations scans straight
-- into Go strings, so a NULL aprs_password is not a blank password but a 500 from
-- /api/remote-stations ("converting NULL to string is unsupported").
INSERT INTO remote_stations
  (station_id, station_name, station_type,
   aprs_enabled, aprs_callsign, aprs_password,
   wu_enabled, wu_station_id, wu_api_key,
   aeris_enabled, aeris_client_id, aeris_client_secret,
   pws_enabled, pws_station_id, pws_password,
   registered_at, last_seen)
VALUES
  ('b7c1e9f4-0e2d-4a7b-9c31-5f6a2d8e1b04', 'ridge-remote',  'davis',
   1, 'N0CALL-7', '', 1, 'KCOCONTRACT1', '', 0, '', '', 1, 'CONTRACT-RIDGE', '',
   '2026-10-21 16:14:51', '2026-10-28 16:14:51'),
  ('3f2a7c18-55b9-4d0e-8a16-9c4471d2e5aa', 'valley-remote', 'campbellscientific',
   0, '', '', 0, '', '', 0, '', '', 0, '', '',
   '2026-10-21 13:15:51', '2026-10-28 13:15:51');

-- The REST controller. Port 18080 is not arbitrary: contract/tools/rig/mangle_proxy.py forwards
-- to 127.0.0.1:18080, and /api/stations reports the port back in each website block.
INSERT INTO controller_configs
  (config_id, controller_type, enabled, rest_port, rest_listen_addr, grpc_port, grpc_listen_addr)
VALUES
  (1, 'rest', 1, 18080, '127.0.0.1', 18090, '127.0.0.1');

COMMIT;
