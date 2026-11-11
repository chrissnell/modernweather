-- Seed rows for the configuration database, used by ci/db/migrate-twice.sh.
--
-- The counterpart of ci/db/seed.sql, and it exists for the same reason: a migration that
-- silently drops data passes a schema comparison. Path B of the two-path comparison applies
-- the previous release's migrations, seeds, and then applies HEAD's, so every later
-- migration is exercised against rows rather than against empty tables.
--
-- It is also what makes "a second application is a no-op" mean something stronger than
-- "nothing errored": the second-application path seeds, re-runs the applier, and asserts
-- both that no migration ran and that these rows are still readable.
--
-- Every row below is deliberately a case the schema's CHECK constraints have an opinion
-- about, so a constraint that is accidentally always-false fails here rather than in
-- production. The deployment it describes is W7 §8.3's: a collector with a local station,
-- one satellite, and an uploader that beacons on the satellite's behalf.

PRAGMA foreign_keys = ON;

BEGIN;

-- The collector itself.
INSERT INTO node (id, node_id, node_name, timezone, spool_path)
VALUES (1, x'0193f3c28b207c71ae3f10cc92d4b7aa', 'suncrest', 'America/Denver',
        '/var/lib/modernweather/spool');

INSERT INTO weather_database (id, url)
VALUES (1, 'postgresql://mw:secret@localhost:5432/modernweather');

-- A hostname with TLS, and a second website that is the unmatched-Host default ('').
INSERT INTO website (name, hostname, page_title, apple_app_id,
                     tls_cert_path, tls_key_path, is_default_tls)
VALUES ('suncrest', 'suncrest.example.net', 'Suncrest Weather', '6755874087',
        '/etc/modernweather/suncrest.pem', '/etc/modernweather/suncrest-key.pem', 1);
INSERT INTO website (name, hostname, page_title, is_portal)
VALUES ('portal', '', 'Weather Management Portal', 1);

UPDATE listener SET https_port = 8443 WHERE id = 1;

INSERT INTO management_cors_origin (origin) VALUES ('https://suncrest.example.net');

-- A local station on a wire: outbound transport, therefore `fixed` identity. `id` is given
-- explicitly because it is the integer `/stationinfo` returns, not an implementation detail.
INSERT INTO stations (id, station_key, owning_node_id, name, type, latitude, longitude,
                      altitude_m, timezone, wind_dir_correction_degrees,
                      snow_base_distance_mm, identity_kind, transport_kind,
                      transport_dev, transport_baud)
VALUES (1, 'suncrest:roof', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'roof', 'davis',
        39.6403, -105.2094, 2103.1, 'America/Denver', -7, 1422.0,
        'fixed', 'serial', '/dev/ttyUSB0', 19200);

-- The configured-device vocabulary, not the driver's: `campbellscientific`, which
-- docs/legacy-api-contract.md §3.1 freezes and which `campbell` would break.
INSERT INTO stations (id, station_key, owning_node_id, name, type, timezone,
                      identity_kind, transport_kind, transport_host, transport_port)
VALUES (2, 'suncrest:ridge', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'ridge',
        'campbellscientific', 'America/Denver',
        'fixed', 'tcp_out', '10.0.0.9', 6785);

-- A streaming outbound transport, so `transport_streaming` -- which W2a §4a.3's
-- trigger-implication table reads at plan time -- is exercised rather than merely declared.
INSERT INTO stations (id, station_key, owning_node_id, name, type, timezone,
                      identity_kind, transport_kind, transport_url, transport_streaming)
VALUES (3, 'suncrest:snow', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'snowgauge', 'snowgauge',
        'America/Denver', 'fixed', 'grpc_out', 'https://10.0.0.11:50052', 1);

-- An inbound endpoint serving several stations: `from_query`, never `fixed`. This is the
-- row the Ambient CHECK exists for.
INSERT INTO stations (id, station_key, owning_node_id, name, type, timezone,
                      identity_kind, ingest_secret_hash, transport_kind, transport_mount)
VALUES (4, 'suncrest:greenhouse', x'0193f3c28b207c71ae3f10cc92d4b7aa', 'greenhouse',
        'ambient-customized', 'America/Denver',
        'from_query', 'sha256:0f1e2d3c', 'http_in', '/ingest/ambient');

-- The satellite, enrolled, relaying nothing, speaking only for itself. No `auth_material`:
-- this is docs/collector.md §8.3's `[[peer]]` block exactly, which under a shared CA has
-- nothing to put there.
INSERT INTO peers (node_id, name, auth_kind, relay, software)
VALUES (x'0193f3c27a107b649c2e4f1b8a6d0e11', 'satellite-a', 'mtls-ca',
        0, 'modernweather 1.4.2');

-- And a pinned peer, which is the kind that does carry material.
INSERT INTO peers (node_id, name, auth_kind, auth_material, relay)
VALUES (x'0193f3c2c1607f0ad6c399fe24b71d53', 'satellite-b', 'mtls-pinned',
        'sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08', 0);
INSERT INTO peer_origins (node_id, origin_node_id)
VALUES (x'0193f3c27a107b649c2e4f1b8a6d0e11', x'0193f3c27a107b649c2e4f1b8a6d0e11');

-- An outstanding enrolment token, so "enrolment is open" is visible (W7 §5.3, N-11).
INSERT INTO peer_enrolment_token (id, peer_name, token_hash, issued_at, expires_at)
VALUES (x'0193f3c2ad407e9fc5b288ef13a60c42', 'satellite-b',
        '$argon2id$v=19$m=65536,t=3,p=4$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaA',
        1762000000, 1762003600);

-- A station received by registration: no local transport, owned by the satellite.
INSERT INTO stations (id, station_key, owning_node_id, name, type, latitude, longitude,
                      timezone, aprs_callsign, identity_kind)
VALUES (5, 'satellite-a:backyard', x'0193f3c27a107b649c2e4f1b8a6d0e11', 'backyard', 'davis',
        39.7392, -104.9903, 'America/Denver', 'W0ABC-13', 'from_registration');

-- A received station naming a driver this node has never heard of. A newer peer must be
-- able to enrol against an older collector (W7 §5.2), so the type vocabulary is constrained
-- for local stations only -- and this row is what says so.
INSERT INTO stations (id, station_key, owning_node_id, name, type, timezone, identity_kind)
VALUES (6, 'satellite-b:lidar', x'0193f3c2c1607f0ad6c399fe24b71d53', 'lidar',
        'some-future-driver', 'America/Denver', 'from_registration');

-- Both halves of D-44, on one node: it ingests and it has no uplink.
-- docs/collector.md §8.3 verbatim: the gRPC listener names the CA and the REST one does
-- not, with the same `peer_auth`. A per-row NOT NULL would make the second row
-- unrepresentable, which is why there is no such constraint.
-- `allow_insecure` is set because this build serves the gRPC listener over plaintext and
-- does not enforce `peer_auth` yet, and decision 35 makes that a plan error without the
-- explicit opt-out (`mw_grpc::GrpcRxPlanError::InsecureIngest`). Set here so this fixture
-- stays a configuration the daemon will actually start on; it comes back out when M6-4
-- lands mTLS on the socket.
INSERT INTO ingest (listen, peer_auth, ca_path, allow_insecure)
VALUES ('grpc', 'mtls-ca', '/etc/modernweather/peers-ca.pem', 1);
INSERT INTO ingest (listen, peer_auth)
VALUES ('rest', 'mtls-ca');

-- Egress on the collector's own credentials, for the satellite's station (W7 §6.1).
INSERT INTO uploader (kind, name, enabled, interval_seconds, api_key)
VALUES ('wunderground', 'wu-backyard', 1, 300, 'wu-collector-key');
INSERT INTO station_service (station_key, service, uploader_id, remote_station_id)
VALUES ('satellite-a:backyard', 'wunderground',
        (SELECT id FROM uploader WHERE name = 'wu-backyard'), 'KCOXXXX');

-- APRS over a KISS TNC: the callsign is on the station, the passcode is ours.
INSERT INTO uploader (kind, name, enabled, interval_seconds, aprs_passcode,
                      aprs_symbol_table, aprs_symbol_code, aprs_comment, aprs_server,
                      aprs_transport, aprs_kiss_tcp_address, aprs_kiss_path,
                      aprs_kiss_destination)
VALUES ('aprs', 'aprs-backyard', 1, 600, '12345', '/', '_', 'ModernWeather',
        'noam.aprs2.net:14580', 'kiss-tcp', '127.0.0.1:8001',
        'WIDE1-1,WIDE2-1', 'APRS');
INSERT INTO station_service (station_key, service, uploader_id)
VALUES ('satellite-a:backyard', 'aprs',
        (SELECT id FROM uploader WHERE name = 'aprs-backyard'));

-- An Aeris pairing, which has no account row here: the credential is M6-3's and this is
-- only "fetch forecasts and alerts for this station". It is what W3 §7's
-- `aerisWeatherEnabled` reads.
INSERT INTO station_service (station_key, service)
VALUES ('suncrest:roof', 'aeris');

-- An administrator, a live session, and a Calendar occurrence the supervisor has recorded.
INSERT INTO admin_credential (id, username, password_phc)
VALUES (1, 'chris',
        '$argon2id$v=19$m=65536,t=3,p=4$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaA');
INSERT INTO admin_session (id, token_hash, created_at, last_seen_at, expires_at, user_agent)
VALUES (x'0193f3c2be50710a9d7742e1c8b30fa5',
        x'5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03',
        1762000000, 1762000000, 1763209600, 'Firefox/140.0');

INSERT INTO component_schedule (component_id, local_date, slot_index, completed_at, outcome)
VALUES ('rollup:daily:suncrest:roof', '2026-11-02', 0, 1762056000, 'ok');

COMMIT;
