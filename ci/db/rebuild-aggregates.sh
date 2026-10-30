#!/usr/bin/env bash
# The aggregate rebuild procedure. W2 assumption 8 leaves it here;
# generated/sql/090_rebuild_aggregates.sql is the shape it executes, not the procedure.
#
# Operational tool, not a gate. It is in ci/db/ because that is where the database scripts
# live and because the gate reuses its recent-first logic; nothing in CI calls it.
#
# WHY A SCRIPT AND NOT A MIGRATION
#
# `refresh_continuous_aggregate` cannot run inside a transaction and sqlx wraps every
# migration in one, so a migration can create an aggregate but never materialise it. Every
# migration that touches a level therefore leaves it WITH NO DATA and this script is what
# fills it. A deploy that runs migrations and forgets this step leaves /span empty for
# every band whose level was recreated, with no error anywhere — which is why
# `--check-watermarks` exists below.
#
# WHAT IT DOES DIFFERENTLY FROM A FULL REFRESH
#
# Recent ranges first, oldest last. OQ-W2-2's recommendation, and the reason is a measured
# asymmetry: a full rebuild of the minute level over 35.9 M raw rows takes 7 to 15 minutes
# depending on commit durability, while the last six hours of it — which is the entire
# range /span ever reads from that level — takes well under a second. Adding a metric
# forces a drop-and-recreate of every level that carries it (D-15), so without this
# ordering "adding a metric" means an hour of empty charts at ten years of history. With
# it, the charts are correct in seconds and the backfill is a background concern.
#
# The ordering is also *required*, not just nicer: each level reads the level below it, so
# a range must be materialised bottom-up or an upper level reads a hole and caches it.
#
# RELAXED COMMIT DURABILITY (D-17)
#
# Every refresh session sets synchronous_commit = off. Measured on the minute level over
# 35.9 M rows: see docs/database-schema.md §6 measurement 3. It is safe for exactly the
# reason aggregates are disposable (D-15) — a crash mid-rebuild costs a re-run, not
# information — and it is a property of these sessions only. Ingest keeps full durability.
#
# Usage:
#   ci/db/rebuild-aggregates.sh                     # recent-first, then full backfill
#   ci/db/rebuild-aggregates.sh --recent-only       # the /span bands and stop
#   ci/db/rebuild-aggregates.sh --from 2026-01-01 --to 2026-02-01
#   ci/db/rebuild-aggregates.sh --check-watermarks  # report, change nothing
#
# DATABASE_URL must point at the weather database.

set -euo pipefail
: "${DATABASE_URL:?set DATABASE_URL to the weather database}"

psql_() { psql --set ON_ERROR_STOP=1 -X -q "$DATABASE_URL" "$@"; }

# Level, and the window /span can ever read it over (docs/legacy-api-contract.md §2.3).
# `/span` takes no time anchor, so every span is [now - span, now] and these are exhaustive.
LEVELS=(observations_1m observations_5m observations_1h observations_1d)
BANDS=('6 hours' '48 hours' '14 days' '366 days')

RECENT_ONLY=0
FROM=""
TO=""
CHECK_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --recent-only)      RECENT_ONLY=1; shift ;;
    --check-watermarks) CHECK_ONLY=1; shift ;;
    --from)             FROM="$2"; shift 2 ;;
    --to)               TO="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$CHECK_ONLY" -eq 1 ]; then
  # A level whose watermark is behind the band it serves is a level serving short or empty
  # responses. This is the check a deploy should run after migrating.
  psql_ -c "
    SELECT ca.view_name,
           COALESCE(_timescaledb_functions.to_timestamp(
               _timescaledb_functions.cagg_watermark(ca.materialization_hypertable_id)),
               '-infinity'::timestamptz) AS watermark,
           now() AS now
      FROM _timescaledb_catalog.continuous_agg ca
      JOIN timescaledb_information.continuous_aggregates i
        ON i.view_name = ca.user_view_name
     ORDER BY 1" 2>/dev/null \
  || psql_ -c "
    SELECT view_name, materialized_only, compression_enabled
      FROM timescaledb_information.continuous_aggregates ORDER BY 1"
  exit 0
fi

refresh() {   # refresh <level> <start|NULL> <end|NULL>
  local level="$1" start="$2" end="$3" t0 t1
  t0=$(date -u +%s)
  psql_ -c "SET synchronous_commit = off" \
        -c "CALL refresh_continuous_aggregate('$level', $start, $end)"
  t1=$(date -u +%s)
  printf '  %-16s %-26s -> %-26s %5ds\n' "$level" "${start//\'/}" "${end//\'/}" "$((t1 - t0))"
}

if [ -n "$FROM" ] || [ -n "$TO" ]; then
  : "${FROM:?--to requires --from}"
  : "${TO:?--from requires --to}"
  echo "=== bounded range, bottom-up: $FROM -> $TO"
  for level in "${LEVELS[@]}"; do refresh "$level" "'$FROM'" "'$TO'"; done
  exit 0
fi

# Phase 1. Each level over exactly the window /span can read it over, bottom-up. This is
# the phase that has to finish before the charts are right; it is seconds, not minutes.
echo "=== phase 1: the /span bands, bottom-up"
for i in "${!LEVELS[@]}"; do
  refresh "${LEVELS[$i]}" "now() - INTERVAL '${BANDS[$i]}'" "NULL"
done
echo "    /span is now correct for every band. Phase 2 is history."

if [ "$RECENT_ONLY" -eq 1 ]; then
  echo "=== --recent-only: stopping. Older ranges are NOT materialised."
  exit 0
fi

# Phase 2. Everything older, bottom-up again. Interruptible: each level's refresh is
# independent and re-running the script re-does only what is missing, because a refresh
# over an already-materialised range is a no-op unless the range was invalidated.
#
# Phase 2 is not needed at all for the three lower levels under normal operation — they
# carry 7, 14 and 90 days of retention (W4 decision 5), so there is no history for them to
# hold. It matters when a bounded range of history is wanted back for a one-off query, and
# for observations_1d, which is the one unbounded level.
echo "=== phase 2: full history, bottom-up"
for level in "${LEVELS[@]}"; do refresh "$level" "NULL" "NULL"; done
echo "=== done"
