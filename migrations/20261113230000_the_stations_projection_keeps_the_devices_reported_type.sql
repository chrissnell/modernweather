-- The device's own claim about what it is, once per station instead of once per reading.
--
-- M5-5 retires `observations.stationname` and `observations.stationtype` to
-- `role = "metadata"`: both reach the daemon on the wire and neither is a column of a
-- hundred-million-row table any more. `migrations/20261029234500`'s own note says this is
-- where they were always going -- "the denormalisation the prompt is actually objecting to
-- is `stationname` and `stationtype` repeated per row, and that is addressed below and in
-- OQ-W4-2" -- and `stations.name` already covers the first of the two.
--
-- `stationtype` needs a column of its own rather than `stations.type`, and that is the
-- frozen contract rather than tidiness. docs/legacy-api-contract.md §3.1: `/stationinfo`'s
-- `stations[].type` is the CONFIGURED device type (`campbellscientific`) and `/latest`'s
-- `stationtype` is the DRIVER's self-reported name (`campbell`), and "the two vocabularies
-- are not the same set and must not be unified". `stations.type` is the configured one --
-- `mw_store::writer`'s projection writes `config.stations.type` into it and falls back to
-- the reading only for a station it has no configuration row for -- so serving `/latest`
-- from it would unify exactly the two sets that may not be unified. Two columns is the
-- structural way to keep them apart: a reader that wants one cannot accidentally get the
-- other.
--
-- Nullable, and the NULL means "this station has not told us": `/latest`'s `stationtype`
-- is `omitempty` in Go and optional in the iOS client, so an absent key is the honest
-- answer for a device that reports no type and is what the Go daemon already emits for
-- one. `stations.type` stays NOT NULL because it is configuration and configuration always
-- has an answer.
--
-- Written by `writer:timescale` from the envelope's own `station_type`, last non-NULL
-- wins, and read back by `ReadingStore::latest` so that `snapshot:warm` reconstructs the
-- same `/latest` body after a restart as the folded reading produced before it.
ALTER TABLE stations ADD COLUMN reported_type text;

COMMENT ON COLUMN stations.reported_type IS
    'What the DEVICE says it is -- /latest''s `stationtype`, e.g. `campbell`. NOT the '
    'configured device type in `stations.type`, e.g. `campbellscientific`: '
    'docs/legacy-api-contract.md 3.1 requires the two vocabularies to stay distinct. '
    'NULL means the station has not reported one, which /latest emits as an absent key.';

-- The migrated history had the answer on every row and this is the one chance to keep it:
-- after M5-5 nothing writes `observations.stationtype` again, so a station that is not
-- currently reporting would otherwise lose a type it has been sending for a decade. The
-- column is still there -- M5-5's migration emits no `DROP COLUMN`, because a dropped
-- attribute number is never released (H-4, D-19) -- so the newest non-NULL value per
-- station is readable exactly once, here.
--
-- **One seek per station, not one pass over the table.** Written as a correlated subquery rather
-- than as `DISTINCT ON (station_id) ... ORDER BY station_id, time DESC` over the whole hypertable,
-- because PostgreSQL has no index skip scan: the `DISTINCT ON` form reads every row of
-- `observations`, decompressing as it goes, where this is the same `ORDER BY time DESC LIMIT 1`
-- shape on `(station_id, time)` that `ReadingStore::latest` uses -- once per station, over a
-- handful of stations. The argument for that shape being a seek is in `latest_statement`'s own
-- documentation and is not restated here.
--
-- The one case that degrades is a station whose `stationtype` is NULL on every row: the subquery
-- walks that station's chunks and returns NULL. Bounded by one station rather than by the table,
-- and the answer it arrives at is the right one. A fresh database has no rows at all and this
-- updates nothing.
--
-- Verified by hand against the CI instance before this landed, in a transaction that was rolled
-- back: the newest non-NULL value wins over an older one and is not displaced by newer NULL rows,
-- a station with rows but no type stays NULL, a station with no rows stays NULL, and the
-- `WHERE reported_type IS NULL` guard leaves an already-known value alone -- which is what makes
-- re-running this safe.
UPDATE stations s
   SET reported_type = (
         SELECT o.stationtype
           FROM observations o
          WHERE o.station_id = s.station_id
            AND o.stationtype IS NOT NULL
          ORDER BY o.time DESC
          LIMIT 1
       )
 WHERE s.reported_type IS NULL;
