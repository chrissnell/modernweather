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
#   carry-out   the readings already imported that the source's raw window no longer
#               reaches, copied out of `observations` before a re-import truncates it.
#               Reads the source only for its min(time). Pairs with `carry-in`.
#   reset-target
#               undo every TARGET-side effect of stage..rollup, so the procedure can run
#               again: drop the w9 schema and the legacy relations, truncate the readings
#               and the four levels, empty the station dimension. Refuses without
#               CONFIRM_RESET=<target database>. Touches nothing on the source.
#   stage       create the w9 schema on the target and load both inventories.
#   stations    seed w9.station_map with the DERIVED station ids and populate `stations`
#               from it. Nothing can be imported before it.
#   carry-in    the carried readings back, re-keyed through w9.station_map. After
#               `stations`, before `raw`.
#   raw         stream the raw hypertable, slice by slice, resumably.
#   aggregates  create the four levels and materialise them bottom-up, recent-first.
#   rollup      import the archived daily/hourly buckets into observations_*_legacy.
#   reconcile   both digests, the rollup comparison and the spot checks.
#   spot        the spot checks alone, against the same window `reconcile` would use.
#   rollup-check
#               the new hierarchy against the old daily view, alone.
#   policies    refresh policies, compression and retention — last, deliberately.
#
# Each step is idempotent and each can be run alone, with one exception named where it is:
# `carry-out` refuses to overwrite an existing carry rather than retaking it, because that
# carry may be the only copy of what is in it. `all` runs stage..reconcile.
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

# Schema-qualified, and that is not decoration: the whole point of this table is that it
# is NOT in w9, because `reset-target` drops that schema, and an unqualified name resolves
# against whatever the connection's search_path happens to put first.
CARRY=public.w9_carried_observations

# The readings the source can no longer supply, held across a re-import.
#
# `reset-target` truncates `observations` and `raw` re-imports it, and `raw` can only
# import what the source still has. Production's raw retention is 365 days and the policy
# is the GO DAEMON's — it re-adds it on every restart
# (remoteweather internal/storage/timescaledb/sql.go:2038) — so a re-run weeks or hours
# after the first import faces a source whose raw window has receded, and the target is
# then the only place the readings between the two minima exist. §2c: 462,495 of them,
# fourteen days, dropped on the source two hours after M5-1 imported them.
#
# `rollup` is NOT the answer for those. It replaces rollup-only history with daily and
# hourly buckets, which is right for history that was already rollup-only and wrong for
# raw that still exists: it is a per-reading series turned into 14 daily rows. D-30 is that
# readings are the thing that must not be lost.
#
# So the rows below the source's minimum are copied out before the reset, into a table
# OUTSIDE w9 because the reset drops that schema, and inserted back after `stations` —
# re-keyed through `w9.station_map` on `stationname`, which is the same mapping every
# imported row goes through, and before `raw` so that `set_legacy_boundary`,
# `populate_station_metrics` and the aggregate rebuild all see them.
# Refuses while an ENABLED retention policy sits on the source's raw hypertable. Both carry
# steps call it, and the reason is the sequence in §2c: `carry-out` cuts the carry at the
# source's minimum and `raw` re-reads that minimum later, so a retention run BETWEEN them
# drops rows that are in neither — gone from the source and truncated from the target. The
# policy is the Go daemon's and comes back on every restart, so this is checked at both
# ends rather than once.
assert_source_retention_paused() {
  local jobs
  jobs="$(srcq "SELECT string_agg(job_id::text, ', ' ORDER BY job_id)
                  FROM timescaledb_information.jobs
                 WHERE proc_name = 'policy_retention'
                   AND hypertable_name = 'weather' AND scheduled")"
  # An `if`, not `[ -z "$jobs" ] && return 0`: that list's exit status is 1 when the test
  # fails, and under `set -euo pipefail` a failing list in this position exits the shell
  # before the refusal below can be printed.
  if [ -z "$jobs" ]; then
    return 0
  fi
  cat >&2 <<EOM

  REFUSING: retention on the source's \`weather\` is ENABLED (job $jobs).

  This step and \`raw\` each read the source's min(time) separately. If that policy runs
  between them it deletes raw the carry has already been cut below, and those readings are
  then in neither the source nor the target. Run \`pause-source-retention\` first and
  re-check it — remoteweather re-adds the policy on every start (sql.go:2038), so a paused
  job id is not a durable fact.
EOM
  return 1
}

step_carry_out() {
  say "carry-out — the readings the source can no longer supply"
  assert_source_retention_paused
  local lo n
  lo="$(srcq "SELECT min(time) FROM weather")"
  echo "  the source's raw now begins at $lo"
  if [ "$(tgtq "SELECT to_regclass('$CARRY') IS NOT NULL")" = "t" ]; then
    n="$(tgtq "SELECT count(*) FROM $CARRY")"
    echo "  $CARRY already exists with $n rows — leaving it alone" >&2
    echo "  DROP it by hand if this is a fresh carry; refusing to overwrite a carry that" >&2
    echo "  may be the only copy of what is in it." >&2
    return 1
  fi
  # CREATE TABLE AS, so the column list and its order are the destination's by
  # construction: `carry-in` inserts with no column list and asserts the two agree. An
  # empty result is the normal case and is not an error — it means the source still
  # reaches everything the target holds, and `carry-in` then puts nothing back.
  tgt -c "CREATE TABLE $CARRY AS SELECT * FROM observations WHERE time < '$lo'::timestamptz"
  tgt -c "COMMENT ON TABLE $CARRY IS
          'Raw readings the migration source no longer holds, carried across a re-import
           by ci/db/migrate-from-go/migrate-from-go.sh (docs/data-migration.md 2c).
           Droppable once carry-in has reported both EXCEPT ALL counts as zero AND a
           backup has been taken -- NOT when the reconciliation passes, which cannot see
           these rows at all because they are below its window.'"
  tgt -c "SELECT station_id, extract(year from time AT TIME ZONE 'UTC')::int AS yr,
                 count(*) AS rows, min(time) AS lo, max(time) AS hi
            FROM $CARRY GROUP BY 1, 2 ORDER BY 1, 2"
  tgt -c "SELECT count(*) AS carried FROM $CARRY"
}

step_carry_in() {
  say "carry-in — those readings back, keyed the way every imported row is"
  assert_source_retention_paused
  if [ "$(tgtq "SELECT to_regclass('$CARRY') IS NULL")" = "t" ]; then
    echo "  no $CARRY table — nothing was carried out, so there is nothing to put back" >&2
    return 1
  fi
  # The insert below names no columns, which is only safe while the two shapes agree. They
  # do by construction — CREATE TABLE AS — and a migration applied in between is the one
  # thing that could change it, so it is checked rather than assumed.
  if [ "$(tgtq "SELECT (SELECT array_agg(attname ORDER BY attnum) FROM pg_attribute
                         WHERE attrelid = 'observations'::regclass AND attnum > 0 AND NOT attisdropped)
                     = (SELECT array_agg(attname ORDER BY attnum) FROM pg_attribute
                         WHERE attrelid = '$CARRY'::regclass AND attnum > 0 AND NOT attisdropped)")" != "t" ]; then
    echo "  REFUSING: $CARRY and observations no longer have the same columns in the same" >&2
    echo "  order, so an unqualified INSERT would put values in the wrong ones." >&2
    return 1
  fi
  # Re-key first, on the one table in the database small enough for an UPDATE to be the
  # obvious thing to do — and through w9.station_map, so a carried row and an imported row
  # of the same station cannot end up under different ids.
  tgt -c "UPDATE $CARRY c SET station_id = m.station_id
            FROM w9.station_map m WHERE m.stationname = c.stationname
             AND c.station_id IS DISTINCT FROM m.station_id"
  # Counted, not aggregated by name. A carried row may have a NULL `stationname` — the
  # column is nullable — and such a row matches no map entry, so the re-key above leaves
  # its station_id alone and it would land under a stale id that no `/span` join reaches.
  # `string_agg` over NULL names returns NULL, which `coalesce` turns into the empty
  # string, which `[ -n ]` reads as "no orphans": the guard would have passed.
  local orphans
  orphans="$(tgtq "SELECT count(*) FROM $CARRY c WHERE c.stationname IS NULL
                      OR NOT EXISTS (SELECT 1 FROM w9.station_map m
                                      WHERE m.stationname = c.stationname)")"
  if [ "$orphans" != "0" ]; then
    echo "  REFUSING: $orphans carried readings have no w9.station_map entry for their" >&2
    echo "  stationname (a NULL stationname counts), so they cannot be re-keyed:" >&2
    tgt -c "SELECT coalesce(quote_literal(c.stationname), 'NULL') AS stationname,
                   c.station_id, count(*) AS rows
              FROM $CARRY c
             WHERE c.stationname IS NULL
                OR NOT EXISTS (SELECT 1 FROM w9.station_map m
                                WHERE m.stationname = c.stationname)
             GROUP BY 1, 2 ORDER BY 1" >&2
    return 1
  fi
  # ON CONFLICT so the step is what the header claims every step is: re-runnable. The
  # conflict target is observations_station_id_time_key, and a carried row that is already
  # in place is identical to the one being inserted — it came from there.
  tgt -c "INSERT INTO observations SELECT * FROM $CARRY
          ON CONFLICT (station_id, time) DO NOTHING"
  # The losslessness proof, and it has to be here rather than in a document: the
  # reconciliation CANNOT see these rows. `recon_window` starts at the source's minimum and
  # w9.digest_body filters on it, so every carried row is below the window by construction
  # and a forgotten carry reconciles perfectly clean. This is the only check that covers
  # them — an exact row-set comparison over every column, both directions.
  local missing extra
  missing="$(tgtq "SELECT count(*) FROM (SELECT * FROM $CARRY
                                         EXCEPT ALL
                                         SELECT * FROM observations
                                          WHERE time <= (SELECT max(time) FROM $CARRY)) q")"
  extra="$(tgtq "SELECT count(*) FROM (SELECT * FROM observations
                                        WHERE time <= (SELECT max(time) FROM $CARRY)
                                       EXCEPT ALL
                                       SELECT * FROM $CARRY) q")"
  echo "  carried rows not in observations: $missing; rows in that range not carried: $extra"
  if [ "$missing" != "0" ]; then
    echo "  REFUSING: the carry is not in observations, so the re-import would lose it." >&2
    return 1
  fi
  # `stations` was populated from the source's own extent, which no longer reaches this
  # far back. Same shape as w9.populate_stations()'s conflict clause.
  tgt -c "UPDATE stations s
             SET first_seen = least(s.first_seen, c.lo),
                 last_seen  = greatest(s.last_seen, c.hi)
            FROM (SELECT station_id, min(time) AS lo, max(time) AS hi
                    FROM $CARRY GROUP BY 1) c
           WHERE c.station_id = s.station_id"
  tgt -c "SELECT station_id, extract(year from time AT TIME ZONE 'UTC')::int AS yr,
                 count(*) AS rows, min(time) AS lo, max(time) AS hi
            FROM observations GROUP BY 1, 2 ORDER BY 1, 2"
}

# Everything `stage`..`rollup` put on the TARGET, removed, so the procedure can be run a
# second time. THE SOURCE IS NOT TOUCHED by this step either.
#
# It exists because `stage` is not re-runnable: generated/00-staging.sql creates the w9
# tables and generated/20-legacy-rollup.sql creates observations_1d_legacy,
# observations_1h_legacy and the two union views, all without IF NOT EXISTS (DESIGN.md D-8
# forbids it), so a second `stage` fails on "relation already exists". A procedure whose
# second run needs hand-typed SQL is a procedure nobody can repeat, and M5-1b is what made
# that concrete: the station ids of the first run could not be station keys, so the import
# had to happen again. Re-importing is also the cheap direction — 53 of the target's 55
# chunks were compressed when that was measured, so correcting the ids in place would
# decompress and rewrite 6.4 M rows and then re-materialise four levels anyway — and it is
# safe because W9 D-3 keeps the source read-only, which makes the target reproducible by
# construction. What it is NOT safe for is a reading the source no longer has: `carry-out`
# above is that, and it runs first.
step_reset_target() {
  say "reset-target — undoes every TARGET-side effect of stage..rollup"
  local db
  db="$(tgtq "SELECT current_database()")"
  # Not just non-zero exit: a PSQL_TARGET wrapper can succeed with empty output, and an
  # empty $db would make the comparison below `"" != ""` — false, so the guard would be
  # skipped and the reset would proceed unconfirmed.
  : "${db:?the target did not report a database name; refusing to reset}"
  if [ "${CONFIRM_RESET:-}" != "$db" ]; then
    cat >&2 <<'EOM'

  REFUSING. This deletes every imported reading on the target, the four aggregate levels
  built from them, the station dimension and everything keyed to it, and the w9 schema
  with the station map in it. Only the source makes it recoverable.
EOM
    echo "  Re-run with: CONFIRM_RESET=$db $0 reset-target" >&2
    return 1
  fi

  # --- interlock: the same shape as pause-source-retention's, and for the same reason.
  # The readings below the source's minimum are the ones only this database has, and the
  # TRUNCATE below is what destroys them. `carry-out` is the step that saves them, and in
  # the wrong order there is no window in which anything notices — the reconciliation
  # cannot see them either (see carry-in).
  local src_lo exposed
  src_lo="$(srcq "SELECT min(time) FROM weather")"
  exposed="$(tgtq "SELECT count(*) FROM observations WHERE time < '$src_lo'::timestamptz")"
  if [ "$exposed" != "0" ] && [ "$(tgtq "SELECT to_regclass('$CARRY') IS NULL")" = "t" ]; then
    cat >&2 <<EOM

  REFUSING TO RESET. Run \`carry-out\` first.

  The source's raw begins at $src_lo and this target holds $exposed readings below that.
  They exist nowhere else: \`raw\` can only re-import what the source still has, and
  \`rollup\` would replace a per-reading series with daily and hourly buckets. D-30 is
  that readings are the thing that must not be lost.
EOM
    return 1
  fi
  echo "  carry interlock: $exposed readings below the source's minimum of $src_lo"

  say "before"
  tgt -c "SELECT 'observations' AS relation, count(*) AS rows FROM observations
    UNION ALL SELECT 'stations', count(*) FROM stations
    UNION ALL SELECT 'station_metrics', count(*) FROM station_metrics
    UNION ALL SELECT 'w9 relations',
                     (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                       WHERE n.nspname = 'w9')
    ORDER BY 1"

  # The four objects 20-legacy-rollup.sql creates outside w9, views first because they
  # read the tables. Then the schema that holds everything else, station map included:
  # the map is reconstructed by `stations`, which is the point of M5-1b's change.
  tgt -c "DROP VIEW IF EXISTS observations_1d_with_legacy, observations_1h_with_legacy"
  tgt -c "DROP TABLE IF EXISTS observations_1d_legacy, observations_1h_legacy"
  tgt -c "DROP SCHEMA IF EXISTS w9 CASCADE"

  # TRUNCATE and drop_chunks, never DELETE: a DELETE over compressed chunks decompresses
  # them, which is the cost this step exists to avoid.
  tgt -c "TRUNCATE observations"
  for lvl in 1m 5m 1h 1d; do
    tgt -c "SELECT count(*) AS dropped_chunks_${lvl}
              FROM drop_chunks('observations_${lvl}', older_than => 'infinity'::timestamptz)"
  done
  # The dimension last, and by DELETE on purpose: all seven foreign keys to `stations` are
  # ON DELETE CASCADE, so this empties station_metrics, almanac_cache, snow_totals_cache,
  # forecasts, alerts, snow_depth_estimates and snow_events — every row keyed to a station
  # that is about to be re-imported under a different id. A future table referencing
  # `stations` without a cascade makes this fail loudly, which is the right direction.
  tgt -c "DELETE FROM stations"

  say "after"
  tgt -c "SELECT 'observations' AS relation, count(*) AS rows FROM observations
    UNION ALL SELECT 'observations_1m', count(*) FROM observations_1m
    UNION ALL SELECT 'observations_5m', count(*) FROM observations_5m
    UNION ALL SELECT 'observations_1h', count(*) FROM observations_1h
    UNION ALL SELECT 'observations_1d', count(*) FROM observations_1d
    UNION ALL SELECT 'stations', count(*) FROM stations
    UNION ALL SELECT 'station_metrics', count(*) FROM station_metrics
    UNION ALL SELECT 'almanac_cache', count(*) FROM almanac_cache
    UNION ALL SELECT 'snow_totals_cache', count(*) FROM snow_totals_cache
    UNION ALL SELECT 'forecasts', count(*) FROM forecasts
    UNION ALL SELECT 'alerts', count(*) FROM alerts
    UNION ALL SELECT 'snow_depth_estimates', count(*) FROM snow_depth_estimates
    UNION ALL SELECT 'snow_events', count(*) FROM snow_events
    ORDER BY 1"
  cat <<'EOM'
  The schema is untouched: migrations/ is not re-applied and does not need to be. The
  aggregate definitions, their policies and every index survive — only rows were removed.
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
  # The default is the DERIVED station id, w9.station_key(stationname): a station id
  # becomes a component id (`station:<driver>:<id>`), so a Go display name is usually not
  # one. M5-1 imported the names and two of its three stations could not then be
  # configured against their own history (M5-1b). The derivation and both refusals are in
  # generated/00-staging.sql; override a row by inserting into w9.station_map yourself
  # before running this step, and this leaves it alone.
  tgt -c "CALL w9.seed_station_map()"
  tgt -c "SELECT stationname, station_id, stationtype FROM w9.station_map ORDER BY stationname"
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

# The comparison window, shared by `reconcile` and `spot` so the two cannot disagree
# about what range they are checking. Sets RECON_LO and RECON_HI.
recon_window() {
  RECON_LO="$(srcq "SELECT min(time) FROM weather")"
  # The TARGET's maximum, not the source's. The source is live and stays live — it is the
  # rollback (W9 D-2) — so by the time this runs it holds readings that arrived after
  # `raw` finished, and a window bounded by the source's max would compare a range the
  # import never covered and report the difference as missing rows. M5-1 measured 1,255
  # and 2,085 extra old-side rows on its two live stations from exactly that.
  #
  # Bounded by the lesser of the two maxima: on a quiesced source they are equal, and on a
  # live one this is the imported extent. A source that is somehow BEHIND the target still
  # bounds the comparison correctly.
  #
  # `lo` stays the SOURCE's minimum on purpose. Bounding it by the target too would hide
  # the one failure worth catching at that end — an import that missed the earliest rows
  # would simply fall outside the window and reconcile clean.
  RECON_HI="$(tgtq "SELECT least(
                (SELECT max(time) FROM observations),
                '$(srcq "SELECT max(time) FROM weather")'::timestamptz)
              + INTERVAL '1 microsecond'")"
  echo "  window: $RECON_LO .. $RECON_HI"
  # What the window leaves out, said out loud rather than left to be inferred: readings
  # that arrived in the source after the import. Zero on a quiesced source.
  echo "  source rows above the window (arrived after the import): \
$(srcq "SELECT count(*) FROM weather WHERE time >= '$RECON_HI'")"
}

step_reconcile() {
  say "reconcile — the old database stays read-only until this passes"
  local lo hi
  recon_window
  lo="$RECON_LO"; hi="$RECON_HI"
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
  carry-out)  step_carry_out ;;
  reset-target) step_reset_target ;;
  stage)      step_stage ;;
  gap)        step_gap ;;
  stations)   step_stations ;;
  carry-in)   step_carry_in ;;
  raw)        step_raw ;;
  aggregates) shift; step_aggregates "$@" ;;
  rollup)     step_rollup ;;
  reconcile)  step_reconcile ;;
  # The two tails of `reconcile`, runnable alone. Both are read-only and neither touches
  # w9.recon, so a digest run that passed does not have to be repeated to re-check these
  # — which on a production-sized source is the difference between minutes and an hour.
  spot)       recon_window; step_spot "$RECON_LO" "$RECON_HI" ;;
  rollup-check) step_rollup_check ;;
  policies)   step_policies ;;
  # `carry-in` is conditional on a carry existing, and that is the point: forgetting to
  # put the carried readings back is the one mistake in this procedure that loses them
  # silently, and the table only exists because somebody ran `carry-out`.
  all)        step_stage; step_stations
              if [ "$(tgtq "SELECT to_regclass('$CARRY') IS NOT NULL")" = "t" ]; then
                step_carry_in
              fi
              step_raw; step_aggregates; step_rollup; step_reconcile ;;
  # The header comment, to the line before `set -euo`, rather than a fixed line range: a
  # step added to the ORDER list above used to fall off the end of the help silently.
  *) sed -n "2,$(($(grep -n '^set -euo' "${BASH_SOURCE[0]}" | cut -d: -f1) - 1))p" \
       "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
