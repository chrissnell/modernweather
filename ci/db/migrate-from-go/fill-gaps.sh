#!/usr/bin/env bash
# Backfill a window the new database is missing and the old one still has.
#
# migrate-from-go.sh moves the whole history; this moves a window. The two are not the
# same operation. `raw` plans slices from the source's own extent and imports every
# station in each slice, which is right exactly once — into an empty table. After cutover
# the target is no longer empty: a live daemon is writing into it, so a window the target
# lacks for one station usually HAS rows for another, and a slice-shaped import would
# collide with them on observations_station_id_time_key rather than fill anything. Hence
# per-station ranges here, and hence `fill` refusing a range the target is not empty over.
#
# THE SOURCE IS READ-ONLY. Every statement sent to $SOURCE is a SELECT or a COPY ... TO
# STDOUT. There is no step here that writes to it, not even the one migrate-from-go.sh
# carves out for `archive`.
#
# The column list and the stationname -> station_id mapping are NOT retyped: both come
# from the w9 schema the migration left on the target (w9.import_columns(),
# w9.station_id_expr(), w9.copy_in_sql(), w9.station_map). A second copy of either would
# be a second thing to keep in step with metrics/registry.toml, and the station ids in
# particular are the ones M5-1b had to re-derive -- `CSI` is not a legal station key and
# `csi` is, so a hand-written id is a station nobody can configure.
#
#   report      gaps on the TARGET, per station, with the SOURCE's row count for each,
#               AND the trailing edge, which is the gap lag() cannot see. Writes nothing.
#               A gap with a source count of 0 is the station's own outage, recorded
#               identically on both sides, and there is nothing to fill.
#   fill        one (station, range). Refuses if the target already holds rows there.
#
#   SOURCE=postgresql://.../weather_v2_0_0  TARGET=postgresql://.../modernweather \
#     ci/db/migrate-from-go/fill-gaps.sh report --since '2026-11-12'
#
#   ... fill-gaps.sh fill 'CSI' '2026-11-13 06:44:08.426706+00' '2026-11-13 17:56:58.371+00'
#
# `fill`'s range is OPEN at both ends -- `time > t0 AND time < t1` -- so a row from
# `report` can be pasted into it unchanged. `report` prints the readings that BOUND the
# gap, and both of those exist on the target already; a half-open [t0, t1) would include
# the lower one and `fill` would refuse its own report's output every time.
#
# PSQL_SOURCE / PSQL_TARGET override the psql invocation entirely, which is what reaching
# a database inside a Kubernetes pod needs.
#
# TWO THINGS TO DO AROUND THIS SCRIPT, NEITHER OF WHICH IT CAN DO FOR YOU
#
#   1. Stop the live writer first (scale the deployment to zero) and start it again
#      after. Not for correctness -- the ranges are disjoint from what it writes -- but
#      because its own downtime is then one more range to fill, and you are already here.
#   2. Refresh the aggregates over what you filled:
#      ci/db/rebuild-aggregates.sh --from <lo> --to <hi>. All four levels, bottom-up, which
#      is what that script's bounded mode does. A raw row with no bucket above it is
#      invisible to /span, so an unrefreshed fill looks exactly like no fill at all.
#
# COMPRESSION. A range older than the compression policy's `compress_after` lands in a
# compressed chunk. `report` names the chunks a range touches and whether they are
# compressed, because an INSERT into a compressed chunk is a different operation with
# different costs, and the 2026-11-16 run happened not to need one.
set -euo pipefail

: "${SOURCE:=}"; : "${TARGET:=}"
PSQL_SOURCE="${PSQL_SOURCE:-psql -X -q -v ON_ERROR_STOP=1 $SOURCE}"
PSQL_TARGET="${PSQL_TARGET:-psql -X -q -v ON_ERROR_STOP=1 $TARGET}"

src()  { $PSQL_SOURCE "$@"; }
tgt()  { $PSQL_TARGET "$@"; }
srcq() { $PSQL_SOURCE -tA -c "$1"; }
tgtq() { $PSQL_TARGET -tA -c "$1"; }

say() { printf '\n=== %s\n' "$*"; }

# The source name for a target station id, and the reverse. Both through w9.station_map,
# which is the only place the two vocabularies are related.
station_id_for() { tgtq "SELECT station_id FROM w9.station_map WHERE stationname = '$1'"; }

step_report() {
  local since="2026-11-12" min_gap="2 minutes"
  while [ $# -gt 0 ]; do
    case "$1" in
      # `${2:-}` and an explicit test rather than `$2`: under `set -u` a flag given with
      # no value dies with "unbound variable", which is not a usage message.
      --since)   [ -n "${2:-}" ] || { echo "--since needs a timestamp" >&2; exit 2; }
                 since="$2"; shift 2 ;;
      --min-gap) [ -n "${2:-}" ] || { echo "--min-gap needs an interval" >&2; exit 2; }
                 min_gap="$2"; shift 2 ;;
      *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
  done

  say "gaps on the target since $since, longer than $min_gap"
  # lag() over the raw hypertable, per station. Bounded by --since because an unbounded
  # scan is the whole history and the answer for settled history is already known: the
  # reconciliation compared it, metric by metric, with no tolerance.
  local gaps
  gaps="$(tgtq "
    WITH g AS (
      SELECT station_id, time,
             lag(time) OVER (PARTITION BY station_id ORDER BY time) AS prev
        FROM observations WHERE time >= '$since'
    )
    SELECT station_id, prev, time, time - prev
      FROM g WHERE prev IS NOT NULL AND time - prev > INTERVAL '$min_gap'
     ORDER BY station_id, prev")"

  if [ -z "$gaps" ]; then
    echo "  none"
  else
    printf '  %-22s %-31s %-31s %-16s %9s\n' station_id from to length "in source"
    while IFS='|' read -r sid t0 t1 len; do
      [ -n "$sid" ] || continue
      local name n
      name="$(tgtq "SELECT stationname FROM w9.station_map WHERE station_id = '$sid'")"
      if [ -z "$name" ]; then
        # No map row means no way to name this station on the source. Printing 0 here
        # would read as "both sides agree", which is the one thing it does not mean.
        n="?"
      else
        # The open interval: the readings bounding the gap exist on both sides, and
        # counting them would report a gap the source cannot fill as fillable by two.
        n="$(srcq "SELECT count(*) FROM weather
                    WHERE stationname = '$name' AND time > '$t0' AND time < '$t1'")"
      fi
      printf '  %-22s %-31s %-31s %-16s %9s\n' "$sid" "$t0" "$t1" "$len" "$n"
    done <<<"$gaps"
    echo
    echo "  'in source' = 0 means the old database has no readings there either: the"
    echo "  station was down, both sides agree, and there is nothing to fill."
    echo "  '?' means no w9.station_map row for that station_id, so the source was not"
    echo "  asked: the two databases cannot be compared for it until the map has one."
  fi

  say "the trailing edge — what lag() cannot see"
  # A gap is a hole BETWEEN two readings. A station the new database simply stopped
  # recording has no reading after the hole, so lag() reports nothing and `report`
  # would say "none" forever while the station falls further behind every day. That is
  # `suncrest-air-quality` exactly: the configuration store holds one device, nothing
  # ingests the other two, and their histories end where the migration ended.
  printf '  %-22s %-31s %-31s %9s\n' station_id "target last" "source last" "to fill"
  local rows name tlast slast n
  rows="$(tgtq "SELECT station_id, stationname FROM w9.station_map ORDER BY station_id")"
  while IFS='|' read -r sid name; do
    [ -n "$sid" ] || continue
    tlast="$(tgtq "SELECT max(time) FROM observations WHERE station_id = '$sid'")"
    slast="$(srcq "SELECT max(time) FROM weather WHERE stationname = '$name'")"
    if [ -z "$tlast" ]; then
      n="all"
    else
      n="$(srcq "SELECT count(*) FROM weather
                  WHERE stationname = '$name' AND time > '$tlast'")"
    fi
    printf '  %-22s %-31s %-31s %9s\n' "$sid" "${tlast:-(none)}" "${slast:-(none)}" "$n"
  done <<<"$rows"
  echo
  echo "  'to fill' counts readings the source has after the target's last one. A live"
  echo "  station reports a handful -- the seconds since the query -- and a station"
  echo "  nothing is ingesting reports its whole backlog."

  say "the chunks those gaps land in"
  # An INSERT into a compressed chunk is a different operation from an INSERT into an
  # uncompressed one, and whether a range is compressed is decided by the compression
  # policy's compress_after rather than by anything visible in the gap list.
  tgt -c "SELECT chunk_name, range_start, range_end, is_compressed
            FROM timescaledb_information.chunks
           WHERE hypertable_name = 'observations' AND range_end > '$since'
           ORDER BY range_start"
}

step_fill() {
  local name="$1" t0="$2" t1="$3" sid cols sidexpr copy_in copy_out src_n got
  sid="$(station_id_for "$name")"
  [ -n "$sid" ] || { echo "no w9.station_map row for source station '$name'" >&2; exit 1; }

  # A target that already holds rows in the range is not a gap, and COPY has no ON
  # CONFLICT: it would fail on the first duplicate against
  # observations_station_id_time_key. Refusing here says why instead.
  got="$(tgtq "SELECT count(*) FROM observations
                WHERE station_id = '$sid' AND time > '$t0' AND time < '$t1'")"
  if [ "$got" != "0" ]; then
    echo "refusing: target already has $got rows for $sid in ($t0, $t1)" >&2
    echo "hint: the bounds are exclusive. If an earlier run already filled part of this" >&2
    echo "      range, use that station's current last reading as t0:" >&2
    echo "      $(tgtq "SELECT max(time) FROM observations WHERE station_id = '$sid'")" >&2
    exit 1
  fi

  cols="$(tgtq "SELECT string_agg(quote_ident(c), ', ') FROM unnest(w9.import_columns()) c")"
  sidexpr="$(tgtq "SELECT w9.station_id_expr()")"
  copy_in="$(tgtq "SELECT w9.copy_in_sql()")"
  copy_out="COPY (SELECT $sidexpr AS station_id, $cols FROM weather
                   WHERE time > '$t0' AND time < '$t1' AND stationname = '$name')
            TO STDOUT (FORMAT csv)"

  src_n="$(srcq "SELECT count(*) FROM weather
                  WHERE time > '$t0' AND time < '$t1' AND stationname = '$name'")"
  printf '  %-22s %s .. %s  source=%s ' "$sid" "$t0" "$t1" "$src_n"
  src -c "$copy_out" | tgt -c "$copy_in" > /dev/null
  got="$(tgtq "SELECT count(*) FROM observations
                WHERE station_id = '$sid' AND time > '$t0' AND time < '$t1'")"
  printf -- '-> imported=%s\n' "$got"

  if [ "$src_n" != "$got" ]; then
    # The source is live. A range whose upper bound has not passed yet grows between the
    # count and the COPY, and the import is then a PREFIX of the range rather than wrong
    # — the 2026-11-16 run hit this on `suncrest-air-quality`. Distinguish the two cases
    # by asking the source what time it is, and in the benign one print the exact
    # follow-up rather than an exit code and a shrug.
    local open
    open="$(srcq "SELECT now() < '$t1'::timestamptz")"
    if [ "$open" = "t" ]; then
      echo "  t1 has not passed: the source gained rows in this range while it was being" >&2
      echo "  copied, so the import is a prefix of it, not a disagreement. Top up with:" >&2
      echo "    $0 fill '$name' '$(tgtq "SELECT max(time) FROM observations
                                          WHERE station_id = '$sid'")' '<a t1 that has passed>'" >&2
      return 0
    fi
    echo "  counts differ over a closed range: source $src_n, target $got." >&2
    echo "  Nothing explains this one away. Stop and look." >&2
    exit 1
  fi
}

case "${1:-help}" in
  report) shift; step_report "$@" ;;
  fill)   shift; [ $# -eq 3 ] || { echo "usage: fill <source-stationname> <t0> <t1>" >&2; exit 2; }
          step_fill "$@" ;;
  # The header comment, to the line before `set -euo`, rather than a fixed line range:
  # migrate-from-go.sh learned this the hard way when a step fell off the end of its help.
  *) sed -n "2,$(($(grep -n '^set -euo' "${BASH_SOURCE[0]}" | cut -d: -f1) - 1))p" \
       "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
