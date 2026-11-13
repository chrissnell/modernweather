# ModernWeather — implementation plan

**What this is.** The ordered build sequence for the Rust implementation, derived from the
sixty-seven decisions in `docs/DESIGN.md`'s ledger and the sixteen work items
`docs/reconciliation.md` §4 raised. **It writes down nothing the design documents did not decide**;
where a milestone needs something undecided, it says so and names the open question rather than
inventing an answer.

**Shape.** Eight milestones (M0–M7), thirty-four work items. Each item is sized to be **reviewable in one
sitting** — roughly one PR a reviewer can hold in their head — and carries its own dependencies,
done-criteria and test strategy.

**One standing rule for gates.** `ci/gates.toml` now declares twelve gates; five are armed and
seven are deferred, each on a path a milestone below creates (**D-64**). **The work item that arms a
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

## Milestone 0 — the database platform *(added 2026-10-31)*

**Independent of every other milestone and on the critical path for M5-1 only.** It can run in
parallel with M1 and M2 because nothing in those touches a real database beyond CI's service
container. `docs/reconciliation.md` §7 has the full reasoning and the measurements.

### M0-1 — TimescaleDB as a StatefulSet on a dedicated node *(N-17)*
**Depends on:** the owner adding the node. **Blocks:** M5-1.
A single-replica StatefulSet on a `local` PersistentVolume, `timescale/timescaledb-ha:pg17` (the
plain image lacks `timescaledb_toolkit`, which D-2 requires — W8 D-17's reason), on **its own Talos
worker**, tainted so nothing else schedules there, with a `local` PV copying the shape of
`graywolf-publish-big-bulky-1` — the pattern transfers; only the node name changes.
**Storage must not be NFS, and should be NVMe** — W4 D-26's refresh policy has 3.06–3.22 s of measured
lag against a 10-second deadline, and A-W4-3 showed ~47% of a durable refresh is WAL fsync. This
workload is fsync-latency-bound, so disk *latency* is the spec that matters; capacity is not (a decade
is ~36 GB).
**`big-bulky-1` is explicitly not the target** — rejected by the owner 2026-10-31 on operating
experience: it is too busy with map builds. `docs/reconciliation.md` §7.3 records why the "measure it
first" recommendation was withdrawn rather than deleted.
**Done when:** `ci/sql/assert-timescaledb-version.sql` passes against it; the schema migrations apply
from empty into a fresh `modernweather` database; the node is tainted and the PV is bound.

### M0-2 — weekly off-site backup, and a restore rehearsal in CI *(N-19)* — ✅ **DONE 2026-11-12**
**Depends on:** M0-1. **Blocks:** cutover (M7-5). **This is the owner's stated condition for
accepting a single replica**, so it is not optional and it is not after cutover.
A weekly `CronJob` dumping **per year** to Cloudflare R2 — so the recurring upload is the current year
(~4–5 GB) rather than the whole decade, since raw is append-only and a historical year cannot change.
**Dump raw + `observations_1d_legacy`/`_1h_legacy` + `config.db`. Do not dump the aggregate levels**
(D-15 makes them disposable, and they would be the largest thing in the file). The legacy archive is
the one that gets forgotten and the only copy of the ~294 rollup-only days.
**Done when:** a restore has been performed from an R2 object and reconciled, **in CI**, not
documented. D-6's first half is the mechanism; this is its untested second half. Also: record the real
dump size, which replaces §7.5's only extrapolated figure.
**Must carry two measured W9 findings:** A-W9-2 — an aggregate *accepts* an `INSERT` and discards it
on the next refresh, so pre-seeded aggregate rows are not a restore mechanism; and W4 D-23's
phase-1-first ordering (0.87 s to make the charts right against 467 s to materialise everything), so
a restore runbook does not block on a full rebuild.
**Landed:** `ci/db/offsite-backup.sh` (a closed relation inventory; raw per calendar year; the legacy
archive dumped with its schema because `migrations/` does not create it; the four aggregate levels
excluded), `ci/db/restore-rehearsal.sh` and the `db-restore-rehearsal` gate, which reconciles the
restored database against the manifest's **sha256 of the ordered COPY stream** rather than against a
row count, re-measures A-W9-2 instead of citing it, and asserts the four `/span` bands after phase 1
only. A restore of 1 382 400 rows from a real R2 object reconciled byte for byte in CI's database.
The dump size is measured and §7.5's extrapolation holds — ~3.4 GB/year stored against its 40–50 GB
decade; `docs/backup-and-restore.md` §4. Manifests in `kube-cluster-helm-values` under
`apps/modernweather-db/backup/`.

---

## Milestone 1 — pay the registry's debts and make CI honest

**No application code.** Five of the six items are under half a day. This milestone exists first
because every one of its items gets more expensive the later it lands, and two of them
(**M1-1**, **M1-2**) become breaking changes once a consumer exists.

**Why this is not "retrofitting CI".** The gate already exists and is green. What M1-4 fixes is a
gate that is green *by deferral* while the deliverable it guards is complete — `docs/reconciliation.md`
G-1. Leaving it until a Rust daemon exists means the suite's first real run is also the first run
that can fail, against the binary it is meant to be validating.

### M1-1 — proto3 field presence *(N-12 / G-3)* — ✅ **DONE 2026-10-31**
**Depends on:** nothing. **Blocks:** M3-2, M3-4, M6-4.
**Landed:** 212 `optional` metric fields in `generated/proto/reading.proto`, dimensions deliberately
excluded, three tests scanning the emitted proto (presence on every metric, presence on no dimension,
and that no field was renumbered). Both of the first two were verified able to fail.
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

### M1-2 — dimension `role = "metadata"` *(N-14 / OQ-W4-2, OQ-W2-6)* — ✅ **DONE 2026-11-01**
**Depends on:** nothing. **Blocks:** M1-3's pruning test; the eventual `stationname`/`stationtype`
drop; D-61's provenance column if it is ever wanted.
**Landed:** `Role::Metadata`, and `deprecated`/`deprecated_in` on `[[dimension]]` gated to that one
role. `snapshot::from_registry` now reads a dimension's `deprecated` instead of hard-coding `false`,
which is the constant that made `[[dimension]]` removal a one-way door. `generated/` is
byte-identical, because the capability adds nothing until a registry entry uses it.
**The role transition is classified, which M5-5 depends on:** the snapshot records stored-ness in
its `kind`, so `attribute` → `metadata` is BREAKING (the column leaves the DDL; no `DROP COLUMN` is
emitted, because an attribute number is never released) and `metadata` → `attribute` is an added
column. Before that, a role edit changed nothing the diff compared, so `validate` reported `all
check out` while `codegen` removed the column. `migrate` also now resolves an added column from the
dimensions as well as the metrics; it searched only `reg.metrics`, so a new `[[dimension]]` reached
`generated/sql/` and never reached a deployed database.
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

### M1-3 — `codegen` prunes what it no longer writes *(N-15 / OQ-W4-4)* — ✅ **DONE 2026-11-01**
**Depends on:** nothing. **Done when:** adding and then removing a `[[level]]` leaves no orphan under
`generated/`, and `cargo xtask codegen` followed immediately by `codegen --check` always exits 0.
**Test strategy:** the round-trip above as a test, which is also the regression: today adding the
5-minute level orphaned two files and left a tree `--check` rejects.
**Landed:** `codegen` deletes every file under `generated/` that it did not write, names each one on
stdout, and removes a directory the deletion leaves empty. It reaches nowhere else and does not
follow a symlink out of the directory it owns. The round trip is a test — insert a 2-minute level
between `1m` and `5m`, which renumbers three files, regenerate, remove it, regenerate — run against a
scratch repository root holding a copy of `metrics/`, so the intermediate state is never somewhere it
could be committed. `generated/` is byte-identical: the committed tree had no orphan to remove.
**The prune and the check now agree on what an orphan is, which they did not.** `--check` listed the
tree by reading each file as UTF-8, so a file under `generated/` that is not valid UTF-8 was invisible
to it while being stale by construction — nothing an emitter writes is binary. Both halves now answer
by path, driven from one `codegen_files` call, and the test asserts the agreement on exactly that
file. Verified able to fail: with the delete removed the round trip reports the three
`in the tree but not generated` lines of OQ-W4-4; with the prune walking the repository root instead
of `generated/` it reports `metrics/keep.toml`; with orphan detection back on file contents it misses
the binary one.

### M1-4 — run the conformance suite against the Go daemon in CI *(N-3 / G-1)* — ✅ **DONE 2026-11-01**
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
**Landed:** `ci/conformance/go-target.sh` plus three SQL files and a README; a
`conformance-fixtures` job in `pr.yml` wired into `gate`. 70/70 pass against the Go daemon, and
the same harness fails 26 fixtures through `mangle_proxy.py` while 44 still pass. Three failure
directions were watched rather than argued: `CONFORMANCE_FORCE_MANGLE=1` sends the accepted
replay through the proxy and the gate goes red; `MIN_MANGLED_FAILURES=99` makes the floor reject
a control that stopped biting; and pointing the runner at a dead port produces 70 `FAIL` lines
that are all `request failed:`, which the transport guard rejects and a bare floor would have
accepted as a held control. Two rig properties had to be pinned for the run to be reproducible
rather than green-by-luck, both now recorded in `legacy-api-contract.md` §10 because they are
W1's rig rather than this harness: the emulators derive their solar curve from their own wall
clock, so they run in a fixed-offset zone at midday (otherwise `/almanac` loses `high_solar`
after 18:00 local); and `refresh_almanac_cache` is unscheduled after the migrations, because a
post-seed fire was observed taking `/almanac` from 8 keys to 14 and breaking two fixtures. The `conformance`
gate was confirmed deferred on `crates/modernweather/Cargo.toml`, a path M2-1 creates, and its
job now reports that deferral into the PR summary instead of only into its log.
**Also in this item:** repoint the `conformance` gate (the Rust-target one) at
`crates/modernweather/Cargo.toml`, done in W6's PR, and verify `cargo xtask gates` shows it deferred
with an honest reason rather than an unreachable path.

### M1-5 — the gate runner distinguishes exit 1 from exit 2 *(N-8 / C-W2-20)* — ✅ **DONE 2026-11-01**
**Depends on:** nothing. **Done when:** `cargo xtask gate codegen-drift` reports "regenerate and
commit" on exit 1 and "your registry change is not allowed" on exit 2, and a gate may declare a
per-code message in `ci/gates.toml`.
**Test strategy:** two `xtask` tests driving a stub command that exits 1 and 2.
**Landed:** `on_exit.<code>` in `ci/gates.toml`, declared for `codegen-drift`'s 1 and 2 and
available to any gate; `note` stays the catch-all for an undeclared code. `gates::run` returns the
command's own exit code instead of mapping every failure onto 2, so the distinction reaches the
exit status as well as the log — `cargo xtask gate codegen-drift` exits 1 on drift and 2 on a
rejected registry change. Both were watched end to end, not only through the stub: a line appended
to `generated/ts/metrics.ts` produced exit 1 with "regenerate and commit", and a duplicated
`field_number` produced exit 2 with "your registry change is not allowed". Nine `xtask` tests, and
four negative controls were run rather than argued — reinstating the collapsed single string fails
both code tests; emptying the manifest validator fails the three malformed-`on_exit` tests;
deleting the two messages from `ci/gates.toml` fails the test that reads the shipped manifest; and
hard-coding the message lookup to 1 fails the exit-2 and the fallback test.
**Not in this item:** `legacy-import-drift` has the same 1-vs-2 shape and could declare the same
pair, but it is W9's gate and nothing in this item requires it.

### M1-6 — `solarjoules`, and the four W2 follow-ups' loose ends *(N-9, N-7, N-13, N-16)* — ✅ **DONE 2026-11-02**
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
**Landed:**

- **N-9 — `solarjoules` is a `total`.** `pkg/solar` is not where the answer was: it holds the
  clear-sky irradiance models and the sunrise arithmetic, all in W/m², and never mentions
  `solarjoules`. Nothing computes the value — it is the Campbell datalogger's `slr_mj` field,
  carried through untouched by `internal/weatherstations/campbell/station.go`, and it is energy per
  unit area accumulated since the logger's previous output, which is why the same packet also
  carries `slr_w` for the irradiance. The plan's "one registry line" is one registry line, and it
  is a **BREAKING** change against the snapshot (an aggregation-strategy change), so it also needs
  `migrate --allow-breaking`, a committed migration that drops and recreates all four levels, and a
  regenerated snapshot. Three consequences the plan did not name, the first of which contradicts
  this brief:
    - **"`solarjoules` is not on the frozen surface" is wrong.** `contract/legacy-v1.json` lists it
      on `/latest` and `/span/{span}` in the Go response type, the Swift response type and the
      client crosswalk. `/span`'s value for that key therefore changes by roughly the bucket's
      reading count — ~30× at the 6 h band to ~43,200× at 366 d — and `/latest`, which serves the
      last raw reading, now reports a different physical quantity under the same key and the same
      declared unit. Recorded as **DEV-7** in the deviation ledger and **marked as needing the
      owner's acknowledgement**: the physics is an agent's judgement, whether a chart's numbers may
      move three orders of magnitude is not. The conformance suite compares shape, type and
      formatting rather than values, so nothing in CI will ever flag it.
    - The rebuild's `CASCADE` takes W4's four composite indexes, three retention policies and four
      refresh policies with it, so this item carries a hand-written follow-up migration that
      re-asserts them. `ci/db/schema-assertions.sql` A-8 and A-9 catch the policy half; **the index
      half had no assertion at all**, so an omission there would have been green on every gate in
      the repository — this item adds **A-10**. After the W9 cutover the same `CASCADE` also takes
      `observations_1d_with_legacy` and `observations_1h_with_legacy`, which only the import
      recreates; both are now named in the migration header. **N-22** is the structural fix.
    - No Go view computed `sum(solarjoules)`, so `solarjoules` becomes the fifth unconsumed archive
      column and `solarjoules_total` lands NULL for pre-cutover history rather than importing a mean
      as a total. The archive's `avg(solarwatts)` × bucket seconds *is* a route to a derived total,
      so this is a decision rather than an impossibility; it is rejected on the merits — a derived
      number is not a measured one, and it depends on **N-23** — and it is W9's to revisit.
- **N-7 — a `[[unit]]` table.** 25 entries, `name` plus the `label` a reader is shown. `validate`
  rejects a unit no `[[unit]]` declares, naming the case-insensitive near miss when there is one;
  rejects a duplicate declaration; and rejects two declarations differing only in case, because
  declaring `degf` beside `degF` is the obvious way to make the typo validate and would reopen the
  hole. The table is emitted as `UNITS` into `generated/rust/metrics.rs` and
  `generated/ts/metrics.ts` (plus `UNIT_LABELS`), so the UI has a label to render instead of the
  internal string — which is the half of C-W2-19 a validator alone does not close.
- **N-13 — `affected_levels` compares rendered level definitions.** `metrics/snapshot.toml` now
  records each level's own definition — bucket, source, `materialized_only` — beside the allocated
  identifiers, and `affected_levels` compares that against the registry's, with every aggregate
  column and the expression that produces it. The level's own definition is recorded and not just
  its column list because the 5-minute insertion is the case this exists for and the hourly level's
  *columns were identical* across it: only the relation it reads changed. A snapshot with no
  `[[level]]` section is read as "nothing to compare against" and rebuilds everything, which is the
  safe direction and the behaviour this function had before. The rendered definition also carries
  the time and partition column names, resolved on each side by stable key: `aggregate_sql`
  interpolates both into every level, so without them renaming either rewrote all four generated
  views and returned nothing to rebuild. A companion `removed_levels` reports levels the snapshot
  records and the registry no longer declares — dropped and *not* recreated, which is a different
  statement and so a different list; it could not be computed before the snapshot held levels, and
  without it removing a `[[level]]` left its continuous aggregate and both its policies in every
  deployed database forever, reading the level below it. And `migrate` no longer prints "registry
  and snapshot agree" on a level-only change: `changes.is_empty()` stopped meaning that the moment
  the snapshot grew a level record.
- **N-16 — phase 1 uses the widest band above.** `ci/db/rebuild-aggregates.sh` refreshes each level
  over the widest band any level above it serves, and `--recent-only` names the levels it left
  incomplete — `observations_1d` alone under these bands, because it is the only level whose
  retention is unbounded. The cost is stated in the script's header rather than absorbed: phase 1
  is no longer seconds, because a correct 366-day daily level needs a correct 366-day hourly level
  beneath it and, under D-11, every level below that as well. The seconds it used to take were the
  seconds of not doing the work, and the work not done was being written into `observations_1d` as
  zeroes. Two costs the fix moves rather than removes, both raised as **N-21**: phase 1 now
  materialises ~8.8 GB into a level that retains 7 days, and because it runs for minutes a
  retention job firing mid-phase could hand an upper level a truncated source — an exposure phase 2
  has always had over unbounded history and that N-16 extends to `--recent-only`. **N-25** records a
  cumulative-widening phasing that would be correct *and* keep D-23's sub-second first pass; it is
  not taken here because its correctness rests on invalidation propagating between levels, which is
  a property to verify against a database rather than to reason about. D-23 is marked partly
  superseded where it is stated, and the 0.87 s / 537× figure is marked as measuring the superseded
  form everywhere it appears.

**Ninety-seven tests, twenty-nine of them new** after a review pass. Seven for N-7 (the undeclared
string, a no-near-miss control, the case collision, a duplicate declaration, an empty label, a name
that would not survive codegen, and the shipped registry against the emitted table); fourteen for
N-13 (the 5-minute insertion asserting the minute level is *not* rebuilt, a source-only change, a
bucket change, a deprecation, a change the levels cannot see, both no-previous-record paths, a
time-column rename, a partition-column rename, a removed level reported as gone rather than
affected, the migration that drops it without recreating it, a level-only change that is not
"nothing to emit", an unchanged hierarchy reporting nothing gone, and the shipped pair forcing no
rebuild); one for N-9 against the emitted SQL at all four levels; seven driving the shell script
itself with `PSQL=echo`, which is testable only because W9 had already made the psql invocation
overridable. Two existing `legacy` tests change their asserted figures, which is the N-9 decision
arriving where it should.

**Twelve negative controls, each watched failing rather than argued:** deleting the unit check fails
two unit tests; removing the near-miss hint fails the one that reads the message; removing the
case-collision check fails that test alone; declaring the units and not emitting them fails the
shipped-registry test; reverting `affected_levels` to the metric-keyed rule fails five; dropping
the level record from the rendered snapshot fails four; putting phase 1 back on each level's own
band fails two script tests; removing the `--recent-only` warning fails one; and reverting the one
`solarjoules` line fails six — the N-9 test, both `legacy` figures, the shipped-registry rebuild
test and both `prune` tests, which is three more than was first claimed. The three controls added
during review: making `removed_levels` return nothing fails two; deleting the unit-name charset
check fails one; and the two rename tests were watched failing before the dimension columns were
resolved per side rather than taken from the current registry on both — they reported `[]`, which
is the defect itself. **A-10 has no local control**: it needs a database, so it runs for the first
time in CI, and the honest statement is that its *absence* was demonstrated (no gate in the
repository looked at an aggregate level's index) rather than its presence.

**Not in this item, and each is now a tracked work item rather than a paragraph** — prose under a
heading is not a work item and does not surface again, which is G-5's whole lesson. **N-21**:
suspend the retention jobs for a rebuild and state its transient storage. **N-22**: make W4's policy
block re-assertable the way `migrate` already re-asserts the finalisers, which removes both the
hand-copied follow-up migration and the window in which a deployed database has aggregates with no
policies. **N-23**: `solarjoules` is declared `unit = "J_per_m2"` and `slr_mj` is MJ/m² — a factor
of 10⁶ in a rendered label, one non-breaking line, deliberately not bundled with DEV-7's change to
the meaning of the same key. **N-24**: `migrate` emits `ALTER COLUMN ... TYPE` before the drops, so
§2.2's documented breaking type-change path has never been executable — pre-existing, found while
reviewing the block N-13 rewrote. **N-25**: the cumulative-widening phase 1.

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
captured frames; and **DEV-8 is recorded in `docs/reconciliation.md` §3 and acknowledged by the
owner** — the Go driver stores Celsius in a `degF` column, hPa in an `inHg` column and rain tip
counts in inch columns, so correcting it changes what an existing WeatherLink Live series means and
no gate can see it (the same carrier M1-6 has for DEV-7).
**Test strategy:** the three levels W3 D-82 specifies — pure codec against captured frames, `Session`
against a recorded transcript, and the whole driver against a Go emulator. **The transcript level is
the one that matters**: it is what would have caught the Davis "stops polling after one batch" defect.
For a driver with no `Session` — which WeatherLink Live is, and every inbound driver is (W2a §4a.2) —
that level is *a recorded sequence of transport output*, and the assertion is the same one: every
frame in the sequence produces a reading, not only the first.
**The emulator is not necessarily the pinned reference tree's**: it has none for WeatherLink Live, so
M3-1's lives in this repository and `tests/emulators/manifest.toml` records what that costs against
D-40. See **O-13**. `tests/emulators/manifest.toml` is created here, which arms `driver-emulators` — an
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
> **D-56's first clause is not implementable, and the implementation departs from it.** `admit` may
> do no I/O but the spool append (**D-87**) and `ci/layers.toml` forbids `mw-pipeline` both `sqlx`
> and `mw-store`, so "upsert a PostgreSQL row" is not an expression that crate can contain. Both
> projections are maintained by `writer:timescale` instead, in one transaction with the rows they
> describe. Filed as proposed amendment **A-M3-3-1** in `docs/reconciliation.md`'s *Proposed
> amendments* section; D-56 stands as written until that is applied. (Note also that this line says
> `config.stations` where D-56 says `stations`: those are two different tables, and D-56 is about the
> weather schema's read-side projection.)
**Done when:** readings land in `observations`; a replayed prefix produces no duplicate rows; an
unreported metric is **NULL**, not `0.0` (D-26); `station_metrics` reflects what has been written.
**Test strategy:** needs a real TimescaleDB container, and per W3 D-84 **the test fails CI if it
skips** — `cargo xtask gate db-tests` is created and armed here, which is one of G-4's four
undeclared gates and the one whose whole purpose is to make "green because it did not run"
unreachable.
**Constraint from M2-4, recorded here because this item is where it lands:** `db:postgres` must
**assert the database version when it first connects**, reusing `mw_store::version::check`. M2-4
asserts **D-62** in the boot sequence, but only against a server that answers: a server that is
unreachable or slow at startup is logged and started past, because refusing to boot because
TimescaleDB is down is the failure **D-27** rejects. That carve-out is only safe while something
re-asserts, and this is the item that owns the pool. `tests/db.rs` should also cover
`mw_store::version::probe` itself against the container — its SQL, its column aliases and its
decoded types have no other test, because no gate runs PostgreSQL for `mw-store` until this item
arms one.

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
**Constraint inherited from M3-3, which is the first item to need the read half of the store.**
`db:postgres` publishes exactly one handle per capability and today that handle is the **write** half
— `Arc<dyn ReadingSink>`, which is what `writer:timescale` resolves. W3 §2a.1 keeps the read half a
separate trait on purpose (**D-30**: "named separately so nobody makes the website a sink"), so
`span`/`latest`/`window`/`almanac` arrive as their own trait and **not** as four more methods on the
write one. This item is the first to need it, so it chooses between the two shapes the design leaves:
one published handle offering both halves, or the write half travelling outside the capability bus
the way `Ingress` does today. `mw_component::store`'s module documentation states both and
deliberately does not pick, because the choice depends on the read side's query set.

**One more inherited from M3-3.** `stations.timezone` is populated by `writer:timescale`, so
`Calendar { TzSource::Station }` has a zone to read for any station that has reported — **and this
item validates it**, because nothing else does. The W4 migration's comment claimed the writer did;
it does not, it copies the column and substitutes `'UTC'` for a NULL or blank one, and that comment
is now corrected to say so. An IANA name cannot be checked by a SQL `CHECK` (`now() AT TIME ZONE x`
is STABLE), so the natural place is `mw_store::RegistrySnapshot::new`, which already normalises a
blank zone to absent — a `chrono_tz` parse there turns an operator's typo into a startup refusal
naming the station instead of a rollover that silently happens in UTC.

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

### M4-2 — `mw-api-legacy` *(landed 2026-11-09)*
**Depends on:** M4-1, M2-5. **Blocks:** M4-3, M5.
The compatibility adapter, which exclusively owns the 133-key DTO, both duplicate key pairs, the
`mm`→inch split, `omitempty` and the four error-body conventions. Legacy naming cannot leak
into the core because `mw-types` has no `serde` dependency at all, and zero-filling cannot leak into
storage because the only `unwrap_or(0.0)` in the workspace is `mw_api_legacy::zf` (D-10) — both
enforced by the layout gate.
`/span` per **D-54** and **D-55**: `ReadingStore::span` returns the materialised prefix plus the
trailing bucket aggregated from raw, and the 60-second cache holds the **prefix only**.
`/forecast` reads the stored `jsonb` text byte for byte and splices it between a prefix and a suffix
(D-39) — nothing parses or re-emits it, which is how Postgres's whitespace and the missing trailing
newline survive.
**Constraint inherited from M3-3 (DEV-9): `windch` and `heatidx` are RECOMPUTED here, never read
from their columns.** `metrics/registry.toml` marks both `legacy_read_time_computed` and
`docs/metric-registry.md` §6.1 left "which one the contract actually promises" to W1; the frozen
handler recomputes them from `OutTemp` plus wind speed or humidity on every request
(`transform.go:18`), and M3-3 now stores `NULL` where the formula does not apply. Reading the column
instead would emit `mean(windchill)` where the contract emits `windchill(mean(outtemp),
mean(windspeed))`, which differ by far more than rounding at aggregate levels — measured, 46.04
against 33.64 for a two-reading bucket at (40 °F, 10 mph) and (60 °F, 10 mph). So this is settled
rather than open.
**Scope narrowed 2026-10-31** (`docs/reconciliation.md` §6): only the **five** endpoints iOS calls are
frozen — `/latest`, `/span/{hours}h`, `/snow`, `/stationinfo`, `/forecast/{hours}`. `/alerts`,
`/api/remote-stations` and `format=msgpack` are **not implemented** (§6.3), which also disposes of
W1-BUG-2 by deleting its subject. The website's endpoints are **coupled, not frozen** — they may
change, but only in lockstep with the embedded JavaScript in the same PR, because server and website
ship as one binary (D-69). O-5's `/stationinfo` fix lands here as a signed deviation with a new
fixture.
**Done when:** `cargo xtask gate conformance` is **green against the Rust binary** for every
frozen-class and coupled-class fixture, and the negative control still fails. This is the milestone's
headline. **The `conformance` job is also back among the `gate` job's dependencies in
`.github/workflows/pr.yml`, restored in this item's own PR** — M2-5 had taken it out because a gate
that is red by design for two milestones must not be the single required check, and the line went
back the moment it was green. The two halves of that decision are asserted to be one in
`xtask/src/conformance.rs`, so neither can be undone alone.
**Test strategy:** the conformance suite is the specification. Plus a regression test for W1-BUG-1 —
`/span` for a station with no non-NULL `winddir` must return 200 with `windd: 0`, which W4 D-8's
finaliser makes true and which was a 500 in Go.
**Deviations that land here:** DEV-1 (`ETag` on `/latest` and `/snow`) and DEV-5 (the `X-MW-*`
headers) — both additive and invisible to the suite by a verified property of the runner
(`contract/conformance/src/lib.rs:320`) — plus **DEV-10** (O-5's `/stationinfo` fix, carried on the
two existing fixtures rather than on a new one), **DEV-11** (a non-finite value zero-fills rather
than truncating the body), **DEV-12** (`/span`'s window is bucket-aligned, so a response carries one
element more than Go's — the price of D-54's stable cache key) and **DEV-13** (an unqualified
`/forecast` takes the most recently updated row, because the new table has no surrogate `id` to
recover Go's insertion order from). DEV-12 and DEV-13 were found by this item's code review; both are
one-line decisions the owner can reverse.

#### What landed, and the four places it is narrower or wider than the above

**1. The done-criterion was not satisfiable as written, and closing that was part of the item — in
two directions rather than one.** The scope narrowing meant the nine `retired` fixtures would still
fail when every implemented endpoint was right, which this item's own brief recorded. The **second**
direction it did not: the coupled class also contains the six HTML pages and seven embedded assets,
and those are **M4-3's** by this plan's own text three paragraphs down — the `askama` port, the
`GoHtmlEscaper`, the mime table, and a done-criterion that is a byte diff against those same
recorded bodies. So "every coupled-class fixture" could not be green at M4-2 either.

What landed is the classification as **data the runner reads** (work item **N-18**), plus a
**policy** in `xtask/src/conformance.rs` that is checked in both directions:

| Class | Count | The gate's rule |
|---|---|---|
| `frozen` | 42 | every one must pass |
| `coupled` | 19 | every one must pass **except** the thirteen declared pending on M4-3 |
| `retired` | 9 | reported, never gating |

The pending list is declared by name next to the policy rather than read off the fixtures, and a
fixture excused without a declaration **or** a declaration whose fixture has started passing is exit
2. **M4-3's done-criteria therefore gain one line: the `PENDING` list in
`xtask/src/conformance.rs` must be empty when that item lands**, and the gate will say so by
refusing a stale excuse.

**2. `/forecast` needed a table W4 never specified.** `docs/database-schema.md` does not mention a
forecast anywhere; W1 assumption 12 names `almanac_cache` and `snow_totals_cache` as the two caches
the frozen contract reads directly and stops. M6-3 owns the Aeris *components*, and this item owns
the `/forecast` **handler** — which had no relation to read from. `migrations/` gains a narrow
`forecasts` table, created by the item that first needed it rather than left for M6-3; **M6-3 should
extend it rather than recreate it**, which an `ADD COLUMN` makes cheap (**D-9**). This is the same
class of gap `docs/reconciliation.md` **G-2** records for the configuration store.

**3. M4-1's `cx.soft()` defect has a second instance with no second fix, so the mechanism changed.**
`BuildCx::soft` reads the slot once and the supervisor never rebuilds a `Running` component when a
soft capability later arrives. M4-1 fixed `snapshot:latest` by taking the handle from the registrar,
which works because `RollupState` is a root resource `main` owns. `api:legacy`'s three read stores
belong to `db:postgres`'s pool, which cannot be one. `mw_component::Deferred<H>` is the answer — a
soft-only handle read when it is used — and it is recorded as **A-M4-2-1** in
`docs/reconciliation.md`'s *Proposed amendments*.

**4. `api:legacy` is planned only where a `LatestSnapshot` provider will be.** The hard need set is
exactly `{LatestSnapshot}` and a test asserts it, which is the obligation M4-1 recorded. The
consequence nobody had written down is that `plan()` refuses a configuration whose hard need nobody
provides — and `snapshot:latest` is the **only** provider of that capability (`snapshot:warm` writes
the table and does not publish it, because the capability is `Exclusive` and two providers would be
a plan error). So a node with an empty snapshot key set, or with neither a spool nor a weather
database, would have been a daemon that refuses to start. `mw_pipeline::snapshot::
provides_latest_snapshot` is the one copy of that condition and the registrar asks it.

One consequence of that worth stating rather than discovering: a node with **no website row** gets no
frozen route table, so a request to it is `mw-http`'s bare `404` rather than the contract's
`503 {"error":"No weather websites configured",…}` (W3 §6.3). That body is reachable only on a node
that has a station and at least one website whose `Host` the request does not match. No fixture
covers the no-website case.

**What else it needed, and did not have:** the weather database had to be **seeded**, which
`ci/conformance/README.md` already recorded as this item's. `ci/conformance/rust-seed/` is a
translation of W1's rig onto `observations` and W4's four aggregate levels, asserted per station and
in both directions — two stations must have *no* readings, because that is what four fixtures are.
The rig also configures a `node` row, which the Go rig had no equivalent of: without one the daemon
has no spool, so no reading stream, so no `snapshot:latest`.

**`MIN_MANGLED_FAILURES` was re-derived, and the shape changed rather than the number.**
`contract/README.md`'s 26 is a count of *failures* against a Go daemon that passes all 70 directly.
Against this target 22 fail whatever the proxy does, so a failure floor would be cleared by a proxy
that mangled **nothing** — the exact failure mode W1 Decision 10 exists to prevent. The control now
measures `direct.passed - control.passed`, the responses the proxy actually **broke**: measured at 23
of the 48 the target serves, with a declared floor of 20.

**Two deviations beyond the two listed above.** **DEV-10** is O-5's `/stationinfo` fix, recorded on
the two affected fixtures as a `deviation` block the Rust-target replay applies and the Go-target
replay does not — one fixture, both halves, both gates still asserting what their own target
produces. **DEV-11** is `zf` filling a non-finite stored value like an absent one; Go fails the
whole encode for it and `serde_json` would write `null`, and neither is a response a frozen route
may give.

**Every constraint M2-5 and M4-1 recorded is discharged.** M2-5's three — the done-criterion's
satisfiability, the seeded weather database and `MIN_MANGLED_FAILURES` — are the three sections
above. M4-1's five are: `Config::snapshot_render` returns `Some` and both snapshot components plan
(the `boot.rs` tripwire inverted and its count went from 6 to 9); `api:legacy`'s hard needs are
asserted to be exactly `{LatestSnapshot}`; the two additive headers read `Snapshot::age_seconds` and
`Staleness::missing` and go out only when the snapshot is degraded; and `ETag::matches` is what
`/latest` compares `If-None-Match` with, with `Cache-Control` left alone.

**D-48's byte-level half is the fifth, and it has its own test**:
`crates/mw-api-legacy/tests/degraded.rs` drives the real router with all three soft capability slots
**empty** -- which is how the supervisor expresses a database that is down -- and asserts each of the
five frozen routes' contract response plus a blanket "not one `503` and not one `5xx`" over nineteen
paths. `cargo xtask gate conformance` cannot establish this, because it replays against a running
database; that is exactly why the test is in-process and takes the capability away rather than the
server.

### M4-3 — `mw-http` and `mw-web` *(landed 2026-11-10)*
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
**Constraint from M4-2: the `PENDING` list in `xtask/src/conformance.rs` must be empty when this
item lands.** Thirteen coupled-class fixtures -- the six HTML pages and the seven embedded assets --
are excused from the `conformance` gate by that list because their subject is this item's. The gate
checks it in **both** directions, so a declaration whose fixture has started passing is exit 2 and
says to remove the entry; the list emptying is this item's visible done-criterion on that gate.
**Two further responses are this item's, and both are currently `mw-http`'s bare 404**: the
`404 page not found\n` body with `Content-Type: text/plain; charset=utf-8` that the asset file server
gives (fixtures `asset-not-found` and `html-portal-on-non-portal`), and the `Vary: Accept-Encoding`
that every one of the 70 fixtures asserts. `mw-api-legacy` sets that header explicitly on every
response, because the runner compares a header's **value** exactly and `tower_http`'s
`CompressionLayer` writes `vary: accept-encoding` in lower case -- so when this item adds compression
it must not also add a second `Vary` (`mw_api_legacy::reply`'s module documentation).
**Constraints from M4-1**, recorded here rather than in a source comment (`docs/reconciliation.md`
G-5):
1. **This item builds the thing that can emit a `503`**, which is the unmounted-declared-path
   response W2a Decision 19 specifies. **D-48** forbids it on the five frozen patterns, and M4-1
   established the structural precondition; this is where the response itself first exists, so the
   route table's mounting rule is the last place the invariant can be broken. `grep` for `503` found
   only prose before this item.

   *Landed differently from that text, and M4-3's code review asked for the difference to be
   stated: nothing in `mw-http` emits a `503`. The per-website tables do —
   `mw_web::component::unmounted`, and `crates/mw-web/tests/routes.rs` asserts its bytes — and for
   a declared path whose component nobody planned, `boot.rs`'s
   `assert_route_tables_match_the_plan` makes the state unrepresentable at startup rather than
   answering it per request. That is the stronger outcome. The constraint's substance — the mount
   is the last place D-48 can be broken — is `mw_http::MountError::FrozenPatternShadowed`.*
2. **`mw_snapshot_age_seconds` and `mw_snapshot_degraded` freeze rather than climb.** Both are
   written only when a snapshot is folded, so a station that stops reporting — the one transition
   worth alerting on — leaves its gauge where it was. The sweep belongs on the scrape path, which is
   this item's: recompute every key's age from `LatestSnapshot::table()` when `/metrics` is rendered.
   Until then the honest alert is `rate(mw_snapshot_published_total)` going flat.
3. **`web:site:*` requires `LatestSnapshot` Hard** (`docs/component-model.md` §2.6), so M4-2's
   constraint 2 applies to the website's route tables as well: a hard need outside
   `{LatestSnapshot, ConfigStore}` on a table carrying a frozen pattern reopens D-48.

#### What landed, and the six places it is narrower, wider or different

**1. The byte diff passes, and getting there found two things §7.1's account of `GoHtmlEscaper`
leaves out.** The acceptance test reads `contract/fixtures/html-weather-station.json`,
`html-portal.json`, `html-portal-root.json`, `html-privacy.json` and `html-support.json` — the
recordings themselves rather than a copy — applies the one expected delta as a patch, and compares
every byte. It has a negative control, and the delta is asserted to be load-bearing: comparing
against the *unpatched* recording must fail, or the configuration block would not be being emitted.

The two things it found are **A-M4-3-1** and **A-M4-3-2** in `docs/reconciliation.md`:

- §7.1's table is missing `+` → `&#43;` and gets NUL wrong (Go writes `U+FFFD`, not `&#0;`), and
  `html/template` is **contextual**, so the three values inside `<script id="radar-config"
  type="application/json">` are escaped as *JavaScript* — numbers padded with spaces by
  `jsValEscaper`, which no HTML escaper would produce. `mw_web::gojs` is that second escaper, and
  `crates/mw-web/tests/escaping.rs` asserts every byte of both tables against output produced by
  **running** Go's `html/template` over every ASCII codepoint in four contexts.
- `html/template` **removes comments**, so the recorded bodies carry **42** fewer than
  `weather-station.html.tmpl` does — 29 HTML comments and 13 JavaScript line comments. The port
  keeps all 42 as askama `{# … #}` comments, which occupy no output bytes, and
  `assets/portal.html` is the Go server's rendered output rather than its template source. Byte
  equality is therefore real rather than normalised.

  **The rule is "remove" for an HTML or JavaScript *line* comment and "replace with a space"
  for a CSS or JavaScript *block* comment**, which M4-3's code review corrected: `escapeText`
  substitutes a single space for `stateCSSBlockCmt` and a space or a newline for
  `stateJSBlockCmt`. `weather-station.html.tmpl` carries no `/* */` so nothing here depends on
  the difference — but `portal.html.tmpl`'s one CSS comment became a space in the rendered file
  that is now the static asset, and applying the simpler rule to a future `/* */` would silently
  drop a byte the recording has.

**2. `GoHtmlEscaper` is pinned by the layout gate, which needed a new rule.** `ci/layers.toml`'s
`[templates]` check already rejected `safe` and the `escape` filter's `none` argument under
`crates/mw-web/templates/`, and its own `reason` recorded the hole it could not see: "an
`askama.toml` that reassigns the `html` escaper would turn escaping off globally without any of
these appearing anywhere. There is no such file, and the day one arrives it needs a rule of its
own." The day arrived — a custom escaper is the only way to get Go's spellings — so `[templates]`
gains `escaper_config`, `escaper_path` and `escaper_extensions`, and the gate refuses a different
path, a missing extension, a second `[[escaper]]` entry (askama matches the first) and a missing
file. All four are shown failing in `xtask/src/layout.rs`.

**3. The `PENDING` list is empty, and one fixture changed with it.** All nineteen `coupled` fixtures
pass, which is this milestone's done-criterion. `asset-weather-app-js` had to change: **D-38** moves
the nine configuration values out of `/js/weather-app.js` and into the inline block, so the four
`pullFromDevice: 'davis-full'`-style substrings it asserted exist on the Go target and not on this
one — and the **deviation mechanism cannot carry an assertion**, only a recorded response
(`contract/conformance/src/lib.rs:432` makes `assert` the fixture's for both targets by design). So
its `required_substrings` become three that identify the module on both targets, and what the suite
stops checking — that the nine values reach the client — is checked by `crates/mw-web`'s own tests:
the inline block's key set and order, its byte agreement with `/config.json`, and the byte diff.
That is a coupled-class change under **D-68**/**D-69**, moving both halves in one commit.

**`judge_direct` now takes the excuse list as a parameter.** With `PENDING` empty, a test that read
the real list would assert nothing — so the both-directions property is tested over a synthetic list
and the production call site passes `&PENDING`. A both-directions check that cannot fail is worth as
much as a gate that cannot.

**4. `http:listener` is a component, and a held port is no longer a startup failure.** M2-4 bound one
socket directly from `boot.rs` and recorded two consequences: `main` owned a fourth root resource,
and the listener's teardown ran *concurrently* with the supervisor's reverse-topological stops rather
than being ranked by them. The second is closed — the socket now closes after the components whose
tables it serves have stopped. The first is not, and is now explicit: the `Mount` is a root resource
`main` builds, because the tables come from `mw-api-legacy` and `mw-web` and `mw-http` may name
neither.

**The behaviour change is that a port another process holds is a `Failed → Backoff` loop instead of a
non-zero exit** (W2a Decisions 3 and 30, §3.4's cap of 60 s, no permanent give-up), with the address
in `last_error` and the failure in `STATUS=`. That is better than what it replaces rather than a
regression — `Restart=always` plus `StartLimitBurst` turned the old exit into a unit that eventually
gave up and stayed down — and it means `READY=1` can be sent by a daemon whose listener is backing
off, because §3.5's readiness is "the supervisor has converged" and deliberately not "every component
is `Running`".

**5. There are two website components, not one.** `docs/component-model.md` §2.6 names
`web:site:<id>` and stops. The pages are per-website and that is right for them; the **asset tree is
not** — the Go handlers for `/privacy` and `/support` never look at the website (§6.3: they "bypass
the website check entirely") and neither does the file server behind `router.PathPrefix("/")`. So
`web:static` owns the asset tree, `/privacy` and `/support`, requires nothing, and is planned on
every node including one with no website at all. Attaching them to a website's table would have
meant three copies of the asset tree, an arbitrary website owning everyone's assets, or the assets
disappearing when website 1 stops. Recorded as **A-M4-3-3**, which also proposes dropping §2.6's hard
`LatestSnapshot` need on `web:site:*`: it is declared, nothing reads it (the page is a function of
configuration and the binary's version, which §7.3 says in as many words), and declaring it means a
node with websites and no station gets no website — which the registrar handles the way
`mw_api_legacy::register` handles the same thing, rather than refusing the whole configuration.

**6. Two checks exist that the design asked for in prose and nothing performed.** M4-1's constraint 1
says the route table's mounting rule is the last place **D-48** can be broken:
`mw_http::MountError::FrozenPatternShadowed` refuses a `Scope::Site` table that claims one of the
five frozen patterns, which `plan()` cannot see because two tables in different scopes are not a
route conflict. The five patterns live in `mw-http` — the crate that needs them may not depend on the
crate that serves them — and a test in `mw-api-legacy` pins its own list to that one. And
`assert_route_tables_match_the_plan` pairs every mounted table's owner against every planned
`HttpRoutes` provider in **both** directions: a table with no owner is the permanent body-less `503`
M4-2's review found, and an owner with no table is a component that reaches `Running` while its paths
404.

**M4-1's other two constraints are discharged.** `mw_pipeline::metrics::sweep_snapshot_ages`
recomputes both gauges from `LatestSnapshot::table()` on every scrape, so
`mw_snapshot_age_seconds > 600` is an alert that can fire for the station that stopped reporting; and
the hard-need rule on a frozen pattern is check 6 above.

**Three smaller things, each recorded where it happens.** Host normalisation is now one function in
`mw-http` and `mw-api-legacy` asks it rather than carrying a second copy — two answers to "does
`EXAMPLE.COM.` match `example.com`" on a lookup that decides whose data a request gets. `ryu` gains a
`[[dep_rule]]` naming the two crates that may produce Go's digits, which the workspace manifest had
claimed existed since M4-2 and which did not; `crates/modernweather/tests/go_numbers.rs` asserts the
two implementations agree. And **DEV-6's refusal sends `access_denied`, not `unrecognized_name`**:
rustls chooses the alert for a resolver that returns `None` and offers no hook
(**A-M4-3-4**). What DEV-6 is for — a diagnosable refusal instead of a wrong certificate — is
unaffected.

**7. The code review found two Critical defects, and they were the whole reason to run one.**
Three reviewers, scoped to the listener, the website and the gates. Both Criticals have the
same shape — a mechanism that looked present and was not — and neither is visible to any gate
this programme has.

**(a) An HTTP/2 request carries no `Host` header.** RFC 9113 §8.3.1 replaces it with the
`:authority` pseudo-header, which hyper's h2 server puts in the URI and does not mirror into
a header — and both host-resolving call sites read `headers[HOST]`. The listener offers `h2`
**first** in its ALPN list (§6.6, matching the Go server's `NextProtos`), so every browser on
the TLS port negotiates HTTP/2, and every one of its requests resolved to the **default**
website:

- `/` and `/portal` served the default website's page under somebody else's hostname;
- **`/latest`, `/span`, `/snow`, `/stationinfo` and `/forecast` returned the default
  station's readings** — the frozen surface answering with the wrong station's data.

That is the same class of defect **DEV-2** exists to close (the Go version's case-sensitive
map lookup serving someone else's website), reopened through a different door and firing on
every request rather than on a mixed-case one. The conformance suite cannot see it: the
runner is `ureq` over HTTP/1.1, so all 70 fixtures pass either way.

The fix is one function — `mw_http::host::of_request`, which takes the URI's authority first
and the `Host` header second. That precedence is right for both protocols: RFC 9112 §3.2.2
makes an HTTP/1.1 absolute-form request target authoritative over `Host`, and origin-form
(every ordinary HTTP/1.1 request) has no authority in the URI and falls through to the
header. `mw-api-legacy` asks the same function rather than carrying a second copy, so the
seven frozen handlers gained a `Uri` extractor. Three regression tests: the dispatch end to
end over an h2-shaped request, the absolute-form precedence, and the frozen surface's own
seam composed with the website lookup.

**(b) `askama::filters::HtmlSafe` does nothing under a custom escaper.** The page's two
coordinates have to bypass the escaper, because Go writes `1e+21` for a magnitude outside
`[1e-6, 1e21)` and `html/template`'s table turns `+` into `&#43;` — which is not JSON inside
the `<script type="application/json">` block, so `JSON.parse` throws and the radar panel never
initialises. `HtmlSafe` was the mechanism, and its bypass is implemented **only for askama's
own `Html` escaper** (`askama-0.16.1/src/filters/escape.rs:229`); with `GoHtmlEscaper`
registered the autoref specialisation never selects it. askama's `Safe` wrapper is generic
over the escaper and is what works.

**The test that was supposed to catch it asserted a property of the escaper, not of a page**,
so it passed either way — which is the finding behind the finding. There is now a test that
renders a page with `latitude = 1e21` and asserts Go's bytes, and the bypass is gated:
`no-template-escape-opt-out` forbids every askama bypass in the workspace, exempts one file,
and `crates/mw-web/src/page.rs`'s own test reads that file and asserts the exemption is still
exactly two fields each wrapping a number. `stations.latitude` is a nullable `REAL` with no
range `CHECK`, so a bad import reaches this.

**Three Important findings, each closing a check that could not fail.**

- **`FrozenPatternShadowed` compared raw pattern strings**, so M4-1's constraint 1 was
  walkable three ways: `/span/{window}` is the same claim as `/span/{span}`, and a prefix
  mount on `/` or on `/forecast` contains the frozen paths without equalling any of them. It
  now uses `RouteEntry::conflicts_with`, the relation the rest of the module uses.
- **`tests/tls.rs`'s DEV-6 negatives were `is_err()`**, which both outcomes satisfy: if the
  server hands out the wildcard certificate — what Go does — the *client* rejects it for the
  name. Reverting `matches_one_label` to `strings.HasSuffix` left the test green. It now
  asserts the server's refusal alert, and that revert turns it red (shown).
- **`Directory::website_for_host` had no wildcard arm while `HostTable` did.** A
  `*.wild.test` row is representable — the column's three `CHECK`s accept it — so `/` was
  served by that website and `/latest` by the **default** one, on the same request host: one
  hostname, two websites' data. It now asks `mw_http::matches_one_label`, the single-label
  rule the host table and the certificate store already share.

**Two gates could be silenced by one line, which is the same defect in the tooling.**
`check_askama_escaper` was called *after* `check_templates`'s `if !dir.exists() { return }`,
so repointing `[templates].dir` at a missing path silenced both the escaper pin and the
needle scan — while the rule's own prose claimed a missing file was a violation. And an
`askama.toml` `[general] dirs` key parsed clean, moving the templates out of the scan without
touching `ci/layers.toml` at all. The check is hoisted, a missing directory is a violation,
`AskamaConfig` is `deny_unknown_fields`, and both doors have tests that watch them shut.
Separately, **`go-number-digits` could be deleted with nothing failing** — armed but absent
from the id list the layout test walks. It is pinned now.

**Everything else the reviews raised is fixed or recorded.** Eight more findings landed as
code: the catch-all's scope is checked, a `Scope::Any` provider's own fallback no longer
panics `Router::merge`, the rustls provider is named rather than defaulted, the redirector
validates its `Host` against the host table, accept errors are split the way axum splits them,
`/config.json`'s two deliveries are compared **byte for byte** (the docs claimed that and the
test compared parsed values), the mime capture is paired against the embedded tree so a new
asset cannot go unchecked, and the two Go golden generators moved out of gitignored `scratch/`
into `ci/go-golden/` behind `cargo xtask go-golden` — one had an absolute path containing the
runtime's working directory, so nobody else could re-derive the table the tests rest on. Three
things that had no test now do: `assert_route_tables_match_the_plan` in both directions,
`config_db::aeris_stations`, and `mw-web`'s five handlers. A dozen false or drifted claims in
comments and prose are corrected, including the comment count (42, not 29), the rule for block
comments (a space, not nothing), five Go line citations and this item's own `503` text.

**What is deliberately not here.** No gRPC *service*: the socket exists and serves the same router,
and `grpc:weatherv1`, `grpc:weatherappsv1` and `station:grpcrx` are M6-1 and M6-4. No `RouteFlags`
struct: §6.6 lists CORS and `Cache-Control` as bits, and both are set per *response* by the provider
that knows the endpoint — `mw_api_legacy::reply` emits CORS on its JSON bodies and deliberately not
on its four error bodies, and `Cache-Control` differs per endpoint — so a table-level flag would be
less faithful, not more. No `--assets-dir` override (§7.2's development knob): it is a second way for
the asset tree to differ from the binary, which is the property embedding buys. And no `ArcSwap`
*writer*: the mechanism is there and tested across a swap, and the writer is a plan diff, which has
none until M7-1.

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
**The Campbell station is not free in the same way.** A serial line has exactly one reader, so for
that station (b) does not exist until a multiplexer sits in front of the port — **M5-1a**, which is
why that item is where it is.
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
**Depends on:** M3-3, M4-2, **M0-1**. **Blocks:** M5-2, M7-4.
Execute `ci/db/migrate-from-go/migrate-from-go.sh` with pitcairn as source and **M0-1's Kubernetes
instance** as target — a fresh `modernweather` database, which is what the script's own usage header
has always specified. W9 built this; this item *runs* it and records the result.
**pitcairn is never upgraded and never modified** (W9 D-2, D-3 keep the source read-only apart from
one named archive step), so the platform move and the data migration are **one** operation and the
old box stays as the rollback. This is also what reduces **O-1** from a fork in the plan to a detail:
there is no in-place major-version upgrade, because the transform is a logical export and import.
**Prerequisites — all discharged 2026-10-31.** **O-2** done: archive taken and verified, raw
retention paused (`alter_job(1004, scheduled => false)`). **O-1** answered: source is PostgreSQL
**16.6** / TimescaleDB **2.17.2**, so the hierarchy is constructible and there is no in-place
upgrade. **O-7** is **moot** — the rollup history it referred to was already destroyed
(`docs/reconciliation.md` §8), so the rollup import step has no input. **Run `rollup` anyway**: it is
idempotent, it costs seconds, and it is the step that would carry the data if an older `pg_dump` of
pitcairn ever surfaces.
**Done when:** `w9` reconciliation passes with zero tolerance on every metric per year; the two
bounded statistics are inside their derived bound; `w9.assert_no_unknown_source_columns()` raises
nothing.
**Test strategy:** W9's negative controls — the reconciliation was proved able to fail, and a
reconciliation that ran against zero rows raises rather than passes.
**O-4 is already answered and the gap is zero:** production's `weather` has **215 columns**, exactly
the Go source definition, so `w9.expected_column INTERSECT w9.source_column` will match everything.
`w9.assert_no_unknown_source_columns()` should raise nothing — and if it does, something has changed
since 2026-10-31 and that is worth stopping for.

**DONE 2026-11-13. The migration ran, and all four criteria are met.**
`docs/data-migration.md` §2a is the record and §2b is what the run found. In short: **17,162,826**
readings for three stations over 2025-10-30 .. 2026-11-12, reconciled at **1,182 of 1,182** (station,
year, metric) digests with zero tolerance and zero mismatches; the two bounded statistics at 5.77e-7
and 1.65e-3 against bounds of 1.03e-3 and 1.69e-1, with the other twelve **exact** over 964 buckets;
`w9.assert_no_unknown_source_columns()` silent at 215 of 215; 42 of 42 spot checks exact including
each extreme's timestamp; per-year counts in §2a. `pitcairn` was not written to at all — `archive`
found the 2026-10-31 tables and left them, and `pause-source-retention` found nothing enabled and
issued no statement.

**But the station ids it imported could not be station keys, so M5-1b re-ran it.** See below.

### M5-1b — the migrated station keys were not usable as station keys
**Depends on:** M5-1. **Blocks:** the owner's own side-by-side comparison, which is why it is carved
out of M7-4 rather than waiting behind it.
A `station_id` becomes a station key and a station key becomes a `ComponentId`, which permits only
lower-case ASCII, digits, `-`, `_`, `.` and `:`. M5-1 took the documented default `station_id =
stationname`, and two of the three Go display names — `CSI` and `Suncrest Air Quality` — are refused
by that type, so a live station configured against a year of its own history is
`StationPlanError::Key` at plan time. The preview showed it: it minted a **second** station `csi`
beside the migrated `CSI`, and `/span?station=CSI` answered one bucket of live data.
**The fix is a derived default and two refusals, not three corrected values.** Any station with a
capital letter or a space would have hit this. `w9.station_key(stationname)` derives the id,
`w9.assert_station_map_usable()` refuses any id — derived or hand-written — that could not be a
station key, and `w9.seed_station_map()` names two source names that derive one id rather than
letting a unique violation name a constraint. Done through `w9.station_map`, which is what
`30-reconcile.sql` also maps through, so the reconciliation still compares the right rows.
**Two things the re-run needed that did not exist.** `reset-target`, because `stage` creates its
relations without `IF NOT EXISTS` and a procedure whose second run needs hand-typed SQL cannot be
repeated; and `carry-out` / `carry-in`, because the source's raw window had receded 14 days since
M5-1 and 462,495 readings existed only in the target (data-migration.md Decision 3a).
**DONE 2026-11-13.** `docs/data-migration.md` §2c is the record. **1,182 of 1,182** digests `ok` with
zero tolerance, the two bounded statistics at 5.7742e-7 and 1.6502e-3 with the other twelve exact
over 964 buckets, `assert_no_unknown_source_columns()` silent at 215 of 215, 42 of 42 spot checks
exact, and **every settled per-year row count identical to §2a's** — the only two deltas are the two
live stations' 7 h 26 m of new readings. Verified against the running preview:
`/span/8760h?station=CSI` returns **365 daily buckets, 2025-11-13 → 2026-11-13** beside the live
reading, and `stations` holds one row per device instead of four.

**Three defects only this run could find**, all three green in the lab, all three fixed and
re-verified here: the legacy boundary compared a bucket's *start* against a reading's timestamp and
double-counted one bucket per station; the digest's year key was the server's **local** year, and
US/Mountain against Etc/UTC reported all 1,182 rows as mismatches with nothing missing; and the
reconciliation window ran past the imported extent because the source is live. §2b has the evidence
for each, including the control that distinguished the second from real loss.

**Two things for whoever plans the cutover.**

- **The pause O-2 took is not durable.** Retention on `weather` was job 1004, paused 2026-10-31. It
  is now job **1022**, `drop_after = 365 days`, **enabled** — a new job id with identical config,
  which is what the Go provisioner creating policies at start-up looks like. It has already dropped
  raw below **2025-11-12**, so the new database is the only copy of 2025-10-30 .. 2025-11-12. Those
  14 days *were* reconciled against production before it dropped them. Not re-paused here: it no
  longer protects data the new database lacks, and unbounded raw growth on `pitcairn` is the owner's
  disk. One statement reverses it, and a restart will undo that too.
- **The parallel database was two migrations behind** and nothing would have caught up on its own —
  the daemon applies `migrations/config` at start-up, not `migrations/`. Fixed, with the
  `_sqlx_migrations` rows `sqlx` itself would have written; all nine checksums are the SHA-384 of the
  files on `main`.

### M5-1a — a serial multiplexer, so two daemons can read one station *(GRA-787)*
**Depends on:** nothing — both ends already support it. **Blocks:** M5-2's mechanism (b) on a serial
station, and nothing else.

**Not in the original plan.** Proposed on 2026-11-12 and recorded here rather than left on an issue,
because it is the only thing that makes (b) available on a serial station and because deploying it
touches a live machine. It is **N-20**'s production path arriving a milestone early: `ser2net` owns
the character device, and both daemons read it over TCP. The Go side is a configuration change and
not a code change (`campbell/station.go:254` takes the serial path only when `serial_device` is
non-empty); the Rust side already maps Campbell as `Serial | TcpOut`.

**The deliverable is the proof before it is the bridge.** The question that decides whether any of
this is worth doing is whether the multiplexer *broadcasts* every byte to every connected client or
hands bytes to whichever client reads first. If it is the second, the two daemons split the stream
and **both** produce gap-ridden data that still parses — nothing downstream could tell it from
weather, which makes it the worst available outcome and strictly worse than having no (b) for that
station at all. `ser2net`'s documentation says `max-connections` broadcasts; the documentation is
not the measurement.

**Done when:** `ci/serial-bridge/prove.sh` is green — two clients connected at once receive the
identical byte stream, asserted byte for byte over a known sequence pushed through a `socat` pty, so
the whole proof runs with no hardware and without touching a live station; the bridge runs as a unit
that survives a reboot; the Go daemon reads it over TCP with its site serving current readings,
checked by a timestamp and not by a process being up; and `modernweather`'s Campbell driver reads
the same stream at the same time.

**Test strategy:** the assertion plus **two** negative controls, because an identity assertion that
cannot be made to fail proves nothing (W1 **D-10**): the same apparatus with `max-connections: 1`,
where the refused client's stream must differ; and the same apparatus with the bridge removed and
one character device read twice, which is the real failure mode. The second control is also
load-bearing for the runbook — `remoteweather`'s serial library takes no exclusive lock, so a bridge
started while the daemon still holds the port opens it successfully and the two then split the line
with no error anywhere. That is why `packaging/serial-bridge/README.md`'s cutover order is a
measured requirement rather than caution.

**What this item introduces, stated rather than discovered:** the port is writable. `ser2net`
forwards what a client sends to the datalogger. Neither daemon writes to a Campbell line — checked
in both trees — but before this change the only thing that *could* was a descriptor the daemon held.
That is the one new hazard and it is in the README beside the mitigation.

**Nothing in this repository records a particular installation.** The device path, the line rate and
the listen address are site-specific, and each of them fails *quietly* when guessed wrong, so
`packaging/serial-bridge/ser2net.yaml.example` leaves them as placeholders that are deliberately not
valid rather than shipping a plausible default. An operator's actual values belong wherever that
machine's configuration is kept, which is not here.

### M5-2 — the diff harness
**Depends on:** M5-1, M3-1, and **M5-1a** for the Campbell half of (b). Build (b) and (c) above as a
runnable harness in `ci/diff/`, with its expected-diff set encoded as data rather than prose so an
unexpected diff is a failure rather than a judgement call.
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
**Depends on:** M5-3 passing. One row out of the plan. Trivial, and deliberately gated on the
previous item rather than bundled with it.
**Deviation that lands here:** **DEV-17**.

**Amended on running it (M5-4): there was no table and no capability to delete.** This row said "a
table and a capability deleted"; neither ever existed in this tree. `migrations/config/` was authored
from scratch without the table, `Capability` has twelve variants and none is `SunTimes`,
`mw_api_legacy::sun` already evaluates both values per request, and `docs/rewrite/README.md`'s
capability inventory already strikes the row through (reconciliation decision 30). W2a Decision 36
was applied at the point each of those files was written, so this item is the **verification** of a
deletion rather than the deletion: no `DROP TABLE sun_times` was or could be written.

That makes the deletion a property of the *absence* of a statement, which nothing would notice being
undone — so what this item adds is the guard that would.
`mw_store::config_db::tests::the_sun_times_table_is_not_carried_forward` rejects the identifier in
any `.sql` under **two** roots: `migrations/`, weather schema included, and `ci/db/`, which is where
the Go data actually crosses over and is not governed by D-8's no-startup-DDL rule at all.
`the_sun_times_guard_bites` is its negative control, and it runs the real reader over a synthetic
tree rather than a copy of it — dropping the reader's `to_uppercase`, its recursion or its extension
match turns the control red, which was checked by making each of those three edits. The capability
needs no guard: `Capability::ALL.len() == 12` is already asserted in `capability.rs`.
Reinstating W2a §4.3's fallback starts by deleting the guard, which is the point of it.

**On the gate, stated plainly, because the reading is not the literal one.** M5-3's ten assertions
all pass. But OQ-B's instruction — compare to the minute against a fresh Meeus evaluation, and
reinstate §4.3 if they diverge — is **not** satisfiable as written: the served value never once
agrees with a textbook evaluation to the minute, by 2–7 min at sunrise and 1–8 min early at sunset
(M5-3; `every_configured_station_diverges_from_a_fresh_meeus_evaluation`). M5-3's own "Done when …
passes to the minute" was not met in that literal sense.

Reinstating the table cannot close that gap, because the table is filled by the same
`solar.CalculateSunriseSunset` the per-request evaluation ports — the divergence is the Go formula's
missing horizon-altitude term, and the table computes it identically. What OQ-B was protecting
against is instead established by `the_served_strings_are_the_stored_go_tables`: the per-request
evaluation reproduces the table's own output, 6 of 6 byte for byte. W2a §4.3's own words support
reading it that way — it calls `pkg/solar` "Meeus, already implemented" — but the reinterpretation is
recorded here rather than assumed, and the three cases where deletion *does* change a served value
are in **DEV-17** with their measurements and their directions. Whether to correct the formula is a
separate decision, deliberately not taken here.

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

### M6-1 — the remaining six drivers *(includes N-20)*
**Depends on:** M3-1. Davis (serial and TCP), Campbell Scientific, Ambient, AirGradient, the snow
gauge, and the gRPC receiver. **Davis and Campbell get a `socat` pty pair** (W3 D-83) because serial
framing and read-timeout behaviour is where that class of defect lives.
**Ambient is the structural one:** it becomes **N stations on one endpoint** — one
`HttpIn { /ingest/ambient }` component, identity from `?ID=` against `StationRegistry`, secret from
`?PASSWORD=` compared constant-time (W2a D-43). A second Ambient station is a config row, not a second
HTTP server.
**Done when:** each driver passes all three test levels against its pinned Go emulator.
**Constraint inherited from M3-3 (DEV-9): the snow gauge must discard a distance greater than the
station's configured base distance**, which is what `snowgauge/station.go:171-174` does at the device.
M3-3 moved `snowdepth` to ingest, so such a reading now stores a **negative** depth into a column
with `min`/`mean`/`max` aggregates over it, where in the Go version it never reached storage at all.
The enrich stage is the wrong layer to filter at — it is handed an already-admitted reading, and
dropping a metric there would make it a filter rather than a transform — so the discard belongs to
the driver that knows the reading is a fault.
**N-20 lands with Campbell:** the owner's station is on USB serial and the daemon is moving into
Kubernetes, so Campbell must work over `TcpOut` against a `ser2net` bridge as well as over `Serial`.
W2a §4a.5 already maps it as `Serial | TcpOut` and the Go version already dials TCP when
`SerialDevice` is empty, so this is a test case rather than a new capability — but it is the one
driver where the TCP path is load-bearing in production rather than a convenience.
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
**Done when:** `/forecast` is conformance-green; `Freshness` renders "forecast last
updated 3 h ago / never" as distinct from component state (W2a D-20).
**`/alerts` was removed from this criterion on 2026-11-12** — see `docs/reconciliation.md`'s
**A-M6-3-1**. This line was written 2026-10-30; `docs/reconciliation.md` §6.3 retired `/alerts` the
next day, so the criterion required an endpoint a later decision had already deleted. Build the
Aeris alert components and the `gc` sweep; do **not** implement the `/alerts` endpoint, and leave its
four fixtures in the conformance runner's `retired` roster.

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
| **M0-1** | **O-12 — a dedicated Talos worker node for the database.** NVMe-backed, not NFS: W4 D-26's refresh policy has 3.06–3.22 s of measured lag against a 10-second deadline and ~47% of a durable refresh is WAL fsync, so disk *latency* is the spec and capacity is not (a decade is ~36 GB). `big-bulky-1` was rejected 2026-10-31 as too busy with map builds | **Yes for M0-1, which blocks M5-1 and — through M0-2 — cutover.** Nothing in M1 or M2 touches a real database, so this is not on the critical path yet |
| ~~—~~ | ~~**O-2** archive-then-pause the raw retention policy~~ | **Done 2026-10-31.** Archive taken and verified; job 1004 paused. The loss it was racing had **already completed** — see §8 |
| ~~M5-1~~ | ~~**O-1** the production version~~ | **Answered 2026-10-31: PostgreSQL 16.6 / TimescaleDB 2.17.2.** Above 2.9, so the hierarchy is constructible and M5-1 is **not** a major-version upgrade — the new instance is pg17/2.31 and W9's logical export/import bridges them. The source half of that procedure has since been re-verified on the 16.6/2.17.2 pair |
| ~~M5-1~~ | ~~**O-7** the history option~~ | **Moot.** §8 established there is nothing to carry: the ~294 rollup-only days were already destroyed, and §8.1b shows `/span` refuses any span over a year, so no endpoint could have served them. The archive was still taken and is redundant rather than irreplaceable |
| ~~M4-2~~ | ~~**O-5** the `/stationinfo` hazard~~ | **Answered: option (b).** Ships as a signed deviation with a new fixture |
| ~~M6-2, M7-4~~ | ~~**O-8** the RF beacon floor~~ | **Answered: 600 s.** M7-4's migration raises a sub-600 s KISS device with a one-line report |
| ~~M7-2~~ | ~~**O-10** the four admin-surface calls~~ | **Answered: all four accepted** |
| ~~M7-3~~ | ~~**O-11** which `chonky` gaps to upstream~~ | **Answered: all twelve are fair game.** Upstream G2/G4/G9/G10 now; the rest as their shape settles in use |
| ~~before first release~~ | ~~**O-9** is a Pi Zero in use?~~ | **Answered: retain the broadest hardware support.** All four targets stay; with no ARM1176 to test on, W8 D-25's qemu run under `-cpu arm1176` **is** the validation — no new hardware needed |
| ~~N-1~~ | ~~**O-6** which legacy surface is dead~~ | **Answered structurally** — only the iOS endpoints are frozen, so `legacy-v1.json` settles it and the access-log query is unnecessary |
| ~~—~~ | ~~**O-1**–**O-4**, the four production queries~~ | **All four answered 2026-10-31** against production. See `docs/reconciliation.md` §5 Group A for the measured values and §8 for the data-loss finding. Two premises were wrong: there is no column gap, and the rollup history was already destroyed |
