#!/usr/bin/env bash
# The restore rehearsal. implementation-plan.md M0-2's done-criterion is "a restore has been
# performed from a real R2 object and reconciled, IN CI -- not documented", and this is the
# thing that does it. DESIGN.md D-6 is the mechanism; the rehearsal is its untested second
# half, and docs/reconciliation.md 7.5's reason for rehearsing rather than documenting is
# that a backup you hesitate to restore from is not a backup.
#
# TWO MODES, AND THE SCRIPT ALWAYS SAYS WHICH ONE IT RAN
#
#   round-trip (default)  seed a source database -> `offsite-backup.sh dump` -> restore into
#                         a second database -> reconcile. The archive goes to a LOCAL rclone
#                         remote under a temporary directory, so this needs no credential
#                         and runs on every PR. It exercises the layout, the per-year
#                         slicing, the column lists, the reconciliation and the rebuild.
#
#   --from-archive        restore the REAL archive at MW_BACKUP_REMOTE and reconcile against
#                         its own manifest. Nothing is seeded and nothing is dumped. This is
#                         the mode that proves R2 specifically, and the mode the monthly
#                         in-cluster CronJob runs.
#
# Why the default is the one without the credential, when the credential is the point: a
# gate that is red on every PR because a secret is unavailable gets overridden until nobody
# reads it, which is docs/reconciliation.md G-1 reached by a different route. So the PR gate
# runs the half that can always run, prints in one line which half that was, and never
# claims to have talked to R2. `--from-archive` is scheduled rather than per-PR.
#
# WHAT IT PROVES BEYOND "THE BYTES CAME BACK"
#
#   1. Reconciliation is EXACT, not a row count. The manifest records the sha256 of the
#      ordered COPY stream each object was produced from; this re-runs the identical ordered
#      query against the restored database and compares digests. A restore that silently
#      dropped, duplicated or mangled one value fails here.
#   2. A-W9-2 is re-measured rather than cited. A continuous aggregate accepts an INSERT and
#      discards it on the next refresh, which is the whole reason the aggregate levels are
#      not in the archive. The rehearsal pre-seeds a level, refreshes, and requires the row
#      to be gone. If it is ever still there, the premise of the exclusion has changed and
#      that is a verdict worth failing on.
#   3. W4 D-23's ordering is exercised, not described. The rebuild runs PHASE 1 ONLY -- the
#      recent-first pass -- and the four /span bands are asserted correct against raw before
#      phase 2 is considered. A restore runbook that blocks on a full rebuild is an outage
#      that did not need to be one, so this script's default is not to run phase 2 at all.
#   4. The background jobs are suspended for the duration. M0-1 found that TimescaleDB runs
#      a new policy's first job IMMEDIATELY -- measured at under a second after the migration
#      that created it -- so the three retention policies drop the chunks a rebuild has just
#      materialised, and a re-refresh does not repair it: it answers "already up-to-date"
#      because the watermark reads as refreshed while the chunk is gone. A restore into a
#      freshly migrated database is exactly that window. ci/db/rebuild-aggregates.sh raises
#      the same exposure as N-21 and does not fix it; this fixes it for the restore path
#      only, which is the path M0-2 owns.
#
# Exit codes follow the `db-tests` and `conformance` precedent (docs/reconciliation.md
# C-W2-20): 1 is a verdict about the restore path, 2 is "the run reached no verdict". A
# missing tool and a restore that loses a reading must not produce the same red.
#
# Usage:
#   ci/db/restore-rehearsal.sh                 # round-trip, no credential needed
#   ci/db/restore-rehearsal.sh --from-archive  # the real archive at MW_BACKUP_REMOTE
#   ci/db/restore-rehearsal.sh --keep          # leave the scratch databases behind
#   ci/db/restore-rehearsal.sh --full-rebuild  # also run phase 2, for timing only
#
# Environment:
#   DATABASE_URL        a server where databases may be created and dropped. Required.
#   MW_BACKUP_REMOTE    the archive. Required by --from-archive; ignored otherwise.
#   RCLONE_CONFIG       rclone's own variable, for --from-archive.
#   MW_REHEARSAL_DB_SRC / _DST   scratch database names.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MODE="round-trip"
KEEP=0
FULL_REBUILD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --from-archive) MODE="from-archive"; shift ;;
    --keep)         KEEP=1; shift ;;
    --full-rebuild) FULL_REBUILD=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

no_verdict() { printf 'NO VERDICT: %s\n' "$*" >&2; exit 2; }
verdict()    { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
step()       { printf '\n=== %s\n' "$*"; }

# Anything that fails without having gone through verdict()/no_verdict() reached no verdict,
# so it exits 2 rather than passing psql's or rclone's own status through. 1 has to keep
# meaning exactly one thing -- "the restore path is wrong" -- or the distinction the two
# codes exist for is gone (docs/reconciliation.md C-W2-20). `exit` does not trigger ERR, so
# this does not catch the two functions above.
trap 'printf "NO VERDICT: the rehearsal failed at line %s before reaching a verdict. Fix the run, then read its verdict.\n" "$LINENO" >&2; exit 2' ERR

: "${DATABASE_URL:?}" 2>/dev/null || no_verdict "set DATABASE_URL to a server where databases may be created and dropped"
for t in psql pg_dump pg_restore rclone python3 gzip; do
  command -v "$t" >/dev/null 2>&1 || no_verdict "$t is not on PATH"
done

SRC="${MW_REHEARSAL_DB_SRC:-mw_rehearsal_src}"
DST="${MW_REHEARSAL_DB_DST:-mw_rehearsal_dst}"
BASE_URL="${DATABASE_URL%/*}"
WORK="$(mktemp -d)"
cleanup() {
  if [ "$KEEP" -eq 0 ]; then
    admin -c "DROP DATABASE IF EXISTS $DST WITH (FORCE)" >/dev/null 2>&1 || true
    [ "$MODE" = "round-trip" ] && admin -c "DROP DATABASE IF EXISTS $SRC WITH (FORCE)" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

admin() { psql --set ON_ERROR_STOP=1 -X -q "$BASE_URL/postgres" "$@"; }
on()    { psql --set ON_ERROR_STOP=1 -X -q "$BASE_URL/$1" "${@:2}"; }
onv()   { psql --set ON_ERROR_STOP=1 -X -q -At "$BASE_URL/$1" "${@:2}"; }

recreate() {
  admin -c "DROP DATABASE IF EXISTS $1 WITH (FORCE)"
  admin -c "CREATE DATABASE $1"
  on "$1" -c "CREATE EXTENSION IF NOT EXISTS timescaledb"
}

# The same applier ci/db/migrate-twice.sh uses, and for the same reason: it must run before
# the daemon's toolchain exists, and a loop that re-applies every file in filename order
# would say nothing about the schema. Matches sqlx-migrate on the two properties that
# matter here -- each migration applied exactly once, in filename order, in a transaction.
LEDGER="_mw_ci_applied"
apply_migrations() {
  local db="$1" f base
  on "$db" -c "CREATE TABLE IF NOT EXISTS $LEDGER (filename text PRIMARY KEY,
                                                   applied_at timestamptz NOT NULL DEFAULT now())"
  for f in $(find migrations -maxdepth 1 -name '*.sql' | sort); do
    base="$(basename "$f")"
    [ -z "$(onv "$db" -c "SELECT 1 FROM $LEDGER WHERE filename = '$base'")" ] || continue
    on "$db" -1 -o /dev/null -f "$f"
    on "$db" -c "INSERT INTO $LEDGER (filename) VALUES ('$base')"
  done
  printf '  %s migrations applied\n' "$(onv "$db" -c "SELECT count(*) FROM $LEDGER")"
}

# ----------------------------------------------------------------------------------------
# Background jobs. Suspended for the whole restore, re-enabled before the script returns.
#
# This is the M0-1 finding, applied. A new retention policy's first job runs IMMEDIATELY,
# and the restore path's rebuild is inside that window by construction. The compression
# policy is suspended too -- not because an insert into a compressed chunk fails (2.31
# allows it) but because compressing a chunk the restore is still writing into is work the
# machine does twice.
# ----------------------------------------------------------------------------------------
suspend_jobs() {
  local n
  n="$(onv "$1" -c "
    SELECT count(*) FROM (
      SELECT alter_job(job_id, scheduled => false)
        FROM timescaledb_information.jobs
       WHERE proc_name IN ('policy_retention', 'policy_compression',
                           'policy_refresh_continuous_aggregate')) z")"
  printf '  %s background jobs suspended\n' "$n"
}
resume_jobs() {
  local n
  n="$(onv "$1" -c "
    SELECT count(*) FROM (
      SELECT alter_job(job_id, scheduled => true)
        FROM timescaledb_information.jobs
       WHERE proc_name IN ('policy_retention', 'policy_compression',
                           'policy_refresh_continuous_aggregate')) z" 2>/dev/null || echo 0)"
  printf '  %s background jobs resumed\n' "$n"
}

# ----------------------------------------------------------------------------------------
# The fixture, for round-trip mode only.
#
# Two time windows on purpose. One in the PREVIOUS calendar year, so the dump produces more
# than one year object and the per-year slicing is exercised rather than asserted; one in the
# last few hours, so every /span band has something in it and the rebuild assertions below
# are not vacuous. Both are inside the 366-day window the daily level serves.
#
# Anchored to now() rather than to a fixed date, for the reason ci/db/seed.sql gives: a fixed
# date is dropped by a retention job that happens to fire, and the flake looks like data
# loss.
# ----------------------------------------------------------------------------------------
seed() {
  local db="$1"
  on "$db" -c "
    INSERT INTO stations (station_id, name, type, timezone, latitude, longitude)
    VALUES ('reh-a', 'Rehearsal A', 'davis',    'UTC',  40.0, -105.0),
           ('reh-b', 'Rehearsal B', 'campbell', 'UTC', -33.9,  151.2);

    INSERT INTO station_metrics (station_id, metric_key)
    VALUES ('reh-a', 'outtemp'), ('reh-a', 'barometer'), ('reh-b', 'outtemp');

    -- The two caches are in the archive because excluding them is not worth the thought
    -- (7.5). Seeded so the reconciliation has something to compare rather than agreeing
    -- about two empty tables.
    INSERT INTO almanac_cache (station_id, record, value, observed_at, wind_dir)
    VALUES ('reh-a', 'high_temp', 38.5, now() - INTERVAL '3 hours', NULL),
           ('reh-a', 'low_barometer', 28.71, now() - INTERVAL '5 hours', 211.5),
           ('reh-b', 'high_wind_speed', 44.0, now() - INTERVAL '2 hours', 300.0);

    INSERT INTO snow_totals_cache (station_id, snowdepth, snowtoday, snowseason, source_through)
    VALUES ('reh-a', 120.5, 4.0, 311.25, now() - INTERVAL '1 hour');

    -- The recent window: two stations, every 30 s for the last seven hours, so the 6-hour
    -- band is fully covered and the minute level has a closed bucket for every minute of
    -- it. 30 s rather than 2 s keeps the fixture small; nothing here measures throughput.
    INSERT INTO observations (time, station_id, outtemp, barometer, outhumidity, windspeed, winddir)
    SELECT t, s.id,
           20 + 5 * sin(extract(epoch from t) / 900.0),
           29.9 + 0.1 * cos(extract(epoch from t) / 1800.0),
           50 + 10 * sin(extract(epoch from t) / 1200.0),
           3 + 2 * cos(extract(epoch from t) / 600.0),
           (extract(epoch from t)::int / 60) % 360
      FROM generate_series(time_bucket(INTERVAL '1 minute', now()) - INTERVAL '7 hours',
                           time_bucket(INTERVAL '1 minute', now()) - INTERVAL '30 seconds',
                           INTERVAL '30 seconds') t,
           (VALUES ('reh-a'), ('reh-b')) AS s(id);

    -- The historical window: one day in the previous calendar year. This is what makes the
    -- dump produce two year objects, and it is also the only part of the fixture a
    -- retention policy would reach -- which is what suspend_jobs is for.
    INSERT INTO observations (time, station_id, outtemp, barometer)
    SELECT t, 'reh-a',
           10 + 3 * sin(extract(epoch from t) / 3600.0),
           30.1
      FROM generate_series(
             date_trunc('year', now()) - INTERVAL '40 days',
             date_trunc('year', now()) - INTERVAL '39 days 1 minute',
             INTERVAL '5 minutes') t;

    INSERT INTO forecasts (station_id, span_hours, data)
    VALUES ('reh-a', 12, '[{\"t\": 1, \"temp\": 20}]'::jsonb),
           ('reh-b', 72, '[]'::jsonb);
  " >/dev/null
  printf '  %s raw rows across %s calendar years\n' \
         "$(onv "$db" -c 'SELECT count(*) FROM observations')" \
         "$(onv "$db" -c 'SELECT count(DISTINCT extract(year from time)) FROM observations')"
}

# ----------------------------------------------------------------------------------------
# Reconciliation. The manifest records the sha256 of the ordered COPY stream each object was
# produced from; this re-runs the identical query against the restored database.
#
# Byte equality of a stream, not a row count. A row count cannot see a value that came back
# as 0 instead of NULL, which is the write-path defect `db-tests` exists for and exactly the
# kind of thing a restore is capable of doing on its own.
# ----------------------------------------------------------------------------------------
reconcile() {
  local man="$1" db="$2" bad=0 y cols want got rows dbrows order
  step "reconciling the restored database against the archive's manifest"
  while IFS='|' read -r y cols want rows; do
    [ -n "$y" ] || continue
    got="$(onv "$db" -c "COPY (SELECT $cols FROM observations
                                WHERE time >= '$y-01-01 00:00:00+00'::timestamptz
                                  AND time <  '$((y + 1))-01-01 00:00:00+00'::timestamptz
                                ORDER BY station_id, time) TO STDOUT" | sha256sum | cut -d' ' -f1)"
    dbrows="$(onv "$db" -c "SELECT count(*) FROM observations
                             WHERE time >= '$y-01-01 00:00:00+00'::timestamptz
                               AND time <  '$((y + 1))-01-01 00:00:00+00'::timestamptz")"
    if [ "$got" = "$want" ] && [ "$dbrows" = "$rows" ]; then
      printf '  raw %s   %s rows   sha256 %s   match\n' "$y" "$dbrows" "${got:0:16}"
    else
      printf '  raw %s   %s rows (archive says %s)   sha256 %s (archive says %s)   MISMATCH\n' \
             "$y" "$dbrows" "$rows" "${got:0:16}" "${want:0:16}" >&2
      bad=1
    fi
  done < <(python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
for y, e in sorted(m["objects"]["raw"].items()):
    print("|".join([y, ",".join(e["columns"]), e["sha256"] or "", str(e["rows"])]))' "$man")

  local t
  while IFS='|' read -r t cols want rows; do
    [ -n "$t" ] || continue
    # The SAME total order the dump used -- every column, not the first one. See
    # `order_by_all` in ci/db/offsite-backup.sh: `ORDER BY 1` is not a total order on any of
    # the tables with a composite primary key, and a digest comparison over a
    # non-deterministic order fails on data that restored perfectly.
    order="$(printf '%s' "$cols" | awk -F, '{ for (i = 1; i <= NF; i++) printf "%s%d", (i > 1 ? "," : ""), i }')"
    got="$(onv "$db" -c "COPY (SELECT $cols FROM public.$t ORDER BY $order) TO STDOUT" \
           | sha256sum | cut -d' ' -f1)"
    dbrows="$(onv "$db" -c "SELECT count(*) FROM public.$t")"
    if [ "$got" = "$want" ] && [ "$dbrows" = "$rows" ]; then
      printf '  %-22s %s rows   sha256 %s   match\n' "$t" "$dbrows" "${got:0:16}"
    else
      printf '  %-22s %s rows (archive says %s)   sha256 %s (archive says %s)   MISMATCH\n' \
             "$t" "$dbrows" "$rows" "${got:0:16}" "${want:0:16}" >&2
      bad=1
    fi
  done < <(python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
for t, e in sorted(m["objects"]["tables"].items()):
    print("|".join([t, ",".join(e["columns"]), e["sha256"] or "", str(e["rows"])]))' "$man")

  # The pg_dump-mode tables carry their own schema, so there is no reproducible stream to
  # compare -- a pg_dump container is not byte-stable. Row count is what is available, and
  # it is said plainly rather than presented as the same check.
  while IFS='|' read -r t rows; do
    [ -n "$t" ] || continue
    dbrows="$(onv "$db" -c "SELECT count(*) FROM public.$t" 2>/dev/null || echo missing)"
    if [ "$dbrows" = "$rows" ]; then
      printf '  %-22s %s rows   row count only (pg_dump archives are not byte-stable)\n' "$t" "$dbrows"
    else
      printf '  %-22s %s rows (archive says %s)   MISMATCH\n' "$t" "$dbrows" "$rows" >&2
      bad=1
    fi
  done < <(python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
for t, e in sorted(m["objects"]["pgdump"].items()):
    print("|".join([t, str(e["rows"])]))' "$man")

  local absent
  absent="$(python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
print(" ".join(m["absent"]) or "-")' "$man")"
  printf '  declared and absent from the source database: %s\n' "$absent"
  printf '  deliberately not backed up: %s\n' \
         "$(python3 -c '
import json, sys
print(" ".join(sorted(json.load(open(sys.argv[1]))["not_backed_up"])))' "$man")"
  [ "$bad" -eq 0 ] || verdict "the restored database does not match the archive. The lines marked
      MISMATCH above name every object that differs and in which direction."
}

# ----------------------------------------------------------------------------------------
# A-W9-2, re-measured. The exclusion of the aggregate levels from the archive rests on this
# being true, so it is checked rather than cited.
# ----------------------------------------------------------------------------------------
assert_aggregates_reject_a_preseed() {
  local db="$1" before after bucket
  step "A-W9-2: an aggregate accepts an INSERT and discards it on the next refresh"
  bucket="$(onv "$db" -c "SELECT to_char(time_bucket(INTERVAL '1 minute', now()) - INTERVAL '400 days',
                                         'YYYY-MM-DD HH24:MI:SS+00')")"
  # A range with no raw behind it, so a refresh has nothing legitimate to materialise and
  # the only row that could survive is the pre-seeded one.
  on "$db" -c "INSERT INTO observations_1m (bucket, station_id, outtemp_sum, outtemp_count)
               VALUES ('$bucket'::timestamptz, 'a-w9-2-probe', 1234.5, 1)" >/dev/null \
    || no_verdict "the aggregate refused the INSERT outright. A-W9-2 measured it ACCEPTING
      one, so this is a different behaviour from the one the exclusion is argued from and
      the rehearsal cannot reach a verdict on it. Re-measure before trusting either."
  before="$(onv "$db" -c "SELECT count(*) FROM observations_1m WHERE station_id = 'a-w9-2-probe'")"
  on "$db" -c "CALL refresh_continuous_aggregate('observations_1m',
                 '$bucket'::timestamptz - INTERVAL '1 day',
                 '$bucket'::timestamptz + INTERVAL '1 day')" >/dev/null
  after="$(onv "$db" -c "SELECT count(*) FROM observations_1m WHERE station_id = 'a-w9-2-probe'")"
  printf '  the INSERT was accepted and read back: %s row(s)\n' "$before"
  printf '  after a refresh over the same range:   %s row(s)\n' "$after"
  [ "$before" = "1" ] || verdict "the pre-seeded aggregate row did not read back at all ($before rows),
      so this run did not reproduce A-W9-2's first half."
  [ "$after" = "0" ] || verdict "the pre-seeded aggregate row SURVIVED a refresh ($after rows).
      A-W9-2 is the reason the four aggregate levels are not in the archive, and if a
      pre-seeded aggregate row now survives then that reasoning has changed and the backup's
      contents are worth revisiting. This is a deliberate failure, not a flake."
  printf '  gone, as A-W9-2 measured. The restore path rebuilds the levels; it never loads them.\n'
}

# ----------------------------------------------------------------------------------------
# W4 D-23. Phase 1 only, then the four bands asserted against raw.
#
# LEVELS/BANDS/GRAINS are the same three facts ci/db/rebuild-aggregates.sh carries and have
# to be kept in step with it; the bands are what /span can ever read each level over
# (docs/legacy-api-contract.md 2.3) and the grains are the bucket widths the migrations
# declare. Both ends of each window are bucket-aligned, so the buckets inside it hold
# exactly the raw rows inside it and the comparison is equality rather than a tolerance.
# ----------------------------------------------------------------------------------------
LEVELS=(observations_1m observations_5m observations_1h observations_1d)
BANDS=('6 hours' '48 hours' '14 days' '366 days')
GRAINS=('1 minute' '5 minutes' '1 hour' '1 day')

# The time the bands are measured from. `/span` takes no time anchor -- every band is
# [now - span, now] -- so round-trip mode, whose fixture is written up to now(), anchors on
# now() and asserts the real thing.
#
# --from-archive cannot. A weekly archive of a live database has readings up to the moment
# it was taken, but an archive is not guaranteed to be fresh and this script must not report
# "the charts are wrong" when what it means is "this archive is a week old". So that mode
# anchors on the newest reading the archive actually carries, which asks the same question
# of the same data: are the four bands above that instant correct. It is a weaker claim and
# the run says which one it made.
anchor_sql() { if [ "$MODE" = "round-trip" ]; then printf 'now()'; else printf '(SELECT max(time) FROM observations)'; fi; }

phase_1_and_assert_the_bands() {
  local db="$1" i level band grain t0 t1 raw_n agg_n raw_s agg_s rel bad=0 asserted=0 anchor
  anchor="$(anchor_sql)"
  step "W4 D-23: phase 1 only -- the recent-first rebuild, not a full materialisation"
  t0="$(date -u +%s)"
  DATABASE_URL="$BASE_URL/$db" ci/db/rebuild-aggregates.sh --recent-only 2>&1 | sed 's/^/  /'
  t1="$(date -u +%s)"
  local phase1=$((t1 - t0))
  printf '  phase 1 took %ss\n' "$phase1"

  step "the four /span bands, against raw"
  printf '  measured from %s\n' "$(onv "$db" -c "SELECT ($anchor)::text")"
  # Phase 1 refreshes from now() - 366 days, so an archive whose newest reading is older
  # than that has nothing phase 1 could have materialised and nothing /span could read. That
  # is a fact about the archive, not a defect in the restore path, so it is said and the
  # band assertions are skipped rather than failed.
  if [ "$(onv "$db" -c "SELECT (($anchor) < now() - INTERVAL '366 days')::int")" = "1" ]; then
    printf '  SKIPPED: the newest reading in this archive is older than the 366 days phase 1\n'
    printf '           refreshes, so no /span band can read it. The restore is still\n'
    printf '           reconciled above; what is not asserted is the rebuild.\n'
    return 0
  fi
  # The COUNT is compared exactly, because an integer count of readings is the thing a
  # restore can break and there is no honest tolerance for "a reading went missing".
  #
  # The SUM is compared to a relative tolerance of 1e-4, and the reason is float4 rather
  # than slack. `observations.outtemp` is `real`, so `sum(outtemp)` accumulates in single
  # precision over a thousand-odd values while the aggregate's `outtemp_sum` was
  # accumulated in double from the same inputs -- the level is the MORE accurate of the
  # two, and the gap is ~2e-6 relative, which is float4's ~7 significant digits spread
  # over the row count. D-12's "bit-identical to a direct scan" is about the roll-up
  # between LEVELS, each of which carries sum and count in the same type; it does not
  # claim a float8 roll-up equals a float4 scan, and asserting that it does would be
  # asserting something false.
  for i in "${!LEVELS[@]}"; do
    level="${LEVELS[$i]}"; band="${BANDS[$i]}"; grain="${GRAINS[$i]}"
    read -r raw_n agg_n raw_s agg_s rel <<<"$(onv "$db" -c "
      WITH w AS (SELECT time_bucket(INTERVAL '$grain', ($anchor) - INTERVAL '$band') AS lo,
                        time_bucket(INTERVAL '$grain', ($anchor))                    AS hi),
           r AS (SELECT count(outtemp) AS n, coalesce(sum(outtemp), 0)::numeric AS s
                   FROM observations, w WHERE time >= w.lo AND time < w.hi),
           a AS (SELECT coalesce(sum(outtemp_count), 0) AS n,
                        coalesce(sum(outtemp_sum), 0)::numeric AS s
                   FROM $level, w WHERE bucket >= w.lo AND bucket < w.hi)
      SELECT r.n || ' ' || a.n || ' ' || round(r.s, 3) || ' ' || round(a.s, 3) || ' ' ||
             to_char(CASE WHEN greatest(abs(r.s), 1) = 0 THEN 0
                          ELSE abs(r.s - a.s) / greatest(abs(r.s), 1) END, 'FM0.0999999999')
        FROM r, a")"
    if [ "$raw_n" = "0" ]; then
      # No raw rows in the band, so there is nothing for the level to be right or wrong
      # about. What that MEANS depends on the mode, and conflating the two is how a check
      # ends up either crying wolf or passing vacuously.
      #
      #   round-trip     the fixture is this script's own and puts readings in all four
      #                  bands on purpose. An empty band is a defect here or in the
      #                  fixture, so it fails.
      #   --from-archive the archive's coverage is a property of the archive. A station
      #                  that was offline, or a thin archive, legitimately has empty
      #                  bands -- so the band is NOT ASSERTED and the run says so. The
      #                  guard against vacuous green is below: if NO band could be
      #                  asserted, the run has proved nothing and fails.
      if [ "$MODE" = "round-trip" ]; then
        printf '  %-16s %-9s VACUOUS: no raw readings in the band, and the fixture puts some in every band\n' "$level" "$band" >&2
        bad=1
      else
        printf '  %-16s %-9s not asserted: the archive has no readings in this band\n' "$level" "$band"
      fi
    elif [ "$raw_n" = "$agg_n" ] && python3 -c "import sys; sys.exit(0 if float('$rel') <= 1e-4 else 1)"; then
      printf '  %-16s %-9s %s readings, sum %s   match (float4 gap %s)\n' \
             "$level" "$band" "$raw_n" "$raw_s" "$rel"
      asserted=$((asserted + 1))
    else
      printf '  %-16s %-9s raw %s/%s vs level %s/%s   relative gap %s   MISMATCH\n' \
             "$level" "$band" "$raw_n" "$raw_s" "$agg_n" "$agg_s" "$rel" >&2
      bad=1
    fi
  done
  [ "$bad" -eq 0 ] || verdict "phase 1 did not make every /span band correct. W4 D-23's whole claim
      is that the recent-first pass is what the charts need and the full rebuild is not, so a
      band that is wrong after phase 1 breaks the restore runbook rather than just this gate."
  # The guard against a green that proved nothing. Four bands all reporting "not asserted"
  # would otherwise read as success.
  [ "$asserted" -gt 0 ] || verdict "not one of the four /span bands could be asserted -- every band
      the archive covers is empty. Whatever this run restored, it has said nothing about
      whether phase 1 makes the charts right, and reporting that as a pass is the one thing
      this gate must never do."
  printf '  %s of %s bands asserted against raw\n' "$asserted" "${#LEVELS[@]}"
  printf '\n  Phase 2 -- unbounded history -- was NOT run, and that is the point: at ten years\n'
  printf '  it is an hour-scale operation (A-W4-3, 0.87s against 467s) and the charts are\n'
  printf '  already correct without it. A restore runbook that blocks on it is an outage\n'
  printf '  that did not need to be one.\n'
  if [ "$FULL_REBUILD" -eq 1 ]; then
    step "phase 2, for timing only (--full-rebuild)"
    t0="$(date -u +%s)"
    DATABASE_URL="$BASE_URL/$db" ci/db/rebuild-aggregates.sh 2>&1 | sed 's/^/  /'
    t1="$(date -u +%s)"
    printf '  phase 1 %ss, phase 1 + phase 2 %ss\n' "$phase1" "$((t1 - t0))"
  fi
}

# ----------------------------------------------------------------------------------------
MANIFEST="$WORK/manifest.json"

printf 'restore rehearsal, mode: %s\n' "$MODE"

if [ "$MODE" = "round-trip" ]; then
  if [ -n "${MW_REHEARSAL_REMOTE:-}" ]; then
    ARCHIVE="$MW_REHEARSAL_REMOTE"
    printf 'archive: %s (MW_REHEARSAL_REMOTE)\n' "$ARCHIVE"
  else
    ARCHIVE="$WORK/archive/modernweather"
    mkdir -p "$ARCHIVE"
    printf 'archive: %s -- a LOCAL rclone remote. This run did NOT talk to R2.\n' "$ARCHIVE"
  fi

  step "source database $SRC at HEAD"
  recreate "$SRC"
  apply_migrations "$SRC"
  suspend_jobs "$SRC"
  step "fixture"
  seed "$SRC"

  step "dump"
  # A real configuration database would be the daemon's SQLite file. There is no daemon
  # yet, so the rehearsal makes a file of its own: the dump copies it verbatim and the
  # restore has to reproduce it byte for byte, which is the whole of what the config.db
  # half of the archive has to do. It is not a SQLite assertion and does not pretend to be.
  head -c 65536 /dev/urandom >"$WORK/config.db"
  DATABASE_URL="$BASE_URL/$SRC" MW_BACKUP_REMOTE="$ARCHIVE" MW_CONFIG_DB="$WORK/config.db" \
    ci/db/offsite-backup.sh dump 2>&1 | sed 's/^/  /'

  step "destination database $DST at HEAD, empty"
  recreate "$DST"
  apply_migrations "$DST"
  suspend_jobs "$DST"

  step "restore"
  DATABASE_URL="$BASE_URL/$DST" MW_BACKUP_REMOTE="$ARCHIVE" \
    MW_RESTORE_CONFIG_DB="$WORK/config.restored.db" \
    ci/db/offsite-backup.sh restore --into "$DST" 2>&1 | sed 's/^/  /'

  MW_BACKUP_REMOTE="$ARCHIVE" ci/db/offsite-backup.sh manifest >"$MANIFEST"

  step "the configuration database, byte for byte"
  if cmp -s "$WORK/config.db" "$WORK/config.restored.db"; then
    printf '  %s bytes, identical\n' "$(wc -c <"$WORK/config.db")"
  else
    verdict "the restored configuration database differs from the one that was dumped. It holds
      station calibration, certificates and website configuration (7.5), none of which a
      human can re-derive."
  fi
else
  : "${MW_BACKUP_REMOTE:?}" 2>/dev/null || no_verdict "--from-archive needs MW_BACKUP_REMOTE"
  printf 'archive: %s -- the real one\n' "$MW_BACKUP_REMOTE"
  ci/db/offsite-backup.sh manifest >"$MANIFEST" 2>/dev/null \
    || no_verdict "no manifest at $MW_BACKUP_REMOTE. Nothing has been dumped there, or the
      credential cannot read it."
  printf 'archive run %s, layout version %s\n' \
    "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["run"])' "$MANIFEST")" \
    "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["layout_version"])' "$MANIFEST")"

  step "destination database $DST at HEAD, empty"
  recreate "$DST"
  apply_migrations "$DST"
  suspend_jobs "$DST"

  step "restore"
  DATABASE_URL="$BASE_URL/$DST" ci/db/offsite-backup.sh restore --into "$DST" 2>&1 | sed 's/^/  /'
fi

reconcile "$MANIFEST" "$DST"

# An archive with no raw readings in it reconciles trivially and has nothing to rebuild, so
# everything below would be vacuous. That is a fact about the archive -- and it is the state
# the first archive is genuinely in, taken against a database that has the schema at HEAD and
# zero rows -- so it is NO VERDICT with the reason, not a red that looks like data loss.
if [ "$(onv "$DST" -c 'SELECT count(*) FROM observations')" = "0" ]; then
  step "verdict"
  no_verdict "this archive carries no raw readings. The manifest reconciled, so the layout and
      the transport are sound, but there is nothing to rebuild and nothing for a /span band to
      read -- so this run proves nothing about the restore path. Take a dump against a database
      with readings in it, then rehearse against that."
fi

assert_aggregates_reject_a_preseed "$DST"
phase_1_and_assert_the_bands "$DST"

step "background jobs"
resume_jobs "$DST"

step "verdict"
printf 'the archive restored, reconciled byte for byte, and phase 1 made every /span band\n'
printf 'correct. Mode: %s.\n' "$MODE"
if [ "$MODE" = "round-trip" ]; then
  printf 'This run used a local rclone remote and proves the layout, the per-year slicing and\n'
  printf 'the restore path. It does NOT prove R2 -- `--from-archive` is the mode that does.\n'
fi
