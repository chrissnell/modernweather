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
`127.0.0.1:5432/remoteweather` as user `postgres`). It must be the `timescale/timescaledb-ha`
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
8. Replays them again through `contract/tools/rig/mangle_proxy.py`. **At least 26 must fail.**

Step 8 is not a diagnostic. W1 Decision 10 refused to accept a green conformance run without a
negative control, and this gate enforces that: it is green only when the direct replay passes
*and* the mangled one fails.

## The two things that are not reproducible by accident

Both are documented at the point they are handled in `go-target.sh`; they are repeated here
because they are the only places where this harness does something to the rig rather than with
it.

**The emulators' solar curve follows their own wall clock.** Both emit zero solar radiation
outside 06:00–18:00 local, and migration 030 sources the almanac's `high_solar` from the raw
table filtered on `solarwatts > 0`. On a runner whose clock reads 23:00 the metric is absent and
`/almanac?station=davis-full` loses a key `almanac-populated` requires. The emulators therefore
run in a fixed-offset zone picked to put their clock at midday. Nothing else is shifted.

**`refresh_almanac_cache` is scheduled hourly.** If it fires after the air-quality station is
seeded, `/almanac?station=davis-full` grows four air-quality keys the fixture does not have —
`/almanac` reads the extremes of the *website's* air quality device. `freeze-jobs.sql` deletes
that one job after the migrations have run. The other provisioned jobs are deliberately left
alone; they move values, not key sets.

## Watching it fail

A gate nobody has seen fail is `docs/reconciliation.md` G-1 again, so there are two ways to make
this one fail on purpose, and both were exercised before it was relied on:

```sh
CONFORMANCE_FORCE_MANGLE=1 cargo xtask gate conformance-fixtures   # 26 fixtures fail, gate red
MIN_MANGLED_FAILURES=99    cargo xtask gate conformance-fixtures   # negative control too weak, gate red
```

## Files

| File | What it is |
|---|---|
| `go-target.sh` | the harness; the file whose existence arms the gate |
| `config-seed.sql` | the capture rig's devices, websites and remote stations, as data |
| `freeze-jobs.sql` | stops the almanac refresh job from rewriting a seeded snapshot mid-run |
| `nullfill.sql` | the NULL→0 fill `contract/tools/README.md` leaves as a comment |
