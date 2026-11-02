-- The configuration store's initial schema. SQLite.
--
-- Hand-written, unlike migrations/*.sql, which `cargo xtask migrate` generates from
-- metrics/registry.toml. The weather schema is derived from a registry; this one is not
-- derived from anything, so it is authored and reviewed.
--
-- DESIGN.md D-8 as amended by A-W6-2: schema changes happen solely through migrations,
-- there is no DDL at startup, and the `IF NOT EXISTS` idiom does not appear. That last
-- clause is why this file exists at all. The Go configuration database is the actual
-- instance of the defect D-8 was written about -- pkg/config/provider_sqlite.go passes
-- `CREATE TABLE IF NOT EXISTS` to one Exec at every start, so its schema has been frozen
-- at first-run shape ever since, and four of this programme's design documents went on to
-- assume nine-plus tables into existence with no DDL behind them at all
-- (docs/reconciliation.md G-2). A rewrite that applied D-8 to the weather schema and
-- reinvented startup DDL here would reproduce the original defect in the only place it
-- ever happened.
--
-- Applied by name from `migrations/config/`, by `sqlx::migrate` through
-- `mw_store::config_db` and by ci/db/migrate-twice.sh. Because no statement here is
-- conditional, a second application is not a tolerated no-op -- it is an error, and the
-- ledger is the only thing standing between the two. That is the property
-- `cargo xtask gate db-migrate-twice` checks.
--
-- Two conventions, both departures from the Go schema and both deliberate:
--
--   STRICT          SQLite ignores a declared type it does not recognise, so the Go
--                   schema's `BOOLEAN`, `DATETIME` and `CHAR(1)` columns accept anything
--                   at all -- `aprs_symbol_table CHAR(1)` holds a 400-character string
--                   today if something writes one. Every table below is STRICT, so the
--                   declared type is enforced. Requires SQLite 3.37; `unixepoch()` below
--                   requires 3.38.
--   INTEGER times   Unix seconds, matching the `admin_session` definition W5 §3.3 gives
--                   in SQL. The Go schema mixes `DATETIME DEFAULT CURRENT_TIMESTAMP`
--                   (text) with `TIMESTAMP` (also text) and compares them to integers in
--                   places.
--
-- A CHECK constraint appears below only where a design document names the illegal state,
-- never as general hygiene. Each one carries the reference.
--
-- One SQLite rule governs how they are written: **a CHECK that evaluates to NULL PASSES.**
-- Only an explicit false rejects. So `x IS NULL OR y = 'z'` silently admits every row where
-- `y` is NULL, and `date(d) = d` admits every invalid date, because `date()` returns NULL
-- for one. Every constraint below that touches a nullable column therefore uses `IS`,
-- `COALESCE` or an `IS NOT NULL` guard rather than bare `=`/`IN`. Three constraints in this
-- file were accidentally always-true before that rule was applied, which is why it is
-- written down here rather than remembered.

-- ---------------------------------------------------------------------------------------
-- The node, and the spool
--
-- W7 §8.1. One row, enforced by `CHECK (id = 1)`: a `config_id` foreign key on every table
-- -- the Go schema's shape, with one `configs` row in it since the day it was written -- is
-- a join that has never selected anything.
--
-- No row is inserted here. `node_id` is allocated once at first start and never edited, so
-- the daemon writes it; a migration that invented one would make every fresh install claim
-- the same identity. An empty table therefore means "not yet started", which is a state
-- M2-4's boot sequence has to handle anyway.
--
-- There is no `role`, no `mode` and no `is_collector` column anywhere in this file. D-44:
-- a node's position in the topology is the set of components its configuration produces,
-- which is why "both" needs no syntax. `ingest` rows make a node a collector; an `uplink`
-- row makes it a satellite; rows in both make it the default case.

CREATE TABLE node (
    id                    INTEGER PRIMARY KEY CHECK (id = 1),
    -- 16 bytes, UUIDv7, allocated by the node itself (W7 §5.2). A BLOB rather than the
    -- Go schema's TEXT: `remote_stations.station_id` is a 36-character hyphenated string
    -- compared against other 36-character strings, which is 20 wasted bytes and one
    -- formatting convention away from a mismatch.
    node_id               BLOB NOT NULL UNIQUE CHECK (length(node_id) = 16),
    -- Operator-facing, mutable. Renaming a node is not an identity change (W7 §5.2).
    node_name             TEXT NOT NULL,
    -- `TzSource::Deployment` for components not scoped to a station (W2a §1.6). An IANA
    -- name; `"MST"` and `"-0700"` are rejected at plan time, because a fixed offset cannot
    -- express DST and that is the entire reason `Calendar` exists.
    timezone              TEXT NOT NULL DEFAULT 'UTC',
    spool_path            TEXT NOT NULL,
    -- W3 §2.4's default, in bytes. W7 §3.5: at the ceiling the policy is to refuse at the
    -- edge, counted and logged, never drop-oldest.
    spool_ceiling_bytes   INTEGER NOT NULL DEFAULT 2147483648 CHECK (spool_ceiling_bytes > 0),
    -- W3 §2.7's default, in seconds. W7 D-16 requires this to fire before the ceiling does
    -- at the node's observed rate. That cannot be a CHECK and is not a plan() check either:
    -- it depends on the admit rate, which is a runtime fact, so it is the
    -- `spool_time_remaining_seconds` gauge and a startup warning (W7 §3.5).
    spool_max_lag_seconds INTEGER NOT NULL DEFAULT 604800 CHECK (spool_max_lag_seconds > 0),
    created_at            INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at            INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- ---------------------------------------------------------------------------------------
-- The listener
--
-- W3 §6.1: one socket for HTTP, HTTPS and gRPC-over-ALPN, plus the gRPC port, which keeps
-- its own socket -- verified rather than conceded (W3 D-92): `grpc_port` is a real config
-- column defaulting to 50051 and the Go management API already validates that it differs
-- from the HTTP and HTTPS ports, so folding it onto 443 would be a fleet-wide flag day.
-- That validation becomes the CHECK below.
--
-- The management API's own port and certificate live here rather than on a page of their
-- own (W5 §1.4): under W5 §3 the management auth token no longer exists, so the thing that
-- made `PUT /api/config/controllers/management` able to lock an operator out is gone, and
-- what is left is a `System -> Advanced` concern.

CREATE TABLE listener (
    id                       INTEGER PRIMARY KEY CHECK (id = 1),
    listen_addr              TEXT NOT NULL DEFAULT '0.0.0.0',
    http_port                INTEGER NOT NULL DEFAULT 8080
                                 CHECK (http_port BETWEEN 1 AND 65535),
    https_port               INTEGER
                                 CHECK (https_port IS NULL OR https_port BETWEEN 1 AND 65535),
    grpc_port                INTEGER NOT NULL DEFAULT 50051
                                 CHECK (grpc_port BETWEEN 1 AND 65535),
    -- A-W5-3: the default bind becomes loopback. Today it is `0.0.0.0`, which is half of
    -- what makes W5 §3.4's unclaimed-instance takeover window reachable at all.
    management_listen_addr   TEXT NOT NULL DEFAULT '127.0.0.1',
    management_port          INTEGER NOT NULL DEFAULT 8081
                                 CHECK (management_port BETWEEN 1 AND 65535),
    management_tls_cert_path TEXT,
    management_tls_key_path  TEXT,
    created_at               INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at               INTEGER NOT NULL DEFAULT (unixepoch()),
    -- W3 D-92's shipped validation, as a constraint rather than a handler check.
    CHECK (grpc_port <> http_port),
    CHECK (grpc_port <> management_port),
    CHECK (http_port <> management_port),
    CHECK (https_port IS NULL OR (https_port <> http_port
                              AND https_port <> grpc_port
                              AND https_port <> management_port)),
    -- A certificate without its key is not a TLS configuration, it is a failed start.
    CHECK ((management_tls_cert_path IS NULL) = (management_tls_key_path IS NULL))
) STRICT;

-- Defaults are seeded, unlike `node`: there is nothing here that has to be allocated, and
-- a daemon that has to upsert its own listener row before it can bind is a startup DDL
-- path wearing different clothes.
--
-- W3 §6.4's SNI fallback is NOT a column here. It would be a foreign key to `website`,
-- which is three sections down, and SQLite resolves a foreign key's parent when a row is
-- written rather than when the table is created -- so this INSERT would fail with "no such
-- table: main.website" on any connection that has `foreign_keys` on, which is every
-- connection the daemon opens. It is `website.is_default_tls` instead, which is the better
-- normalisation anyway: "at most one, and it must have a certificate" is two constraints on
-- the table that owns the certificate.
INSERT INTO listener (id) VALUES (1);

-- A-W5-3: CORS becomes an exact-match allowlist defaulting to empty, replacing
-- `Access-Control-Allow-Origin: *` on the admin API (W5 §3.1). Empty means no rows, so the
-- default is expressed by the absence of data rather than by a sentinel value.
CREATE TABLE management_cors_origin (
    origin     TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- An exact match is exact: a scheme, a non-empty authority, and nothing else. GLOB
    -- rather than LIKE throughout, because SQLite's LIKE is ASCII-case-insensitive --
    -- `origin LIKE 'https://%'` accepts `HTTPS://EVIL.COM`, which can never match a
    -- browser `Origin` header and therefore reads as an allowlist entry that silently does
    -- nothing. Three independent clauses rather than one disjunction: a disjunction here is
    -- how a constraint becomes accidentally always-true.
    CHECK (origin GLOB 'http://?*' OR origin GLOB 'https://?*'),
    -- No wildcard, and not a list in one row.
    CHECK (origin NOT GLOB '*[*, ]*'),
    -- No path, no trailing slash: the only two slashes are the scheme's.
    CHECK (length(origin) - length(replace(origin, '/', '')) = 2)
) STRICT;

-- ---------------------------------------------------------------------------------------
-- The weather database
--
-- W5's "Data storage" page, narrowed from the Go schema's `storage_configs`, which is
-- keyed by `backend_type` over three unrelated backends and carries APRS columns and four
-- health columns. The health columns go away with `storage.GlobalHealthManager` (W2a §3.2):
-- component state is the health surface, so `GET /api/health/storage` merges into
-- `GET /api/v1/runtime/components`. The gRPC and APRS rows go away because a backend that
-- is really an egress is an `uploader` row or an `uplink` row.
--
-- What is left is one PostgreSQL connection, and its ABSENCE is meaningful: a node with no
-- row here has no `db:postgres` and therefore no `writer:timescale` (D-44, W7 §8.2). The
-- satellite in W7 §8.2 is exactly that node.

CREATE TABLE weather_database (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    -- One URL rather than the Go schema's host/port/database/user/password/ssl_mode/
    -- timezone columns. It carries a password, so it is a `SecretString` from the moment
    -- it is read (D-33) and `GET /api/v1/config` omits it rather than redacting it (W5 §3.2).
    -- GLOB, not LIKE: LIKE is ASCII-case-insensitive, so `POSTGRES://` would pass a check
    -- that claims to pin the scheme.
    url        TEXT NOT NULL CHECK (url GLOB 'postgres://?*' OR url GLOB 'postgresql://?*'),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- ---------------------------------------------------------------------------------------
-- config.stations
--
-- D-65: this table is `config.stations` in every document and comment, and the
-- `observations`-side `stations` of docs/database-schema.md §3 is the read-side projection.
-- One is authoritative and the other is a cache; W4 and W7 each created a table called
-- `stations` without knowing the other had. SQLite has no schema namespace, so the
-- qualification is a naming RULE rather than a table name -- which is why it is stated
-- here, at the top of the table it applies to.
--
-- W7 §5.1's row, plus the hardware configuration that is on the Go `devices` row, minus
-- that row's twenty-three third-party service columns, which are now `station_service` and
-- `uploader` (D-66).

CREATE TABLE stations (
    -- The surrogate integer the FROZEN contract returns. `/stationinfo` emits
    -- `stations[].id` as an integer (contract/fixtures/stationinfo-full.json: `{"id":4,
    -- "name":"aqmon",...}`) and W3 §7's config payload emits `stationID` and
    -- `airQualityDeviceID` as integers. `station_key` is the domain identity and is unique,
    -- but it is text, so it cannot be the source of those three frozen fields -- which is
    -- why the Go `devices.id` surrogate is carried forward rather than dropped.
    id                          INTEGER PRIMARY KEY,
    station_key                 TEXT NOT NULL UNIQUE,
    -- The node that measures this station. Equal to `node.node_id` for a local station;
    -- a peer's `node_id` for one that arrived by registration, which is what makes
    -- W5 §0.6's read-only "Received from {peer}" entry a query rather than a flag.
    -- Deliberately NOT a foreign key to `peers`: a station may be two hops away, so its
    -- owning node is not necessarily a node this one has a peer row for (W7 §8.4's relay,
    -- and W7 D-36's default that stops it being the common case).
    owning_node_id              BLOB NOT NULL CHECK (length(owning_node_id) = 16),
    name                        TEXT NOT NULL,
    -- The CONFIGURED device type, which is the vocabulary `/stationinfo` returns --
    -- `campbellscientific`, not `campbell`. docs/legacy-api-contract.md §3.1 is explicit
    -- that this and `/latest`'s driver-self-reported `stationtype` "are not the same set
    -- and must not be unified": for Campbell the configured type is `campbellscientific`
    -- and the driver name is `campbell`. Five frozen fixtures carry the configured form.
    -- The permitted set is constrained below, for local stations only.
    type                        TEXT NOT NULL,
    -- W5 §1.2's "Collecting readings" toggle. Under W2a §2.5 this produces a plan diff
    -- that stops the station component; today it writes the column and nothing stops,
    -- because `ReloadControllersConfig` never compares contents.
    enabled                     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),

    latitude                    REAL CHECK (latitude IS NULL OR latitude BETWEEN -90 AND 90),
    longitude                   REAL CHECK (longitude IS NULL OR longitude BETWEEN -180 AND 180),
    altitude_m                  REAL,
    -- IANA name (W2a D-11). A station with a `Calendar` component and no valid zone is a
    -- plan error rather than a silent fallback to UTC, which is what the Go rollovers do
    -- today -- `date_trunc('day', NOW())` truncates in the database server's zone and the
    -- device's declared `timezone` is used nowhere but rendering sun times.
    timezone                    TEXT,
    -- Public identity, printed on every packet. Not a credential, and not the APRS-IS
    -- passcode, which is on the `uploader` row (W7 §6.2).
    aprs_callsign               TEXT,

    -- Calibration. The Go names are `wind_dir_correction` (degrees, untyped) and
    -- `base_snow_distance` (millimetres, untyped); the units are in the names here because
    -- a mistyped snow base distance produces plausible wrong numbers rather than an error
    -- (docs/reconciliation.md G-2).
    wind_dir_correction_degrees INTEGER NOT NULL DEFAULT 0
                                    CHECK (wind_dir_correction_degrees BETWEEN -360 AND 360),
    snow_base_distance_mm       REAL CHECK (snow_base_distance_mm IS NULL
                                            OR snow_base_distance_mm > 0),

    -- `StationIdentity` (W2a §4a.4).
    identity_kind               TEXT NOT NULL CHECK (identity_kind IN (
                                    'fixed', 'from_path', 'from_query', 'from_payload',
                                    'from_registration')),
    -- Ambient sends `?ID=` and `?PASSWORD=` in one request. The secret is compared in
    -- constant time against the registry (D-33); today any request arriving on the port is
    -- trusted as that station.
    ingest_secret_hash          TEXT,

    -- `Transport` (W2a §4a.3), as a tagged union over one row. A transport is 1:1 with the
    -- station it configures -- it IS the station's hardware -- so sparse columns are the
    -- right shape here, which is exactly what they are not for services (D-66).
    -- NULL for a station received from a peer: that station is inventory on this node, not
    -- a component, and its transport is configured on the node that owns it.
    -- W2a §4a.3's `Transport`, minus `GrpcIn`. The one driver that used `GrpcIn` is the
    -- gRPC receiver, which is an `ingest` row here (see the comment above on
    -- `from_registration`) -- so listing it would admit a combination no driver has while
    -- the combination §4a.5 DOES have, `GrpcIn` + `FromRegistration`, is rejected by the
    -- transport/identity CHECK below. Removing it is what makes the two agree.
    transport_kind              TEXT CHECK (transport_kind IS NULL OR transport_kind IN (
                                    'serial', 'tcp_out', 'http_out', 'grpc_out', 'mqtt_sub',
                                    'http_in', 'udp_in')),
    transport_dev               TEXT,
    transport_baud              INTEGER CHECK (transport_baud IS NULL OR transport_baud > 0),
    transport_host              TEXT,
    transport_port              INTEGER CHECK (transport_port IS NULL
                                               OR transport_port BETWEEN 1 AND 65535),
    transport_url               TEXT,
    transport_method            TEXT,
    -- `HttpOut { auth: Option<SecretString> }` (W2a §4a.3), for §4a.7's vendor-cloud poll.
    transport_auth              TEXT,
    -- `GrpcOut { streaming: bool }` (W2a §4a.3). Load-bearing rather than descriptive: it
    -- is what §4a.3's trigger-implication table reads to decide `Service` against `Every`,
    -- and that implication "is checked at plan time", so M2-3 has to be able to see it.
    transport_streaming         INTEGER CHECK (transport_streaming IS NULL
                                               OR transport_streaming IN (0, 1)),
    transport_mount             TEXT,
    transport_group             TEXT,
    transport_topic             TEXT,
    transport_tls_ca_path       TEXT,
    transport_tls_cert_path     TEXT,
    transport_tls_key_path      TEXT,

    registered_at               INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen                   INTEGER,
    created_at                  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at                  INTEGER NOT NULL DEFAULT (unixepoch()),

    -- A station has a local transport if and only if it was not received by registration.
    CHECK ((transport_kind IS NULL) = (identity_kind = 'from_registration')),

    -- The configured device types, in the spelling the frozen contract uses -- Go's
    -- `validTypes` (pkg/config/provider.go:449) plus `weatherlink-live`, minus
    -- `grpcreceiver`, which is an `ingest` row here and never a local transport. NOT the
    -- driver names: `campbellscientific` and `ambient-customized` are what
    -- docs/legacy-api-contract.md §3.1 freezes, and writing `campbell`/`ambient` here would
    -- make M7-4's configuration migration either impossible or a silent contract break.
    --
    -- Local stations only. A station received by registration carries the SATELLITE's
    -- `station_type`, and a newer peer with a driver this collector has never heard of must
    -- not fail to enrol against an older collector: W7 §5.2 puts `software` on the peer row
    -- precisely because version skew between nodes is expected. `/stationinfo` can still
    -- emit `grpcreceiver` for such a row, which is the seventh name in §3.1's list.
    CHECK (transport_kind IS NULL OR type IN (
               'campbellscientific', 'davis', 'snowgauge', 'ambient-customized',
               'airgradient', 'weatherlink-live')),

    -- W2a §4a.4, the invariant MQTT broke and the one that makes the Ambient bug
    -- unrepresentable: cardinality is One if and only if identity is Fixed, and connection
    -- direction only sets the DEFAULT identity strategy. An inbound transport therefore
    -- cannot be `fixed` -- that is the bug, where the port a request arrived on was the
    -- station's identity. Checked here as well as at plan time because a schema that can
    -- hold the state means every reader has to handle it.
    CHECK (identity_kind <> 'fixed'
           OR transport_kind NOT IN ('http_in', 'udp_in', 'grpc_in')),

    -- Each transport variant's required fields. Without these, "a serial station with no
    -- device path" is a row rather than a rejection.
    CHECK (transport_kind <> 'serial'
           OR (transport_dev IS NOT NULL AND transport_baud IS NOT NULL)),
    CHECK (transport_kind <> 'tcp_out'
           OR (transport_host IS NOT NULL AND transport_port IS NOT NULL)),
    CHECK (transport_kind <> 'http_out' OR transport_url IS NOT NULL),
    CHECK (transport_kind <> 'grpc_out'
           OR (transport_url IS NOT NULL AND transport_streaming IS NOT NULL)),
    -- COALESCE for the same reason: `transport_kind` is NULL on a received station, and
    -- `NULL = 'grpc_out'` is NULL, so these two would admit a received station carrying a
    -- streaming flag or a credential for a transport it does not have.
    CHECK (transport_streaming IS NULL OR COALESCE(transport_kind, '') = 'grpc_out'),
    CHECK (transport_auth IS NULL OR COALESCE(transport_kind, '') = 'http_out'),
    CHECK (transport_kind <> 'mqtt_sub'
           OR (transport_url IS NOT NULL AND transport_topic IS NOT NULL)),
    CHECK (transport_kind <> 'http_in' OR transport_mount IS NOT NULL),
    CHECK (transport_kind <> 'udp_in' OR transport_port IS NOT NULL),
    -- A certificate without its key, again.
    CHECK ((transport_tls_cert_path IS NULL) = (transport_tls_key_path IS NULL))
) STRICT;

CREATE INDEX stations_owning_node_id ON stations (owning_node_id);
-- Unique across local AND received stations, because the frozen contract looks a station up
-- by name: `?station=davis-full` is in contract/fixtures/forecast-by-station-name.json, and
-- `/latest`, `/span` and `/forecast` all accept it (D-68). The Go schema has
-- `UNIQUE(config_id, name)` on `devices` and a separate `UNIQUE` on
-- `remote_stations.station_name`, so a local device and a received station CAN share a name
-- there and the lookup is then ambiguous. One table with one unique index is what closes
-- that; a collision on the configuration migration (M7-4) is a loud failure rather than a
-- silently wrong answer to a frozen request.
CREATE UNIQUE INDEX stations_name ON stations (name);

-- ---------------------------------------------------------------------------------------
-- Third-party egress: `uploader` and `station_service`
--
-- D-66, closing OQ-W5-4. The Go schema configures the same service in two places: the
-- `devices` row carries that station's PWS Weather, Weather Underground and APRS settings
-- (pkg/config/provider.go:196-240) while the global credentials for those same services
-- live on `controller_configs`. W5 §1.2 surfaced it and deferred the fix to W4 on the
-- grounds that normalising costs a migration; W4 scoped itself to PostgreSQL and never
-- picked it up. Since this schema is written from scratch there is no in-place migration
-- to avoid, which is the one place G-2 being found late made the answer easier.
--
-- The split is by what the thing is a property OF, which is what keeps it from becoming two
-- sources of truth again:
--
--   uploader         the remote ACCOUNT and the component. One row is one uploader
--                    component (W2a D-55), carrying the credential and the schedule.
--   station_service  the PAIRING. Which of this node's stations is shared with which
--                    service, through which account, and that station's identifier at the
--                    far end.
--
-- D-45 and A-W7-2 are what make this correct rather than merely tidier: the credential on
-- an `uploader` row is always THIS node's own. A collector that uploads on behalf of a
-- satellite selects the station and uses its own account; nothing transmits a third-party
-- credential to another node, and a satellite has no `uploader` rows at all, so there is no
-- code path on a satellite that reads a third-party credential in order to send it
-- somewhere. The four plaintext columns the Go registration carries --
-- `aprs_password`, `wu_api_key`, `aeris_client_secret`, `pws_password` -- have no successor
-- here, which is D-67.

CREATE TABLE uploader (
    id                      INTEGER PRIMARY KEY,
    -- Aeris is NOT here. It is four fetching components (W2a D-33, M6-3) rather than an
    -- egress, so its account configuration belongs to M6-3's own migration; the Go schema
    -- filed it next to the uploaders because `controller_configs` is keyed by controller
    -- type and had nowhere else to put it.
    kind                    TEXT NOT NULL CHECK (kind IN ('wunderground', 'pwsweather', 'aprs')),
    -- The component id's stable suffix, so `wu:backyard` survives a rename of anything else.
    name                    TEXT NOT NULL UNIQUE,
    -- W5 §1.4's "Stop sharing with Weather Underground". Under W2a §2.5 this stops the
    -- component; today `RemoveController` deletes a map entry and logs "will stop on next
    -- app restart".
    enabled                 INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    -- `Trigger::Every`'s period. NULL means the kind's default. The RF beacon floor is
    -- deliberately NOT a CHECK: it is owner decision O-8, it is one constant in
    -- `RadioInterval`'s constructor either way, and W2a OQ-A's upgrade path requires a
    -- sub-floor value to be RAISED with a one-line report rather than rejected -- which a
    -- constraint here would make impossible.
    interval_seconds        INTEGER CHECK (interval_seconds IS NULL OR interval_seconds > 0),
    api_endpoint            TEXT,

    -- Weather Underground and PWS Weather: one credential per account. `SecretString`
    -- (D-33); no endpoint returns it.
    api_key                 TEXT,

    -- APRS/CWOP. The callsign is on the station row (public identity); the passcode is the
    -- account credential and lives here, alongside the callsign it is authorised to beacon
    -- for (W7 §6.2).
    aprs_passcode           TEXT,
    aprs_symbol_table       TEXT CHECK (aprs_symbol_table IS NULL
                                        OR length(aprs_symbol_table) = 1),
    aprs_symbol_code        TEXT CHECK (aprs_symbol_code IS NULL
                                        OR length(aprs_symbol_code) = 1),
    aprs_comment            TEXT,
    aprs_server             TEXT,
    aprs_transport          TEXT CHECK (aprs_transport IS NULL OR aprs_transport IN (
                                'aprs-is', 'kiss-serial', 'kiss-tcp')),
    aprs_kiss_serial_device TEXT,
    aprs_kiss_serial_baud   INTEGER CHECK (aprs_kiss_serial_baud IS NULL
                                           OR aprs_kiss_serial_baud > 0),
    aprs_kiss_tcp_address   TEXT,
    aprs_kiss_path          TEXT,
    aprs_kiss_destination   TEXT,

    created_at              INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at              INTEGER NOT NULL DEFAULT (unixepoch()),

    -- Not redundant with the primary key: it is the parent index `station_service`'s
    -- composite foreign key needs, which is what stops a pairing naming one service while
    -- pointing at an account of another.
    UNIQUE (id, kind),

    -- A column belongs to its kind, and the list has to be EXHAUSTIVE to mean anything.
    -- The Go `controller_configs` row carries every service's columns at once because it is
    -- keyed by controller type, so `pws_api_key` on the APRS row is a representable state
    -- nothing rejects -- and a partial exclusion list here is the same defect with fewer
    -- columns in it.
    CHECK (kind = 'aprs' OR (aprs_passcode IS NULL AND aprs_symbol_table IS NULL
                             AND aprs_symbol_code IS NULL AND aprs_comment IS NULL
                             AND aprs_server IS NULL AND aprs_transport IS NULL
                             AND aprs_kiss_serial_device IS NULL
                             AND aprs_kiss_serial_baud IS NULL
                             AND aprs_kiss_tcp_address IS NULL
                             AND aprs_kiss_path IS NULL
                             AND aprs_kiss_destination IS NULL)),
    CHECK (kind <> 'aprs' OR api_key IS NULL),
    -- And the KISS columns belong to a KISS transport, not merely to the APRS kind: a
    -- `kiss_tcp_address` on an `aprs-is` uploader is configuration nothing will ever read.
    -- COALESCE, not a bare IN: `NULL IN (...)` is NULL, so an APRS uploader with no
    -- transport chosen yet would admit every KISS column.
    CHECK (COALESCE(aprs_transport, '') IN ('kiss-serial', 'kiss-tcp')
           OR (aprs_kiss_serial_device IS NULL AND aprs_kiss_serial_baud IS NULL
               AND aprs_kiss_tcp_address IS NULL AND aprs_kiss_path IS NULL
               AND aprs_kiss_destination IS NULL)),
    CHECK (aprs_transport <> 'kiss-serial'
           OR (aprs_kiss_serial_device IS NOT NULL AND aprs_kiss_serial_baud IS NOT NULL
               AND aprs_kiss_tcp_address IS NULL)),
    CHECK (aprs_transport <> 'kiss-tcp'
           OR (aprs_kiss_tcp_address IS NOT NULL AND aprs_kiss_serial_device IS NULL
               AND aprs_kiss_serial_baud IS NULL))
) STRICT;

CREATE TABLE station_service (
    station_key       TEXT NOT NULL REFERENCES stations (station_key) ON DELETE CASCADE,
    -- `aeris` is here even though there is no Aeris `uploader`, because the Go `devices`
    -- row carries `aeris_enabled` per station and M7-4 -- which depends on M2-2 -- has to
    -- move it somewhere. "Which stations get forecasts and alerts" is a station/service
    -- pairing like any other; the Aeris ACCOUNT (client id, secret, endpoint, refresh
    -- interval) is M6-3's, and it is not an uploader, so the pairing carries no
    -- `uploader_id`. That is also what makes W3 §7's `aerisWeatherEnabled` derivable.
    service           TEXT NOT NULL CHECK (service IN ('wunderground', 'pwsweather',
                                                       'aprs', 'aeris')),
    -- W7 §6.1's `for`, from the other end. The selector is written as a list there so that
    -- a future fleet-identity kind needs no migration; as rows, a list costs nothing and
    -- the one-station arity of `wu`/`pws`/`aprs` stays a `PlanError::UploaderArity` rather
    -- than a constraint, because W7 D-38 makes it a property of the KIND and not of the
    -- selector.
    uploader_id       INTEGER,
    -- This station's identifier at the far end -- a Weather Underground or PWS Weather
    -- station id. A property of the pairing, not of the account: one account can carry
    -- several stations, each with its own id.
    remote_station_id TEXT,
    created_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    -- One pairing per station per service. There is deliberately no `enabled` column: the
    -- switch is `uploader.enabled`, and a second toggle on the same fact is the two-places
    -- defect this table exists to remove.
    PRIMARY KEY (station_key, service),
    -- An upload pairing names an account; an Aeris pairing has none.
    CHECK ((service = 'aeris') = (uploader_id IS NULL)),
    -- A COMPOSITE foreign key, not `uploader_id` alone. The kind is written on both rows,
    -- and this is the one join where that could disagree: without it, a pairing with
    -- `service = 'aprs'` can point at an account with `kind = 'wunderground'`, which is the
    -- same-service-in-two-places defect this table exists to remove, wearing a join.
    FOREIGN KEY (uploader_id, service) REFERENCES uploader (id, kind) ON DELETE CASCADE
) STRICT;

CREATE INDEX station_service_uploader_id ON station_service (uploader_id);

-- ---------------------------------------------------------------------------------------
-- The public website
--
-- The Go `weather_websites` row, carried forward (D-38: the website is carried forward
-- as-is, except that template-generated JavaScript becomes static JavaScript plus a JSON
-- config endpoint). The nine values that endpoint must carry are
-- docs/legacy-api-contract.md §9's list, fixed as a payload by W3 §7.
--
-- Three of the nine are not on this table and are worth naming, because each one is a
-- frozen field whose source had to exist somewhere: `stationID` and `airQualityDeviceID`
-- are integers, which is why `stations` carries the `id` surrogate, and
-- `aerisWeatherEnabled` is a `station_service` row with `service = 'aeris'`.

CREATE TABLE website (
    id                      INTEGER PRIMARY KEY,
    name                    TEXT NOT NULL UNIQUE,
    -- '' is the DEFAULT website, which an unmatched `Host` resolves to -- W3 §6.3 preserves
    -- that behaviour exactly. Stored lower-case and unique: today the host table is a
    -- case-sensitive map, so `EXAMPLE.COM` silently serves the default website's data under
    -- the wrong hostname. The CHECK makes that unrepresentable rather than handled.
    -- W3 §6.3 normalises a `Host` header in three steps before matching -- lower-case,
    -- strip the port, strip a single trailing dot -- so a stored hostname that has not had
    -- all three applied is a row that can never match. One CHECK per step.
    hostname                TEXT NOT NULL DEFAULT '' UNIQUE
                                CHECK (hostname = lower(hostname))
                                CHECK (hostname NOT GLOB '*:*')
                                CHECK (hostname NOT GLOB '*.'),
    page_title              TEXT NOT NULL DEFAULT '',
    about_station_html      TEXT NOT NULL DEFAULT '',
    -- The per-website primary station. W3 §2a.3 resolves a request to
    -- `(website, station)`, and this is the fallback when neither `?station_id=` nor
    -- `?station=` is given.
    primary_station_key     TEXT REFERENCES stations (station_key) ON DELETE SET NULL,
    snow_enabled            INTEGER NOT NULL DEFAULT 0 CHECK (snow_enabled IN (0, 1)),
    snow_station_key        TEXT REFERENCES stations (station_key) ON DELETE SET NULL,
    air_quality_enabled     INTEGER NOT NULL DEFAULT 0 CHECK (air_quality_enabled IN (0, 1)),
    air_quality_station_key TEXT REFERENCES stations (station_key) ON DELETE SET NULL,
    is_portal               INTEGER NOT NULL DEFAULT 0 CHECK (is_portal IN (0, 1)),
    apple_app_id            TEXT NOT NULL DEFAULT '',
    radar_enabled           INTEGER NOT NULL DEFAULT 0 CHECK (radar_enabled IN (0, 1)),
    -- Write-only: set by registration, never returned by any endpoint. W5 §1.5 shows
    -- "Registered on <date>" and a Re-register action instead -- which is what
    -- `radar_registered_at` is for. The token is nonetheless PUBLIC in the rendered page
    -- markup (docs/legacy-api-contract.md §9), so "never returned" is about the management
    -- API, not about secrecy.
    radar_token             TEXT NOT NULL DEFAULT '',
    radar_registered_at     INTEGER,
    tls_cert_path           TEXT,
    tls_key_path            TEXT,
    -- W3 §6.4: the SNI resolver's fallback, tried before the default website's own
    -- certificate. A flag on the website that owns the certificate rather than an id on
    -- `listener`, so "the fallback has no certificate" is not a representable state.
    is_default_tls          INTEGER NOT NULL DEFAULT 0 CHECK (is_default_tls IN (0, 1)),
    created_at              INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at              INTEGER NOT NULL DEFAULT (unixepoch()),
    -- A feature enabled with nothing to read from renders an empty panel the operator
    -- cannot explain. W3 §2a.3's `(website, station)` key depends on these.
    CHECK (snow_enabled = 0 OR snow_station_key IS NOT NULL),
    CHECK (air_quality_enabled = 0 OR air_quality_station_key IS NOT NULL),
    CHECK (radar_enabled = 0 OR radar_token <> ''),
    CHECK ((radar_token = '') = (radar_registered_at IS NULL)),
    CHECK ((tls_cert_path IS NULL) = (tls_key_path IS NULL)),
    CHECK (is_default_tls = 0 OR tls_cert_path IS NOT NULL)
) STRICT;

-- One index per ON DELETE SET NULL reference. Without one SQLite scans the child table for
-- every parent delete, and all three of these columns carry that clause.
CREATE INDEX website_primary_station_key ON website (primary_station_key);
CREATE INDEX website_snow_station_key ON website (snow_station_key);
CREATE INDEX website_air_quality_station_key ON website (air_quality_station_key);
-- At most one SNI fallback. A partial unique index rather than a CHECK, because "one row in
-- the table has this" is not something a row-level constraint can say.
CREATE UNIQUE INDEX website_default_tls ON website (is_default_tls) WHERE is_default_tls = 1;

-- ---------------------------------------------------------------------------------------
-- The peer registry
--
-- W7 §5.1. `PeerRegistry` is not a new capability: it is the peer-facing accessor set on
-- `StationRegistryHandle`, which is why registration writes both a `peers` row and a
-- `config.stations` row, and why `Write` on `StationRegistry` is held by `ingest:*` alone
-- -- registration mints keys and nothing else may.

CREATE TABLE peers (
    node_id       BLOB PRIMARY KEY CHECK (length(node_id) = 16),
    name          TEXT NOT NULL UNIQUE,
    auth_kind     TEXT NOT NULL CHECK (auth_kind IN ('mtls-ca', 'mtls-pinned', 'psk')),
    -- An SPKI fingerprint or a PSK hash, by `auth_kind`. A hash in the PSK case: W7 §5.4
    -- authenticates a peer, it does not need to reproduce its secret.
    --
    -- NULL for `mtls-ca`, and that is the common case rather than an exception:
    -- docs/collector.md §8.3's `[[peer]]` block is `node` / `id` / `relay` / `origins` and
    -- carries nothing to put here, because under a shared CA the certificate chain does the
    -- authenticating and the peer row only says who is allowed. `NOT NULL` would force a
    -- placeholder, and a placeholder in a credential column is worse than a null.
    auth_material TEXT,
    -- W7 D-36: false by default, so a peer may speak only for itself and a compromised leaf
    -- cannot inject history for another station.
    relay         INTEGER NOT NULL DEFAULT 0 CHECK (relay IN (0, 1)),
    -- `NodeIdentity.software`, e.g. "modernweather 1.4.2" (W7 §5.2). Load-bearing rather
    -- than decorative: W7 OQ-W7-6 gates removal of the Go compat shim on this showing no
    -- Go-era peers.
    software      TEXT,
    enrolled_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen     INTEGER,
    -- Revocation is a timestamp rather than a deletion, because "this peer was trusted
    -- until Tuesday" is what an operator needs to read off W5's fleet page.
    revoked_at    INTEGER,
    -- The material is required exactly where it is the thing being compared.
    CHECK ((auth_material IS NOT NULL) = (auth_kind IN ('mtls-pinned', 'psk')))
) STRICT;

CREATE TABLE peer_origins (
    node_id        BLOB NOT NULL REFERENCES peers (node_id) ON DELETE CASCADE,
    -- Which origins this peer may deliver. NOT a foreign key to `peers`: with `relay`, an
    -- origin can be a node two hops away that this node has no peer row for -- which is
    -- what W7 §5.4's origin-authorisation clauses are checked against, and why D-59 makes
    -- an envelope's `path` an ordered list rather than a set.
    origin_node_id BLOB NOT NULL CHECK (length(origin_node_id) = 16),
    PRIMARY KEY (node_id, origin_node_id)
) STRICT;

-- W7 §5.3. `Enroll` is served only while at least one unexpired, unconsumed token exists;
-- otherwise it returns FAILED_PRECONDITION "enrolment is closed", distinguishable from
-- UNIMPLEMENTED and PERMISSION_DENIED because an operator debugging a join needs to tell
-- those three apart. W5 surfaces outstanding tokens and their expiry (N-11), because
-- leaving enrolment open is the mistake and it should be visible.
CREATE TABLE peer_enrolment_token (
    id          BLOB PRIMARY KEY CHECK (length(id) = 16),
    peer_name   TEXT NOT NULL,
    -- Argon2id PHC, D-35's primitive (W7 §5.3). The token itself is a `SecretString`,
    -- printed once and never logged -- unlike the current management token, which is
    -- printed in cleartext at startup and on every failed authentication.
    token_hash  TEXT NOT NULL CHECK (token_hash GLOB '$argon2id$?*'),
    issued_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at  INTEGER NOT NULL,
    -- Single-use: burned in the same transaction that inserts the `peers` row.
    consumed_at INTEGER,
    consumed_by BLOB CHECK (consumed_by IS NULL OR length(consumed_by) = 16),
    CHECK (expires_at > issued_at),
    CHECK ((consumed_at IS NULL) = (consumed_by IS NULL))
) STRICT;

CREATE INDEX peer_enrolment_token_open ON peer_enrolment_token (expires_at)
    WHERE consumed_at IS NULL;

-- W7 §8.3's `[[ingest]]`. The only thing that makes this node a collector -- no flag, no
-- mode (D-44). D-46: admission precedes the pipeline, so the authentication kind is
-- configured on the listener rather than decided per reading.
CREATE TABLE ingest (
    listen          TEXT PRIMARY KEY CHECK (listen IN ('grpc', 'rest')),
    peer_auth       TEXT NOT NULL CHECK (peer_auth IN ('mtls-ca', 'mtls-pinned', 'psk')),
    -- Not required even for `mtls-ca`. docs/collector.md §8.3 sets `ca` on the gRPC listener
    -- and omits it on the REST one with the same `peer_auth`, so the CA is a node-level
    -- trust store that a listener may name rather than a per-listener setting -- and §8 says
    -- those examples are the shape of the `ci/examples/*.db` fixtures `plan()` is checked
    -- against. Requiring it per row made a documented fixture unrepresentable. "Where the
    -- CA comes from when no listener names one" is M6-4's to decide, and a plan() check
    -- rather than a constraint.
    ca_path         TEXT,
    -- docs/collector.md §5.4 and its plan-time check at §8.5: an ingest listener with no
    -- TLS and no explicit opt-out is `PlanError::InsecureIngest`. The escape hatch has to
    -- be expressible for M6-4 to be able to refuse its absence.
    allow_insecure  INTEGER NOT NULL DEFAULT 0 CHECK (allow_insecure IN (0, 1)),
    created_at      INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- W7 §8.2's `[[uplink]]`. The other half of D-44.
CREATE TABLE uplink (
    peer_name     TEXT PRIMARY KEY,
    endpoint      TEXT NOT NULL,
    auth_kind     TEXT NOT NULL CHECK (auth_kind IN ('mtls', 'psk')),
    tls_cert_path TEXT,
    tls_key_path  TEXT,
    tls_ca_path   TEXT,
    psk           TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (auth_kind <> 'mtls' OR (tls_cert_path IS NOT NULL
                                   AND tls_key_path IS NOT NULL
                                   AND tls_ca_path IS NOT NULL)),
    CHECK (auth_kind <> 'psk' OR psk IS NOT NULL)
) STRICT;

-- ---------------------------------------------------------------------------------------
-- Administrative authentication
--
-- W5 §3, D-35. One administrator, no RBAC, no user table, no roles, no permissions. The
-- `CHECK (id = 1)` below is that decision as a constraint: "one administrator" is not a
-- convention a second INSERT can break.

CREATE TABLE admin_credential (
    id                       INTEGER PRIMARY KEY CHECK (id = 1),
    username                 TEXT NOT NULL,
    -- The FULL PHC string, `$argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>` (W5 §3.2), so the
    -- parameters travel with the hash and login can verify against the stored ones and
    -- re-hash in place when they fall below policy. That transparent upgrade is the whole
    -- reason for storing PHC rather than a bare digest, and the CHECK is what stops a bare
    -- digest being written here later.
    -- GLOB, not LIKE, for the same reason as above: `$ARGON2ID$...` is not a PHC string the
    -- `argon2` crate will parse, and a check that accepts it is not checking the format.
    password_phc             TEXT NOT NULL CHECK (password_phc GLOB '$argon2id$?*'),
    -- W5 §3.5's progressive delay: attempts 1-3 free, then min(2^(n-3), 30) seconds,
    -- per-username rather than per-IP, because there is one username and per-IP is
    -- trivially evaded. Persisted so a restart is not a way to clear the delay.
    failed_attempts          INTEGER NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    last_failed_at           INTEGER,
    -- W5 §3.3's lifetimes, configurable under `System -> Administrator`. 14 days absolute,
    -- 7 days idle; both generous because this is one operator on their own network and a
    -- short expiry buys nothing but friction.
    session_absolute_seconds INTEGER NOT NULL DEFAULT 1209600
                                 CHECK (session_absolute_seconds > 0),
    session_idle_seconds     INTEGER NOT NULL DEFAULT 604800
                                 CHECK (session_idle_seconds > 0),
    created_at               INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at               INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (session_idle_seconds <= session_absolute_seconds)
) STRICT;

-- W5 §3.3, as written there, with the type adaptations STRICT requires. This is the
-- specific structure for the specific defect: today the session cookie is set to the API
-- token's own value, so a browser session cannot be revoked without rotating the
-- credential, and `Logout` can only clear the client's copy.
CREATE TABLE admin_session (
    -- 16 random bytes. The public handle, safe to log.
    id           BLOB PRIMARY KEY CHECK (length(id) = 16),
    -- SHA-256 of the 32-byte secret. The secret is never stored. SHA-256 rather than
    -- Argon2id because the secret is 32 random bytes, so there is nothing to brute-force;
    -- the slow hash is only needed for low-entropy human input. Hashing it at all is not
    -- ceremony: a read of this file -- a backup, a support dump, an `scp` -- must not hand
    -- over a live session.
    token_hash   BLOB NOT NULL CHECK (length(token_hash) = 32),
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at   INTEGER NOT NULL,
    -- For "which browser is this?" in the session list.
    user_agent   TEXT,
    created_ip   TEXT,
    CHECK (expires_at > created_at)
) STRICT;

-- Individually revocable sessions mean the sweep is by expiry, and "sign out everywhere"
-- is a DELETE over the whole table.
CREATE INDEX admin_session_expires_at ON admin_session (expires_at);

-- W5 §3.4's one-time setup claim. The paragraph in that section exists so that a future
-- "let's make first-run friendlier" change has to argue with it: an unclaimed instance
-- bound to 0.0.0.0 means whoever reaches the setup page first owns the install, and the
-- window is "from first start until the owner gets round to it", which on a headless Pi is
-- days. The code closes it, because only someone with console or service-log access has it.
CREATE TABLE admin_setup_token (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    -- SHA-256 of 32 random bytes, base32-encoded without ambiguous characters for retyping.
    -- Compared in constant time (W5 §3.5). Invalidated in the SAME TRANSACTION that creates
    -- the account, so a race between two claimants resolves to exactly one.
    token_hash BLOB NOT NULL CHECK (length(token_hash) = 32),
    issued_at  INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- ---------------------------------------------------------------------------------------
-- The supervisor's own durable state
--
-- W2a D-12 and OQ-D, answered by W3 §3.1. Every `Calendar` firing has an occurrence key
-- `(component_id, local_date, slot_index)`, and the supervisor persists the most recent
-- occurrence's outcome per component so that `Misfire::RunLatest` is correct across a
-- restart. Without it a restart loop re-runs every rollover.
--
-- The absence of a foreign key here is the schema half of W3 D-41. The SQLite pool is a
-- root resource owned by `main`, which hands a `ScheduleStore` over THIS TABLE ALONE to the
-- supervisor and the full handle to `config:sqlite`. The supervisor never sees
-- `ConfigStore` and `config:sqlite` never sees `component_schedule` -- routing either need
-- through the other would be the `AppReloader` cycle in a new coat. A foreign key from here
-- to `stations` would make that separation a convention rather than a fact.

CREATE TABLE component_schedule (
    -- One row per component: the most recent occurrence, not a history.
    component_id TEXT PRIMARY KEY,
    -- The occurrence key's local date, ISO-8601, in the component's resolved zone. Keyed on
    -- LOCAL DATE and slot rather than on an instant, which is what makes fall-back's second
    -- pass recognisable as already completed (W2a D-13).
    -- `date()` rather than a GLOB over digits alone, which accepts '2026-99-99'. `IS`
    -- rather than `=`: `date('2026-99-99')` is NULL, and `NULL = '2026-99-99'` is NULL,
    -- which a CHECK treats as a pass.
    local_date   TEXT NOT NULL
                     CHECK (local_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
                            AND date(local_date) IS local_date),
    slot_index   INTEGER NOT NULL CHECK (slot_index >= 0),
    completed_at INTEGER NOT NULL,
    -- `ok`, not `completed`: W3 §5.6's `ticks_total{outcome="ok"}` is the only enumerated
    -- outcome vocabulary in the design, and the other four line up with `failed_total`,
    -- `deadline_exceeded_total`, `skipped_total` and `misfired_total`. M2-3 writes both.
    outcome      TEXT NOT NULL CHECK (outcome IN (
                     'ok', 'failed', 'deadline_exceeded', 'skipped', 'misfired'))
) STRICT;
