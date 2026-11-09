-- The `stations.timezone` claim, corrected — implementation-plan.md M3-4.
--
-- `20261029234500_w4_station_dimension_caches_indexes_and_policies.sql` creates this
-- column with the comment:
--
--     -- An IANA name, validated by the writer rather than by a CHECK:
--     -- `now() AT TIME ZONE x` is STABLE, so it cannot appear in a constraint.
--
-- The second clause is true and the first was not. `writer:timescale` does not validate
-- the value: it copies `config.stations.timezone` into this column and substitutes `'UTC'`
-- for a NULL or blank one (`mw_store::writer`), so an operator's `Amercia/Denver` was
-- stored verbatim and every rollover that claimed to use it happened in the database
-- server's zone instead — which is the whole of the defect W3 §2.5's consequence 2
-- describes and this item closes.
--
-- The validation now happens at startup, in `mw_store::RegistrySnapshot::new`, which is
-- where the column is already read and already normalised: a non-blank value that
-- `chrono-tz` does not resolve is a **startup refusal naming the station**, and a station
-- with no zone at all is a `PlanError::MissingTimeZone` naming both the station and its
-- `rain:rollover:<station>` component. Neither is a thing this schema can express.
--
-- **Why this is a new migration and not an edit to that file.** `sqlx`'s migrator records
-- a checksum per applied migration and refuses a file that has changed since it ran, so
-- editing a comment in a migration that has been applied anywhere turns every existing
-- database into a startup failure. A `COMMENT ON COLUMN` is also strictly better than the
-- `--` comment it corrects: it is what `\d+ stations` shows an operator who is looking at
-- the column rather than at this repository.
--
-- Idempotent, so `ci/db/migrate-twice.sh` passes: `COMMENT ON` replaces.

COMMENT ON COLUMN stations.timezone IS
    'The station''s IANA time zone, copied from config.stations.timezone by '
    'writer:timescale, with ''UTC'' substituted for a NULL or blank one. Every calendar '
    'rollover for this station is computed in this zone and not the server''s (W3 '
    'assumption 23) -- the Go version wrote date_trunc(''day'', NOW()), which truncates in '
    'the server''s zone and made the device''s declared timezone decorative. NOT '
    'validated here and not validatable here: now() AT TIME ZONE x is STABLE, so it '
    'cannot appear in a CHECK. The value is validated at daemon startup instead, in '
    'mw_store::RegistrySnapshot::new, which refuses to start and names the station when '
    'config.stations.timezone is not an Area/Location IANA name.';
