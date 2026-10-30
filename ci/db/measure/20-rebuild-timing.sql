-- Measurement 3: full-rebuild time per level, with and without relaxed commit
-- durability (D-17). Run twice, once per durability setting, against a freshly
-- recreated set of aggregates — see 10-recreate-aggregates.sh.
--
-- \set durability on|off  before running.
\timing off
SET synchronous_commit = :durability;
SELECT current_setting('synchronous_commit') AS synchronous_commit,
       (SELECT count(*) FROM observations) AS raw_rows;
\timing on
CALL refresh_continuous_aggregate('observations_1m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_5m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1h', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1d', NULL, NULL);
\timing off
