#!/usr/bin/env bash
# Stage the serial bridge on the machine the station's cable is plugged into, by DERIVING
# this installation's values rather than asking anyone to edit placeholders.
#
#   sudo ./install.sh check     discover and print everything. Changes nothing.
#   sudo ./install.sh stage     create the user, render the config, install the unit,
#                               validate it. Starts NOTHING and takes no device.
#   sudo ./install.sh status    after the fact: who is listening, who is reading.
#
# `check` and `stage` are safe to run on a live station and are safe to run twice. Neither
# opens the serial port and neither restarts the daemon. The two commands that DO change what
# reads the station are in README.md, deliberately not here, because their ORDER is the part
# that matters.
#
# Why derive. Three values decide whether this works and every one of them fails QUIETLY when
# guessed: the device, the line rate, and the listen address. A wrong rate does not produce
# silence -- the bytes arrive framed wrong, every record fails to parse, and the station looks
# connected and mute. So rather than ship a default, this reads the two that are knowable off
# the running system, from independent sources, and says so when they disagree.
#
# Root, because the authoritative answer to "which device is the station on" is the file
# descriptor the running daemon has open, and that is under /proc/<pid>/fd.
#
# Environment:
#   DAEMON       the unit currently reading the station, default remoteweather
#   CONFIG_DB    its configuration database, default /var/lib/remoteweather/config.db
#   STATION      the device row's name; default: the single enabled campbellscientific row
#   DEVICE       override discovery
#   BAUD         override discovery
#   BRIDGE_ADDR  what the bridge listens on, default 127.0.0.1
#   BRIDGE_PORT  default 7001
#   UNIT         default campbell-serial-bridge
#   CONFIG_OUT   default /etc/ser2net/<UNIT without -serial-bridge>.yaml
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DAEMON=${DAEMON:-remoteweather}
CONFIG_DB=${CONFIG_DB:-/var/lib/remoteweather/config.db}
BRIDGE_ADDR=${BRIDGE_ADDR:-127.0.0.1}
BRIDGE_PORT=${BRIDGE_PORT:-7001}
UNIT=${UNIT:-campbell-serial-bridge}
CONFIG_OUT=${CONFIG_OUT:-/etc/ser2net/${UNIT%-serial-bridge}.yaml}
BRIDGE_USER=ser2net

die()   { printf '\n  x %s\n' "$*" >&2; exit 1; }
note()  { printf '    %s\n' "$*"; }
ok()    { printf '  + %-26s %s\n' "$1" "${2-}"; }
warn()  { printf '  ! %-26s %s\n' "$1" "${2-}" >&2; }
head_() { printf '\n%s\n' "$1"; }

find_ser2net() {
  local p
  for p in "${SER2NET:-}" ser2net /usr/bin/ser2net /usr/sbin/ser2net; do
    [ -n "$p" ] || continue
    if command -v "$p" >/dev/null 2>&1; then command -v "$p"; return 0; fi
  done
  return 1
}

daemon_pid() {
  systemctl show -p MainPID --value "$DAEMON" 2>/dev/null | grep -E '^[1-9][0-9]*$' || true
}

# The device the daemon is reading RIGHT NOW. This is the authoritative answer: not what a
# document says and not what a configuration row says, but what the process has open.
device_from_process() {
  local pid=${1:-} fd target
  [ -n "$pid" ] || return 1
  [ -d "/proc/$pid/fd" ] || return 1
  for fd in /proc/"$pid"/fd/*; do
    target=$(readlink "$fd" 2>/dev/null) || continue
    case "$target" in
      /dev/ttyS[0-9]*|/dev/ttyUSB[0-9]*|/dev/ttyACM[0-9]*|/dev/ttyAMA[0-9]*)
        printf '%s\n' "$target"; return 0 ;;
    esac
  done
  return 1
}

sq() { sqlite3 -noheader "$CONFIG_DB" "$@" 2>/dev/null; }

resolve_station() {
  local names count
  if [ -n "${STATION:-}" ]; then printf '%s' "$STATION"; return 0; fi
  names=$(sq "SELECT name FROM devices WHERE type = 'campbellscientific' AND enabled;") || return 1
  count=$(printf '%s' "$names" | grep -c . || true)
  [ "$count" = "1" ] || return 1
  printf '%s' "$names"
}

# A /dev/serial/by-id symlink for `dev`, which is the path to CONFIGURE for anything on USB:
# /dev/ttyUSB* numbering is not stable across a replug, and gensio follows a symlink (checked).
stable_path_for() {
  local dev=$1 link
  [ -d /dev/serial/by-id ] || return 1
  for link in /dev/serial/by-id/*; do
    [ -e "$link" ] || continue
    if [ "$(readlink -f "$link")" = "$(readlink -f "$dev")" ]; then
      printf '%s\n' "$link"; return 0
    fi
  done
  return 1
}

# The live line rate, read while the daemon still holds the port.
#
# `stty` opens the device, and that is only safe BECAUSE the daemon has it open: a tty sends a
# hangup when its LAST descriptor closes, and this is not the last. Run after the daemon has
# let go and the same call would toggle the line on a station nothing is reading -- which is
# one more reason `check` is a before-you-change-anything command.
baud_from_line() {
  local dev=$1
  command -v stty >/dev/null || return 1
  stty -F "$dev" 2>/dev/null | sed -n 's/^speed \([0-9]\+\) baud.*/\1/p' | head -1
}

discover() {
  SER2NET_BIN=$(find_ser2net) || die "ser2net is not on PATH. Arch: pacman -S ser2net"
  ok "ser2net" "$("$SER2NET_BIN" -v 2>&1 | head -1)"

  PID=$(daemon_pid)
  if [ -n "$PID" ]; then
    ok "$DAEMON" "running, pid $PID"
  else
    warn "$DAEMON" "not running -- the live device and rate cannot be read from it"
  fi

  DEV_PROC=$(device_from_process "$PID" || true)
  DEV_DB=""; BAUD_DB=""; STATION_NAME=""
  if [ -r "$CONFIG_DB" ] && command -v sqlite3 >/dev/null; then
    STATION_NAME=$(resolve_station || true)
    if [ -n "$STATION_NAME" ]; then
      esc=${STATION_NAME//\'/\'\'}
      DEV_DB=$(sq "SELECT ifnull(serial_device,'') FROM devices WHERE name='$esc';")
      BAUD_DB=$(sq "SELECT ifnull(baud,0) FROM devices WHERE name='$esc';")
      ok "station row" "$STATION_NAME"
    else
      warn "station row" "no single enabled campbellscientific row -- set STATION"
    fi
  else
    warn "config database" "$CONFIG_DB unreadable or sqlite3 absent -- that source skipped"
  fi

  # A disagreement between the two sources STOPS, rather than picking one and warning. Every
  # way this can be wrong is a quiet one, so the only safe thing to do with an ambiguity is
  # to hand it back: `DEVICE=` on the command line is how you say which answer is right.
  if [ -z "${DEVICE:-}" ] && [ -n "$DEV_PROC" ] && [ -n "$DEV_DB" ] &&
     [ "$(readlink -f "$DEV_PROC")" != "$(readlink -f "$DEV_DB")" ]; then
    warn "device MISMATCH" "the process has $DEV_PROC, the row says $DEV_DB"
    note "The process is what is actually being read, so it is probably the right one -- but"
    note "this will not choose for you. Re-run with DEVICE=$DEV_PROC once you agree, and fix"
    note "the configuration row so the next reader of it is not misled."
    die "the device is ambiguous"
  fi
  DEVICE=${DEVICE:-${DEV_PROC:-$DEV_DB}}
  if [ -z "$DEVICE" ]; then
    head_ "Serial devices on this machine:"
    ls -l /dev/serial/by-id/ 2>/dev/null || note "no /dev/serial/by-id (nothing on USB serial)"
    ls -l /dev/ttyS[0-9]* /dev/ttyUSB* /dev/ttyACM* 2>/dev/null || true
    die "could not determine the station's device. Pick one above and re-run with DEVICE=..."
  fi
  [ -e "$DEVICE" ] || die "DEVICE=$DEVICE does not exist"

  CONFIGURE_AS=$DEVICE
  if STABLE=$(stable_path_for "$DEVICE"); then
    CONFIGURE_AS=$STABLE
    ok "device" "$DEVICE  ->  configuring as $STABLE"
    note "the by-id symlink, because /dev/ttyUSB* numbering moves on a replug"
  else
    ok "device" "$DEVICE"
    case "$DEVICE" in
      /dev/ttyUSB*|/dev/ttyACM*)
        warn "no by-id symlink" "USB adapter with no stable name; a replug may renumber it" ;;
    esac
  fi
  DEV_GROUP=$(stat -c %G "$DEVICE" 2>/dev/null || echo '?')
  ok "owning group" "$DEV_GROUP"

  BAUD_LIVE=""
  if [ -n "$PID" ] && [ -n "$DEV_PROC" ]; then
    BAUD_LIVE=$(baud_from_line "$DEVICE" || true)
  fi
  if [ -z "${BAUD:-}" ] && [ -n "$BAUD_LIVE" ] && [ -n "$BAUD_DB" ] && [ "$BAUD_DB" != "0" ] &&
     [ "$BAUD_LIVE" != "$BAUD_DB" ]; then
    warn "line rate MISMATCH" "the line is at $BAUD_LIVE, the row says $BAUD_DB"
    note "The live line is what the daemon actually set, so it is probably the right one."
    note "Re-run with BAUD=$BAUD_LIVE once you agree. This will not choose for you, because a"
    note "wrong rate does not go silent -- it frames every byte wrong and every record then"
    note "fails to parse, which looks like a dead station rather than a misconfigured one."
    die "the line rate is ambiguous"
  fi
  if [ -z "${BAUD:-}" ]; then
    if [ -n "$BAUD_LIVE" ]; then
      BAUD=$BAUD_LIVE; BAUD_SRC="read off the live line"
    elif [ -n "$BAUD_DB" ] && [ "$BAUD_DB" != "0" ]; then
      BAUD=$BAUD_DB; BAUD_SRC="from the configuration row"
    else
      die "could not determine the line rate. Re-run with BAUD=..."
    fi
  else
    BAUD_SRC="given on the command line"
  fi
  ok "line rate" "$BAUD ($BAUD_SRC)"

  # Parsed rather than filtered: `ss`'s filter grammar is easy to get subtly wrong, and a
  # filter that silently matches nothing would turn this guard off without saying so.
  if command -v ss >/dev/null &&
     ss -ltnH 2>/dev/null | awk '{print $4}' | grep -qE "[:.]$BRIDGE_PORT\$"; then
    die "something is already listening on port $BRIDGE_PORT. Set BRIDGE_PORT."
  fi
  ok "listener" "$BRIDGE_ADDR:$BRIDGE_PORT"
  [ "$BRIDGE_ADDR" = "127.0.0.1" ] ||
    warn "not loopback" "anything that reaches $BRIDGE_ADDR:$BRIDGE_PORT can also WRITE to the station"

  if systemctl is-enabled ser2net.service >/dev/null 2>&1; then
    warn "distro ser2net.service" "enabled -- it will fight this unit for the device"
    note "systemctl disable --now ser2net.service"
  fi
  if systemctl is-active "$UNIT.service" >/dev/null 2>&1; then
    warn "$UNIT" "already running; 'stage' will not touch it"
  fi
}

render() {
  sed -e "s|<DEVICE>|$CONFIGURE_AS|" \
      -e "s|<BAUD>|$BAUD|" \
      -e "s|^  accepter: tcp,127\.0\.0\.1,7001\$|  accepter: tcp,$BRIDGE_ADDR,$BRIDGE_PORT|" \
      -e "s|^# EXAMPLE\..*\$|# Generated by packaging/serial-bridge/install.sh on $(date -Is). Edit freely.|" \
      "$HERE/ser2net.yaml.example"
}

# Parse the rendered config with the real binary, with no client and therefore no device
# opened: ser2net checks the connector string -- the baud and framing token included -- at
# config-parse time, and reports a bad one on stderr WHILE STILL LISTENING. That last part is
# why this greps the output rather than trusting the exit status.
#
# What it cannot catch is a rate that is valid but wrong. Nothing here can; that is what the
# two independent sources above are for.
validate() {
  local cfg=$1 log rc=0
  log=$(mktemp)
  timeout 4 "$SER2NET_BIN" -n -d -c "$cfg" >"$log" 2>&1 || rc=$?
  # 124 is `timeout` killing a process that started cleanly, which is the good case.
  if [ "$rc" -ne 124 ] && [ "$rc" -ne 0 ]; then
    sed -n '1,10p' "$log" >&2; rm -f "$log"
    die "ser2net would not start with that configuration"
  fi
  if grep -qiE 'invalid|error|failure' "$log"; then
    grep -iE 'invalid|error|failure' "$log" | head -5 >&2; rm -f "$log"
    die "ser2net rejected that configuration"
  fi
  rm -f "$log"
}

case "${1:-check}" in
check)
  head_ "Discovering this installation's values. Nothing is changed."
  discover
  head_ "The configuration this would write to $CONFIG_OUT:"
  render | grep -vE '^[[:space:]]*#|^$' | sed 's/^/    /'
  head_ "Next:  sudo $0 stage"
  note "then the cutover commands it prints, in that order"
  ;;

stage)
  [ "$(id -u)" = "0" ] || die "stage needs root"
  head_ "Discovering this installation's values."
  discover

  head_ "Staging. $DAEMON keeps reading the station throughout this."
  if id "$BRIDGE_USER" >/dev/null 2>&1; then
    ok "user" "$BRIDGE_USER exists"
  else
    useradd --system --no-create-home \
            --shell "$(command -v nologin || echo /sbin/nologin)" "$BRIDGE_USER"
    ok "user" "$BRIDGE_USER created"
  fi
  if [ "$DEV_GROUP" != '?' ] && [ "$DEV_GROUP" != root ]; then
    usermod -aG "$DEV_GROUP" "$BRIDGE_USER"
    ok "group" "$BRIDGE_USER added to $DEV_GROUP, which owns $DEVICE"
  else
    warn "group" "$DEVICE is group '$DEV_GROUP'; granting access to that is yours to decide"
  fi

  install -d -m 0755 "$(dirname "$CONFIG_OUT")"
  tmp=$(mktemp); render > "$tmp"
  validate "$tmp"
  ok "configuration" "validated by $(basename "$SER2NET_BIN") itself, no device opened"
  if [ -f "$CONFIG_OUT" ] && ! cmp -s "$tmp" "$CONFIG_OUT"; then
    cp -a "$CONFIG_OUT" "$CONFIG_OUT.bak.$(date +%s)"
    note "existing $CONFIG_OUT backed up"
  fi
  install -m 0644 "$tmp" "$CONFIG_OUT"; rm -f "$tmp"
  ok "installed" "$CONFIG_OUT"

  # The unit names no device, so the only edit it needs is the supplementary group.
  tmp=$(mktemp)
  sed -e "s|^SupplementaryGroups=.*|SupplementaryGroups=$DEV_GROUP|" \
      -e "s|^ExecStart=.*|ExecStart=$SER2NET_BIN -n -c $CONFIG_OUT|" \
      "$HERE/$UNIT.service" > "$tmp"
  install -m 0644 "$tmp" "/etc/systemd/system/$UNIT.service"; rm -f "$tmp"
  ok "installed" "/etc/systemd/system/$UNIT.service"
  systemctl daemon-reload
  ok "daemon-reload" ""

  head_ "Staged, and nothing has changed about what reads the station."
  cat <<EOF
    The commands that DO change it are these, and their ORDER is the whole point:
    $DAEMON must let go of $DEVICE BEFORE the bridge starts, or both of them open it
    and the kernel splits the stream between them with no error anywhere.

      ./cutover.sh plan                     # read it
      ./cutover.sh apply                    # writes ./rollback.sql first
      systemctl restart $DAEMON             # gap starts; it dials and retries every 5 s
      systemctl enable --now $UNIT          # gap ends

    Then:  sudo $0 status
    To undo: see "Rollback" in README.md -- the bridge is stopped FIRST.
EOF
  ;;

status)
  SER2NET_BIN=$(find_ser2net) || die "ser2net is not on PATH"
  head_ "The bridge"
  systemctl --no-pager --lines=0 status "$UNIT" 2>&1 | head -5 || true
  head_ "Who has the device"
  bpid=$(systemctl show -p MainPID --value "$UNIT" 2>/dev/null || true)
  for pid in $bpid $(daemon_pid); do
    [ -n "$pid" ] && [ "$pid" != 0 ] || continue
    for fd in /proc/"$pid"/fd/*; do
      t=$(readlink "$fd" 2>/dev/null) || continue
      case "$t" in
        /dev/tty*) printf '    pid %-7s %-24s %s\n' "$pid" \
          "$(tr -d '\0' < /proc/"$pid"/comm 2>/dev/null)" "$t" ;;
      esac
    done
  done
  head_ "Who is connected to the bridge"
  if command -v ss >/dev/null; then
    ss -tnp 2>/dev/null | awk -v p=":$BRIDGE_PORT" 'NR==1 || index($4,p)' | sed 's/^/    /'
  fi
  head_ "Is the station actually reporting"
  note "a process being up is not a reading landing. Check a timestamp:"
  note 'psql -c "SELECT max(time), now() - max(time) AS age FROM weather;"'
  ;;

*)
  die "usage: $0 {check|stage|status}"
  ;;
esac
