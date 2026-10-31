# ModernWeather — implementation plan

**What this is.** The ordered build sequence for the Rust implementation, derived from the
sixty-seven decisions in `docs/DESIGN.md`'s ledger and the sixteen work items
`docs/reconciliation.md` §4 raised. **It writes down nothing the design documents did not decide**;
where a milestone needs something undecided, it says so and names the open question rather than
inventing an answer.

**Shape.** Seven milestones, thirty-two work items. Each item is sized to be **reviewable in one
sitting** — roughly one PR a reviewer can hold in their head — and carries its own dependencies,
done-criteria and test strategy.

**One standing rule for gates.** `ci/gates.toml` now declares twelve gates; four are armed and
eight are deferred, each on a path a milestone below creates (**D-64**). **The work item that arms a
gate also adds its job to `.github/workflows/pr.yml` and wires it into the required `gate` job.** W6
declared the manifest entries and deliberately did not add the jobs: a job that prints `DEFERRED` and
exits 0 is exactly the shape of `docs/reconciliation.md` G-1, and adding six of them would have been
the defect this plan exists to close. `cargo xtask gates --markdown` already writes the full deferred
table into every PR's summary, which is how the not-yet-armed set stays visible in the meantime.

## The five ordering constraints, and where each is satisfied

| Constraint | Where |
|---|---|
| **The metric registry and its generator come first**, because everything else is downstream of generated types | **Milestone 1.** The registry already exists on `main`; M1 closes its five outstanding follow-ups *before* any consumer is written, including the proto3-presence defect (**N-12**) that is free now and a fleet-wide flag day later |
| **W1's conformance suite must be green before any legacy endpoint is reimplemented**, not after | **M1-4 and M2-5.** M1-4 makes it run in CI against the **Go** daemon, which is possible today and proves the fixtures describe reality; M2-5 arms it against the Rust binary the moment one exists. No legacy endpoint is implemented before M4, which is two milestones later |
| **The first station driver is WeatherLink Live** — UDP and HTTP, iterable without serial hardware, best-covered driver in the Go tree at 360 test lines | **M3-1.** Davis and Campbell, which need a `socat` pty pair, come in M6 |
| **CI exists from the first commit**, not retrofitted | **Already true.** W8 landed the gate before any application code; thirteen jobs feed one required `gate` job. M1 and M2 *add* gates rather than creating the system |
| **Every milestone leaves the Go version in production and the Rust version independently verifiable** | **Every milestone's done-criteria.** §3 states the side-by-side mechanism; M5 is the milestone whose entire purpose is the diff |

## Dependency graph

```
M1  registry debts + CI honesty        (no application code; unblocks everything)
     │
M2  the skeleton: crates, supervisor, config store
     │     ├── M3  ingest: pipeline, spool, first driver (WLL)
     │     │        │
     │     └── M4  read path: snapshot, legacy API, website
     │              │
     │              ├── M5  side-by-side validation + data migration
     │              │        │
     │              └── M6  the rest of the fleet: drivers, outputs, collector
     │                       │
     └────────────────────── M7  management UI, then cutover
```

**M3 and M4 are parallelisable** after M2 — they share `mw-types` and the supervisor and touch
disjoint crates. Everything else is serial.

---

## Milestone 1 — pay the registry's debts and make CI honest

**No application code.** Five of the six items are under half a day. This milestone exists first
because every one of its items gets more expensive the later it lands, and two of them
(**M1-1**, **M1-2**) become breaking changes once a consumer exists.

**Why this is not "retrofitting CI".** The gate already exists and is green. What M1-4 fixes is a
gate that is green *by deferral* while the deliverable it guards is complete — `docs/reconciliation.md`
G-1. Leaving it until a Rust daemon exists means the suite's first real run is also the first run
that can fail, against the binary it is meant to be validating.

### M1-1 — proto3 field presence *(N-12 / G-3)*
**Depends on:** nothing. **Blocks:** M3-2, M3-4, M6-4.
Emit `optional` on every metric field in `generated/proto/reading.proto`. One line per field in the
generator; `generated/rust/reading.rs` is already `Option`-valued.
**Done when:** `grep -c optional generated/proto/reading.proto` is **212** — one per metric; the four
`[[dimension]]` fields (`readingTimestamp`=1, `station_id`=221, `stationName`=2, `stationType`=3) are
unaffected, which is also the assertion. `cargo xtask codegen --check` is clean; `cargo test -p xtask`
asserts presence is emitted for every `[[metric]]` and for no `[[dimension]]`.
**Test strategy:** a generator output assertion, plus a round-trip test proving a `None` metric
encodes to an absent field and decodes back to `None` rather than `0.0`.
**Why first:** without it a NULL becomes `0.0` across a hop *and* across a spool restart (W7 D-11),
and retrofitting presence after a collector is deployed is a fleet-wide flag day.

### M1-2 — dimension `role = "metadata"` *(N-14 / OQ-W4-2, OQ-W2-6)*
**Depends on:** nothing. **Blocks:** M1-3's pruning test; the eventual `stationname`/`stationtype`
drop; D-61's provenance column if it is ever wanted.
Add a third dimension role: present in the protobuf and the Rust type, absent from the raw DDL and
the aggregates.
**Done when:** `cargo xtask validate` accepts a `metadata` dimension; the generated DDL omits it; a
`cargo test -p xtask` case proves a `metadata` dimension can be **removed** from the registry without
the FATAL that `[[dimension]]` removal currently raises unconditionally.
**Test strategy:** the three generator-output assertions above plus the removal case, which is the
one that matters — `snapshot::from_registry` hard-codes `deprecated: false` for dimensions today, so
this is a one-way door until it is opened.
**Scope note:** this item adds the *capability*. It does **not** move `stationname`/`stationtype`;
that is M5-5, after the migration has proved it reads them from `config.stations` instead.

### M1-3 — `codegen` prunes what it no longer writes *(N-15 / OQ-W4-4)*
**Depends on:** nothing. **Done when:** adding and then removing a `[[level]]` leaves no orphan under
`generated/`, and `cargo xtask codegen` followed immediately by `codegen --check` always exits 0.
**Test strategy:** the round-trip above as a test, which is also the regression: today adding the
5-minute level orphaned two files and left a tree `--check` rejects.

### M1-4 — run the conformance suite against the Go daemon in CI *(N-3 / G-1)*
**Depends on:** nothing. **Blocks:** M2-5, and by the second ordering constraint every legacy
endpoint in M4.
The largest item in this milestone and the only one that is not small. Stand the Go daemon up in CI
from `remoteweather` @ `c3ec9f7` against a `timescale/timescaledb-ha:pg17` service container, seed it
with W1's rig (`contract/tools/rig/`), replay all 70 fixtures, and run
`contract/tools/rig/mangle_proxy.py` as the negative control **W1 D-10 requires for a green run to
be accepted at all**.
**Done when:** a `conformance-fixtures` job is green with 70/70 passing; the same job is **red** when
run through `mangle_proxy.py`; `ci/conformance/go-target.sh` exists, which is what arms the gate
declared in this PR.
**Test strategy:** the negative control *is* the test strategy — a conformance suite that cannot be
made to fail proves nothing, which is why W1 refused to accept a green run without one.
**Also in this item:** repoint the `conformance` gate (the Rust-target one) at
`crates/modernweather/Cargo.toml`, done in W6's PR, and verify `cargo xtask gates` shows it deferred
with an honest reason rather than an unreachable path.

### M1-5 — the gate runner distinguishes exit 1 from exit 2 *(N-8 / C-W2-20)*
**Depends on:** nothing. **Done when:** `cargo xtask gate codegen-drift` reports "regenerate and
commit" on exit 1 and "your registry change is not allowed" on exit 2, and a gate may declare a
per-code message in `ci/gates.toml`.
**Test strategy:** two `xtask` tests driving a stub command that exits 1 and 2.

### M1-6 — `solarjoules`, and the four W2 follow-ups' loose ends *(N-9, N-7, N-13, N-16)*
**Depends on:** nothing. Four independent small items, grouped because each is under an hour and none
blocks anything:
- **N-9** — read `remoteweather:pkg/solar` at `c3ec9f7` and settle whether `solarjoules` is a total
  or a mean. One registry line if it is a total. *Nothing is blocked on this; the cost of being wrong
  is a wrong number in a chart, and `solarjoules` is not on the frozen surface.*
- **N-7** — a `[[unit]]` table validated by the generator, closing the `degf`-for-`degF` hole.
- **N-13** — `affected_levels` compares rendered level definitions, so adding a metric to one level
  rebuilds one level.
- **N-16** — `rebuild-aggregates.sh` phase 1 refreshes each level over the **widest** band any level
  above it serves, and `--recent-only` warns naming the levels it left incomplete.
**Done when:** each has its own test; `cargo xtask validate` rejects an unknown unit string.

**Milestone 1 done when:** every gate in `cargo xtask gates` is either armed-and-green or deferred
for a reason that names a path a later milestone's done-criteria create (**D-64**); the conformance
suite has run against the Go daemon in CI and the negative control has been observed failing; and
`generated/proto/reading.proto` carries field presence. **The Go version is untouched throughout.**

---

## Milestone 2 — the skeleton

The first application code. Nothing in this milestone serves a request or reads a station; it
establishes the eighteen crates, the supervisor, and the configuration store — and the layout gate
that keeps the crate boundaries real.

### M2-1 — the workspace, `mw-types`, and the layout gate
**Depends on:** M1-1. **Blocks:** everything.
Create the eighteen crates of W3 §1.2 as stubs with their real `[dependencies]` and nothing else,
populate `[workspace.dependencies]`, and adopt `generated/rust/*.rs` into `mw-types` via the three
`#[path]` declarations (D-5 — `#[path]` rather than `include!`, because `reading.rs` carries an inner
attribute a macro expansion cannot contribute).
**Done when:** `cargo build --workspace --locked` passes; `cargo xtask gate layout` is **armed** and
green, enforcing the layer order, every crate's forbidden list, "only `mw-types` mentions
`generated/`", "only `mw-http` binds a socket", and "only the binary depends on `anyhow`".
**Test strategy:** the layout gate is the test. It must be proved able to fail: a test adds a
forbidden dependency to a scratch manifest and asserts the gate rejects it. **A boundary gate that
has never been seen failing is G-1 again.**
**Note:** `crates/modernweather/Cargo.toml` is created here, which arms both the release pipeline
(`ci/release.toml`'s `crate_manifest`) and the `conformance` gate. Expect the latter to go from
`DEFERRED` to failing — that is correct, and M2-5 closes it.

### M2-2 — the configuration schema *(N-4 / G-2)*
**Depends on:** M2-1. **Blocks:** M2-3, M2-4, M3-3, M6-4, M7.
Define the SQLite configuration schema and its migration mechanism under **D-8 as amended by
A-W6-2**: `migrations/config/` applied by name, no startup DDL, no `IF NOT EXISTS`.
Covers `component_schedule` (W2a D-12), `admin_credential` and `admin_session` (W5 D-23/D-24),
`peers` / `peer_origins` / `config.stations` (W7 §5.1, named per **D-65**), the station, website and
uploader configuration, and `station_service` per **D-66**.
**Done when:** `cargo xtask gate db-migrate-twice` covers the config database as well as the weather
one; a second application of every migration is a no-op; no `IF NOT EXISTS` appears under
`migrations/`.
**Test strategy:** apply-twice from empty and from the previous schema, the same two-path comparison
W8 D-19 built for PostgreSQL.
**Open question it must respect:** none. OQ-W5-4 is closed by **D-66** precisely because this schema
is new — there is no in-place migration to avoid.

### M2-3 — `mw-component`, `mw-supervisor`, and `plan()`
**Depends on:** M2-1, M2-2. **Blocks:** M3, M4, M6, M7.
W2a's model as code: `ComponentSpec`, `Trigger`, `Capability`, `Need`/`Provision` (including
`ProvisionDetail::Routes` per **D-51**), `Tick`/`OnReading`/`Serve`, the lifecycle states, the hard-edge
digraph, the single timer heap replacing 21 hand-rolled clocks, the task table, and capability
watches. `ComponentControl` carries the log-level handle (**D-52**) and `applied_generation`
(**D-53**).
**Done when:** `plan()` is a pure function and every structural error in W2a Decision 2's list is a
unit test — unknown capability, duplicate exclusive provider, unprovided requirement, cycle, bad
deadline, sub-floor interval, impossible identity, duplicate route.
**Test strategy:** `plan()`'s purity is what makes this testable without a runtime, which is the
whole point of W2a Decision 2 — every one of those errors is a unit test rather than a runtime
surprise. Plus the two concurrency tests W3 D-85 specifies: **config reload racing 64 readers**
(asserting the absence of a torn read, not the absence of a crash) and **shutdown with a stalled
sink** under `tokio::time::pause()`. Both run under ThreadSanitizer and `loom`; the `race` and `loom`
jobs currently find nothing to run, and this is what fills them.

### M2-4 — the binary, boot sequence, and observability
**Depends on:** M2-3. **Blocks:** M2-5, everything runnable.
`crates/modernweather`: `--config <path>` (W8 assumption 2), the SQLite pool as a root resource owned
by `main` handing a one-table `ScheduleStore` to the supervisor (D-41), `tracing` setup with the ring
and dedupe layers, the metrics recorder, and `sd-notify` readiness where `READY=1` means *the
supervisor has converged* (W3 D-47).
**Also:** **D-62** — assert the database version at startup, after the pool opens and before
`plan()` runs, with the same comparison `ci/sql/assert-timescaledb-version.sql` makes. This is the
half of D-42 that had no owner.
**Done when:** `modernweather --config <path>` starts with an empty configuration, converges, notifies
readiness, serves `/metrics`, and exits cleanly on SIGTERM inside the 10-second grace.
**Test strategy:** an integration test asserting readiness notification and clean shutdown; a startup
test against a deliberately-wrong database version asserting the failure message names both versions.

### M2-5 — arm the Rust conformance target *(closes G-1)*
**Depends on:** M1-4, M2-4. Implement `cargo xtask conformance --target-binary <path>`: build the
binary, bring it up against a seeded test database, shell into `contract/conformance` with
`--locked`, and pass it a base URL.
**Done when:** the `conformance` gate is **armed** and reports honestly — at this point it will be
*failing*, because no legacy endpoint exists yet, and that is the correct state. The second ordering
constraint is satisfied by this failure being visible from M2 onward rather than discovered in M4.
**Test strategy:** assert the runner reports 70 fixtures attempted. A green run is M4's done-criteria,
not this item's.

**Milestone 2 done when:** the daemon starts, converges and stops; the layout gate is armed, green,
and has been seen failing; the configuration schema applies twice cleanly; both concurrency jobs run
real tests. **The Go version is still in production and nothing has been migrated.**

---

## Milestone 3 — ingest

The write path, end to end, for one station type. After this milestone the Rust daemon can collect
and store readings **from a station the Go daemon is not using**, which is the first point at which
the two can be compared at all.

### M3-1 — `Transport`, `Session`, `Codec`, `Identity`, and WeatherLink Live
**Depends on:** M2-3. **Blocks:** M3-4, M5, M6-1.
W2a §4a's four-part decomposition, then **WeatherLink Live first** — two transports (UDP broadcast
and HTTP) feeding one codec family plus an outbound re-arm that is its own `Every` component
(W2a D-44). Chosen because it needs no serial hardware and is the best-covered driver in the Go tree
at 360 test lines.
**Done when:** a WLL station configured in the config database produces readings; the codec is pure
(no `tokio`, no clock, no socket — enforced by the layout gate) and passes golden tests against
captured frames.
**Test strategy:** the three levels W3 D-82 specifies — pure codec against captured frames, `Session`
against a recorded transcript, and the whole driver against the pinned Go emulator. **The transcript
level is the one that matters**: it is what would have caught the Davis "stops polling after one
batch" defect. `tests/emulators/manifest.toml` is created here, which arms `driver-emulators` — an
explicit deliverable per **D-64**, so that gate does not become a second G-1.
**Constraint from another document:** discovery uses an **in-process mDNS client**, not `getaddrinfo`
of a `.local` name. This follows from D-43's static-musl linkage — musl's resolver supports neither
`nsswitch.conf` nor NSS modules (W8 OQ-4) — and the constraint comes from W8's decision, not W2a's.

### M3-2 — the spool, `Ingress`, and the fan-out
**Depends on:** M2-3, M1-1. **Blocks:** M3-3, M3-4, M6-4.
W7 §3's format and W3 §2's mechanics as one thing, because **D-30a** leaves no other place for
durability: 8 MiB segments, per-record length/`seq`/CRC32C, a group-commit cap of **64 append calls**
(not records — W7 D-13), a 2 GiB ceiling, two-slot `O_SYNC` cursors, and recovery that scans only the
newest segment. `Ingress::admit` plus `accept` (**D-60**), with `path` as an ordered
`SmallVec<[OriginId; 8]>` (**D-59**).
**Done when:** `admit` does no I/O but the spool append (W3 D-87); a kill -9 mid-append truncates the
segment at the bad record, counted and logged, and loses nothing acknowledged; a filtered envelope
advances the cursor (W7 D-9).
**Test strategy:** power-loss simulation by truncating a segment at every byte offset of the last
record; a property test that a replayed prefix is idempotent; the stalled-sink test from M2-3
re-pointed at a real spool. **The ceiling policy needs its own test**: at the ceiling, drop at the
edge, counted and alarmed, with shutdown still completing (W3 D-94).

### M3-3 — enrich, the writer, and the projections
**Depends on:** M3-2, M2-2. **Blocks:** M4-1, M5.
The enrich stage under **R1** — a derived value is stored as a column iff it is a pure function of one
reading plus static station metadata — so wind chill, heat index, AQI, solar potential/joules and snow
depth are computed at ingest and stored (W3 D-23). Enrich is idempotent, writing a metric only where
the value is `None` (W3 D-25), which is what makes a forwarded envelope safe across a hop.
`writer:timescale` as a spool cursor with 256-envelope batches and
`ON CONFLICT (station_id, time) DO UPDATE`. Per **D-56**, `admit` upserts `config.stations` and the
writer maintains `last_seen` and `station_metrics`.
**Done when:** readings land in `observations`; a replayed prefix produces no duplicate rows; an
unreported metric is **NULL**, not `0.0` (D-26); `station_metrics` reflects what has been written.
**Test strategy:** needs a real TimescaleDB container, and per W3 D-84 **the test fails CI if it
skips** — `cargo xtask gate db-tests` is created and armed here, which is one of G-4's four
undeclared gates and the one whose whole purpose is to make "green because it did not run"
unreachable.

### M3-4 — window rollup and the component-maintained values
**Depends on:** M3-3. **Blocks:** M4-1, M6-2.
`window:rollup` reading 1-minute buckets on minute boundaries within its 10-second deadline (W4 D-26
measured the lag at 3.06–3.22 s), and the values **R2** keeps out of SQL: wind gust, rain rate,
rainfall day/month/year and 24/48/72 h, storm totals and the snow totals. Rainfall rollovers happen in
the **station's** time zone via `Calendar { TzSource::Station }` (W3 D-24) — today's
`date_trunc('day', NOW())` truncates in the database server's zone and makes the device `timezone`
field decorative.
`WindowedView` resolves `Mean`, `Min`, `Max`, `Sum`→`total` and `CircularMean` from the registry's
declared strategies, per **D-50**; there is no `Agg::Last`.
**Done when:** a ten-minute mean over `WindowedView` equals the same mean computed directly from raw;
a DST transition in a station's zone produces exactly one daily rollover.
**Test strategy:** the DST cases are unit tests over `chrono-tz` with no database — a nonexistent
local time fires at the first instant after the gap, an ambiguous one fires on the first occurrence
only, deduplicated by occurrence key (W2a D-13).

**Milestone 3 done when:** a WeatherLink Live station feeds the Rust daemon, readings are durable
across a kill -9, and `observations` contains NULLs where the station reports nothing. **The Go
version is in production on its own stations and database; nothing has been migrated.**

---

## Milestone 4 — the read path, and the frozen contract

The milestone the second ordering constraint governs. **No legacy endpoint is written until M1-4 has
proved the fixtures describe reality and M2-5 has armed the runner against the Rust binary.**

### M4-1 — `LatestSnapshot` and the hot path
**Depends on:** M3-3, M3-4. **Blocks:** M4-2.
The snapshot keyed per `(website, station)` (W3 D-31), JSON pre-serialised eagerly once per
reading-version with msgpack and protobuf lazy behind a `OnceCell` (D-33), and `snapshot:warm` as an
`OnStartup` component that seeds from the store so a restart does not 404 a station inside the
10-minute window (D-37).
**Done when:** the `/latest` handler does **four** things and performs **no queries** — enforced
structurally, because `api:legacy` requires `LatestSnapshot` and is not handed a `ReadingStore`
handle, so it *cannot* (D-35). With the database down the snapshot keeps its last good
window-derived values and records `Staleness::Degraded`, surfaced through additive headers and never
through the frozen body (D-36).
**Test strategy:** **D-48's invariant is a test here** — with the database stopped, every frozen route
returns its contract response and never `503`. That is the check W1 and W2a both deferred to each
other and neither made.

### M4-2 — `mw-api-legacy`
**Depends on:** M4-1, M2-5. **Blocks:** M4-3, M5.
The compatibility adapter, which exclusively owns the 133-key DTO, both duplicate key pairs, the
`mm`→inch split, `omitempty`, the four error-body conventions and msgpack. Legacy naming cannot leak
into the core because `mw-types` has no `serde` dependency at all, and zero-filling cannot leak into
storage because the only `unwrap_or(0.0)` in the workspace is `mw_api_legacy::zf` (D-10) — both
enforced by the layout gate.
`/span` per **D-54** and **D-55**: `ReadingStore::span` returns the materialised prefix plus the
trailing bucket aggregated from raw, and the 60-second cache holds the **prefix only**.
`/forecast` caches the stored `jsonb` text byte for byte and splices it between a prefix and a suffix
(D-39) — nothing parses or re-emits it, which is how Postgres's whitespace and the missing trailing
newline survive.
**Done when:** `cargo xtask gate conformance` is **green: 70/70 against the Rust binary**, and the
negative control still fails. This is the milestone's headline.
**Test strategy:** the conformance suite is the specification. Plus a regression test for W1-BUG-1 —
`/span` for a station with no non-NULL `winddir` must return 200 with `windd: 0`, which W4 D-8's
finaliser makes true and which was a 500 in Go.
**Deviations that land here:** DEV-1 (`ETag` on `/latest` and `/snow`) and DEV-5 (the `X-MW-*`
headers). Both are additive and invisible to the suite by a verified property of the runner
(`contract/conformance/src/lib.rs:320`).

### M4-3 — `mw-http` and `mw-web`
**Depends on:** M4-2. **Blocks:** M5, M7.
One router over N sockets per **D-31 as amended by A-W3-4** — the main port, an optional redirecting
plaintext port, and the **legacy gRPC port, which keeps its own socket** (W3 D-92) so no peer's
endpoint changes at upgrade. `ArcSwap` for the router, the host table and the certificate store.
The three templates ported to `askama` with a `GoHtmlEscaper` reproducing `html/template`'s
`&#34;`/`&#39;`, and `portal.html.tmpl` becoming a static asset because it consumes nothing (D-75).
Assets embedded with a hard-coded mime table, not `mime_guess`, because two frozen content types —
`/js/weather-app.js` without a charset and `site.webmanifest` as `text/plain` — are quirks a mime
crate would "correct" (D-76).
The config payload inline as `<script id="mw-config" type="application/json">` carrying exactly W1
§9's nine values (D-79, D-80).
**Done when:** the HTML acceptance test is a **byte diff** against W1's recorded bodies, which is what
the `GoHtmlEscaper` decision buys; the layout gate rejects `safe` and `escape("none")` anywhere under
`crates/mw-web/templates/` (D-81).
**Deviations that land here:** DEV-2 (case-insensitive `Host`), DEV-3 (404 instead of directory
listings), DEV-6 (single-label wildcard matching and the deterministic SNI miss chain). All three
close defects; see `docs/reconciliation.md` §3.

**Milestone 4 done when:** the Rust daemon serves the entire frozen surface with the conformance suite
green against it, and the website renders byte-identically. **It is serving its own stations from its
own database; the Go version is still in production.**

---

## Milestone 5 — side-by-side validation, and the data migration

**This is the validation story for everything that has no test oracle**, and it is the answer to the
fifth ordering constraint. Everything before this point was verified against a fixture, an emulator
or a property. The values a weather station actually produces have no oracle — so the oracle is the
Go daemon running on the same inputs.

### 5.0 Where the two implementations run side by side, and how they are diffed

Three mechanisms, in increasing strength. None of them requires cutting over, which is what makes
this milestone satisfy the constraint.

**(a) Same fixtures, both binaries — already running from M1.** The conformance runner takes a base
URL precisely so it can target either (R-3). M1-4 points it at Go, M2-5 at Rust. This diffs the
*contract*: status, headers, key set, value types, numeric formatting, key order. It does **not** diff
values, by design (W1 D-3) — values change every two seconds, so byte equality is untestable against
a live server. **Known limitation, and it is the one that matters here:** because the suite is
value-blind, it cannot catch a wrong *number*. That is what (b) and (c) are for.

**(b) Same station, both daemons, diffed readings.** A WeatherLink Live station broadcasts over UDP,
so **both daemons can receive the same packets at the same time** with no reconfiguration of the Go
side and no risk to it. Each writes to its own database. A diff harness then compares, per timestamp:
every stored metric column, and the derived values **R1** stores at ingest (wind chill, heat index,
AQI, solar, snow depth).
*Expected diffs, and they are predicted rather than discovered:* NULL where Go writes `0.0` for an
unreported metric (D-26 — the intended change), and nothing else. **Any other diff is a defect in the
Rust implementation**, which is what makes this harness useful: its expected output is "one known
class of difference, enumerated in advance".

**(c) Same database, both read paths — the strongest diff, and the one with a true oracle.** After
M5-1 imports production's history into the new schema, the two read paths can be run against
*equivalent data* and their `/span`, `/latest`, `/snow` and `/almanac` responses compared **by value**,
not just by shape. This is where a wrong aggregate shows up.
*Expected diffs, enumerated in advance from the design:*
- `/span`'s `period_rain` and `windd` differ in the 6th significant figure, in 100% of buckets —
  **DEV-4**, a consequence of D-14, and the new numbers are the accurate ones.
- Nothing else. W9's reconciliation measured exactly zero difference on twelve of fourteen
  statistics, summing in `numeric` with **no tolerance**, and derived the bound for the two that
  cannot agree arithmetically (`n × 2^-24` relative).

**Why this ordering is the one that works.** (c) is the strongest diff and it needs the migration,
which is why the migration is in the validation milestone rather than at cutover. Running the
migration into a *parallel* database — which W9's design already does, since the source is read-only
throughout apart from one named archive step (W9 D-2, D-3) — means the strongest validation available
runs weeks before anything is decommissioned.

### M5-1 — run the migration into a parallel database
**Depends on:** M3-3, M4-2. **Blocks:** M5-2, M7-4.
Execute `ci/db/migrate-from-go/migrate-from-go.sh` against production as source and a new instance as
target. W9 built this; this item *runs* it and records the result.
**Prerequisites that are not this item's to satisfy:** **O-2** (archive first, then pause — and the
archive is correct under every answer, so it does not wait on anything), **O-1** (the version, which
decides whether this is also a major-version PostgreSQL upgrade), **O-7** (the history option).
**Done when:** `w9` reconciliation passes with zero tolerance on every metric per year; the two
bounded statistics are inside their derived bound; `w9.assert_no_unknown_source_columns()` raises
nothing.
**Test strategy:** W9's negative controls — the reconciliation was proved able to fail, and a
reconciliation that ran against zero rows raises rather than passes.
**Also produces O-4's answer as a side effect:** the import computes
`w9.expected_column INTERSECT w9.source_column`, so the ~72-column gap is enumerated by running it.

### M5-2 — the diff harness
**Depends on:** M5-1, M3-1. Build (b) and (c) above as a runnable harness in `ci/diff/`, with its
expected-diff set encoded as data rather than prose so an unexpected diff is a failure rather than a
judgement call.
**Done when:** (b) reports only NULL-versus-zero differences across a 24-hour run; (c) reports only
`period_rain` and `windd` differences, within DEV-4's measured bound, across all four `/span` bands
and `/latest`, `/snow` and `/almanac`.
**Test strategy:** the harness's own negative control — inject a 1% error into one metric and assert
the harness reports it. **Same principle as W1 D-10: a differ that cannot be made to differ proves
nothing.**

### M5-3 — the `sunrise`/`sunset` assertion *(N-10 / C-W2a-27)*
**Depends on:** M4-1. **Blocks:** M5-4.
Compare `/latest`'s `sunrise` and `sunset` strings against a fresh Meeus evaluation for the fixture's
station and date — the one place where a fixture's *value* is the thing under test, exempt from W1
D-3's value-blindness for that reason.
**Done when:** the assertion exists and passes to the minute for every configured station.
**Why it blocks M5-4:** W2a Decision 36 deletes the `sun_times` table **on the strength of this
check**, which does not currently exist. The stored Go table is day-of-year keyed and therefore
slightly wrong in non-leap years, so the two *should* disagree somewhere — and if they do, W2a §4.3
already records the fallback component's shape.

### M5-4 — delete `sun_times`
**Depends on:** M5-3 passing. One row out of the plan, a table and a capability deleted. Trivial, and
deliberately gated on the previous item rather than bundled with it.

### M5-5 — drop `stationname` and `stationtype` from the raw table
**Depends on:** M1-2, M5-1. Now safe: the `metadata` role exists, and the migration has proved
`snapshot:latest` reads display metadata from `config.stations` rather than off a reading (W4
assumption 24). The import's column list shrinks automatically, because it is derived from the
registry.
**Done when:** `observations` carries **214** generator-owned columns (216 today, minus two) plus the
hand-authored `extra jsonb`; nothing reads `stationname` from a reading; the diff harness still
reports only its expected set.

**Milestone 5 done when:** the Rust implementation has been diffed against the Go implementation by
value, on real data, with every difference predicted in advance from a decision. **The Go version is
still in production.** This is the milestone that makes cutover a decision rather than a leap.

---

## Milestone 6 — the rest of the fleet

Everything that is "another instance of a pattern M3 and M4 established". Parallelisable; none of it
blocks M7.

### M6-1 — the remaining six drivers
**Depends on:** M3-1. Davis (serial and TCP), Campbell Scientific, Ambient, AirGradient, the snow
gauge, and the gRPC receiver. **Davis and Campbell get a `socat` pty pair** (W3 D-83) because serial
framing and read-timeout behaviour is where that class of defect lives.
**Ambient is the structural one:** it becomes **N stations on one endpoint** — one
`HttpIn { /ingest/ambient }` component, identity from `?ID=` against `StationRegistry`, secret from
`?PASSWORD=` compared constant-time (W2a D-43). A second Ambient station is a config row, not a second
HTTP server.
**Done when:** each driver passes all three test levels against its pinned Go emulator.
**Decision point that lands here, not before:** **OQ-W4-3** — whether the registry gains a `jsonb`
type so `observations.extra` can be a registry entry. W4's note is explicit that this is "worth
deciding before W7 writes its first driver that receives an unknown field", and this is that moment.
Recommendation stands at (a)-then-(c): leave it until a driver needs it, then add `SqlType::Jsonb`,
the `metadata` role and a protobuf `map<string, double>` so the hatch works across a hop.

### M6-2 — the rate-limited outputs
**Depends on:** M3-4. APRS/CWOP, Weather Underground and PWS Weather as **scheduled producers reading
a `WindowedView`** — **D-30b**, not stream sinks. Each `OutputEncoder` declares
`&[(Metric, Window, Agg)]` so the CWOP table is reviewable line by line against the specification.
`Retry` outside `RateLimit`, `Timeout` innermost (W2a D-57) — reversed, the first failure spends one
permit and then bursts through the limit. Twelve pure APRS functions move into `mw-aprs`, golden-
tested against captured packets.
**Done when:** CWOP receives a **5-minute averaged** wind direction and speed with a **10-minute peak**
gust — which is the protocol's requirement and the defect D-30 as written would have carried forward.
A golden test compares an encoded packet against a captured one byte for byte.
**Owner decision this needs:** **O-8** — the RF beacon floor, 600 s or 300 s. `RadioInterval`'s
constructor is one constant either way; the migration's handling of an existing sub-600 s KISS device
is the part that depends on the answer.

### M6-3 — the Aeris components and the caches
**Depends on:** M2-3. Aeris becomes four components — two forecast spans, alerts, and the expired-alert
sweep — because they have independent periods, failures and rate budgets, and "alerts failing,
forecasts fine" must be expressible (W2a D-33). The snow cache becomes three.
**`aeris:alerts:gc` stays the `Calendar` component W2a specified**, per C-W2a-15: OQ-C offered W4 a
retention policy as the alternative and W4 declined to make the alerts table a hypertable, which
settles it by elimination.
**Done when:** `/forecast` and `/alerts` are conformance-green; `Freshness` renders "forecast last
updated 3 h ago / never" as distinct from component state (W2a D-20).

### M6-4 — the collector
**Depends on:** M3-2, M1-1, M2-2. W7's `collector.v1.Collector` service in a **new file** —
`WeatherV1` is not touched (W7 D-19). `Ingest(stream IngestBatch) returns (stream IngestAck)`,
bidirectional and acked, replacing a signature under which no sender could learn what its collector
persisted. An ack means **durably appended to the collector's spool**, not written to PostgreSQL
(W7 D-21), because acking on the database would make a stalled database back-pressure every satellite
in the fleet.
Three admission layers, all preceding the pipeline (**D-46**): mTLS or PSK at the handshake, a per-RPC
interceptor resolving the peer before any method body runs, and per-envelope origin authorisation.
Enrolment is a separately gated RPC with operator-minted single-use tokens (W7 D-33).
`relay = false` by default (W7 D-36), so a compromised leaf cannot inject history for another station.
**Done when:** a satellite survives a collector outage and resumes without duplicates; the six-clause
receive check rejects every malformed path; `uplink_loop_refused_total` stays zero, which per W7 D-41
means a bug in this code rather than a configuration problem.
**Test strategy:** a two-node integration test with an injected network partition; a property test
over `path` asserting a relay cycle is unrepresentable rather than filtered.
**Also:** the Go compat shim with its three stated limitations (no resume or cross-reconnect dedup, no
relay, no field presence), gated for removal on `peer_software_info` showing no Go-era peers
(W7 OQ-W7-6).

---

## Milestone 7 — the management UI, then cutover

### M7-1 — the management API
**Depends on:** M2-2, M2-3, M4-3. 47 API registrations over `ConfigAdmin` and `ComponentControl`
only — `mw-api-mgmt` may not depend on `mw-store` or `mw-pipeline`, enforced by the layout gate. Every
route is `/api/v1/*` per **D-49**. The OpenAPI schema is derived from the Rust handlers via `utoipa`
and gated by **`mgmt-schema-drift`** (**D-57**), *not* by `codegen-drift`, following
`legacy-import-drift`'s precedent.
`POST /api/v1/config/preview` runs the supervisor's pure `plan()` against the proposed configuration,
which is what lets §2.4's save preview and §8.7's deletion consequences come from the same code path
that will actually run.
**Done when:** `mgmt-schema-drift` is armed and green; a save's preview names exactly the components
that then restart.

### M7-2 — authentication and first-run
**Depends on:** M7-1, M2-2. Argon2id at m=64 MiB/t=3/p=4, sessions as hashed 32-byte secrets in an
`HttpOnly`/`SameSite=Strict` cookie that is **never the API credential**, CSRF as `SameSite=Strict`
**plus** a required `X-MW-Request` header, and a single-use setup code printed **to stderr only and
never to the log ring** (W5 D-27). The management API binds loopback while unclaimed, with
`--setup-listen` for headless installs. Default bind becomes `127.0.0.1` and CORS becomes an
exact-match allowlist defaulting to empty — **A-W5-3**.
**Done when:** `modernweather reset-password` works on the host; changing the password revokes every
other session and keeps the current one; the setup code is invalidated in the same transaction that
creates the account.
**Owner decisions this needs:** **O-10**'s four (loopback-until-claimed, the breached-password list,
no API tokens in v1, one step of config history). All four have recommendations and none blocks the
others.

### M7-3 — the UI
**Depends on:** M7-1, M7-2. SvelteKit 2 + `adapter-static` + Vite 6 in SPA mode on `chonky`, embedded
with `rust-embed`. Twenty-six pages, seven local components, one SSE endpoint with multiple channels.
Per **D-58**, `cargo xtask dist` builds the bundle **once, natively, before** the cross matrix and
fails if it is missing or stale — the bundle is architecture-independent static files, so building it
inside each `cross` container would be strictly harder.
**Gates created here:** `ui-bundle` against `ui/budget.toml` (150 KB gzipped initial route, 280 KB all
routes, 60 KB largest lazy chunk, 45 KB fonts) and **`ui-copy`**, the vocabulary lint that
**A-W5-2 exists to make mechanical** — without it D-36 degrades into a code-review preference within
a release or two.
**Includes N-11** (C-W7-29): the fleet numbers W7 specified after W5 had finished — outstanding
enrolment tokens and the enrolment log (because leaving enrolment open is the mistake and it should be
visible), `spool_time_remaining_seconds`, the `ceiling < max_lag × rate` advisory, and
`peer_reported_depth_envelopes` **labelled as peer-reported**.
**Done when:** both gates armed and green; five Playwright flows pass against a real binary; no page
polls — the only client-side timer is the `Backoff` countdown rendering a server-supplied absolute
timestamp (W5 D-46).
**Owner decision this needs:** **O-11** — which `chonky` gaps to upstream.

### M7-4 — the configuration migration *(N-5 / G-2)*
**Depends on:** M2-2, M7-1, M5-1. Move the owner's existing configuration onto the new schema:
stations with their calibration, websites with hostnames, titles and about-text HTML, certificate
paths, per-service settings into `station_service`, and `remote_stations` →
`peers`/`peer_origins`/`config.stations` with the **four plaintext secret columns dropped** (**D-67**).
Allocate a UUIDv7 `NodeId` per existing node, **not** reusing the `station_id` UUIDs in
`remote_stations`, which are station identities (W7 assumption 35).
**Done when:** the Rust daemon starts from the migrated configuration and plans the same component set
the Go daemon was running; a sub-600 s KISS device is handled per **O-8**'s answer with a one-line
report rather than a parse error.
**Carried to the owner, not automated:** re-entering the four rotated credentials. They crossed the
wire in cleartext and sat in a second database where nothing read them (A-W7-2); migrating them would
carry the exposure forward. **This is the only part of the configuration the owner re-types.**

### M7-5 — cutover
**Depends on:** M5-2 clean, M6, M7-4.
**Done when:** the Rust daemon serves production; the Go daemon is stopped but **not** decommissioned;
`w9` staging is dropped except the three tables that record human decisions (W9 OQ-W9-5's
recommendation (c)); the source database is retained until the owner says otherwise.
**Rollback:** the Go daemon and its database are untouched and restartable, which is the property
every preceding milestone was ordered to preserve.

**After cutover, and not before:** **N-1**, the v2 API. Its inventory is already assembled in
`docs/reconciliation.md` §4 — the additive routes, the time-anchored endpoint that is the only thing
that will ever read `observations_1d_legacy`, and the deprecation list. **N-2**, updating the Go
provisioner, is independent of all of this and can land whenever.

---

## What this plan does not contain

Per the fourth "do not": **the plan does not exceed what the design documents decided.** Four things a
reader might expect are deliberately absent.

- **No v2 API work.** D-39 puts it out of scope and the fifth ordering constraint forbids it: a new
  API surface is the first thing that is not independently verifiable against the Go version. It is
  named (N-1) and scheduled after cutover.
- **No dashboards or charting.** W5 D-51 excludes them explicitly — no chart library, and the weather
  display already exists and is frozen.
- **No performance work beyond what a decision requires.** W3 §10's efficiency statements are
  prohibitions ("`admit` must do no I/O but the spool append") rather than targets, and W8's OQ-2
  settled that no replacement allocator is adopted pre-emptively.
- **No estimates.** The items are sized as "reviewable in one sitting" because that is what the
  prompt asked for and what a reviewer can act on. Nothing here is an estimate of elapsed time.

## Outstanding owner input, by the milestone that needs it

| Milestone | Needs | Blocking? |
|---|---|---|
| — | **O-2** archive-then-pause the raw retention policy | **Not blocking, and losing data now.** Every day it stays open converts another day of raw into rollup-only history |
| M5-1 | **O-1** the production version | **Yes.** Below TimescaleDB 2.9 the hierarchy cannot be built and M5-1 becomes a major-version upgrade |
| M5-1 | **O-7** the history option | Yes for M5-1's scope; the archive copy is correct either way |
| M4-2 | **O-5** the `/stationinfo` hazard | No — M4-2 ships W1's faithful reproduction unless the owner chooses (b) |
| M6-2, M7-4 | **O-8** the RF beacon floor | Yes for M7-4's migration behaviour; one constant for M6-2 |
| M7-2 | **O-10** the four admin-surface calls | Yes, and all four have recommendations |
| M7-3 | **O-11** which `chonky` gaps to upstream | No — the local components work either way |
| before first release | **O-9** is a Pi Zero in use? | No, but it decides whether ARMv6 needs real-hardware validation before publishing |
| N-1 | **O-6** which legacy surface is dead | No — one access-log query, and it only shapes the deprecation list |
| — | **O-3** the compression hold rate, **O-4** the column gap | No. O-4 falls out of M5-1 as a side effect |
