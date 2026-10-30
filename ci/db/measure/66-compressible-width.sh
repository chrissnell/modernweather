#!/usr/bin/env bash
# Drives mw_measure.probe_ddl / probe_compress across a range of column counts and prints
# one line per width: how wide an aggregate can be and still compress.
set -uo pipefail
: "${PSQL:?set PSQL to a psql invocation}"
for n in "$@"; do
  $PSQL -q -c 'DROP MATERIALIZED VIEW IF EXISTS mw_measure_probe CASCADE' >/dev/null 2>&1
  $PSQL -At -c "SELECT mw_measure.probe_ddl($n)" 2>/dev/null | $PSQL -q -f - >/dev/null 2>&1 || {
    echo "$n cols: CREATE failed"; continue; }
  $PSQL -q -c "ALTER MATERIALIZED VIEW mw_measure_probe SET (timescaledb.compress = true,
       timescaledb.compress_segmentby = 'station_id',
       timescaledb.compress_orderby = 'bucket DESC')" >/dev/null 2>&1
  $PSQL -q -c 'SET synchronous_commit = off' \
          -c "CALL refresh_continuous_aggregate('mw_measure_probe', NULL, NULL)" >/dev/null 2>&1
  printf '%4d cols: %s\n' "$n" "$($PSQL -At -c 'SELECT mw_measure.probe_compress()' 2>&1 | tail -1)"
done
$PSQL -q -c 'DROP MATERIALIZED VIEW IF EXISTS mw_measure_probe CASCADE' >/dev/null 2>&1
