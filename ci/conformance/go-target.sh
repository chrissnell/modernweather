#!/usr/bin/env bash
#
# Replay the frozen legacy REST fixtures against the GO daemon, with a negative control.
#
# This is the half of DESIGN.md R-3 that has been possible since W1 landed and had never run:
# pointing contract/conformance/ at the server the fixtures were captured from proves the 70
# fixtures describe reality rather than describing what the rewrite happens to do. The other
# half -- replaying against the Rust daemon -- is the `conformance` gate, which M4-2 turned
# green.
#
# This half demands 70 passed, 0 failed, 0 skipped and passes NO --deviation: the Go daemon is
# what shipped, so it is measured against the bytes it produced. The Rust half applies DEV-10
# and judges per class. Two targets, two recordings, both gates biting.
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
#   RW_BRANCH      branch to deepen from if the server will not serve a bare SHA (default master)
#   WORKDIR        scratch directory (default: target/conformance-go)
#   CONFORMANCE_BIN  prebuilt legacy-conformance binary; built with cargo if unset
#   MIN_MANGLED_FAILURES  how many fixtures the negative control must break (default 26)
#   KEEP_RUNNING=1 leave the daemon and emulators up after a successful run, for poking at
#   CONFORMANCE_FORCE_MANGLE=1  see above

set -euo pipefail

# The commit contract/tools/README.md records as the source of every fixture. A different commit
# is a different contract, so this is pinned rather than tracking a branch.
RW_COMMIT=${RW_COMMIT:-c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf}
RW_URL=https://github.com/chrissnell/remoteweather.git
# Only used to deepen the fetch when the server will not serve a bare SHA; see fetch_pinned.
RW_BRANCH=${RW_BRANCH:-master}

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
# Deliberately not `remoteweather`: the database is dropped below, and a repo whose sibling
# checkout is named remoteweather is exactly where someone keeps a local database by that name.
PGDATABASE=${PGDATABASE:-conformance}
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
    # SIGTERM, then SIGKILL. An unbounded `wait` here would hang the whole run on one process
    # that declines to exit, and a leaked emulator owns port 22222 for the next run.
    local waited=0
    while [ "$waited" -lt 10 ]; do
        local alive=0
        for pid in "${pids[@]:-}"; do
            [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && alive=1
        done
        [ "$alive" -eq 0 ] && break
        sleep 1
        waited=$((waited + 1))
    done
    for pid in "${pids[@]:-}"; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
    done
    return 0
}
trap cleanup EXIT

# Publishes the new pid as $LAST_BG_PID. Callers that need it read that rather than $!, which
# any intervening command in this function could otherwise invalidate.
LAST_BG_PID=
start_bg() {
    local name=$1; shift
    "$@" >"$log_dir/$name.log" 2>&1 &
    LAST_BG_PID=$!
    pids+=("$LAST_BG_PID")
    printf '    started %s (pid %s), log %s\n' "$name" "$LAST_BG_PID" "$log_dir/$name.log"
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

# Fetch the pinned commit, escalating only as far as the server makes necessary.
#
# `git fetch <sha>` is the cheap and obviously-correct call, and github.com serves it only
# sometimes: this gate's first CI run fetched c3ec9f7 by SHA and its second was refused with
# "upload-pack: not our ref", for the same commit on the same public repository. (A local test
# proves nothing here -- this runtime rewrites github.com URLs to an on-disk mirror, which always
# allows it.) So try the SHA, then the branch tip, then the branch's full history, and check
# after each whether the object actually arrived. The caller asserts HEAD is the pinned commit
# regardless of which branch got there.
fetch_pinned() {
    local repo=$1
    local have="git -C $repo cat-file -e $RW_COMMIT^{commit}"

    if git -C "$repo" fetch -q --depth 1 origin "$RW_COMMIT" 2>/dev/null && $have 2>/dev/null; then
        printf '    fetched the commit directly\n'; return 0
    fi
    if git -C "$repo" fetch -q --depth 1 origin "$RW_BRANCH" 2>/dev/null && $have 2>/dev/null; then
        printf '    the server would not serve the bare SHA; it is the tip of %s\n' "$RW_BRANCH"; return 0
    fi
    printf '    the commit is neither servable by SHA nor the tip of %s; deepening\n' "$RW_BRANCH"
    local deepen=()
    [ "$(git -C "$repo" rev-parse --is-shallow-repository)" = true ] && deepen=(--unshallow)
    git -C "$repo" fetch -q --no-tags "${deepen[@]}" origin "$RW_BRANCH" || {
        git -C "$repo" remote -v
        die "could not fetch $RW_BRANCH from the remote printed above"
    }
    $have 2>/dev/null \
        || die "$RW_COMMIT is not reachable from $RW_BRANCH on $RW_URL. Set RW_BRANCH to a
    branch that contains it, or RW_REPO to a checkout that already has it."
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

# Every port this run binds, checked before anything is started. A process leaked from an
# earlier run satisfies the readiness probes below while the freshly started one dies on
# "bind: address already in use" -- so the replay would silently measure the stale process.
for port in "$REST_PORT" "$MANGLE_PORT" 22222 8123; do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        die "port $port is already in use. A daemon, emulator or proxy leaked from an earlier
    run would be measured instead of the one this script starts. Find it with
    \`ss -ltnp | grep $port\` and kill it."
    fi
done

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
    # From scratch every time, deliberately. Reusing whatever was in WORKDIR made this step
    # depend on the state a previous run left behind -- and in CI that state arrives from
    # Swatinem/rust-cache, which restores target/ wholesale. The fetch is shallow and takes
    # seconds; RW_REPO is the supported way to reuse a checkout.
    rm -rf "$go_repo"
    git init -q "$go_repo"
    git -C "$go_repo" remote add origin "$RW_URL"
    fetch_pinned "$go_repo"
    # The SHA, not FETCH_HEAD: after either fallback below FETCH_HEAD is the branch tip, which is
    # the pinned commit only until the branch moves. The assertion after this would catch it, but
    # checking out the thing we actually want is better than catching the wrong one.
    git -C "$go_repo" checkout -q --detach "$RW_COMMIT"
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
# it.
#
# What it waits for matters. pkg/config/provider_sqlite.go passes the whole schema to one Exec
# with no enclosing transaction, so the statements auto-commit in order and the LAST ones are
# `INSERT INTO configs` and the default `snowcache` row -- several statements after
# weather_websites is created. Probing controller_configs therefore waits for the end of the
# script rather than the middle of it. (And probing a table rather than sqlite_master because
# `select ... from sqlite_master` returning no rows still exits 0, so it would have reported
# ready against an empty file.)
start_bg bootstrap "$bin/rw" -config "$config_db"
bootstrap_pid=$LAST_BG_PID
await "the configuration schema" 60 \
    sqlite3 -bail "$config_db" "select count(*) from controller_configs"
# SIGTERM is asynchronous, and `rw` holds the configuration database open until it has finished
# shutting down. Seeding straight after the kill therefore raced it: the reads in the await above
# share the lock happily, but config-seed.sql's first write -- `DELETE FROM devices`, line 30 --
# failed with `database is locked (5)`. Wait for the process to be gone before writing, and
# escalate to SIGKILL rather than carrying on into the same race.
kill "$bootstrap_pid" 2>/dev/null || true
# `wait` rather than a `kill -0` poll: it is exact, and on a child that has already exited it
# returns at once instead of waiting out a one-second poll interval. The watchdog is what keeps
# it bounded -- the thing the cleanup() comment warns about is an *unbounded* wait, and 15s of
# SIGTERM being ignored is the escalation point, not a reason to seed into a held lock.
bootstrap_started=$SECONDS
( sleep 15; kill -9 "$bootstrap_pid" 2>/dev/null ) &
bootstrap_watchdog=$!
wait "$bootstrap_pid" 2>/dev/null || true
kill "$bootstrap_watchdog" 2>/dev/null || true
wait "$bootstrap_watchdog" 2>/dev/null || true
printf '    the bootstrap daemon exited after %ss\n' "$((SECONDS - bootstrap_started))"
# Dropped from the cleanup list by value: negative array subscripts need bash 4.3, and this
# script is meant to run on a developer's machine as well as on a runner.
remaining=()
for pid in "${pids[@]:-}"; do
    [ -n "$pid" ] && [ "$pid" != "$bootstrap_pid" ] && remaining+=("$pid")
done
pids=("${remaining[@]:-}")

say "Seeding the configuration database"
# -bail throughout: without it the sqlite3 CLI prints "Parse error" and exits 0, so a seed that
# created nothing would look like a seed that worked.
#
# `.timeout` because the CLI's busy timeout defaults to 0, which turns any lock still held into
# an immediate failure rather than a short wait. Belt to the braces of waiting for the bootstrap
# daemon above: five seconds absorbs a slow shutdown, and a lock nobody releases still fails
# loudly rather than being masked.
sqlite_write() { sqlite3 -bail -cmd '.timeout 5000' "$@"; }
sqlite_write "$config_db" < "$here/config-seed.sql"
# The storage backend is the one row that cannot be committed: it carries the DSN of whatever
# database this run was pointed at.
sqlite_write "$config_db" "INSERT INTO storage_configs
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
# `10#` rather than GNU date's %-H: %H is zero-padded, so bare arithmetic would read 08 and 09
# as invalid octal, and %-H is a GNU extension this script should not need.
emulator_tz=$(
    k=$(( 12 - 10#$(date -u +%H) ))
    if [ "$k" -ge 0 ]; then printf 'Etc/GMT-%d' "$k"; else printf 'Etc/GMT+%d' "$(( -k ))"; fi
)
say "Starting the station emulators (TZ=$emulator_tz, putting their solar curve at midday)"
start_bg davis-emulator env TZ="$emulator_tz" "$bin/davis-emulator" -port 22222
start_bg campbell-emulator env TZ="$emulator_tz" "$bin/campbell-emulator" -port 8123 -interval 2s
await "the Davis emulator" 30 grep -q "listening on port 22222" "$log_dir/davis-emulator.log"
await "the Campbell emulator" 30 grep -q "Listening on port 8123" "$log_dir/campbell-emulator.log"

say "Starting the daemon (it provisions the whole TimescaleDB schema on first start)"
start_bg remoteweather "$bin/rw" -config "$config_db"
daemon_pid=$LAST_BG_PID
# `kill -0` as well as the HTTP probe: a socket answering on 18080 is not evidence that THIS
# daemon is the one answering it. Together with the port preflight above that closes the gap.
daemon_ready() {
    kill -0 "$daemon_pid" 2>/dev/null \
        && curl -fsS -o /dev/null -H 'Host: localhost' "http://127.0.0.1:$REST_PORT/stationinfo"
}
await "the REST server" 120 daemon_ready

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

# "It failed" is not the assertion. The assertion is that it failed ON THE MANGLED FIELDS.
#
# contract/conformance/src/lib.rs reports a connection-level error as an ordinary `FAIL` line
# reading "request failed: ...". So if the daemon had died between the two replays, or if a
# proxy leaked from an earlier run owned 18081 and this one exited with "address in use", every
# fixture would "fail", the floor below would be satisfied, and this script would print
# NEGATIVE CONTROL HELD having proved nothing whatsoever. That is the exact shape of
# docs/reconciliation.md G-1, aimed at the one assertion this work item exists to establish.
#
# Two cheap facts rule it out: the proxy only mangles JSON bodies, so the fixtures it does not
# touch must still PASS, and no failure may be a transport failure.
if grep -q 'request failed:' "$mangled_out"; then
    grep -m 5 'request failed:' "$mangled_out"
    die "the negative control saw transport failures rather than contract failures: the daemon
    or the proxy was not answering, so this run proved nothing. Full output in $mangled_out"
fi
mangled_passed=${mangled_summary%% passed,*}
if ! [ "$mangled_passed" -gt 0 ] 2>/dev/null; then
    die "every fixture failed through the proxy, including the ones it does not mangle. That is
    a broken forward path, not a working negative control. Full output in $mangled_out"
fi
if [ "$mangled_failures" -lt "$MIN_MANGLED_FAILURES" ]; then
    die "the negative control failed only $mangled_failures fixtures, expected at least
    $MIN_MANGLED_FAILURES (contract/README.md). Something stopped asserting. Full output in
    $mangled_out"
fi
say "NEGATIVE CONTROL HELD: $mangled_failures fixtures failed through the proxy, $mangled_passed still passed"

say "conformance-fixtures: green. $EXPECTED_FIXTURES/$EXPECTED_FIXTURES against the Go daemon, $mangled_failures failing through the negative control."
