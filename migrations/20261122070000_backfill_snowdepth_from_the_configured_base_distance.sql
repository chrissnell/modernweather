-- Fill `snowdepth` for every stored reading that has a distance and no depth, so that the
-- `/snow` read path can stop deriving the depth from a configured base distance.
--
-- # Why this has to happen before the read path moves
--
-- The Go daemon never wrote `snowdepth`. It computed the depth per request from a
-- per-website cache of the configured base distance (`restserver/controller.go:903`), so
-- the column -- and every aggregate bucket over it -- is NULL for the whole of the migrated
-- history (`docs/reconciliation.md` DEV-9: *"the column and every aggregate bucket are NULL
-- today"*). M3-3 moved the derivation to ingest, so rows this daemon stored have it and
-- rows it inherited do not.
--
-- The `snow:*` components do not read the column at all today: they read `snowdistance` per
-- bucket and subtract the station's `snow_base_distance_mm`. Moving them onto the stored
-- depth -- which is what lets the gauge's own calibration be the only calibration -- would
-- therefore blank every pre-cutover total the moment it shipped. This migration is what
-- makes that switch lossless, and it is the last use of the configured base distance.
--
-- # The arithmetic is the one the read path has been using
--
-- `base_distance - snowdistance`, with the same two guards:
--
--   * `snowdistance > 0` -- the Go read path's own test (`controller.go:902`) and
--     `mw_pipeline::enrich::snow_depth`'s. A non-positive distance is a sensor fault, not a
--     gauge buried to its sensor.
--   * `snow_base_distance_mm > 0` -- a station with no calibration has no bare-ground
--     reference, and the column's own `CHECK` in the configuration store is `> 0`.
--
-- So a row this produces is a row `/snow` was already serving a depth for. Nothing is
-- invented: a reading whose station has no base distance, or whose distance is a fault,
-- keeps its NULL and contributes nothing, exactly as it does now.
--
-- Negative results are kept. A distance past the base is a depth below zero, which is what
-- the aggregate levels will then hold for it -- visibly wrong, which is the right failure
-- for a mis-set base distance and the same choice the owner made for the device's own
-- signed depth (2026-11-21). `threshold::depths_from`'s bare-ground filter is what keeps
-- those buckets out of the totals, and it applies to the stored depth exactly as it applied
-- to the derived one.
--
-- # Idempotent, which is what the gate checks
--
-- `WHERE snowdepth IS NULL` makes a second application match nothing. `cargo xtask gate
-- db-migrate-twice` applies every migration twice and asserts the second is a no-op; this
-- one is, and not because the ledger stopped it.

UPDATE observations o
   SET snowdepth = (s.snow_base_distance_mm - o.snowdistance)::float4
  FROM stations s
 WHERE s.station_id = o.station_id
   AND o.snowdepth IS NULL
   AND o.snowdistance IS NOT NULL
   AND o.snowdistance > 0
   AND s.snow_base_distance_mm IS NOT NULL
   AND s.snow_base_distance_mm > 0;

-- # The aggregates are NOT refreshed by this, and that is not an oversight
--
-- `refresh_continuous_aggregate` cannot run inside a transaction and `sqlx` wraps every
-- migration in one, which is why `ci/db/rebuild-aggregates.sh` exists and why every
-- migration here that touches a level leaves it to that script. The raw rows are correct
-- the moment this commits; the 1-minute, 5-minute, hourly and daily levels still hold the
-- NULLs they were materialised with.
--
-- **So `/snow`'s history stays blank until the rebuild runs**, because the components read
-- the aggregate levels and not the raw table. The operator's sequence is:
--
--     DATABASE_URL=... ci/db/rebuild-aggregates.sh
--
-- and it is safe to run while the daemon is serving -- the script relaxes commit durability
-- per session and refreshes bottom-up, which is the order the levels are sourced in.
--
-- Stated here rather than only in a release note because this file is what a reader finds
-- when they ask why a total is zero.
