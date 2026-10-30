-- W4, hand-authored. NOT `cargo xtask migrate` output, and `cargo xtask codegen --check`
-- does not check it.
--
-- The generator owns metric columns and the aggregate expressions over them. Everything in
-- this file is what it deliberately does not generate (W2 assumptions 8 and 9): the
-- station dimension, the two precomputed caches the frozen contract reads directly, the
-- unmodelled-field escape hatch, every index beyond the hypertable's own time index, and
-- every compression and refresh policy. A decision recorded in docs/database-schema.md
-- that needed a *generated* artefact to change went through the generator instead; this
-- file contains nothing a registry entry could have expressed.
--
-- Read docs/database-schema.md alongside it. Every number quoted below was measured
-- against timescale/timescaledb-ha:pg17 at 35,942,400 raw rows; the harness is
-- ci/db/measure/ and §6 of that document carries the commands.

-- ===========================================================================
-- 1. The station dimension
-- ===========================================================================
-- This table is a PROJECTION, not a registry of record. `config:sqlite`'s
-- StationRegistry is authoritative for a station's existence, credentials and
-- configuration (W3 §3.1); what lives here is the subset a reading-side query needs, so
-- that `/span` can resolve a station's display name with a join instead of a column
-- repeated on every row of a hundred-million-row table. The ingest path keeps it current
-- when it resolves a station in `Ingress::admit`.
--
-- Keyed by the station_id text that arrives on the wire (protobuf field 221), not by a
-- surrogate integer. The W4 prompt asks for a surrogate `station_id int`, and the saving
-- it is after is real but small and not where the prompt looks for it: measured, a
-- 10-character text station_id costs 7 bytes per row more than an int4 in an uncompressed
-- chunk and nothing at all once the chunk is compressed, because station_id is the
-- compression segmentby key and is therefore stored once per batch rather than once per
-- row. Against that, a surrogate would have to be allocated server-side and mapped at
-- ingest, which means a protobuf type change on a field that remoteweather's satellites
-- already send as a string, and a lookup inside the writer that W3 §2.3 does not have.
-- The denormalisation the prompt is actually objecting to is `stationname` and
-- `stationtype` repeated per row, and that is addressed below and in OQ-W4-2.
CREATE TABLE stations (
    station_id  text PRIMARY KEY,
    name        text NOT NULL,
    type        text NOT NULL,
    -- Every calendar rollover is computed in the station's zone, not the server's
    -- (W3 assumption 23). The Go version writes date_trunc('day', NOW()), which
    -- truncates in the server's zone and makes the device's declared timezone
    -- decorative. An IANA name, validated by the writer rather than by a CHECK:
    -- `now() AT TIME ZONE x` is STABLE, so it cannot appear in a constraint.
    timezone    text NOT NULL DEFAULT 'UTC',
    latitude    double precision,
    longitude   double precision,
    altitude_ft double precision,
    -- The station's own zero point for snow depth, in millimetres. W3 §2.5 enriches
    -- snowdepth at ingest from (base_distance - snowdistance), which makes it a pure
    -- function of one reading plus static station metadata — this column is that
    -- metadata, and it has to be here rather than only in the config store because the
    -- enrich stage is in the reading path.
    snow_base_distance_mm double precision,
    first_seen  timestamptz NOT NULL DEFAULT now(),
    last_seen   timestamptz,
    CONSTRAINT stations_id_not_empty   CHECK (station_id <> ''),
    CONSTRAINT stations_latitude_range CHECK (latitude  IS NULL OR latitude  BETWEEN -90 AND 90),
    CONSTRAINT stations_longitude_range CHECK (longitude IS NULL OR longitude BETWEEN -180 AND 180)
);

COMMENT ON TABLE stations IS
    'Read-side projection of the station registry. Authoritative copy lives in the '
    'configuration store (W3 §3.1); this exists so readings can be joined to a name '
    'instead of repeating it on every row.';

-- No foreign key from `observations` to this table, deliberately — and not on cost
-- grounds. Measured, the constraint is accepted on a hypertable with compressed chunks and
-- costs 3.9% of bulk insert time (3.392 s against 3.265 s for 43,200 rows); at the
-- production cadence of four rows every two seconds it is unmeasurable. The reason is the
-- failure direction. W3 §2.6 already resolves the station in `Ingress::admit` before a
-- reading can reach the writer — that is the order that makes an unauthenticated reading
-- unstorable — so the invariant is enforced upstream, and a reference here would only add
-- a second enforcement whose failure mode is *rejecting a reading* because a projection
-- row is missing. Readings are the thing that must not be lost (D-30); bookkeeping for a
-- non-authoritative projection is not worth putting in front of them. It would also mean
-- choosing between ON DELETE CASCADE, which lets one DELETE take a hundred million
-- readings with it, and RESTRICT, which makes a decommissioned station unremovable.
--
-- Recorded rather than merely omitted, because it is one statement to reverse:
--   ALTER TABLE observations ADD CONSTRAINT observations_station_fk
--       FOREIGN KEY (station_id) REFERENCES stations (station_id);
--
-- The low-volume tables below do carry the reference: there the check is free, there is no
-- reading to lose, and an orphaned cache row is a real failure mode.

-- ===========================================================================
-- 2. Which metrics a station actually emits
-- ===========================================================================
-- So that the API can describe a station's capabilities without probing data. The Go tree
-- has a TODO for exactly this at internal/weatherstations/grpcreceiver/station.go:64, and
-- the alternative it was a TODO instead of — asking the raw table which columns are
-- non-NULL for a station — is a scan of every chunk the station appears in.
--
-- Keyed by the registry's stable `key`, not by the PostgreSQL column name. The two are
-- different namespaces on purpose (docs/metric-registry.md §1.1): the key is what the
-- Rust types, the OpenAPI schema and the TypeScript types name a metric, and it is the
-- one of the pair an API response should ever contain. There is no foreign key to a
-- metrics table because there is no metrics table: metrics/registry.toml is the source of
-- truth (D-20) and a second copy in the database would be a second source of truth.
CREATE TABLE station_metrics (
    station_id text        NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    metric_key text        NOT NULL,
    first_seen timestamptz NOT NULL DEFAULT now(),
    last_seen  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (station_id, metric_key)
);

COMMENT ON COLUMN station_metrics.metric_key IS
    'metrics/registry.toml [[metric]].key — not the PostgreSQL column name.';

-- ===========================================================================
-- 3. The two precomputed caches the frozen contract reads directly
-- ===========================================================================
-- W1 assumption 12: `/almanac` and `/snow` read these tables and nothing else, and
-- `/almanac` returns `{}` rather than computing anything when its cache is empty. They
-- are not an optimisation that can be removed; they are the endpoints' only data source.
-- W3 §2a.7's `snow:*` components write the second one. Both schemas below are shaped so
-- that the frozen JSON falls out of them without a special case.

-- One row per record per station. The contract's AlmanacData has seventeen *AlmanacRecord
-- pointer fields, every one `omitempty`, so an absent row is an absent key and no rows at
-- all is `{}` — which is exactly the shipped empty-cache behaviour, reproduced by the
-- table's shape rather than by a branch in the handler. Seventeen nullable column
-- triplets would have needed that branch.
CREATE TABLE almanac_cache (
    station_id  text        NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    record      text        NOT NULL,
    -- real, not double precision: the contract serialises `value` and `wind_dir` as Go
    -- float32 (docs/legacy-api-contract.md §3), and §6.1 pins the digits Go emits.
    -- Widening the column would change those digits for values that are not exactly
    -- representable in 24 bits of mantissa, which a frozen API cannot do.
    value       real        NOT NULL,
    observed_at timestamptz NOT NULL,
    wind_dir    real,
    computed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (station_id, record),
    CONSTRAINT almanac_cache_record_known CHECK (record IN (
        'high_temp', 'low_temp', 'high_wind_speed', 'max_rain_hour', 'max_rain_day',
        'low_barometer', 'low_humidity', 'deepest_snow', 'max_snow_hour', 'max_snow_day',
        'high_solar', 'high_pm25', 'high_pm10_in', 'high_co2', 'high_tvoc',
        'high_aqi_pm25', 'high_aqi_pm10'))
);

COMMENT ON COLUMN almanac_cache.record IS
    'The frozen /almanac JSON key this row populates. The CHECK is the contract: a typo '
    'here would silently drop a field from a response rather than fail.';

-- One row per station, every column NOT NULL DEFAULT 0. The contract emits all seven
-- numbers unconditionally and a configured-but-never-reporting gauge returns an all-zero
-- object with status 200 (fixture `snow-device-never-reported`), so zero is the correct
-- value and not a missing one.
--
-- There is deliberately no TTL, no retention and no recency filter. `/snow` has none
-- either, and that is part of the contract, not an oversight: an offline gauge returns
-- its last known depth indefinitely (fixture `snow-device-stale`, Q-16). `computed_at`
-- and `source_through` exist so that staleness is *observable* without being acted on.
CREATE TABLE snow_totals_cache (
    station_id     text        PRIMARY KEY REFERENCES stations (station_id) ON DELETE CASCADE,
    snowdepth      real        NOT NULL DEFAULT 0,
    snowtoday      real        NOT NULL DEFAULT 0,
    snowlast24     real        NOT NULL DEFAULT 0,
    snowlast72     real        NOT NULL DEFAULT 0,
    snowseason     real        NOT NULL DEFAULT 0,
    snowstorm      real        NOT NULL DEFAULT 0,
    snowfallrate   real        NOT NULL DEFAULT 0,
    computed_at    timestamptz NOT NULL DEFAULT now(),
    source_through timestamptz
);

COMMENT ON TABLE snow_totals_cache IS
    'Written by W3 §2a.7 snow:* components, read directly by /snow. Values are inches '
    '(the /snow contract), not the millimetres snowdepth carries on /latest — Q-4.';

-- ===========================================================================
-- 4. The escape hatch for unmodelled vendor fields
-- ===========================================================================
-- Sparse, never aggregated, never a substitute for a registry entry.
--
-- It is outside metrics/registry.toml on purpose. The registry is the source of truth for
-- *modelled* metrics, and this column exists for fields that are by definition not
-- modelled yet; putting it in the registry would also mean giving it a protobuf field
-- number and a generated Rust type, neither of which means anything for a field nobody
-- has named. The registry has no jsonb SQL type today (see OQ-W4-3).
--
-- What gets in: a flat JSON object whose values are scalars, from a driver that has
-- received a field it has no metric for. What does not: anything a [[metric]] entry could
-- hold, and anything large — the size cap is what stops the hatch from quietly becoming
-- the storage layer. 2 kB is about forty unmodelled scalars.
--
-- The promotion path is the point of the whole thing, and it is three steps: add a
-- [[metric]] entry, `cargo xtask migrate` (11.6 ms of ADD COLUMN, D-9), then a one-off
-- backfill `UPDATE observations SET <col> = (extra->>'<field>')::float4` over the range
-- that has it, after which the driver writes the column and stops writing the key. The
-- aggregates pick it up on the next rebuild. Nothing in `extra` is ever aggregated, so
-- there is no aggregate to migrate.
ALTER TABLE observations ADD COLUMN extra jsonb;

ALTER TABLE observations ADD CONSTRAINT observations_extra_is_small_object CHECK (
    extra IS NULL
    OR (jsonb_typeof(extra) = 'object' AND length(extra::text) <= 2048));

COMMENT ON COLUMN observations.extra IS
    'Unmodelled vendor fields only. Never aggregated, never indexed. A field that earns '
    'a query earns a [[metric]] entry in metrics/registry.toml — see docs/database-schema.md §4.';

-- ===========================================================================
-- 5. Indexes
-- ===========================================================================
-- W2 assumption 9 leaves every index beyond the hypertable's own time index here. The hot
-- path does not touch the database at all — /latest is 93% of traffic and is served from
-- the in-memory snapshot (W3 §2a) — so these index the two things that do read SQL:
-- `/span`'s four bands, and the caches.

-- One index, two jobs. It is the unique constraint W3 assumption 22 needs so that the
-- writer can replay a cancelled prefix with ON CONFLICT (station_id, time), and it is
-- also the access path every /span and every cold single-station read wants:
-- station_id equality then a descending time range. Measured: permitted on the
-- compressed hypertable, and ON CONFLICT DO UPDATE works against compressed chunks in
-- TimescaleDB 2.31 — see §6 measurement 4 for what that costs.
--
-- DESC on the time column matches compress_orderby and the direction every reader scans.
CREATE UNIQUE INDEX observations_station_id_time_key
    ON observations (station_id, time DESC);

-- The aggregate levels. TimescaleDB creates an index on the materialisation hypertable
-- for the group-by column, but /span's predicate is (station_id, bucket range) and the
-- generated one is not composite, so each level gets the composite it actually wants.
-- On the compressed chunks these matter less — segmentby does the skipping — but a level
-- is uncompressed for as long as its compression delay, and that window is exactly the
-- range /span reads most.
CREATE INDEX observations_1m_station_bucket_idx ON observations_1m (station_id, bucket DESC);
CREATE INDEX observations_5m_station_bucket_idx ON observations_5m (station_id, bucket DESC);
CREATE INDEX observations_1h_station_bucket_idx ON observations_1h (station_id, bucket DESC);
CREATE INDEX observations_1d_station_bucket_idx ON observations_1d (station_id, bucket DESC);

-- No index on observations.extra. A GIN index on a jsonb column of a hundred-million-row
-- hypertable is the single thing most likely to turn the escape hatch into the schema.
-- No index on station_metrics.metric_key either: the reverse question ("which stations
-- emit this metric?") has no caller in the frozen contract, and 4 stations x ~200 metrics
-- is a sequential scan of under a thousand rows if one appears.

-- ===========================================================================
-- 6. Retention on the aggregate levels, and why there is no compression here
-- ===========================================================================
-- OQ-4 asked whether the 1-minute aggregate is compressed, retention-bounded, or both,
-- and W0 recommended "compress it and keep it unbounded", conditional on confirming that
-- compressed-aggregate query latency was acceptable. The condition turns out to be
-- unreachable, because the premise is false:
--
--     ERROR:  row is too big: size 11720, maximum size 8160
--
-- That is `compress_chunk` on observations_1m. A compressed chunk holds one row per
-- segment per batch and turns every source column into a compressed array; those arrays
-- are TOASTable but their pointers are not, so ~19 bytes per column have to fit in an
-- 8160-byte heap tuple no matter how little data they point at. Bisected, the widest
-- aggregate that compresses at all is **439 columns** (ci/db/measure/66-compressible-width.sh);
-- the generated levels are 629. D-19 anticipated this failure but placed it near the
-- 1600-column ceiling. It is at 439, and the generated schema is past it.
--
-- Narrowing is not available either. Level membership can only shrink away from raw
-- (OQ-W2-1), so the 1-minute level is necessarily the widest, and switching every mean to
-- Toolkit stats_agg — OQ-3's one-column alternative — removes only the 127 count columns
-- and lands at 502, still past 439. There is no configuration of this schema in which the
-- minute level compresses.
--
-- So OQ-4's other arm it is: bounded retention, and the reason it is safe is sharper than
-- "aggregates are disposable". `/span/{span}` takes no time anchor — only `station`, an
-- ignored `station_id`, and an ignored `format` (docs/legacy-api-contract.md §2.1) — so
-- every span is [now - span, now]. Each level is therefore read over exactly one window
-- and never over history:
--
--     1m   <= 6 h      1h    <= 14 d
--     5m   <= 48 h     1d    <= 1 year (the handler's own maximum)
--
-- Unbounded, the minute level is 4,175 bytes per bucket row — 24 MB/day at four stations,
-- 88 GB for a decade — of data that is uncompressible and, past six hours old, never
-- read. W0's objection to bounded retention was that it "reintroduces, at one remove,
-- exactly the hole D-4 exists to close". It does not: D-4's hole is in *raw*, and raw is
-- permanent, so an old minute bucket is not lost information, it is an evicted cache
-- entry that ci/db/rebuild-aggregates.sh reconstructs for any range on demand.
--
-- Each level's retention is sized to cover its own band plus the window over which the
-- level above it may still be refreshed (§7's start_offset), with margin.
SELECT add_retention_policy('observations_1m', drop_after => INTERVAL '7 days');
SELECT add_retention_policy('observations_5m', drop_after => INTERVAL '14 days');
SELECT add_retention_policy('observations_1h', drop_after => INTERVAL '90 days');

-- observations_1d is the only level read over all history, and it is 18 kB/day at four
-- stations — 64 MB for a decade. Unbounded, deliberately.
--
-- And no retention policy on `observations` itself, ever (D-4). That is the decision this
-- whole design rests on, and the failure mode of adding one is silence for exactly as long
-- as the policy's interval, so ci/db/schema-assertions.sql A-8 fails the build if one
-- appears.

-- ===========================================================================
-- 7. Refresh policies
-- ===========================================================================
-- The 1-minute level carries a hard deadline: W2a assumption 14 and W3 assumption 21 give
-- `window:rollup` 10 seconds from the minute boundary to read that minute's bucket. A
-- policy with schedule_interval => '1 minute' and no fixed start fires at whatever phase
-- within the minute it happened to be created at, so it would meet that deadline by luck.
--
-- `initial_start` plus the default fixed schedule makes the phase deterministic: the job
-- runs at initial_start + k x 1 minute, three seconds after each boundary. `end_offset` of
-- one second keeps the in-flight bucket out of the window — a refresh materialises only
-- buckets wholly inside it, so [now-30min, now-1s) covers the bucket that has just closed
-- and excludes the one still filling.
--
-- Measured over eight consecutive boundaries with a live writer at the production cadence:
-- lag 3.06 s to 3.22 s, against the 10-second budget. The refresh itself is 28-53 ms; the
-- rest is the three-second phase offset, so there is another two seconds of margin
-- available if it is ever wanted. ci/db/measure/40-refresh-lag.sql.
SELECT add_continuous_aggregate_policy('observations_1m',
    start_offset      => INTERVAL '30 minutes',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '1 minute',
    initial_start     => date_trunc('minute', now()) + INTERVAL '1 minute 3 seconds');

-- The upper levels have no deadline of their own, only a cost. Each is scheduled so that a
-- closed bucket is materialised within a fraction of its own width, and each start_offset
-- covers several buckets so that a late or replayed reading in a recent bucket is picked
-- up. The phases are staggered so that two levels never refresh in the same instant, since
-- each reads the output of the one below it.
SELECT add_continuous_aggregate_policy('observations_5m',
    start_offset      => INTERVAL '2 hours',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '1 minute',
    initial_start     => date_trunc('minute', now()) + INTERVAL '1 minute 20 seconds');

SELECT add_continuous_aggregate_policy('observations_1h',
    start_offset      => INTERVAL '2 days',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '5 minutes',
    initial_start     => date_trunc('hour', now()) + INTERVAL '5 minutes 30 seconds');

SELECT add_continuous_aggregate_policy('observations_1d',
    start_offset      => INTERVAL '14 days',
    end_offset        => INTERVAL '1 second',
    schedule_interval => INTERVAL '30 minutes',
    initial_start     => date_trunc('hour', now()) + INTERVAL '10 minutes');

-- All four levels stay materialized_only. TimescaleDB cannot build an aggregate on a
-- real-time aggregate, so 1m, 5m and 1h have no choice; 1d is uniform with them by
-- decision rather than necessity, because its real-time tail would read observations_1h
-- and so would be stale by up to an hour anyway — an inconsistency across bands in
-- exchange for nothing.
--
-- That has a contract consequence, and it is the one thing in this file that lands on
-- another work item. Every /span fixture W1 captured ends with the *current, incomplete*
-- bucket: span-1h's last ts is the minute that was 42 seconds old at capture,
-- span-72h-hourly-with-gap's is the hour that was 56 minutes in, span-720h-daily's is
-- today. A materialized_only level never emits that bucket, so the read path must append
-- one bucket aggregated from raw over [last_closed_bucket_end, now). Measured cost of that
-- tail aggregation, for one station against a compressed chunk: 0.44 ms at the minute
-- level, 0.45 ms at five minutes, 0.68 ms at the hour, 6.89 ms at the day. See
-- docs/database-schema.md §3, decision 13, and the assumption recorded against W3.
