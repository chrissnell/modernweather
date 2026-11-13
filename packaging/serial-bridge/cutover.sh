#!/usr/bin/env bash
# Point `remoteweather`'s Campbell station at the serial bridge, and write its own rollback
# before it changes anything.
#
# The change itself is one row: clear `serial_device`, set `hostname` and `port`.
# `internal/weatherstations/campbell/station.go:254` takes the serial path only
# `if s.config.SerialDevice != ""`, so clearing that column is what moves the station onto
# TCP -- setting `hostname` and `port` alone would do nothing, because serial wins.
#
#   ./cutover.sh plan                 what would change, touching nothing
#   ./cutover.sh apply                apply it, after writing rollback.sql
#   ./cutover.sh revert               apply the rollback.sql written by `apply`
#
# Environment:
#   CONFIG_DB     default /var/lib/remoteweather/config.db, which is the path that
#                 project's own systemd unit uses
#   BRIDGE_HOST   default 127.0.0.1, and it must match the accepter in the bridge's
#                 configuration. Only set it to something else if a reader is on another
#                 machine -- see "the port is writable" in README.md
#   BRIDGE_PORT   default 7001
#   DEVICE_NAME   default: the single enabled `campbellscientific` row, and an error if
#                 there is more than one
#   ROLLBACK      default ./rollback.sql
#
# This writes to a live weather server's configuration. It does NOT restart anything: the
# order in which the daemon and the bridge are restarted is the part that matters, and it is
# in README.md rather than here.
set -euo pipefail

CONFIG_DB=${CONFIG_DB:-/var/lib/remoteweather/config.db}
BRIDGE_HOST=${BRIDGE_HOST:-127.0.0.1}
BRIDGE_PORT=${BRIDGE_PORT:-7001}
ROLLBACK=${ROLLBACK:-./rollback.sql}

die() { printf 'cutover: %s\n' "$*" >&2; exit 1; }
sq() { sqlite3 -noheader -separator '|' "$CONFIG_DB" "$@"; }

command -v sqlite3 >/dev/null || die "sqlite3 is not on PATH"
[ -f "$CONFIG_DB" ] || die "no configuration database at $CONFIG_DB (set CONFIG_DB)"

resolve_device() {
  if [ -n "${DEVICE_NAME:-}" ]; then
    local n
    n=$(sq "SELECT count(*) FROM devices WHERE name = '$(printf '%s' "$DEVICE_NAME" | sed "s/'/''/g")';")
    [ "$n" = "1" ] || die "DEVICE_NAME='$DEVICE_NAME' matches $n rows in devices"
    printf '%s' "$DEVICE_NAME"
    return
  fi
  local names count
  names=$(sq "SELECT name FROM devices WHERE type = 'campbellscientific' AND enabled;")
  count=$(printf '%s' "$names" | grep -c . || true)
  [ "$count" = "1" ] || die "found $count enabled campbellscientific devices${names:+ ($(echo "$names" | tr '\n' ' '))}; set DEVICE_NAME"
  printf '%s' "$names"
}

show() {
  local name=$1
  printf 'devices row %s:\n' "$name"
  sqlite3 -line "$CONFIG_DB" \
    "SELECT name, type, enabled, serial_device, baud, hostname, port
       FROM devices WHERE name = '${name//\'/\'\'}';"
}

name=$(resolve_device)
esc=${name//\'/\'\'}

case "${1:-plan}" in
plan)
  show "$name"
  cat <<EOF

would apply:
  UPDATE devices SET serial_device = '', hostname = '$BRIDGE_HOST', port = '$BRIDGE_PORT'
   WHERE name = '$esc';

and would write the reverse of it to $ROLLBACK before doing so.
EOF
  ;;
apply)
  # IFS on the separator rather than on whitespace: an empty column must stay an empty
  # field, and `tr '|' ' '` would collapse two of them and shift every value left.
  IFS='|' read -r cur_serial cur_baud cur_host cur_port <<<"$(sq \
    "SELECT ifnull(serial_device,''), ifnull(baud,0), ifnull(hostname,''), ifnull(port,'')
       FROM devices WHERE name = '$esc';")"
  # Captured BEFORE the change, so the rollback restores what was actually there rather than
  # what this script assumed was there.
  cat > "$ROLLBACK" <<EOF
-- Rollback for the serial-bridge cutover of '$name', generated $(date -Is) from the row as
-- it was. Apply with:  sqlite3 $CONFIG_DB < $(basename "$ROLLBACK")
-- Then restart remoteweather. The station is back on the serial line, so the bridge must be
-- stopped first or they will both read the port and split the stream.
UPDATE devices
   SET serial_device = '${cur_serial//\'/\'\'}',
       baud          = $cur_baud,
       hostname      = '${cur_host//\'/\'\'}',
       port          = '${cur_port//\'/\'\'}'
 WHERE name = '$esc';
EOF
  printf 'rollback written to %s\n' "$ROLLBACK"
  [ -n "$cur_serial" ] || printf 'cutover: note -- serial_device was already empty, so this station was not on a serial line\n' >&2
  # `baud` is deliberately left alone: it is unused on the TCP path and it is the value the
  # bridge's line setting has to match, so losing it would lose the one record of what the
  # line rate was.
  sqlite3 "$CONFIG_DB" <<EOF
BEGIN IMMEDIATE;
UPDATE devices SET serial_device = '', hostname = '$BRIDGE_HOST', port = '$BRIDGE_PORT'
 WHERE name = '$esc';
COMMIT;
EOF
  show "$name"
  ;;
revert)
  [ -f "$ROLLBACK" ] || die "no $ROLLBACK to apply (set ROLLBACK)"
  sqlite3 "$CONFIG_DB" < "$ROLLBACK"
  show "$name"
  ;;
*)
  die "usage: $0 {plan|apply|revert}"
  ;;
esac
