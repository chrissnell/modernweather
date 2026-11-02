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
# depending on commit durability, while the last six hours of it takes well under a
# second. Adding a metric forces a drop-and-recreate of every level that carries it
# (D-15), so without this ordering "adding a metric" means an hour of empty charts at ten
# years of history.
#
# The ordering is also *required*, not just nicer: each level reads the level below it, so
# a range must be materialised bottom-up or an upper level reads a hole and caches it.
#
# N-16: PHASE 1 USES THE WIDEST BAND ABOVE, NOT EACH LEVEL'S OWN
#
# Phase 1 used to refresh each level over exactly the window /span reads it over — the
# minute level over 6 hours, the daily level over 366 days — and that is wrong in a way
# that survives the run. "Bottom-up over its own band" has the daily level aggregate a
# 366-day range out of an hourly level that was only materialised over 14 days, so the
# days beyond the fourteenth are computed from nothing and *cached* as nothing. A full
# run fixes it on the second pass; `--recent-only` never does, and `--recent-only` is
# what docs/database-schema.md §4 recommends after a metric addition. OQ-W9-3.
#
# So each level is refreshed over the widest band any level above it serves. The hourly
# level gets the daily level's 366 days, and so, transitively, do the 5-minute and minute
# levels — because a correct 366-day daily bucket needs a correct 366-day hourly level
# beneath it, and under D-11 there is no shortcut to raw for the levels in between.
#
# Be clear about what that costs: phase 1 is no longer seconds. It is now the full
# bottom-up rebuild over the trailing year, and on the measured instance the minute level
# is the great majority of it. The seconds it used to take were the seconds of not doing
# the work, and the missing work was being written into the daily level as zeroes. What
# phase 1 still buys over phase 2 is a bounded range: phase 2 is unbounded history, which
# at ten years of `observations_1d` is a different order of magnitude again.
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
#   ci/db/rebuild-aggregates.sh --recent-only       # phase 1 and stop; warns what is short
#   ci/db/rebuild-aggregates.sh --from 2026-01-01 --to 2026-02-01
#   ci/db/rebuild-aggregates.sh --check-watermarks  # report, change nothing
#
# DATABASE_URL must point at the weather database.

set -euo pipefail

# W9 edit, flagged in docs/data-migration.md's closing section: PSQL overrides the whole
# invocation, so this script also works where `psql` cannot reach the database directly —
# a container, a bastion, or the W9 migration lab, which talks to a pod through
# `kubectl exec`. DATABASE_URL remains required unless PSQL is given, because without one
# of the two there is nothing to connect to.
if [ -n "${PSQL:-}" ]; then
  psql_() { $PSQL --set ON_ERROR_STOP=1 -X -q "$@"; }
else
  : "${DATABASE_URL:?set DATABASE_URL to the weather database, or PSQL to a psql invocation}"
  psql_() { psql --set ON_ERROR_STOP=1 -X -q "$DATABASE_URL" "$@"; }
fi

# Level, and the window /span can ever read it over (docs/legacy-api-contract.md §2.3).
# `/span` takes no time anchor, so every span is [now - span, now] and these are exhaustive.
LEVELS=(observations_1m observations_5m observations_1h observations_1d)
BANDS=('6 hours' '48 hours' '14 days' '366 days')
# The same bands in hours. Phase 1 has to pick the widest of a set of them (N-16) and bash
# cannot compare an INTERVAL, so this is the comparable form of the line above and has to
# be kept in step with it.
BAND_HOURS=(6 48 336 8784)
# Retention per level, in hours: 7 days, 14 days, 90 days, unbounded (W4 decision 5,
# docs/database-schema.md §1.2). 0 means unbounded. Used for one thing only — deciding
# whether the band phase 1 refreshed a level over reaches as far back as the level is
# meant to hold data, which is what `--recent-only` warns about.
RETENTION_HOURS=(168 336 2160 0)

# The index of the widest band served at or above level $1. Not simply the last level:
# the bands happen to widen monotonically today and nothing enforces that, and reading
# the array is cheaper than relying on it.
widest_above() {
  local i="$1" j best="$1"
  for ((j = i; j < ${#LEVELS[@]}; j++)); do
    if [ "${BAND_HOURS[j]}" -gt "${BAND_HOURS[best]}" ]; then best="$j"; fi
  done
  printf '%s' "$best"
}

# Levels phase 1 leaves short, one per line: those whose retention window reaches further
# back than the band phase 1 refreshed them over. Unbounded retention always does, which
# is why `observations_1d` is on this list and the other three are not — their whole
# retention window is inside phase 1's band.
incomplete_after_phase_1() {
  local i w
  for i in "${!LEVELS[@]}"; do
    w=$(widest_above "$i")
    if [ "${RETENTION_HOURS[i]}" -eq 0 ]; then
      printf '%s: materialised over the last %s; its retention is unbounded, so every\n' \
             "${LEVELS[i]}" "${BANDS[w]}"
      printf '  bucket older than that is missing.\n'
    elif [ "${RETENTION_HOURS[i]}" -gt "${BAND_HOURS[w]}" ]; then
      printf '%s: materialised over the last %s; it retains %s days.\n' \
             "${LEVELS[i]}" "${BANDS[w]}" "$((RETENTION_HOURS[i] / 24))"
    fi
  done
}

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

# Phase 1. Each level over the widest band any level above it serves, bottom-up (N-16).
# Its own band is not enough: the level above reads this one, and a range the level above
# needs and this one does not have is materialised as nothing and kept.
echo "=== phase 1: the widest /span band above each level, bottom-up"
for i in "${!LEVELS[@]}"; do
  refresh "${LEVELS[$i]}" "now() - INTERVAL '${BANDS[$(widest_above "$i")]}'" "NULL"
done
echo "    /span is now correct for every band. Phase 2 is older history."

if [ "$RECENT_ONLY" -eq 1 ]; then
  echo "=== --recent-only: stopping. Phase 2 did NOT run."
  # Name them. The failure this replaces was silent: the operator was told "older ranges
  # are not materialised" with no way to tell which level that meant or whether it
  # mattered, and for three of the four levels it does not matter at all.
  short="$(incomplete_after_phase_1)"
  if [ -n "$short" ]; then
    {
      echo "!!! These levels are INCOMPLETE and a /span read of them will be short:"
      while IFS= read -r line; do echo "!!!   $line"; done <<<"$short"
      echo "!!! Re-run without --recent-only to materialise the rest."
    } >&2
  else
    echo "    Nothing is left short: every level's retention window is inside its band."
  fi
  exit 0
fi

# Phase 2. Everything older, bottom-up again. Interruptible: each level's refresh is
# independent and re-running the script re-does only what is missing, because a refresh
# over an already-materialised range is a no-op unless the range was invalidated.
#
# Phase 2 is not needed at all for the three lower levels under normal operation — they
# carry 7, 14 and 90 days of retention (W4 decision 5), all of which phase 1 now covers,
# so there is no history left for them to hold. It matters when a bounded range of history
# is wanted back for a one-off query, and for observations_1d, which is the one unbounded
# level and the only name `--recent-only` warns about.
echo "=== phase 2: full history, bottom-up"
for level in "${LEVELS[@]}"; do refresh "$level" "NULL" "NULL"; done
echo "=== done"
