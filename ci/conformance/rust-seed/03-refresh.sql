-- Refresh the four aggregate levels over the whole seeded range.
--
-- Every level is `materialized_only` (`ci/db/schema-assertions.sql` A-6 asserts it, because
-- three of the four are the source of another and none may serve a bucket the next one up
-- has not seen). So a view is empty until something refreshes it, and the refresh POLICIES
-- run on their own schedule -- `observations_1m`'s every minute, the coarser ones less
-- often. A replay that started before the first one fired would read empty views and
-- `/span` would be `[]` for every station, which is a green-looking gate measuring nothing.
--
-- **In order, coarsest last**, because each level reads the one below it (**D-11**): a
-- 5-minute bucket is the sum of its five 1-minute buckets' state, so refreshing `_5m`
-- before `_1m` would materialise it from nothing.
--
-- `CALL` rather than `SELECT`: `refresh_continuous_aggregate` is a procedure and manages its
-- own transactions, which is also why this file has no `BEGIN` -- it cannot be inside one.
--
-- The window is `NULL, NULL` -- the whole hypertable -- rather than the seeded range. It is
-- a 31-day range of about 60,000 raw rows, the measured cost of a full rebuild at that size
-- is seconds (`ci/db/measure/20-rebuild-timing.sql`), and naming the range here would be a
-- second place that has to agree with `02-weather.sql` about how far back the seed goes.

CALL refresh_continuous_aggregate('observations_1m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_5m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1h', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1d', NULL, NULL);
