# The frozen contract, replayed — against the Go daemon here, and against the Rust one

This directory is the Go half. The Rust half is `cargo xtask conformance`, and both are described
here because neither means anything without the other (`DESIGN.md` R-3).

`DESIGN.md` R-3 splits the conformance suite into two checks against the same 70 fixtures:

| | Target | Proves | Gate | Harness |
|---|---|---|---|---|
| 1 | the **Go** daemon | the fixtures describe reality | `conformance-fixtures` | `go-target.sh`, this directory |
| 2 | the **Rust** daemon | the rewrite matches | `conformance` | `cargo xtask conformance` (`xtask/src/conformance.rs`) |

Check 1 has been possible since W1 landed and had never run. `docs/reconciliation.md` G-1 is the
record of what that cost: the `conformance` gate reported green by deferral for five stages while
the suite whose own done-criteria required it to pass had never executed. The existence of
`go-target.sh` is what arms `conformance-fixtures` in `ci/gates.toml`.

**Check 2 is green as of `implementation-plan.md` M4-2**, and back in the `gate` job's `needs` in
`.github/workflows/pr.yml`. It was armed and failing from M2 until then, which was its correct state
and the deliverable of M2-5: the failure was visible from Milestone 2 onward instead of being
discovered in Milestone 4.

**Green does not mean "all 70 passed", and that is a policy rather than a concession.** The fixtures
are classified (`docs/reconciliation.md` §6.4, work item **N-18**) and the classification is **data
the runner reads** — all 70 are still attempted, which is what stops it being a silent filter:

| Class | Count | What the gate does with it |
|---|---|---|
| `frozen` | 42 | every one must pass. A failure is a **contract break** — the iOS client is deployed independently of this server |
| `coupled` | 19 | every one must pass **except** the thirteen whose subject M4-3 owns (the HTML pages and the asset trees), which are excused by a list declared in `xtask/src/conformance.rs` |
| `retired` | 9 | `/alerts`, `/api/remote-stations` and `format=msgpack`, which **D-69** does not carry forward. Reported on every run, never gating |

So a green run today prints `48 passed, 22 failed`. The excuse list is checked in **both**
directions: a fixture excused without a declaration and a declaration whose fixture has started
passing are each exit 2, because the first would make the gate narrower than it says and the second
would leave it excusing a failure that cannot happen.

**The Rust target is replayed with `--deviation DEV-10`.** `/stationinfo` emits `about_text` and
`weather_device` unconditionally, which is `docs/reconciliation.md` **O-5** answered by the owner as
option (b); the two affected fixtures carry the Rust target's response beside the Go server's, and
`go-target.sh` passes no deviation. One fixture holds both halves, which is the only arrangement
under which both gates keep asserting what their own target is supposed to produce.

The two harnesses do the same job in the same order and assert the same things — read
`go-target.sh` before changing either — and the differences are only the ones the target forces:

| | `go-target.sh` | `cargo xtask conformance` |
|---|---|---|
| target | `rw`, built from `remoteweather` @ `c3ec9f7` | `target/debug/modernweather`, built from this tree |
| configuration | `rw -config` + `config-seed.sql`, 7 devices and 6 websites | `migrations/config/`, applied by the daemon, then the `listener` and `weather_database` rows and the same 7 stations and 6 websites (`rust-seed/01-config.sql`) |
| weather data | W1's rig (`contract/tools/rig/`), plus five migrations the Go daemon does not provision | `ci/conformance/rust-seed/` — a translation of that rig onto `observations` and W4's four aggregate levels, plus the three tables the frozen contract reads directly |
| port | 18080, with the emulators on 22222 and 8123 | 18080, which is the port `mangle_proxy.py` forwards to |
| negative control | always, and the run is accepted only if it fails | only once the direct replay is accepted. The floor is **what the proxy broke** rather than how many fixtures failed: 22 fail here whatever the proxy does, so a failure floor would be cleared by a proxy that mangled nothing |
| language | shell, because it must run with no Rust daemon in the tree | Rust, because the verdict — 70 attempted, transport failures, the control's three parts — is then unit-testable without a database |

## Running the Rust target

```sh
cargo xtask gate conformance
```

Same libpq variables as below (the default database is `mw_conformance`), and it needs `psql`,
`sqlite3` and `python3` on `PATH` and a TimescaleDB it may **drop and recreate** `$PGDATABASE` on.
It fails rather than skipping when it cannot have them, for the reason W3 Decision 84 gives about
the database tests: "green because it did not run" has to be unreachable.

That `DROP DATABASE … WITH (FORCE)` takes its name from the environment, so it is guarded rather
than only documented: the run **refuses a `$PGDATABASE` whose name does not contain
`conformance`**. Unset the variable to get the default, or point it at a database whose name says
what it is for.

`cargo xtask conformance --help` lists the knobs. The two that matter when reading a failure are
`--no-build` (replay against the binary already at `--target-binary`) and `--keep-running`, which
leaves the daemon up after any run that reached a verdict — including a red one, which is when you
most want to `curl` it. A run that reached *no* verdict cleans up after itself, because its
processes would only mislead the next one. Replay output and the daemon's log land in
`target/conformance-rust/`.

Its two failure codes are declared in `ci/gates.toml` and mean different things: **1** is "the
replay reached a verdict and the binary does not conform", and **2** is "the run reached no verdict"
— a missing tool, a busy port, a database that would not answer, a daemon that never asserted its
database version, a fixture count that is not 70, a fixture with no `class`, a deviation no fixture
declares, a seed that produced no readings, a disagreement between the excuse list and what actually
failed, failures at the transport layer, or a negative control that did not hold. The split exists
because a dead daemon and a wrong contract both produce 70 failures, and calling the first the
second is the expensive direction for this gate to be wrong in.

## What the Rust target's run does, in order

1. preflight the tools and the two ports;
2. build the daemon and recreate `$PGDATABASE` from `migrations/`;
3. **seed the weather database** from `ci/conformance/rust-seed/` — the `stations` projection, about
   22,700 readings across five stations, a refresh of all four aggregate levels, and
   `almanac_cache`, `snow_totals_cache` and `forecasts`. Asserted **per station** and in both
   directions: two stations must have *no* readings, because that is what `latest-no-recent-data`,
   `span-empty`, `almanac-empty` and `snow-device-never-reported` are;
4. bootstrap the configuration database by starting the binary once, then seed the listener, the
   weather database URL, the node row and W1's seven stations and six websites;
5. start the daemon and wait for **`snapshot:warm`** to have run. That wait is not tidiness:
   `snapshot:warm` is `OnStartup` with a hard `ReadingStore`, so it runs after `db:postgres` has
   connected — which is after the daemon answers on `/metrics`. A replay that started first would
   read a 404 from `/latest` for every station and report it as a contract break;
6. replay all 70 fixtures with `--deviation DEV-10`, and judge by class;
7. if accepted, replay through `mangle_proxy.py` and require it to have broken at least twenty of
   the 48 responses the target serves. Measured at **23**: every JSON object or array on an
   implemented endpoint. The 25 that still pass are the plain-text error bodies, the `403`, and the
   whole `/forecast` family — the proxy rewrites parsed JSON and `/forecast`'s body is `jsonb` text
   it does not match.

**The rig configures a `node` row, which the Go rig had no equivalent of.** Without one the daemon
has no spool, so no reading fan-out, so no `snapshot:latest` — which is the only provider of
`LatestSnapshot`, and `api:legacy` requires it hard (**D-48**). The daemon then refuses the whole
configuration. `spool_path` is relative and lands in `target/conformance-rust/spool`.

## Running the Go target

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
   frozen contract is reachable. The bootstrap daemon is waited out, not just signalled: it holds
   the configuration database open while it shuts down, and seeding into that window failed
   intermittently with `database is locked (5)` on the seed's first write.
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

The Rust target's equivalents, each watched against a real run rather than reasoned about:

```sh
CONFORMANCE_FORCE_MANGLE=1        cargo xtask conformance   # the proxy bites: 20 frozen-class failures, exit 2
CONFORMANCE_FIXTURES=/some/subset cargo xtask conformance   # "attempted 3 fixtures, expected 70", exit 2
PATH=/usr/bin:/bin                cargo xtask conformance   # "missing required tool(s): psql, sqlite3", exit 2
PGDATABASE=weather                cargo xtask conformance   # "refusing to drop `weather`", exit 2
```

All four were run against this tree on 2026-11-09. `CONFORMANCE_FORCE_MANGLE=1` is the one that
matters now that the gate is green: it sends the **accepted** replay through the proxy and the gate
reports twenty frozen-class failures by name, which is a gate being watched to fail rather than
reasoned about.

It exits **2**, not 1, and the distinction is the point. Exit 1 means "the replay reached a verdict
and the binary does not serve the contract"; here the binary is fine and the transport was corrupted
on purpose, so the run proved nothing about the contract either way. Both outcomes of this knob are
exit 2 — a replay that still conformed *through* the proxy is the other one, and it means the proxy
mangled nothing.

A subset now fails on the **deviation** check first unless it happens to include one of the two
deviated fixtures — `--deviation DEV-10` against a fixture set that declares none is a refusal, so
that a typo cannot silently make the Rust target assert the Go bytes. Include a deviated fixture and
the `attempted 3 fixtures, expected 70` message is what comes back.

`CONFORMANCE_FIXTURES` reaches the runner and **not** the gate: `cargo xtask gate conformance` names
`contract/fixtures` on the runner's command line and clears the variable, because the gate validates
that directory's file count and then rests on `attempted == 70`. Reading the variable here had the
two talking about different directories, and any directory holding 70 files satisfied the assertion.
Point the runner at a subset directly — `legacy-conformance --fixtures <dir>` — to see the message
above.

A fixture with no `class` is the other new exit 2: `error: cannot read <dir>: … no `class` (frozen,
coupled or retired)`.

A daemon that dies at startup is the other one worth knowing: it ends the wait at once and says
`the daemon exited during startup`, naming its log. It used to spend the full 60-second deadline and
report a timeout instead — watched both ways with a `--target-binary` that exits on its second
invocation.

Occupying 18080 before a run produces the preflight's other exit 2.

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
| `rust-seed/01-config.sql` | the same devices and websites, in `migrations/config/`'s schema, plus the node row |
| `rust-seed/02-weather.sql` | the `stations` projection and the readings, on `observations` |
| `rust-seed/03-refresh.sql` | the four aggregate levels, refreshed coarsest-last |
| `rust-seed/04-caches.sql` | `almanac_cache`, `snow_totals_cache` and `forecasts` |

The Rust half has no files here: it is `xtask/src/conformance.rs`, because `ci/gates.toml` declares
its command as `cargo xtask conformance` and the judgement it makes is worth having tests for.
