#!/usr/bin/env bash
#
# Replay the frozen legacy REST fixtures against the GO daemon, with a negative control.
#
# This is the half of DESIGN.md R-3 that has been possible since W1 landed and had never run:
# pointing contract/conformance/ at the server the fixtures were captured from proves the 70
# fixtures describe reality rather than describing what the rewrite happens to do. The other
# half -- replaying against the Rust daemon -- is the `conformance` gate, still deferred until
# crates/modernweather/Cargo.toml exists.
#
# The existence of this file arms the `conformance-fixtures` gate in ci/gates.toml.
#
# W1 Decision 10 is why this is not just "replay and assert green": a green conformance run is
# only accepted alongside a passing negative control. So the script does two replays and is
# green only when the first one passes and the second one FAILS:
#
#   1. direct    -> http://127.0.0.1:18080, the daemon. Must be 70 passed, 0 failed, 0 skipped.
#   2. mangled   -> http://127.0.0.1:18081, contract/tools/rig/mangle_proxy.py in front of the
#                   same daemon, introducing four realistic regressions. Must fail, and must
#                   fail on at least MIN_MANGLED_FAILURES fixtures.
#
# Set CONFORMANCE_FORCE_MANGLE=1 to send the *accepted* replay through the proxy instead. That
# is how to watch this gate go red on demand, which the programme requires of any assertion
# before it is relied on (docs/reconciliation.md G-1).
#
# Everything the run needs is either in this repository or at a pinned commit of the Go one.
# Nothing reaches production; DESIGN.md OQ-1 records that production is not reachable from an
# agent runtime, and the fixtures were never captured from it.
#
# Inputs, all optional:
#   PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE PGSSLMODE   libpq-style target (see defaults)
#   RW_REPO        path to an existing remoteweather checkout; cloned at RW_COMMIT if unset
#   RW_COMMIT      the pinned Go commit (default: the one the fixtures were captured from)
#   WORKDIR        scratch directory (default: target/conformance-go)
#   CONFORMANCE_BIN  prebuilt legacy-conformance binary; built with cargo if unset
#   KEEP_RUNNING=1 leave the daemon and emulators up after a successful run, for poking at
#   CONFORMANCE_FORCE_MANGLE=1  see above

set -euo pipefail

# The commit contract/tools/README.md records as the source of every fixture. A different commit
# is a different contract, so this is pinned rather than tracking a branch.
RW_COMMIT=${RW_COMMIT:-c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf}
RW_URL=https://github.com/chrissnell/remoteweather.git

# contract/tools/rig/mangle_proxy.py hard-codes both of these: it forwards to 18080 and listens
# on 18081. ci/conformance/config-seed.sql puts the daemon on 18080 to match.
REST_PORT=18080
MANGLE_PORT=18081

# contract/README.md: "26 of the 70 fixtures must fail" through the proxy. Asserted as a floor
# rather than an equality because the proxy's rewrites are keyed on response content -- a run
# whose live data happens to omit a mangled field would legitimately fail one fewer. A floor
# still catches the failure mode that matters, a negative control that stopped biting.
MIN_MANGLED_FAILURES=${MIN_MANGLED_FAILURES:-26}

EXPECTED_FIXTURES=70

PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5432}
PGUSER=${PGUSER:-postgres}
PGPASSWORD=${PGPASSWORD:-}
PGDATABASE=${PGDATABASE:-remoteweather}
PGSSLMODE=${PGSSLMODE:-disable}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
WORKDIR=${WORKDIR:-$repo_root/target/conformance-go}
rig=$repo_root/contract/tools/rig
here=$repo_root/ci/conformance

mkdir -p "$WORKDIR"
log_dir=$WORKDIR/logs
mkdir -p "$log_dir"

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------------------
# Process lifecycle. Everything this script starts is killed on the way out, including on
# failure: a leaked emulator holds port 22222 and makes the next run fail for the wrong reason.
# ---------------------------------------------------------------------------------------
pids=()
cleanup() {
    local rc=$?
    if [ "$rc" -eq 0 ] && [ "${KEEP_RUNNING:-0}" = "1" ]; then
        say "KEEP_RUNNING=1: leaving ${#pids[@]} process(es) up. Logs in $log_dir"
        return 0
    fi
    local pid
    for pid in "${pids[@]:-}"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    return 0
}
trap cleanup EXIT

start_bg() {
    local name=$1; shift
    "$@" >"$log_dir/$name.log" 2>&1 &
    local pid=$!
    pids+=("$pid")
    printf '    started %s (pid %s), log %s\n' "$name" "$pid" "$log_dir/$name.log"
}

# Poll until `cmd` succeeds. Used instead of a fixed sleep so a slow runner is slow rather
# than broken, and a genuinely dead process fails with the thing we were waiting for named.
await() {
    local what=$1 seconds=$2; shift 2
    local waited=0
    until "$@" >/dev/null 2>&1; do
        waited=$((waited + 1))
        if [ "$waited" -ge "$seconds" ]; then
            die "timed out after ${seconds}s waiting for $what"
        fi
        sleep 1
    done
    printf '    %s ready after %ss\n' "$what" "$waited"
}

# ---------------------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------------------
# Named up front rather than discovered halfway through a four-minute run. `psql` and `sqlite3`
# in particular are easy to assume: one is the only way to reach the hypertable, the other is
# the only way to write the daemon's configuration.
required=(go git psql sqlite3 python3 curl)
[ -n "${CONFORMANCE_BIN:-}" ] || required+=(cargo)
missing=()
for tool in "${required[@]}"; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
[ "${#missing[@]}" -eq 0 ] || die "missing required tool(s): ${missing[*]}"

# ---------------------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------------------
uri_for() {
    local db=$1 auth=$PGUSER
    [ -n "$PGPASSWORD" ] && auth="$PGUSER:$PGPASSWORD"
    printf 'postgresql://%s@%s:%s/%s?sslmode=%s' "$auth" "$PGHOST" "$PGPORT" "$db" "$PGSSLMODE"
}
ADMIN_URI=$(uri_for postgres)
TARGET_URI=$(uri_for "$PGDATABASE")

psql_q() { psql -v ON_ERROR_STOP=1 -q "$@"; }

say "Waiting for PostgreSQL at $PGHOST:$PGPORT"
await "postgres" 120 psql -q "$ADMIN_URI" -c 'select 1'

say "Recreating database $PGDATABASE"
# A fresh database rather than a truncate: the daemon provisions hypertables, continuous
# aggregates, retention policies and ~40 functions on startup, and the fixtures depend on that
# provisioning having happened exactly once over an empty schema. Re-running the script must
# not depend on what the last run left behind.
psql_q "$ADMIN_URI" -c "DROP DATABASE IF EXISTS $PGDATABASE WITH (FORCE)"
psql_q "$ADMIN_URI" -c "CREATE DATABASE $PGDATABASE"
# DESIGN.md D-2 requires TimescaleDB with timescaledb_toolkit, which is why CI runs the -ha
# image. The daemon does not create its own extensions.
psql_q "$TARGET_URI" -c "CREATE EXTENSION IF NOT EXISTS timescaledb"
psql_q "$TARGET_URI" -c "CREATE EXTENSION IF NOT EXISTS timescaledb_toolkit"
psql -At "$TARGET_URI" -c \
    "select 'TimescaleDB ' || extversion from pg_extension where extname = 'timescaledb'"

# ---------------------------------------------------------------------------------------
# The Go tree, at the recorded commit
# ---------------------------------------------------------------------------------------
if [ -n "${RW_REPO:-}" ]; then
    go_repo=$RW_REPO
    say "Using the remoteweather checkout at $go_repo"
else
    go_repo=$WORKDIR/remoteweather
    say "Fetching remoteweather @ ${RW_COMMIT:0:7}"
    if [ ! -d "$go_repo/.git" ]; then
        git init -q "$go_repo"
        git -C "$go_repo" remote add origin "$RW_URL"
    fi
    git -C "$go_repo" fetch -q --depth 1 origin "$RW_COMMIT"
    git -C "$go_repo" checkout -q --detach FETCH_HEAD
fi
have=$(git -C "$go_repo" rev-parse HEAD)
[ "$have" = "$RW_COMMIT" ] || die "remoteweather is at $have, expected $RW_COMMIT"

say "Building the daemon and the two station emulators"
bin=$WORKDIR/bin
mkdir -p "$bin"
# -mod=vendor against the committed vendor tree, and GOTOOLCHAIN=local so the build uses the
# toolchain the runner installed rather than downloading the one go.mod names.
(
    cd "$go_repo"
    export GOTOOLCHAIN=local GOFLAGS=-mod=vendor
    go build -o "$bin/rw" ./cmd/remoteweather
    go build -o "$bin/davis-emulator" ./cmd/davis-emulator
    go build -o "$bin/campbell-emulator" ./cmd/campbell-emulator
)
"$bin/rw" -version

# ---------------------------------------------------------------------------------------
# The conformance runner
# ---------------------------------------------------------------------------------------
if [ -n "${CONFORMANCE_BIN:-}" ]; then
    runner=$CONFORMANCE_BIN
else
    say "Building the conformance runner"
    cargo build --release --locked --manifest-path "$repo_root/contract/conformance/Cargo.toml"
    runner=$repo_root/contract/conformance/target/release/legacy-conformance
fi
[ -x "$runner" ] || die "conformance runner not executable: $runner"

# ---------------------------------------------------------------------------------------
# Configuration database
# ---------------------------------------------------------------------------------------
say "Bootstrapping the SQLite configuration database"
config_db=$WORKDIR/config.db
rm -f "$config_db"
# `rw` creates the configuration schema when the file does not exist and then goes on to run the
# application, so there is no bootstrap-only mode to invoke. Start it, wait for the schema, stop
# it. The schema is written before the application starts, so this is ordered, not raced.
start_bg bootstrap "$bin/rw" -config "$config_db"
bootstrap_pid=${pids[-1]}
# Probing a table rather than sqlite_master: a `select ... from sqlite_master` returning no rows
# still exits 0, so it would have reported ready against an empty file.
await "the configuration schema" 60 \
    sqlite3 -bail "$config_db" "select count(*) from weather_websites"
kill "$bootstrap_pid" 2>/dev/null || true
wait "$bootstrap_pid" 2>/dev/null || true
unset 'pids[-1]'

say "Seeding the configuration database"
# -bail throughout: without it the sqlite3 CLI prints "Parse error" and exits 0, so a seed that
# created nothing would look like a seed that worked.
sqlite3 -bail "$config_db" < "$here/config-seed.sql"
# The storage backend is the one row that cannot be committed: it carries the DSN of whatever
# database this run was pointed at.
sqlite3 -bail "$config_db" "INSERT INTO storage_configs
    (config_id, backend_type, enabled, timescale_host, timescale_port,
     timescale_database, timescale_user, timescale_password, timescale_ssl_mode)
  VALUES (1, 'timescaledb', 1,
     '$PGHOST', $PGPORT, '$PGDATABASE', '$PGUSER', '$PGPASSWORD', '$PGSSLMODE');"
sqlite3 -bail "$config_db" "select 'devices: ' || count(*) from devices
  union all select 'websites: ' || count(*) from weather_websites
  union all select 'remote stations: ' || count(*) from remote_stations;"

# ---------------------------------------------------------------------------------------
# Stations
# ---------------------------------------------------------------------------------------
# Both emulators model solar radiation from their own wall clock and emit zero outside
# 06:00-18:00 LOCAL (cmd/davis-emulator/main.go:177, cmd/campbell-emulator/main.go:98). That is
# not cosmetic: migration 030's refresh_almanac_cache_single sources high_solar from the raw
# weather table filtered on `solarwatts > 0`, so on a runner whose clock reads 23:00 the metric
# is simply absent and /almanac?station=davis-full loses a key the fixture requires. CI fires at
# every hour of the day, so left alone this gate would be green for half of each one.
#
# The emulators therefore run in a fixed-offset zone chosen to put their own clock at midday.
# Etc/GMT+N rather than a POSIX "GMT+N" string on purpose: Go resolves TZ as a zoneinfo NAME and
# silently falls back to UTC when it does not resolve, which would look like this working.
# Etc/GMT's sign is inverted from the offset it names, hence the swap below.
#
# Nothing else is shifted. The daemon keeps the runner's clock, because every other time-derived
# value on the frozen surface -- sunrise, sunset, moon phase, potentialsolarwatts -- is asserted
# as a type and not as a value.
emulator_tz=$(
    k=$(( 12 - $(date -u +%-H) ))
    if [ "$k" -ge 0 ]; then printf 'Etc/GMT-%d' "$k"; else printf 'Etc/GMT+%d' "$(( -k ))"; fi
)
say "Starting the station emulators (TZ=$emulator_tz, putting their solar curve at midday)"
start_bg davis-emulator env TZ="$emulator_tz" "$bin/davis-emulator" -port 22222
start_bg campbell-emulator env TZ="$emulator_tz" "$bin/campbell-emulator" -port 8123 -interval 2s
await "the Davis emulator" 30 grep -q "listening on port 22222" "$log_dir/davis-emulator.log"
await "the Campbell emulator" 30 grep -q "Listening on port 8123" "$log_dir/campbell-emulator.log"

say "Starting the daemon (it provisions the whole TimescaleDB schema on first start)"
start_bg remoteweather "$bin/rw" -config "$config_db"
await "the REST server" 120 \
    curl -fsS -o /dev/null -H 'Host: localhost' "http://127.0.0.1:$REST_PORT/stationinfo"

# The Davis driver stops polling after one successful 20-packet LOOP batch
# (davis/station.go:200, recorded as a finding in docs/legacy-api-contract.md §10), so this is
# waiting for a batch that will not be repeated rather than for a steady stream.
#
# `solarwatts > 0` is part of the condition rather than a separate check: it is the one cheap
# assertion that the emulator timezone above actually took effect. If Go had ignored TZ the rows
# would still arrive, and the run would then fail forty steps later on a missing almanac key.
readings_ready() {
    [ "$(psql -At "$TARGET_URI" -c "
        select count(*) = 2 from (
            select stationname
              from weather
             where stationname in ('davis-full', 'csi-minimal')
               and solarwatts > 0
             group by stationname
            having count(*) >= 8
        ) s")" = t ]
}
say "Waiting for both emulator-driven stations to have written readings"
await "emulator readings" 180 readings_ready

# ---------------------------------------------------------------------------------------
# The rig, in the order contract/tools/README.md records
# ---------------------------------------------------------------------------------------
# Step 5: the five tables the daemon does NOT create. Only these five: `migrate -command up`
# over the whole migrations/weather directory fails at 001 with "function
# update_rainfall_summary() is not unique", because the daemon's startup provisioning already
# created a two-argument form. That collision is itself a W1 finding
# (docs/legacy-api-contract.md §10) and is why the migrations are applied by name.
#
# Order matters beyond dependency: 005/028/029/030 each end by populating almanac_cache, and
# they run HERE -- after the emulators have reported, before anything else is seeded -- because
# that is the state the almanac fixtures recorded. See ci/conformance/freeze-jobs.sql.
say "Applying the five migrations the daemon does not provision"
for m in 005_create_almanac_cache \
         028_complete_almanac_metrics \
         029_almanac_store_pm_only \
         030_almanac_airgradient_pm10 \
         025_create_snow_depth_est; do
    printf '    %s\n' "$m"
    psql_q "$TARGET_URI" -f "$go_repo/migrations/weather/$m.up.sql" >/dev/null
done

psql_q "$TARGET_URI" -f "$here/freeze-jobs.sql" >/dev/null
say "almanac_cache, frozen over emulator-only data"
psql -At "$TARGET_URI" -c \
    "select stationname || '  ' || metric_name from almanac_cache order by 1"

# Step 6: the data the local drivers cannot produce.
say "Seeding the readings no emulator can produce"
for f in seed.sql backfill.sql snowstale.sql aeris.sql; do
    printf '    %s\n' "$f"
    psql_q "$TARGET_URI" -f "$rig/$f" >/dev/null
done

# Step 7: make the seeded stations look like rows the daemon wrote, and move the live stations'
# newest reading up to NOW() so /latest's ten-minute window hits.
say "Topping the live stations up and filling the seeded rows' NULLs"
psql_q "$TARGET_URI" -f "$rig/topup.sql" >/dev/null
psql_q "$TARGET_URI" -f "$here/nullfill.sql" >/dev/null
psql -At -F'  rows=' "$TARGET_URI" -c \
    "select stationname, count(*) from weather group by stationname order by stationname"

# ---------------------------------------------------------------------------------------
# Replay 1: the accepted run
# ---------------------------------------------------------------------------------------
direct_url=http://127.0.0.1:$REST_PORT
if [ "${CONFORMANCE_FORCE_MANGLE:-0}" = "1" ]; then
    say "CONFORMANCE_FORCE_MANGLE=1: sending the accepted replay through mangle_proxy.py"
    start_bg mangle-proxy-forced python3 "$rig/mangle_proxy.py"
    await "the mangling proxy" 30 bash -c "exec 3<>/dev/tcp/127.0.0.1/$MANGLE_PORT"
    direct_url=http://127.0.0.1:$MANGLE_PORT
fi

say "Replaying the fixtures against the Go daemon ($direct_url)"
direct_out=$WORKDIR/direct.txt
direct_rc=0
"$runner" --base-url "$direct_url" > "$direct_out" 2>&1 || direct_rc=$?
cat "$direct_out"

summary=$(tail -n 1 "$direct_out")
want="$EXPECTED_FIXTURES passed, 0 failed, 0 skipped"
[ "$direct_rc" -eq 0 ] || die "the replay against the Go daemon failed (exit $direct_rc)"
[ "$summary" = "$want" ] || die "expected '$want', got '$summary'"
say "ACCEPTED: $summary"

# ---------------------------------------------------------------------------------------
# Replay 2: the negative control (W1 Decision 10)
# ---------------------------------------------------------------------------------------
# The same fixtures, the same daemon, through a proxy that drops a required key, writes a
# decimal where Go writes an integer, retypes `ts` and eats the trailing newline. A suite that
# cannot be made to fail proves nothing, so this replay failing is a precondition for the run
# being accepted -- not a diagnostic.
say "Negative control: replaying through contract/tools/rig/mangle_proxy.py"
start_bg mangle-proxy python3 "$rig/mangle_proxy.py"
await "the mangling proxy" 30 bash -c "exec 3<>/dev/tcp/127.0.0.1/$MANGLE_PORT"

mangled_out=$WORKDIR/mangled.txt
mangled_rc=0
"$runner" --base-url "http://127.0.0.1:$MANGLE_PORT" > "$mangled_out" 2>&1 || mangled_rc=$?

mangled_summary=$(tail -n 1 "$mangled_out")
mangled_failures=$(grep -c '^FAIL ' "$mangled_out" || true)
printf '    %s\n' "$mangled_summary"

if [ "$mangled_rc" -eq 0 ]; then
    sed -n '1,40p' "$mangled_out"
    die "the negative control PASSED. The runner is not asserting anything: a conformance
    suite that cannot be made to fail proves nothing (W1 Decision 10). Full output in
    $mangled_out"
fi
if [ "$mangled_failures" -lt "$MIN_MANGLED_FAILURES" ]; then
    die "the negative control failed only $mangled_failures fixtures, expected at least
    $MIN_MANGLED_FAILURES (contract/README.md). Something stopped asserting. Full output in
    $mangled_out"
fi
say "NEGATIVE CONTROL HELD: $mangled_failures fixtures failed through the proxy"

say "conformance-fixtures: green. $EXPECTED_FIXTURES/$EXPECTED_FIXTURES against the Go daemon, $mangled_failures failing through the negative control."
