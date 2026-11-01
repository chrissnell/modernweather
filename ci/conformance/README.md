# `ci/conformance/` — the frozen contract, replayed against the Go daemon

`DESIGN.md` R-3 splits the conformance suite into two checks against the same 70 fixtures:

| | Target | Proves | Gate |
|---|---|---|---|
| 1 | the **Go** daemon | the fixtures describe reality | `conformance-fixtures` — this directory |
| 2 | the **Rust** daemon | the rewrite matches | `conformance` — deferred until `crates/modernweather/Cargo.toml` exists |

Check 1 has been possible since W1 landed and had never run. `docs/reconciliation.md` G-1 is the
record of what that cost: the `conformance` gate reported green by deferral for five stages while
the suite whose own done-criteria required it to pass had never executed. The existence of
`go-target.sh` is what arms `conformance-fixtures` in `ci/gates.toml`.

## Running it

```sh
cargo xtask gate conformance-fixtures
```

against a TimescaleDB reachable through the standard libpq variables (`PGHOST`, `PGPORT`,
`PGUSER`, `PGPASSWORD`, `PGDATABASE`, `PGSSLMODE`; the defaults are
`127.0.0.1:5432/conformance` as user `postgres`). It must be the `timescale/timescaledb-ha`
image or equivalent: the daemon's startup provisioning uses `timescaledb_toolkit`, which the
plain `timescale/timescaledb` image does not carry (`DESIGN.md` D-2).

The script **drops and recreates** `$PGDATABASE`. Do not point it at anything you care about.

Useful overrides:

| Variable | Effect |
|---|---|
| `RW_REPO` | use an existing `remoteweather` checkout instead of fetching one |
| `RW_COMMIT` | target a different Go commit (and therefore a different contract) |
| `CONFORMANCE_BIN` | use a prebuilt `legacy-conformance` instead of running `cargo build` |
| `WORKDIR` | where the binaries, config database and replay output land (default `target/conformance-go`) |
| `MIN_MANGLED_FAILURES` | how many fixtures the negative control must break (default 26) |
| `KEEP_RUNNING=1` | leave the daemon and emulators up after a green run, to poke at by hand |
| `CONFORMANCE_FORCE_MANGLE=1` | send the **accepted** replay through the mangling proxy, so the gate goes red |

## What the run does

1. Recreates the database and creates `timescaledb` + `timescaledb_toolkit`.
2. Fetches `remoteweather` at the commit the fixtures were captured from and builds `rw`, the
   Davis emulator and the Campbell emulator from it, against the committed vendor tree.
3. Bootstraps a SQLite configuration database with `rw -config` and fills it from
   `config-seed.sql` — seven devices and six websites, chosen by W1 so that every branch of the
   frozen contract is reachable.
4. Starts both emulators and the daemon. The daemon provisions the entire TimescaleDB schema,
   continuous aggregates and functions on first start.
5. Waits for both emulator-driven stations to report, then applies the five migrations the
   daemon does **not** provision. Order matters: those five end by populating `almanac_cache`,
   and the almanac fixtures recorded that snapshot while only the emulator stations had data.
6. Seeds the readings no local driver can produce (`contract/tools/rig/`), tops the live
   stations up to `NOW()` and fills the seeded rows' NULLs (`nullfill.sql`).
7. Replays all 70 fixtures. **All 70 must pass.**
8. Replays them again through `contract/tools/rig/mangle_proxy.py`. **At least 26 must fail, at
   least one must still pass, and none may fail at the transport layer.**

Step 8 is not a diagnostic. W1 Decision 10 refused to accept a green conformance run without a
negative control, and this gate enforces that: it is green only when the direct replay passes
*and* the mangled one fails.

The three-part assertion in step 8 is deliberate, and the last two parts matter as much as the
first. `contract/conformance` reports a connection error as an ordinary `FAIL` line, so a daemon
that died between the two replays — or a proxy leaked from an earlier run holding port 18081 —
would fail all 70 fixtures, clear a bare "at least 26 failed" floor, and report the control held
while having mangled nothing. Requiring that the fixtures the proxy does not touch still pass,
and that no failure reads `request failed:`, is what makes the floor mean something. The floor
itself is a floor rather than an equality on 26 because `mangle_proxy.py`'s `windd` rewrite only
matches an integer-formatted value, so the exact count genuinely varies with live data.

## The things that are not reproducible by accident

The first two are documented at the point they are handled in `go-target.sh`; they are repeated
here because they are the only places where this harness does something to the rig rather than
with it. Both are also recorded in `docs/legacy-api-contract.md` §10, because they are properties
of W1's capture rig rather than of this harness — follow `contract/tools/README.md`'s procedure
by hand without them and you capture a fixture set this suite would reject.

**The emulators' solar curve follows their own wall clock.** Both emit zero solar radiation
outside 06:00–18:00 local, and migration 030 sources the almanac's `high_solar` from the raw
table filtered on `solarwatts > 0`. On a runner whose clock reads 23:00 the metric is absent and
`/almanac?station=davis-full` loses a key `almanac-populated` requires. The emulators therefore
run in a fixed-offset zone picked to put their clock at midday. Nothing else is shifted.

**`refresh_almanac_cache` is scheduled hourly.** If it fires after the air-quality station is
seeded, `/almanac?station=davis-full` goes from 8 keys to 14 — `/almanac` reads the extremes of
the *website's* air quality device, and two of the six extra keys are derived from the other
four. Watched, not assumed: firing the refresh by hand against a seeded rig breaks two fixtures,
`almanac-populated` and `almanac-empty`, the second because those keys arrive from the website's
device no matter which station was asked about. `freeze-jobs.sql` deletes that one job after the migrations have
run. Migration 005 schedules it with `initial_start => NOW()`, so the first fire is at migration
time and a minutes-long CI run was never going to race it; what this removes is the unbounded
case — a `KEEP_RUNNING=1` session, or a hand-driven rig that outlives the hour. The other
provisioned jobs are deliberately left alone; they move values, not key sets.

**`/latest`'s `sunrise` and `sunset` are formatted in the runner's local zone.** `devices.timezone`
is left unset, so the captured `"7:30 AM"` / `"6:02 PM"` strings are not reproducible by this rig.
That is harmless — only the keys' presence and type are asserted, never their values — but it is
the same class of fact as the two above, and the keys *are* presence-asserted: a device at 0,0
gets no sun table and loses both keys, which is why every device a `/latest` fixture touches
carries real coordinates.

## Watching it fail

A gate nobody has seen fail is `docs/reconciliation.md` G-1 again, so there are two ways to make
this one fail on purpose, and both were exercised before it was relied on:

```sh
CONFORMANCE_FORCE_MANGLE=1 cargo xtask gate conformance-fixtures   # 26 fixtures fail, gate red
MIN_MANGLED_FAILURES=99    cargo xtask gate conformance-fixtures   # negative control too weak, gate red
```

The transport-failure guard was watched failing the same way, against real runner output: point
`legacy-conformance` at a port nothing is listening on and it emits 70 `FAIL` lines, every one of
them `request failed: … Connection refused`. A bare floor accepts that as "70 fixtures failed";
the guard rejects it.

## Files

| File | What it is |
|---|---|
| `go-target.sh` | the harness; the file whose existence arms the gate |
| `config-seed.sql` | the capture rig's devices, websites and remote stations, as data |
| `freeze-jobs.sql` | stops the almanac refresh job from rewriting a seeded snapshot mid-run |
| `nullfill.sql` | the NULL→0 fill `contract/tools/README.md` leaves as a comment |
