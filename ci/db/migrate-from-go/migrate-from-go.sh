#!/usr/bin/env bash
# The Go-to-Rust data migration, as one ordered procedure. docs/data-migration.md is the
# reasoning; this is the thing you run.
#
# It is an operational tool and no CI gate calls it. It is in ci/db/ because that is where
# the database scripts live, and it is committed because a migration procedure that exists
# only in a document has not been executed and a migration procedure that exists only in
# somebody's shell history cannot be reviewed.
#
# THE SOURCE IS READ-ONLY. Every statement this script sends to $SOURCE is a SELECT or a
# COPY ... TO STDOUT. It never creates, never drops and never updates anything there. The
# one exception is `archive`, which is the step that has to create two tables in the source
# — and it is a separate, explicitly-named step for exactly that reason. See
# docs/data-migration.md Decision 3.
#
# ORDER
#
#   preflight   report. Versions, retention-policy state, extents, the column gap. Changes
#               nothing anywhere. Run it first, read it, then decide.
#   archive     the only step that CREATES anything in the source: CREATE TABLE
#               weather_1d_archive / weather_1h_archive AS SELECT * FROM the view. Cheap,
#               reversible, and the thing that makes the history decision reversible too.
#   pause-source-retention
#               disable the raw retention policy on the source, so it stops converting a
#               further day of raw into rollup-only history every day. The second of the
#               two statements that touch the source, and it REFUSES TO RUN until
#               `archive` has succeeded — pausing protects the future, the archive
#               protects what is already at risk, and doing them in the wrong order
#               leaves a window in which neither has happened. Reversible:
#               alter_job(..., scheduled => true).
#   stage       create the w9 schema on the target and load both inventories.
#   stations    populate `stations` from w9.station_map. Nothing can be imported before it.
#   raw         stream the raw hypertable, slice by slice, resumably.
#   aggregates  create the four levels and materialise them bottom-up, recent-first.
#   rollup      import the archived daily/hourly buckets into observations_*_legacy.
#   reconcile   both digests, the rollup comparison and the spot checks.
#   policies    refresh policies, compression and retention — last, deliberately.
#
# Each step is idempotent and each can be run alone. `all` runs stage..reconcile.
#
#   SOURCE=postgresql://.../weather_v2_0_0  TARGET=postgresql://.../modernweather \
#     ci/db/migrate-from-go/migrate-from-go.sh preflight
#
# PSQL_SOURCE / PSQL_TARGET override the psql invocation entirely, which is what the lab
# harness uses to reach a database inside a Kubernetes pod.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
GEN="$ROOT/ci/db/migrate-from-go/generated"
HERE="$ROOT/ci/db/migrate-from-go"

: "${SOURCE:=}"; : "${TARGET:=}"
PSQL_SOURCE="${PSQL_SOURCE:-psql -X -q -v ON_ERROR_STOP=1 $SOURCE}"
PSQL_TARGET="${PSQL_TARGET:-psql -X -q -v ON_ERROR_STOP=1 $TARGET}"
SLICE="${SLICE:-7 days}"

src()  { $PSQL_SOURCE "$@"; }
tgt()  { $PSQL_TARGET "$@"; }
# `-f -` and a redirect, never `-f <path>`: PSQL_SOURCE/PSQL_TARGET may be a `kubectl exec`
# into a pod that cannot see this filesystem, and a path would silently be the pod's.
srcf() { $PSQL_SOURCE -f - < "$1"; }
tgtf() { $PSQL_TARGET -f - < "$1"; }
tgtf1() { $PSQL_TARGET -1 -f - < "$1"; }
srcq() { $PSQL_SOURCE -tA -c "$1"; }
tgtq() { $PSQL_TARGET -tA -c "$1"; }

say() { printf '\n=== %s\n' "$*"; }

step_preflight() {
  say "preflight — reports only, writes nothing"
  echo "--- the old database"
  srcf "$HERE/00-preflight-source.sql"
  echo
  echo "--- the column gap (OQ-6) needs the target staged; run \`stage\` then \`gap\`"
}

step_archive() {
  say "archive — THE ONE STEP THAT WRITES TO THE SOURCE"
  # pg_dump -t weather_1d captures a *view definition*: the rows live in
  # _timescaledb_internal._materialized_hypertable_N. Copying through the view is
  # therefore required, and it is also correct, because the old views are declared
  # materialized_only = false and so union materialised and real-time buckets.
  for lvl in 1d 1h; do
    if [ "$(srcq "SELECT count(*) FROM pg_class WHERE relname = 'weather_${lvl}_archive'")" = "0" ]; then
      src -c "CREATE TABLE weather_${lvl}_archive AS SELECT * FROM weather_${lvl}"
      echo "  created weather_${lvl}_archive"
    else
      echo "  weather_${lvl}_archive already exists, leaving it alone"
    fi
    src -c "SELECT 'weather_${lvl}_archive' AS t, count(*) AS rows,
                   min(bucket) AS lo, max(bucket) AS hi,
                   pg_size_pretty(pg_total_relation_size('weather_${lvl}_archive')) AS size
              FROM weather_${lvl}_archive"
  done
  # Verify the copy against the view it came from, before anything depends on it.
  for lvl in 1d 1h; do
    local a v
    a="$(srcq "SELECT count(*) FROM weather_${lvl}_archive")"
    v="$(srcq "SELECT count(*) FROM weather_${lvl}")"
    if [ "$a" != "$v" ]; then
      echo "  WARNING: weather_${lvl}_archive has $a rows, weather_${lvl} now returns $v." >&2
      echo "           A real-time tail bucket closing between the two counts explains a" >&2
      echo "           small difference; anything larger does not." >&2
    else
      echo "  weather_${lvl}_archive verified: $a rows, equal to the view"
    fi
  done
}

# The second statement that touches the source, and the only one that changes behaviour
# rather than adding a table. DESIGN.md A-W9-1 is why the ordering is enforced rather than
# documented: `drop_chunks` logs an invalidation, and a later refresh over that range
# deletes the buckets whose raw has gone — measured at 41 of 83 daily buckets in one policy
# run, with no error. So pausing retention stops the *future* loss and the archive is what
# protects the history that is already exposed. A run that paused first and then failed to
# archive would look like success and be the dangerous half.
step_pause_source_retention() {
  say "pause-source-retention — THE ONE STEP THAT CHANGES THE SOURCE'S BEHAVIOUR"

  # --- interlock: the archive must already exist and be non-empty.
  local missing=0
  for lvl in 1d 1h; do
    local n
    n="$(srcq "SELECT coalesce((SELECT count(*) FROM pg_class
                                 WHERE relname = 'weather_${lvl}_archive'
                                   AND relkind = 'r'), 0)")"
    if [ "$n" = "0" ]; then
      echo "  MISSING: weather_${lvl}_archive" >&2; missing=1; continue
    fi
    n="$(srcq "SELECT count(*) FROM weather_${lvl}_archive")"
    if [ "$n" = "0" ]; then
      echo "  EMPTY: weather_${lvl}_archive exists but has 0 rows" >&2; missing=1; continue
    fi
    echo "  ok: weather_${lvl}_archive present, $n rows"
  done
  if [ "$missing" != "0" ]; then
    cat >&2 <<'EOM'

  REFUSING TO PAUSE. Run `archive` first and let it verify.

  Pausing retention protects the days you have not lost yet. The archive protects the
  ~294 days that are already rollup-only and therefore already exposed to the mechanism
  in DESIGN.md A-W9-1. In the wrong order there is a window where neither is true, and
  it looks like progress.
EOM
    return 1
  fi

  # --- report every policy job before touching one. The refresh policies in particular:
  #     an ACTIVE refresh policy on weather_1d is the thing that destroys rollup history,
  #     but disabling it would also stop the live site's aggregates from advancing. That
  #     is the owner's call, not this script's, so it is reported and left alone.
  say "every policy job on the source, before any change"
  src -c "SELECT j.job_id, j.proc_name, j.hypertable_name, j.scheduled,
                 j.config ->> 'drop_after' AS drop_after, j.next_start,
                 s.last_run_status
            FROM timescaledb_information.jobs j
            LEFT JOIN timescaledb_information.job_stats s USING (job_id)
           WHERE j.proc_name LIKE 'policy_%'
           ORDER BY j.proc_name, j.job_id"

  # --- disable retention on the RAW hypertable only.
  local jobs
  jobs="$(srcq "SELECT job_id FROM timescaledb_information.jobs
                 WHERE proc_name = 'policy_retention'
                   AND hypertable_name = 'weather'
                   AND scheduled
                 ORDER BY job_id")"
  if [ -z "$jobs" ]; then
    echo "  no ENABLED retention policy on \`weather\` — nothing to pause (already done, or never existed)"
  else
    for id in $jobs; do
      src -c "SELECT alter_job($id, scheduled => false)"
      echo "  paused retention job $id on \`weather\`"
    done
  fi

  # --- verify, and say plainly what was NOT touched.
  say "after"
  src -c "SELECT job_id, proc_name, hypertable_name, scheduled,
                 config ->> 'drop_after' AS drop_after, next_start
            FROM timescaledb_information.jobs
           WHERE proc_name = 'policy_retention' ORDER BY job_id"
  cat <<'EOM'
  Retention on `weather` is paused. Reverse with: SELECT alter_job(<id>, scheduled => true);

  NOT touched, deliberately:
    - retention on any aggregate (weather_1m / weather_5m). Those are derived from raw,
      and raw is now being kept.
    - every refresh policy. An active refresh over the rollup-only window is what
      DESIGN.md A-W9-1 describes, but disabling it would stop the live site's aggregates
      from advancing. Read the `scheduled` column above and decide; the archive is
      already taken either way.
    - nothing was dropped, updated or deleted. This step issues one alter_job per
      enabled retention job on `weather`, and nothing else.
EOM
}

step_stage() {
  say "stage — the w9 schema and both inventories, on the target"
  for f in 00-staging 10-import-raw 20-legacy-rollup 30-reconcile; do
    echo "  $f.sql"
    tgtf1 "$GEN/$f.sql"
  done
  echo "  loading the source's column inventory"
  src -c "$(tgtq "SELECT w9.source_inventory_query()")" \
    | tgt -c "COPY w9.source_column FROM STDIN (FORMAT csv)"
  echo "  loading the source's station inventory"
  src -c "$(tgtq "SELECT w9.source_station_query()")" \
    | tgt -c "COPY w9.source_station FROM STDIN (FORMAT csv)"
  step_gap
}

step_gap() {
  say "gap — the column inventory, old against the registry (W0 OQ-6)"
  tgt -c "SELECT verdict, count(*) FROM w9.gap_report GROUP BY 1 ORDER BY 1"
  tgt -c "SELECT column_name, expected_type, source_type, verdict
            FROM w9.gap_report WHERE verdict <> 'ok' ORDER BY verdict, column_name"
  tgt -c "SELECT w9.assert_no_unknown_source_columns()"
  echo "  no source column lacks a destination"
}

step_stations() {
  say "stations — the dimension table, before any reading"
  # The frozen-contract-preserving default: station_id = stationname, so /span?station=
  # keeps addressing the history it addressed before. Override by inserting into
  # w9.station_map yourself before running this step.
  tgt -c "INSERT INTO w9.station_map (stationname, station_id, stationtype)
          SELECT stationname, stationname, stationtype FROM w9.source_station
          ON CONFLICT (stationname) DO NOTHING"
  tgt -c "CALL w9.populate_stations()"
  tgt -c "SELECT station_id, name, type, first_seen, last_seen FROM stations ORDER BY station_id"
}

step_raw() {
  say "raw — streaming the hypertable, slice by slice"
  local lo hi n
  lo="$(srcq "SELECT min(time) FROM weather")"
  hi="$(srcq "SELECT max(time) + INTERVAL '1 microsecond' FROM weather")"
  echo "  source extent: $lo .. $hi"
  tgt -c "CALL w9.plan_import('$lo'::timestamptz, '$hi'::timestamptz, INTERVAL '$SLICE')"
  local copy_in
  copy_in="$(tgtq "SELECT w9.copy_in_sql()")"
  # Two shell details that are not style. IFS='|' rather than `tr '|' ' '`, because a
  # timestamptz contains a space and whitespace splitting cuts it in half. And the slice
  # list arrives on fd 3, not stdin, because the loop body pipes into a psql that reads
  # stdin — on stdin the first iteration would swallow the rest of the list.
  while IFS='|' read -r t0 t1 <&3; do
    [ -n "$t0" ] || continue
    printf '  %s .. %s ' "$t0" "$t1"
    # Clears debris from an interrupted attempt at this slice; normally a no-op.
    tgt -c "CALL w9.reset_slice('$t0'::timestamptz, '$t1'::timestamptz)" > /dev/null
    src -c "$(tgtq "SELECT w9.copy_out_sql('$t0'::timestamptz, '$t1'::timestamptz)")" \
      | tgt -c "$copy_in" > /dev/null
    n="$(tgtq "SELECT count(*) FROM observations WHERE time >= '$t0' AND time < '$t1'")"
    tgt -c "UPDATE w9.import_slice SET rows = $n, done_at = now() WHERE t0 = '$t0'" > /dev/null
    printf -- '-> %s rows\n' "$n"
  done 3< <(tgtq "SELECT t0, t1 FROM w9.import_slice WHERE done_at IS NULL ORDER BY t0")
  tgt -c "SELECT count(*) AS slices, sum(rows) AS rows FROM w9.import_slice WHERE done_at IS NOT NULL"
  say "station_metrics — one pass over the imported history"
  tgt -c "CALL w9.populate_station_metrics()"
  tgt -c "SELECT count(*) AS station_metric_pairs FROM station_metrics"
}

step_aggregates() {
  say "aggregates — bottom-up, recent ranges first (W4 section 4)"
  # W4 owns the rebuild procedure and this does not reimplement it. PSQL carries the
  # target connection through so the same script runs against a pod.
  PSQL="${PSQL_TARGET_RAW:-$PSQL_TARGET}" DATABASE_URL="${TARGET:-}" \
    "$ROOT/ci/db/rebuild-aggregates.sh" "$@"
}

step_rollup() {
  say "rollup — the archived buckets with no raw behind them"
  for lvl in 1d 1h; do
    echo "  streaming weather_${lvl}_archive"
    src -c "COPY (SELECT * FROM weather_${lvl}_archive) TO STDOUT (FORMAT csv)" \
      | tgt -c "COPY w9.weather_${lvl}_archive FROM STDIN (FORMAT csv)"
  done
  tgt -c "CALL w9.set_legacy_boundary()"
  tgt -c "SELECT * FROM w9.legacy_boundary ORDER BY station_id"
  for lvl in 1d 1h; do
    tgt -c "CALL w9.import_legacy_${lvl}()"
    tgt -c "SELECT w9.assert_legacy_disjoint('${lvl}')"
    tgt -c "SELECT 'observations_${lvl}_legacy' AS t, count(*) AS rows, min(bucket) AS lo, max(bucket) AS hi,
                   pg_size_pretty(pg_total_relation_size('observations_${lvl}_legacy')) AS size
              FROM observations_${lvl}_legacy"
  done
}

step_reconcile() {
  say "reconcile — the old database stays read-only until this passes"
  local lo hi
  lo="$(srcq "SELECT min(time) FROM weather")"
  hi="$(srcq "SELECT max(time) + INTERVAL '1 microsecond' FROM weather")"
  tgt -c "TRUNCATE w9.recon" > /dev/null
  echo "  old-side digest"
  src -c "COPY ($(tgtq "SELECT w9.digest_body('old', '$lo'::timestamptz, '$hi'::timestamptz)")) TO STDOUT (FORMAT csv)" \
    | tgt -c "COPY w9.recon (side, station_id, yr, metric, rows, non_null, total, lo, hi) FROM STDIN (FORMAT csv)"
  # The old side groups by stationname; map it to station_id so the two sides join.
  tgt -c "UPDATE w9.recon r SET station_id = m.station_id
            FROM w9.station_map m
           WHERE r.side = 'old' AND r.station_id = m.stationname" > /dev/null
  echo "  new-side digest"
  tgt -c "INSERT INTO w9.recon (side, station_id, yr, metric, rows, non_null, total, lo, hi)
          $(tgtq "SELECT w9.digest_body('new', '$lo'::timestamptz, '$hi'::timestamptz)")" > /dev/null
  tgt -c "SELECT verdict, count(*) FROM w9.recon_report GROUP BY 1 ORDER BY 1"
  tgt -c "SELECT * FROM w9.recon_report WHERE verdict <> 'ok' ORDER BY station_id, yr, metric LIMIT 40"
  tgt -c "SELECT w9.assert_reconciled()"
  echo "  digests agree"
  step_spot "$lo" "$hi"
  step_rollup_check
}

step_spot() {
  local lo="${1:?}" hi="${2:?}"
  say "spot checks — extremes and the timestamps that produced them"
  tgt -c "TRUNCATE w9.extreme" > /dev/null
  while IFS='|' read -r col dir <&3; do
    [ -n "$col" ] || continue
    # Skip a metric production does not have; absent is not a mismatch.
    [ "$(tgtq "SELECT count(*) FROM w9.source_column WHERE column_name = '$col'")" = "1" ] || continue
    src -c "COPY ($(tgtq "SELECT w9.extreme_sql('old', '$col', '$dir', '$lo'::timestamptz, '$hi'::timestamptz)")) TO STDOUT (FORMAT csv)" \
      | tgt -c "COPY w9.extreme (side, station, yr, metric, dir, value, at) FROM STDIN (FORMAT csv)" > /dev/null
    tgt -c "INSERT INTO w9.extreme (side, station, yr, metric, dir, value, at)
            $(tgtq "SELECT w9.extreme_sql('new', '$col', '$dir', '$lo'::timestamptz, '$hi'::timestamptz)")
            ON CONFLICT DO NOTHING" > /dev/null
  done 3< <(tgtq "SELECT col, dir FROM w9.spot_metric ORDER BY col, dir")
  tgt -c "UPDATE w9.extreme r SET station = m.station_id FROM w9.station_map m
           WHERE r.side = 'old' AND r.station = m.stationname" > /dev/null
  tgt -c "SELECT verdict, count(*) FROM w9.extreme_report GROUP BY 1 ORDER BY 1"
  tgt -c "SELECT * FROM w9.extreme_report ORDER BY station, yr, metric, dir"
}

step_rollup_check() {
  say "rollup check — the new hierarchy against the old daily view"
  tgtf "$HERE/40-rollup-check.sql"
}

step_policies() {
  say "policies — refresh, compression, retention. Last, on purpose."
  tgt -c "SELECT job_id, application_name, schedule_interval, next_start
            FROM timescaledb_information.jobs WHERE job_id >= 1000 ORDER BY job_id"
  tgt -c "SELECT count(*) AS compressed_chunks FROM timescaledb_information.chunks
           WHERE hypertable_name = 'observations' AND is_compressed"
}

case "${1:-help}" in
  preflight)  step_preflight ;;
  archive)    step_archive ;;
  pause-source-retention) step_pause_source_retention ;;
  stage)      step_stage ;;
  gap)        step_gap ;;
  stations)   step_stations ;;
  raw)        step_raw ;;
  aggregates) shift; step_aggregates "$@" ;;
  rollup)     step_rollup ;;
  reconcile)  step_reconcile ;;
  policies)   step_policies ;;
  all)        step_stage; step_stations; step_raw; step_aggregates; step_rollup; step_reconcile ;;
  *) sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
