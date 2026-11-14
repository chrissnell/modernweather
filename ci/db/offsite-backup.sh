#!/usr/bin/env bash
# Off-site backup and restore. One file, because dump and restore are one contract: the
# archive layout and the column lists the restore COPYs into are written by `dump` and read
# by `restore`, and two files would be two copies of that contract.
#
# DESIGN.md D-6 says backup and restore is `pg_dump` plus an aggregate refresh and that no
# bespoke tool is built. This script is the schedule and the destination D-6 never had
# (docs/reconciliation.md §7.5, work item N-19) rather than a second mechanism: it builds no
# format of its own, every object it writes is a plain COPY stream or a `pg_dump -Fc`
# archive, and docs/backup-and-restore.md prints the by-hand equivalent of every command it
# runs.
#
# WHAT IS BACKED UP, WHICH IS LESS THAN THE DATABASE
#
# The four aggregate levels are NOT backed up. D-15 makes them disposable derived data and
# they would be the largest thing in the archive -- the minute level alone is 8.8 GB/year at
# the generated width (docs/database-schema.md §6) against raw's 19 GB/year, and raw is what
# they are derived from. They are rebuilt on restore, and A-W9-2 is the reason they could not
# be loaded even if they were dumped: a continuous aggregate ACCEPTS an INSERT and discards
# it on the next refresh, so pre-seeded aggregate rows are not a restore mechanism. The
# failure arrives entirely later than the mistake, which is why it is worth a line here as
# well as in the restore path, which demonstrates it.
#
# `observations_1d_legacy` and `observations_1h_legacy` ARE backed up, and they are the
# reason this script has a declared inventory rather than dumping everything. They are the
# only copy of the ~294 days of roll-up with no raw behind them (W9 D-4), they are
# deliberately outside the aggregate hierarchy, and -- unlike every other table here --
# `migrations/` does not create them. The W9 import does. So they are dumped WITH their
# schema, because a restore into a freshly migrated database has nowhere to put them
# otherwise, and that makes them a different dump mode from everything else.
#
# PER YEAR, BECAUSE RAW IS APPEND-ONLY
#
# Raw is never updated, so a historical year cannot change (§7.5). The archive therefore
# holds one object per year and the weekly run re-dumps only the CURRENT year -- roughly
# 4-5 GB rather than the whole decade -- and a restore of one year does not pull the other
# nine.
#
# "Cannot change" is an assumption about the write path, so this script CHECKS it rather
# than trusting it: every historical year's row count is compared against the count recorded
# in the manifest, and a year that moved is re-dumped and reported loudly. That costs one
# count per year per run, which is a sequential scan of raw and is the whole cost of the
# weekly job above the current year's upload. `MW_BACKUP_VERIFY_HISTORY=0` turns it off and
# records that it did, because a weekly job that is cheaper than it looks should say so in
# the artefact rather than in a comment.
#
# A SECOND check, which is NOT skippable, asks the question a row count cannot: is the object
# a skipped year is relying on still in the archive, at the length the manifest recorded?
# history_object_intact below has the reason that is not paranoia.
#
# AND NOTHING AGE-BASED MAY BE APPLIED TO THIS ARCHIVE FROM OUTSIDE. A historical year's
# object is never rewritten, so its age only ever increases: an object-expiry rule on the
# bucket deletes every year but the current one. The one thing here that does accumulate is
# the timestamped manifests, and `prune_manifests` below is where that is handled -- in the
# job, which only runs when there is a fresh backup to keep, rather than in the object store,
# which would keep deleting after the job stopped.
#
# THE INVENTORY IS CLOSED, AND THAT IS THE POINT
#
# Every ordinary table in the weather database is named in RELATIONS below with a mode and a
# reason. A table that exists and is not named is a hard failure -- not a warning, and not a
# silent omission. "The legacy archive is the one that gets forgotten" is a prediction about
# a future migration, and the only mechanism that beats a prediction like that is a backup
# that refuses to run until somebody says what the new table is for. `plan` prints the
# inventory and touches nothing, so the answer is cheap to see.
#
# Usage:
#   ci/db/offsite-backup.sh plan                    # print the plan, touch nothing
#   ci/db/offsite-backup.sh dump                    # dump and upload
#   ci/db/offsite-backup.sh list                    # list the archive
#   ci/db/offsite-backup.sh manifest                # print the archive's latest manifest
#   ci/db/offsite-backup.sh restore --into <db> [--year Y]...
#   ci/db/offsite-backup.sh layout-version
#
# Environment:
#   DATABASE_URL                     the weather database. Required unless PSQL is given.
#   MW_BACKUP_REMOTE                 rclone destination, e.g. r2:bucket/modernweather.
#   RCLONE_CONFIG                    rclone's own variable. The in-cluster CronJobs set it to
#                                    /dev/null and define the remote entirely through
#                                    RCLONE_CONFIG_R2_* instead, so no config file exists to
#                                    assemble a credential into.
#   MW_CONFIG_DB                     the daemon's SQLite configuration database.
#   MW_BACKUP_ALLOW_NO_CONFIG_DB=1   tolerate MW_CONFIG_DB not existing yet.
#   MW_RESTORE_CONFIG_DB             where `restore` writes the configuration database.
#   MW_BACKUP_VERIFY_HISTORY=0       skip the per-year row-count check. The object-presence
#                                    check always runs; it is one HEAD per year.
#   MW_BACKUP_MANIFEST_RETENTION_DAYS  prune timestamped manifests older than this. Default
#                                    90; 0 keeps them forever. `latest.json` is never
#                                    touched, and nothing outside manifest/ ever is.
#   PSQL, PG_DUMP, PG_RESTORE, RCLONE   override a whole invocation, for a bastion or a test.

set -euo pipefail

# The archive layout's version. `restore` refuses an archive whose layout it does not
# understand rather than guessing -- which is also what turns a drifted copy of this script
# into a visible failure. The manifests repo's CronJob carries a copy (it must not depend on
# GitHub being up at backup time), and the one thing that keeps the two honest is that an
# archive written by one and read by the other has to agree on this number. Bump it when an
# object path or a manifest field changes meaning; not for a comment or a message.
LAYOUT_VERSION=1

# ----------------------------------------------------------------------------------------
# The closed inventory. One line per ordinary relation in the weather database:
#
#   name|mode|reason
#
#   raw-per-year  COPY one object per calendar year, with an explicit column list. Only
#                 `observations`: it is the hypertable, it is append-only, and
#                 `pg_dump -t observations` dumps an empty parent because the rows live in
#                 chunks under _timescaledb_internal.
#   copy          COPY the whole table. For tables migrations/ creates: the archive carries
#                 the data and the migration set carries the schema.
#   pgdump        pg_dump -Fc WITH the schema. For tables migrations/ does NOT create.
#   skip          deliberately not backed up. The reason is the whole content of the line.
#
# A relation present in the database and absent here stops the dump. A relation here and
# absent from the database is reported and recorded, which is how the legacy archive's
# absence before M5-1 appears in the manifest as a fact rather than as nothing at all.
#
# THIS IS NOT A THEORETICAL MECHANISM. It fired within hours of M0-2 merging: M6-3 added
# `alerts`, `snow_depth_estimates` and `snow_events`, and the next dump against a database at
# HEAD stopped with all three named. That is the whole design working -- a migration that adds
# a table arrives as a backup failure rather than as three tables quietly missing from every
# archive until someone needs them -- and the fix is the three lines below, each with a reason
# somebody had to think about rather than a mode somebody guessed.
# ----------------------------------------------------------------------------------------
RELATIONS=(
  "observations|raw-per-year|raw readings. Derived from nothing; D-4 retains them indefinitely"
  "observations_1d_legacy|pgdump|the only copy of the ~294 rollup-only days (W9 D-4); migrations/ does not create it"
  "observations_1h_legacy|pgdump|same, hourly; migrations/ does not create it"
  "stations|copy|the station dimension. raw's station_id means nothing without it"
  "station_metrics|copy|projection of which metrics a station reports"
  "almanac_cache|copy|cache, included because excluding it is not worth the thought (7.5)"
  "snow_totals_cache|copy|cache, same reasoning"
  "forecasts|copy|the read side of the frozen forecast endpoint"
  "alerts|copy|the rows the aeris:alerts:* components write. Small, updated in place, and a past alert cannot be re-fetched -- the Aeris endpoint serves current ones. Included on almanac_cache's reasoning: excluding it is not worth the thought (7.5)"
  "snow_depth_estimates|copy|snow:smoothing's output, and snowlast24 is max-min over THIS smoothed series rather than over the raw gauge distance. A hypertable, not a continuous aggregate, so A-W9-2 does not apply and it loads back. Not rebuildable by a refresh either: rate_limit carries each estimate forward into the next, so the series is recursive over its own output and no CREATE MATERIALIZED VIEW can express it"
  "snow_events|copy|snow:events' output, detected by changepoint detection over a median-filtered series. Same reasoning as the estimates, and the website's accumulation bands read it"
  "observations_1m|skip|aggregate level: disposable (D-15), rebuilt on restore, and A-W9-2 makes loading it impossible anyway"
  "observations_5m|skip|aggregate level, same"
  "observations_1h|skip|aggregate level, same"
  "observations_1d|skip|aggregate level, same"
  "_sqlx_migrations|skip|the migration ledger. A restore applies migrations/ itself, so restoring the ledger would describe a history the restored database never had"
  "_mw_ci_applied|skip|the shell applier's ledger, used by ci/db/migrate-twice.sh and ci/db/restore-rehearsal.sh. Not part of the schema, and absent from any database the daemon migrated"
)

relation_field() { # <name> <1=mode|2=reason>
  local line rest
  for line in "${RELATIONS[@]}"; do
    [ "${line%%|*}" = "$1" ] || continue
    rest="${line#*|}"
    if [ "$2" = "1" ]; then printf '%s' "${rest%%|*}"; else printf '%s' "${rest#*|}"; fi
    return 0
  done
  return 1
}
relation_mode()   { relation_field "$1" 1; }
relation_reason() { relation_field "$1" 2; }
relation_names()  { local line; for line in "${RELATIONS[@]}"; do printf '%s\n' "${line%%|*}"; done; }

# ----------------------------------------------------------------------------------------
# Invocations, each overridable as a whole. ci/db/rebuild-aggregates.sh set the precedent
# and the reason is the same: the scripts in this directory have to work where the database
# is only reachable through `kubectl exec`, and it is also what makes the plan assertable
# with no database anywhere (xtask/tests/offsite_backup.rs).
# ----------------------------------------------------------------------------------------
BASE_URL=""
if [ -n "${PSQL:-}" ]; then
  psql_()  { $PSQL --set ON_ERROR_STOP=1 -X -q "$@"; }
  psqlv_() { $PSQL --set ON_ERROR_STOP=1 -X -q -At "$@"; }
  [ -n "${DATABASE_URL:-}" ] && BASE_URL="${DATABASE_URL%/*}"
else
  psql_()  { psql --set ON_ERROR_STOP=1 -X -q "$DATABASE_URL" "$@"; }
  psqlv_() { psql --set ON_ERROR_STOP=1 -X -q -At "$DATABASE_URL" "$@"; }
  [ -n "${DATABASE_URL:-}" ] && BASE_URL="${DATABASE_URL%/*}"
fi

# Checked at the point of use rather than at load, so `layout-version` -- which the
# manifests repo's CronJob calls to prove its vendored copy agrees with the archive -- needs
# no database.
need_db() {
  [ -n "${PSQL:-}" ] || : "${DATABASE_URL:?set DATABASE_URL to the weather database, or PSQL to a psql invocation}"
}
PG_DUMP="${PG_DUMP:-pg_dump}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"
RCLONE="${RCLONE:-rclone}"
rclone_() { $RCLONE "$@"; }

note() { printf '%s\n' "$*"; }
warn() { printf '!!! %s\n' "$*" >&2; }
die()  { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# ----------------------------------------------------------------------------------------
# The inventory check. Runs before anything is written: a dump that omits a table nobody
# declared is the failure this script exists to make impossible, and finding out after the
# upload is worth nothing.
# ----------------------------------------------------------------------------------------
present_relations() {
  psqlv_ -c "SELECT c.relname
               FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                AND c.relpersistence = 'p'
              ORDER BY 1"
}

PRESENT=""
check_inventory() {
  local undeclared=0 r
  PRESENT="$(present_relations)"
  while IFS= read -r r; do
    [ -n "$r" ] || continue
    if ! relation_mode "$r" >/dev/null 2>&1; then
      warn "public.$r exists and RELATIONS does not name it"
      undeclared=1
    fi
  done <<<"$PRESENT"
  [ "$undeclared" -eq 0 ] || die "a relation nobody declared. Add it to RELATIONS with a mode and a reason --
      \`copy\` if migrations/ creates it, \`pgdump\` if not, \`skip\` with the reason if it
      is derived. This stops rather than warns because a table silently left out of the
      backup is only discovered during a restore."
}

# The columns of a table in attribute order, quoted. Recorded in the manifest and used
# verbatim by both COPY TO and COPY FROM: a positional COPY breaks the first time a
# migration adds a column between dump and restore, and an explicit list restores an older
# archive into a newer schema for as long as no dumped column has been dropped.
columns_of() {
  psqlv_ -c "SELECT string_agg(quote_ident(attname), ',' ORDER BY attnum)
               FROM pg_attribute
              WHERE attrelid = 'public.$1'::regclass AND attnum > 0 AND NOT attisdropped"
}

table_present() { printf '%s\n' "$PRESENT" | grep -qx -- "$1"; }

# The years raw covers. min()/max() rather than DISTINCT extract(year ...), which is a
# sequential scan of a decade to learn something the two endpoints already say.
# `AT TIME ZONE 'UTC'` is load-bearing: the slicing below is hard-coded `+00`, and
# `extract(year from <timestamptz>)` reads it in the **session** zone. On a server whose
# `TimeZone` is not UTC the two disagree at the year boundary, and the newest rows land in no
# object at all -- silently, because the rehearsal iterates the manifest and so cannot see a
# year the manifest never mentioned.
raw_years() {
  local lo hi
  lo="$(psqlv_ -c "SELECT coalesce(extract(year from (min(time) AT TIME ZONE 'UTC'))::int, 0) FROM observations")"
  hi="$(psqlv_ -c "SELECT coalesce(extract(year from (max(time) AT TIME ZONE 'UTC'))::int, 0) FROM observations")"
  [ "${lo:-0}" -gt 0 ] 2>/dev/null || return 0
  seq "$lo" "$hi"
}

year_rows() {
  psqlv_ -c "SELECT count(*) FROM observations
              WHERE time >= '$1-01-01 00:00:00+00'::timestamptz
                AND time <  '$(( $1 + 1 ))-01-01 00:00:00+00'::timestamptz"
}

# ----------------------------------------------------------------------------------------
# Object paths. The only place the layout is spelled out.
# ----------------------------------------------------------------------------------------
: "${MW_BACKUP_REMOTE:=}"
obj_raw()      { printf '%s/raw/observations-%s.copy.gz' "$MW_BACKUP_REMOTE" "$1"; }
obj_table()    { printf '%s/tables/%s.copy.gz' "$MW_BACKUP_REMOTE" "$1"; }
obj_pgdump()   { printf '%s/legacy/%s.pgdump' "$MW_BACKUP_REMOTE" "$1"; }
obj_configdb() { printf '%s/config/config.db.gz' "$MW_BACKUP_REMOTE"; }
obj_manifest() { printf '%s/manifest/%s.json' "$MW_BACKUP_REMOTE" "$1"; }
# Objects are overwritten in place, so a dump that dies half way leaves new objects behind a
# manifest that describes the old ones -- and `--from-archive` would then report MISMATCH on
# a restore that was perfectly faithful to what it was given. This marker is written before
# the first object and rewritten after `latest.json`, so the two disagreeing is the signal
# that a run was interrupted. A marker rather than a rename because a rename is a copy and a
# delete, and nothing else in this script needs delete on the bucket.
obj_state()    { printf '%s/manifest/state.json' "$MW_BACKUP_REMOTE"; }

human() {
  case "${1:-}" in ''|*[!0-9]*) printf '%s' "${1:-?}"; return ;; esac
  python3 -c '
import sys
b = float(sys.argv[1])
for u in ("B", "KiB", "MiB", "GiB", "TiB"):
    if b < 1024 or u == "TiB":
        print(f"{int(b)} B" if u == "B" else f"{b:.1f} {u}"); break
    b /= 1024' "$1"
}

# Reads one field out of the manifest. A single helper so the JSON shape is referenced in
# one style rather than seven ad-hoc python one-liners.
man_get() { # <manifest-file> <python-expression over `m`>
  python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
v = eval(sys.argv[2], {}, {"m": m})
if isinstance(v, (list, tuple)):
    print("\n".join(str(x) for x in v))
elif v is not None:
    print(v)' "$1" "$2"
}

# ----------------------------------------------------------------------------------------
# plan
# ----------------------------------------------------------------------------------------
cmd_plan() {
  need_db
  note "=== layout version $LAYOUT_VERSION, destination ${MW_BACKUP_REMOTE:-<unset>}"
  check_inventory
  note "=== inventory"
  local r
  while IFS= read -r r; do
    printf '  %-24s %-13s %-8s %s\n' "$r" "$(relation_mode "$r")" \
           "$(table_present "$r" && echo present || echo ABSENT)" "$(relation_reason "$r")"
  done < <(relation_names)
  note "=== raw years"
  local y
  for y in $(raw_years); do printf '  %s -> %s\n' "$y" "$(obj_raw "$y")"; done
  note "=== configuration database"
  if [ -n "${MW_CONFIG_DB:-}" ]; then
    printf '  %s -> %s\n' "$MW_CONFIG_DB" "$(obj_configdb)"
  else
    printf '  MW_CONFIG_DB unset\n'
  fi
}

# ----------------------------------------------------------------------------------------
# dump
#
# Everything streams. `psql -c "COPY ... TO STDOUT" | meter | gzip | rclone rcat` writes an
# object without the dump ever touching local disk, which is why the CronJob needs no volume
# and why the same command works against a decade of history from a machine with none free.
# The cost is that the uncompressed size and the digest have to be measured in the stream,
# which is what `meter` is.
# ----------------------------------------------------------------------------------------
MANIFEST_PARTS=()
STAMP=""
METER_SHA=""
METER_BYTES=""
UPLOADED_BYTES=""

# Passes stdin through to stdout, recording sha256 and byte count. One process rather than
# two `tee >(...)`: process substitutions are not reaped by the pipeline, so their output
# files can still be empty when rclone returns, and a manifest with a blank digest in it is
# worse than no manifest.
meter_file=""
meter() { python3 -c '
import sys, hashlib
h = hashlib.sha256(); n = 0
out = sys.stdout.buffer
while True:
    b = sys.stdin.buffer.read(1 << 20)
    if not b:
        break
    h.update(b); n += len(b); out.write(b)
out.flush()
open(sys.argv[1], "w").write(h.hexdigest() + " " + str(n))' "$meter_file"; }

# The digest recorded is of the data as the PRODUCER emitted it, before gzip: gzip output is
# not byte-stable across versions and implementations, and the COPY stream is. That is also
# what makes the restore's reconciliation exact rather than approximate -- re-running the
# same ordered COPY against the restored table has to produce the same bytes.
begin_meter() { meter_file="$(mktemp)"; }
end_meter() {
  METER_SHA="$(cut -d' ' -f1 "$meter_file")"
  METER_BYTES="$(cut -d' ' -f2 "$meter_file")"
  rm -f "$meter_file"
}
stored_bytes() {
  UPLOADED_BYTES="$(rclone_ size --json "$1" 2>/dev/null \
                    | sed -n 's/.*"bytes":[[:space:]]*\([0-9]*\).*/\1/p')"
  UPLOADED_BYTES="${UPLOADED_BYTES:-0}"
}

# Is the object a skippable year is relying on actually STILL THERE, and the size the
# manifest recorded?
#
# This is the check that stops the per-year design from degrading into a single year of
# backup. Skipping an unchanged year trusts the previous manifest, and a manifest describes
# what was written -- not what survived. An R2 lifecycle rule, a mis-aimed `rclone purge` or
# anything else that removes objects out of band leaves the manifest still claiming a decade
# while the bucket holds one year, and NOTHING else in this script would notice: the row
# counts still agree, because they are counts of rows in the database.
#
# Not hypothetical. The bucket this archive originally shared with the Vault raft snapshots
# turned out to hold ZERO objects: that job's last successful upload was 2026-09-09 and every
# snapshot it had ever written was gone. Whatever removed them would have removed historical
# years here in exactly the same silence.
#
# One HEAD per historical year per week. A missing object re-dumps the year and says so; a
# size that disagrees with the manifest also re-dumps, because an object of the wrong length
# is not the object the digest was computed over.
history_object_intact() { # <year> <stored-bytes-the-manifest-recorded>
  local y="$1" expect="$2"
  stored_bytes "$(obj_raw "$y")"
  if [ "${UPLOADED_BYTES:-0}" = "0" ]; then
    warn "$y is in the manifest and NOT IN THE ARCHIVE. Something removed it out of band --"
    warn "    a lifecycle rule on the bucket is the first thing to check, then anything with"
    warn "    delete permission on this token. Re-dumping the year; check the others too."
    return 1
  fi
  if [ -n "$expect" ] && [ "$expect" != "-" ] && [ "$expect" != "$UPLOADED_BYTES" ]; then
    warn "$y is $UPLOADED_BYTES bytes in the archive and the manifest recorded $expect."
    warn "    An object of the wrong length is not the object the digest was computed over."
    warn "    Re-dumping the year."
    return 1
  fi
  return 0
}

cmd_dump() {
  : "${MW_BACKUP_REMOTE:?set MW_BACKUP_REMOTE to an rclone destination, e.g. r2:bucket/modernweather}"
  need_db
  command -v gzip >/dev/null || die "gzip is not on PATH"

  STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
  note "=== off-site backup, layout version $LAYOUT_VERSION, run $STAMP"
  note "    destination $MW_BACKUP_REMOTE"
  check_inventory

  # The previous manifest, so a historical year that has not moved can be skipped.
  local prev_f prev=""
  prev_f="$(mktemp)"
  if rclone_ cat "$(obj_manifest latest)" >"$prev_f" 2>/dev/null && [ -s "$prev_f" ]; then
    prev="$prev_f"
    note "    previous manifest: run $(man_get "$prev" 'm["run"]')"
  else
    note "    no previous manifest, so every year is dumped"
  fi

  local verify="${MW_BACKUP_VERIFY_HISTORY:-1}"
  local this_year raw_cols y rows prev_rows prev_dig prev_bytes prev_stored prev_cols want
  this_year="$(date -u +%Y)"
  raw_cols="$(columns_of observations)"

  printf '{"run": "%s", "status": "in-progress"}' "$STAMP" | rclone_ rcat "$(obj_state)"

  note "=== raw, one object per year (the current year, $this_year, is always re-dumped)"
  for y in $(raw_years); do
    prev_rows=""; prev_dig=""; prev_bytes=""; prev_stored=""; prev_cols=""
    if [ -n "$prev" ]; then
      prev_rows="$(man_get "$prev" "(m['objects']['raw'].get('$y') or {}).get('rows')")"
      prev_dig="$(man_get "$prev" "(m['objects']['raw'].get('$y') or {}).get('sha256')")"
      prev_bytes="$(man_get "$prev" "(m['objects']['raw'].get('$y') or {}).get('bytes')")"
      prev_stored="$(man_get "$prev" "(m['objects']['raw'].get('$y') or {}).get('stored_bytes')")"
      # The column list the stored object was actually dumped with. A carried year's
      # manifest entry has to describe that object, not today's `observations`.
      prev_cols="$(man_get "$prev" "','.join((m['objects']['raw'].get('$y') or {}).get('columns') or [])")"
    fi

    # Two independent questions, and the order is deliberate.
    #
    #   has the DATA moved?   one count(*) over the year -- a sequential scan, and the only
    #                         thing MW_BACKUP_VERIFY_HISTORY=0 turns off.
    #   is the OBJECT there?  one HEAD. Always asked, never skippable: an object removed out
    #                         of band is exactly the failure a row count cannot see, because
    #                         the rows are in the database either way.
    want=1
    rows=""
    if [ "$y" = "$this_year" ] || [ -z "$prev_rows" ]; then
      want=1
    else
      want=0
      if [ "$verify" != "0" ]; then
        rows="$(year_rows "$y")"
        if [ "$prev_rows" != "$rows" ]; then
          want=1
          warn "$y moved: the archive recorded $prev_rows rows and the database has $rows."
          warn "    Raw is supposed to be append-only, so this is a backfill or a write-path"
          warn "    problem. The year is being re-dumped either way."
        fi
      fi
      if [ "$want" = "0" ] && ! history_object_intact "$y" "$prev_stored"; then
        want=1
      fi
      if [ "$want" = "0" ]; then
        if [ "$verify" = "0" ]; then
          rows="$prev_rows"
          note "  $y  $rows rows (carried, unverified)  object present, $(human "$UPLOADED_BYTES")"
        else
          note "  $y  $rows rows  unchanged, and the existing object is still there"
        fi
      fi
    fi
    if [ "$want" = "1" ]; then
      [ -n "$rows" ] || rows="$(year_rows "$y")"
      begin_meter
      psql_ -c "COPY (SELECT $raw_cols FROM observations
                       WHERE time >= '$y-01-01 00:00:00+00'::timestamptz
                         AND time <  '$(( y + 1 ))-01-01 00:00:00+00'::timestamptz
                       ORDER BY station_id, time) TO STDOUT" \
        | meter | gzip -6 | rclone_ rcat "$(obj_raw "$y")"
      end_meter
      stored_bytes "$(obj_raw "$y")"
      printf '  %s  %s rows  %s uncompressed  %s stored  sha256 %s\n' \
             "$y" "$rows" "$(human "$METER_BYTES")" "$(human "$UPLOADED_BYTES")" "${METER_SHA:0:16}"
      MANIFEST_PARTS+=("raw|$y|$(obj_raw "$y")|$rows|$METER_BYTES|$UPLOADED_BYTES|$METER_SHA|$raw_cols|0")
    else
      # Every field here describes the object that is already in the archive. Recording the
      # live row count beside the stored object's digest would make the manifest describe
      # something that does not exist -- which the rehearsal would then report as a
      # MISMATCH against a faithful restore.
      MANIFEST_PARTS+=("raw|$y|$(obj_raw "$y")|$prev_rows|${prev_bytes:--}|${prev_stored:--}|$prev_dig|${prev_cols:-$raw_cols}|1")
    fi
  done

  note "=== declared tables"
  local r mode cols
  while IFS= read -r r; do
    mode="$(relation_mode "$r")"
    case "$mode" in
      raw-per-year) continue ;;
      skip) printf '  %-24s skipped    %s\n' "$r" "$(relation_reason "$r")"
            MANIFEST_PARTS+=("skip|$r|-|-|-|-|-|-|0"); continue ;;
    esac
    if ! table_present "$r"; then
      printf '  %-24s ABSENT     not in this database yet\n' "$r"
      MANIFEST_PARTS+=("absent|$r|-|-|-|-|-|-|0"); continue
    fi
    rows="$(psqlv_ -c "SELECT count(*) FROM public.$r")"
    if [ "$mode" = "pgdump" ]; then
      begin_meter
      dump_pgdump "$r" | meter | rclone_ rcat "$(obj_pgdump "$r")"
      end_meter
      stored_bytes "$(obj_pgdump "$r")"
      printf '  %-24s pg_dump    %s rows  %s  sha256 %s\n' \
             "$r" "$rows" "$(human "$METER_BYTES")" "${METER_SHA:0:16}"
      MANIFEST_PARTS+=("pgdump|$r|$(obj_pgdump "$r")|$rows|$METER_BYTES|$UPLOADED_BYTES|$METER_SHA|-|0")
    else
      cols="$(columns_of "$r")"
      begin_meter
      # Ordered by **every** column, not by the first one, which is only a total order when
      # the first column is unique. It is not: `station_metrics` is keyed on
      # (station_id, metric_key), `almanac_cache` and `forecasts` have duplicate first
      # columns, and all three tables M6-3 added have composite primary keys too. With a
      # non-unique sort key PostgreSQL may return equal-keyed rows in any order, so the
      # digest recorded here and the digest recomputed at restore time could differ on data
      # that restored perfectly -- a reconciliation that fails for no reason, which is worse
      # than one that does not run, because somebody then has to decide whether to trust it.
      # Ordering by every column is a total order up to rows identical in all of them, and
      # two identical rows are interchangeable in a digest by definition.
      psql_ -c "COPY (SELECT $cols FROM public.$r ORDER BY $cols) TO STDOUT" \
        | meter | gzip -6 | rclone_ rcat "$(obj_table "$r")"
      end_meter
      stored_bytes "$(obj_table "$r")"
      printf '  %-24s copy       %s rows  %s  sha256 %s\n' \
             "$r" "$rows" "$(human "$METER_BYTES")" "${METER_SHA:0:16}"
      MANIFEST_PARTS+=("table|$r|$(obj_table "$r")|$rows|$METER_BYTES|$UPLOADED_BYTES|$METER_SHA|$cols|0")
    fi
  done < <(relation_names)

  note "=== configuration database (SQLite)"
  local cfg_state="absent" cfg_dig="-" cfg_bytes="-" cfg_stored="-"
  if [ -z "${MW_CONFIG_DB:-}" ]; then
    cfg_state="unconfigured"; printf '  MW_CONFIG_DB is unset\n'
  elif [ ! -f "$MW_CONFIG_DB" ]; then
    printf '  %s does not exist\n' "$MW_CONFIG_DB"
  else
    begin_meter
    meter <"$MW_CONFIG_DB" | gzip -6 | rclone_ rcat "$(obj_configdb)"
    end_meter
    stored_bytes "$(obj_configdb)"
    cfg_state="present"; cfg_dig="$METER_SHA"; cfg_bytes="$METER_BYTES"; cfg_stored="$UPLOADED_BYTES"
    printf '  %s  %s  sha256 %s\n' "$MW_CONFIG_DB" "$(human "$cfg_bytes")" "${cfg_dig:0:16}"
  fi
  if [ "$cfg_state" != "present" ]; then
    if [ "${MW_BACKUP_ALLOW_NO_CONFIG_DB:-0}" = "1" ]; then
      warn "no configuration database in this archive ($cfg_state), tolerated by"
      warn "    MW_BACKUP_ALLOW_NO_CONFIG_DB=1. It holds station calibration, certificates"
      warn "    and website configuration, none of which a human can re-derive (7.5), so"
      warn "    the flag belongs in the CronJob only until the daemon owns a config.db."
    else
      die "no configuration database to back up ($cfg_state). It is tiny and irreplaceable
      (docs/reconciliation.md 7.5), so its absence stops the run rather than being recorded
      and forgotten. Set MW_CONFIG_DB, or MW_BACKUP_ALLOW_NO_CONFIG_DB=1 while the daemon
      does not own one."
    fi
  fi

  note "=== manifest"
  local man
  man="$(build_manifest "$cfg_state" "$cfg_dig" "$cfg_bytes" "$cfg_stored" "$verify")"
  printf '%s' "$man" | rclone_ rcat "$(obj_manifest "$STAMP")"
  printf '%s' "$man" | rclone_ rcat "$(obj_manifest latest)"
  printf '{"run": "%s", "status": "complete"}' "$STAMP" | rclone_ rcat "$(obj_state)"
  printf '  %s\n  %s\n' "$(obj_manifest "$STAMP")" "$(obj_manifest latest)"
  rm -f "$prev_f"
  prune_manifests
  note "=== the archive now holds"
  rclone_ size "$MW_BACKUP_REMOTE" || true
}

# ----------------------------------------------------------------------------------------
# THE ONLY THING IN THIS ARCHIVE THAT GROWS WITHOUT BOUND, AND THEREFORE THE ONLY THING WITH
# A RETENTION RULE.
#
# It is worth being exact about this, because "delete backups older than N days" is the
# normal instinct and it is actively destructive here:
#
#   raw/observations-<YEAR>.copy.gz   a historical year is written ONCE and never rewritten.
#                                     Its LastModified is frozen for as long as the archive
#                                     exists, so an age-based rule over the bucket deletes
#                                     every year but the current one. That is nine tenths of
#                                     a decade, and it is the part D-4 retains indefinitely.
#   tables/*, legacy/*, config/*      rewritten on every run, so always younger than the
#                                     schedule. An age rule would not reach them -- WHILE THE
#                                     JOB IS HEALTHY. That caveat is the whole problem: a
#                                     backup job that stops, plus an age-based rule, deletes
#                                     the archive. That is exactly what happened to the
#                                     bucket this archive was nearly placed in.
#   manifest/latest.json              same, and it is the entry point `restore` reads. An
#                                     age rule that reaches it makes the archive unreadable.
#   manifest/<stamp>.json             THE ONE THING THAT ACCUMULATES. ~3 KB a week, ~160 KB a
#                                     year. Pruning it saves nothing measurable; it is worth
#                                     doing so the prefix does not grow forever, and worth
#                                     doing HERE rather than in the object store for the
#                                     reason above -- this code only runs when the backup
#                                     ran, so it cannot delete anything on a week when there
#                                     was no new backup to replace it.
#
# Scoped twice on purpose: the path is the manifest prefix and `latest.json` is excluded by
# name. Neither alone would be enough to make "this cannot reach the data" obvious to a
# reader, and that is the property that matters in a function whose job is deleting things.
# ----------------------------------------------------------------------------------------
prune_manifests() {
  local days="${MW_BACKUP_MANIFEST_RETENTION_DAYS:-90}"
  case "$days" in
    0) note "=== manifest retention off (MW_BACKUP_MANIFEST_RETENTION_DAYS=0)"; return 0 ;;
    ''|*[!0-9]*) die "MW_BACKUP_MANIFEST_RETENTION_DAYS must be a whole number of days, not '$days'" ;;
  esac
  note "=== pruning timestamped manifests older than $days days (latest.json is never touched)"
  rclone_ delete --min-age "${days}d" --exclude "latest.json" -v \
          "$MW_BACKUP_REMOTE/manifest" 2>&1 | sed -n 's/^.*: *\(Deleted\)/  \1/p' || true
  printf '  %s manifest(s) remain\n' \
         "$(rclone_ lsf "$MW_BACKUP_REMOTE/manifest" 2>/dev/null | grep -c . || echo '?')"
}

# pg_dump with the connection expressed the way the caller gave it. Split out because the
# `-Fc` tables are the one place this script runs something other than psql, and because
# PG_DUMP has to stay overridable for a bastion.
dump_pgdump() {
  if [ -n "${PSQL:-}" ] && [ -z "${DATABASE_URL:-}" ]; then
    $PG_DUMP -Fc --no-owner --no-acl -t "public.$1"
  else
    $PG_DUMP "$DATABASE_URL" -Fc --no-owner --no-acl -t "public.$1"
  fi
}

build_manifest() { # <cfg_state> <cfg_sha> <cfg_bytes> <cfg_stored> <verified>
  python3 - "$LAYOUT_VERSION" "$STAMP" "$MW_BACKUP_REMOTE" "$1" "$2" "$3" "$4" "$5" \
           "${MANIFEST_PARTS[@]}" <<'PY'
import json, sys

layout, stamp, remote, cfg_state, cfg_sha, cfg_bytes, cfg_stored, verified = sys.argv[1:9]

def num(v):
    return int(v) if v.isdigit() else None

m = {
    "layout_version": int(layout),
    "run": stamp,
    "remote": remote,
    "history_verified": verified != "0",
    "objects": {"raw": {}, "tables": {}, "pgdump": {}},
    "not_backed_up": {},
    "absent": [],
    "config_db": {
        "state": cfg_state,
        "sha256": cfg_sha if cfg_sha != "-" else None,
        "bytes": num(cfg_bytes),
        "stored_bytes": num(cfg_stored),
    },
}
written = 0
for p in sys.argv[9:]:
    kind, name, path, rows, raw_bytes, stored, sha, cols, carried = p.split("|", 8)
    if kind == "skip":
        m["not_backed_up"][name] = True
        continue
    if kind == "absent":
        m["absent"].append(name)
        continue
    entry = {
        "path": path,
        "rows": num(rows),
        "bytes": num(raw_bytes),
        "stored_bytes": num(stored),
        "sha256": sha if sha not in ("-", "") else None,
        "carried_from_a_previous_run": carried == "1",
    }
    if cols != "-":
        entry["columns"] = cols.split(",")
    if entry["stored_bytes"] and not entry["carried_from_a_previous_run"]:
        written += entry["stored_bytes"]
    group = {"raw": "raw", "pgdump": "pgdump"}.get(kind, "tables")
    m["objects"][group][name] = entry
if m["config_db"]["stored_bytes"]:
    written += m["config_db"]["stored_bytes"]
m["stored_bytes_written_this_run"] = written
m["stored_bytes_in_the_archive"] = sum(
    e["stored_bytes"] or 0 for g in m["objects"].values() for e in g.values()
) + (m["config_db"]["stored_bytes"] or 0)
print(json.dumps(m, indent=2, sort_keys=True))
PY
}

# ----------------------------------------------------------------------------------------
# restore
#
# Into an EXISTING database already at HEAD. This command does not create the schema and
# does not refresh the aggregates: `migrations/` owns the first and
# ci/db/rebuild-aggregates.sh owns the second, and a restore tool that reimplemented either
# would be the bespoke tool D-6 declines to build. ci/db/restore-rehearsal.sh puts the three
# in order and is what CI runs.
# ----------------------------------------------------------------------------------------
cmd_restore() {
  local target="" years=() all_years=1
  while [ $# -gt 0 ]; do
    case "$1" in
      --into) target="$2"; shift 2 ;;
      --year) years+=("$2"); all_years=0; shift 2 ;;
      *) die "unknown argument to restore: $1" ;;
    esac
  done
  need_db
  : "${MW_BACKUP_REMOTE:?set MW_BACKUP_REMOTE to the rclone source}"
  [ -n "$target" ] || die "restore needs --into <database>"
  [ -n "$BASE_URL" ] || die "restore needs DATABASE_URL: it connects to a named database"

  local man ver
  man="$(mktemp)"
  rclone_ cat "$(obj_manifest latest)" >"$man" || die "no manifest at $(obj_manifest latest)"
  ver="$(man_get "$man" 'm["layout_version"]')"
  if [ "$ver" != "$LAYOUT_VERSION" ]; then
    die "the archive is layout version $ver and this script reads version $LAYOUT_VERSION.
      Nothing was restored. The two halves of the backup have drifted -- either the copy of
      this script that wrote the archive is older than this one or the reverse. Reconcile
      them before trusting the archive."
  fi
  note "=== restoring layout version $ver from archive run $(man_get "$man" 'm["run"]')"

  on() { psql --set ON_ERROR_STOP=1 -X -q "$BASE_URL/$target" "$@"; }

  if [ "$all_years" = "1" ]; then
    mapfile -t years < <(man_get "$man" 'sorted(m["objects"]["raw"])')
  fi

  # Newest year first. W4 D-23's ordering applied to the load as well as to the rebuild:
  # the recent end is what /span reads, so a restore interrupted half way through has the
  # charts right rather than a decade of history and an empty dashboard.
  note "=== raw, newest year first (W4 D-23)"
  local y path cols
  for y in $(printf '%s\n' "${years[@]}" | sort -r); do
    path="$(man_get "$man" "m['objects']['raw']['$y']['path']")"
    cols="$(man_get "$man" "','.join(m['objects']['raw']['$y']['columns'])")"
    assert_columns "$target" observations "$cols"
    rclone_ cat "$path" | gunzip | on -c "COPY observations ($cols) FROM STDIN"
    printf '  %s  %s rows restored\n' "$y" "$(man_get "$man" "m['objects']['raw']['$y']['rows']")"
  done

  note "=== declared tables"
  local r mode
  while IFS= read -r r; do
    mode="$(relation_mode "$r")"
    case "$mode" in raw-per-year|skip) continue ;; esac
    path="$(man_get "$man" "(m['objects']['tables'].get('$r') or m['objects']['pgdump'].get('$r') or {}).get('path')")"
    if [ -z "$path" ]; then printf '  %-24s not in the archive\n' "$r"; continue; fi
    if [ "$mode" = "pgdump" ]; then
      rclone_ cat "$path" | $PG_RESTORE --no-owner --no-acl -d "$BASE_URL/$target"
      printf '  %-24s pg_restore\n' "$r"
    else
      cols="$(man_get "$man" "','.join(m['objects']['tables']['$r']['columns'])")"
      assert_columns "$target" "$r" "$cols"
      rclone_ cat "$path" | gunzip | on -c "COPY public.$r ($cols) FROM STDIN"
      printf '  %-24s copy\n' "$r"
    fi
  done < <(relation_names)

  note "=== configuration database"
  local cfg; cfg="$(man_get "$man" 'm["config_db"]["state"]')"
  if [ "$cfg" = "present" ]; then
    if [ -n "${MW_RESTORE_CONFIG_DB:-}" ]; then
      rclone_ cat "$(obj_configdb)" | gunzip >"$MW_RESTORE_CONFIG_DB"
      printf '  restored to %s\n' "$MW_RESTORE_CONFIG_DB"
    else
      warn "the archive holds a configuration database and MW_RESTORE_CONFIG_DB is unset,"
      warn "    so it was not restored. Point it at the path the daemon reads."
    fi
  else
    warn "this archive has NO configuration database (state: $cfg). Station calibration,"
    warn "    certificates and website configuration are not in it."
  fi
  rm -f "$man"
  note "=== raw is restored and every aggregate is still empty. A-W9-2 is why they are not"
  note "    in the archive, and ci/db/rebuild-aggregates.sh is what fills them."
}

# A column the archive carries and the destination does not have is the one schema drift
# that silently truncates a restore, so it stops instead.
assert_columns() { # <db> <table> <comma-separated>
  local target="$1" table="$2" cols="$3" have missing="" c
  have="$(psql --set ON_ERROR_STOP=1 -X -q -At "$BASE_URL/$target" -c "
    SELECT quote_ident(attname) FROM pg_attribute
     WHERE attrelid = 'public.$table'::regclass AND attnum > 0 AND NOT attisdropped")"
  for c in ${cols//,/ }; do
    printf '%s\n' "$have" | grep -qx -- "$c" || missing="$missing $c"
  done
  [ -z "$missing" ] || die "public.$table in $target is missing columns the archive carries:$missing.
      The archive predates a migration that dropped them. Nothing was restored into this
      table; restore into a database at the archive's schema instead."
}

# ----------------------------------------------------------------------------------------
case "${1:-}" in
  plan)           shift; cmd_plan "$@" ;;
  dump)           shift; cmd_dump "$@" ;;
  restore)        shift; cmd_restore "$@" ;;
  list)           : "${MW_BACKUP_REMOTE:?}"; rclone_ lsl "$MW_BACKUP_REMOTE/" ;;
  manifest)       : "${MW_BACKUP_REMOTE:?}"; rclone_ cat "$(obj_manifest latest)" ;;
  layout-version) printf '%s\n' "$LAYOUT_VERSION" ;;
  *) sed -n '/^# Usage:/,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^#\{1,\} \{0,1\}//'; exit 2 ;;
esac
