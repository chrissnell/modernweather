-- Three columns `config.stations` needs before a station with a non-`fixed` identity can
-- be resolved, and before a WeatherLink Live station can be configured at all.
--
-- D-8 as amended by A-W6-2: a schema change is a migration, there is no startup DDL, and
-- `IF NOT EXISTS` does not appear. Additive `ALTER TABLE ... ADD COLUMN`, so applying this
-- twice is an error rather than a tolerated no-op -- the ledger is what makes the second
-- application a no-op, which is the property `cargo xtask gate db-migrate-twice` checks.
--
-- Why these are here and not in the initial schema (implementation-plan.md M2-2). M2-2
-- authored the `StationIdentity` and `Transport` halves of W2a §4a.4 and §4a.3 faithfully
-- and did not carry the *value each non-`fixed` identity strategy compares against*:
--
--   * `identity_kind = 'from_payload'` is WeatherLink Live, whose broadcast carries the
--     device serial (`did`) and nothing else (W2a §4a.4). §4a.6 requires that DSN to be
--     resolved against "an allow-list from the registry", and there was no column holding
--     the allowed DSN -- so the allow-list had no contents and every packet resolved to
--     `UnknownStation`.
--   * `identity_kind = 'from_query'` is Ambient, whose `?ID=` has the same problem. That
--     driver is M6-1's; the column is general because the gap is.
--
-- `ingest_secret_hash` is already here for the *secret* half of `from_query`. What was
-- missing is the *identity* half, for every strategy where identity arrives in the frame.

-- The identifier a station presents, for an identity strategy that is not `fixed`.
--
-- A **claim**, not an identity: `FrameMeta::presented_key` is `&str` until
-- `StationRegistry::resolve` matches it here and returns the `StationKey`, and that match
-- is the authentication for a transport with no credential (W2a §4a.6). Deliberately
-- separate from `station_key`: the DSN of a WeatherLink Live unit is a property of the
-- hardware and `station_key` is the operator's domain name for the place it stands, so
-- making one the other would mean replacing a station's key when its hardware is replaced.
--
-- The CHECK is written over `identity_kind` as well as over this column, which SQLite's
-- `ADD COLUMN` does permit. `from_registration` is excluded because that strategy mints
-- the key itself at enrolment (W7 §5.2) rather than comparing a configured one, and
-- `fixed` is excluded because configuration already says which station -- a `fixed`
-- transport that presented a key would be presenting a second answer to a question that
-- already has one.
--
-- **It constrains where the column must be ABSENT and not where it must be present**, and
-- that asymmetry is forced rather than chosen. `ADD COLUMN` sets every existing row to
-- NULL, and SQLite validates a new CHECK against the rows already in the table -- so an
-- `ELSE presented_key IS NOT NULL` arm aborts the ALTER on any database that already holds
-- one non-`fixed` station. `ci/db/config-seed.sql` seeds exactly that (an Ambient station,
-- `from_query`, no claim), so it is not a hypothetical: the strict form fails
-- `cargo xtask gate db-migrate-twice` in two places, and on an operator's own database it
-- fails `sqlx::migrate` at startup with no value to backfill from.
--
-- Nothing is lost. The required half is enforced at plan time with a better message than a
-- constraint can produce -- `mw_station::plan` raises
-- `StationPlanError::Missing { column: "presented_key" }`, which names the station and says
-- what the column is for -- and a `CHECK` that duplicated it would only be able to say
-- "constraint failed". The half that *is* here is the half plan time cannot reach: a
-- `fixed` row carrying a claim is a row the registry would never consult, so nothing would
-- ever notice it.
ALTER TABLE stations ADD COLUMN presented_key TEXT CHECK (
    CASE identity_kind
        WHEN 'fixed'             THEN presented_key IS NULL
        WHEN 'from_registration' THEN presented_key IS NULL
        ELSE presented_key IS NULL OR presented_key <> ''
    END
);

-- Two claims cannot resolve to two stations, or resolution is not a function. Partial, so
-- that the NULL every `fixed` station carries is not a collision -- SQLite permits
-- repeated NULLs in a UNIQUE index, but saying so with a WHERE clause states the intent.
--
-- Over `lower(presented_key)`, because that is the comparison that actually happens.
-- `mw_store::registry` folds ASCII case on both sides -- a WeatherLink Live reports
-- `001D0A700002` and an operator types `001d0a700002`, and a case-sensitive lookup there is
-- a station whose datagrams silently never resolve. A byte-exact index would therefore
-- admit two rows the registry considers one, and the second one read would win: the first
-- station's readings would be stored under the second station's key. Silently wrong data is
-- worse than the flat line the case folding exists to prevent, so the index agrees with the
-- lookup.
CREATE UNIQUE INDEX stations_presented_key ON stations (lower(presented_key))
    WHERE presented_key IS NOT NULL;

-- "Which sensor is which" (W5 §1.6), as the mapping string the Go driver parses:
-- `th:1, rain:1, wind:1, windchill:1, thw:1:appTemp, th_indoor, baro, battery:1`.
--
-- A WeatherLink Live reports a flat list of `conditions` records keyed by transmitter id,
-- and nothing in the payload says which transmitter is the outdoor one or which soil probe
-- is in which port. That mapping is configuration, it has no default that is right for
-- more than one deployment, and a station without it decodes to an empty reading -- so
-- `mw_station`'s registrar rejects a `weatherlink-live` station whose mapping is absent,
-- naming the station and this column.
--
-- Stored resolved rather than as a template id. W5 §1.6 merges the two validation
-- endpoints precisely so that the surface validates "a resolved mapping" and the template
-- picker resolves client-side, which means the four templates are the management UI's
-- vocabulary and never the daemon's.
ALTER TABLE stations ADD COLUMN wll_sensor_mapping TEXT CHECK (
    wll_sensor_mapping IS NULL OR wll_sensor_mapping <> ''
);

-- The HTTP poll period, in seconds. The Go column is `wll_poll_interval`; the unit is in
-- the name here for the reason the calibration columns above give.
--
-- W2a §4a.5 writes the poll trigger as `Every { >= 30s }` and the floor is declared by the
-- registrar and enforced once in `plan()` (W2a Decision 56), not clamped per occurrence as
-- `runPollingMode` does today. The CHECK is the same floor, so a row that `plan()` would
-- reject cannot be written in the first place; NULL means the driver's own default.
ALTER TABLE stations ADD COLUMN wll_poll_interval_seconds INTEGER CHECK (
    wll_poll_interval_seconds IS NULL OR wll_poll_interval_seconds >= 30
);
