#!/usr/bin/env bash
# Drop all four aggregate levels and recreate them WITH NO DATA, so that a refresh
# timing starts from an empty materialisation every time. The definitions come from
# generated/sql/, never from a copy: a measurement made against a hand-retyped view is
# a measurement of the retyping.
#
# The CASCADE below also drops the four composite indexes and all seven policies W4's
# migration creates on these levels, and nothing here puts them back — see
# migrations/20261102042200_reassert_aggregate_indexes_and_policies.sql for the full
# enumeration and N-22 for the structural fix. That is correct for a measurement harness,
# which wants an empty materialisation and no background job touching it, and it is why
# this script is for a scratch instance and not for anything you intend to keep.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
: "${PSQL:?set PSQL to a psql invocation, e.g. 'psql postgresql://...'}"
{
  echo 'DROP MATERIALIZED VIEW IF EXISTS observations_1d CASCADE;'
  echo 'DROP MATERIALIZED VIEW IF EXISTS observations_1h CASCADE;'
  echo 'DROP MATERIALIZED VIEW IF EXISTS observations_5m CASCADE;'
  echo 'DROP MATERIALIZED VIEW IF EXISTS observations_1m CASCADE;'
  for f in 030_observations_1m 040_observations_5m 050_observations_1h 060_observations_1d; do
    cat "$ROOT/generated/sql/$f.sql"
  done
} | $PSQL -q -v ON_ERROR_STOP=1 -f -
