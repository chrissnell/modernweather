# W4 — Database schema, migrations, and continuous aggregates

**Depends on:** W0, W2, W2a, W3 — **all merged.** `docs/DESIGN.md`, `docs/spike-findings.md`,
`docs/metric-registry.md`, `docs/component-model.md` and `docs/service-architecture.md` are on
`main`, as are `metrics/registry.toml`, `generated/` and `migrations/`. Runs in parallel with W5 and
W7.
**Produces:** `docs/database-schema.md`, the migration set, and the schema applied and verified
against a real TimescaleDB in CI.
**This item writes real code** — SQL, migrations and CI wiring. It is not design-only.

## Context

W2 generates the parts of the schema that follow mechanically from the metric registry. **You own
everything it does not generate, and you own every decision it deliberately deferred.** W2's own
assumptions section lists them; this prompt collects them, adds the ones W0, W1, W2a and W3 left you,
and names the one contradiction between documents that only you can settle.

Read, in this order: `docs/DESIGN.md`, `docs/metric-registry.md`, then
`docs/service-architecture.md` §2.5 and §8.2 (the writer and its regression test) and
`docs/legacy-api-contract.md` §2.3 (`/span`'s aggregate selection). Then read what is already in the
tree — `generated/sql/*.sql`, `migrations/20261028165610_initial_schema.sql` and
`metrics/registry.toml` — because a decision you make that contradicts the committed generator
output is a W2 follow-up, not something you patch by hand.

What is generated and already applies cleanly: the `observations` wide hypertable (627 columns, 7-day
chunks, `compress_segmentby = station_id`, `orderby = time DESC`, 7-day compression policy), three
hierarchical aggregates `observations_1m` ← raw, `observations_1h` ← `_1m`, `observations_1d` ←
`_1h`, the `mw_mean` and `mw_circular_mean` finalisers, and a rebuild *shape* in
`090_rebuild_aggregates.sql`.

---

## 0. The contradiction you have to resolve first

**Three documents disagree about how many aggregate levels exist, and the disagreement is visible to
every shipped client.**

- `DESIGN.md` **D-11** says the hierarchy is exactly 1-minute from raw, 1-hour from 1-minute, 1-day
  from 1-hour, and its assumption 16 says the Go version's 5-minute level is *not* assumed to carry
  forward — adding that "if it does, it should derive from the 1-minute level."
- W2 generated **three** levels accordingly.
- W1 §2.3 measured what `/span` actually does, and it reads **four** tables:

  | Span | Table read today |
  |---|---|
  | ≤ 6 h | `weather_1m` |
  | > 6 h and ≤ 48 h | **`weather_5m`** |
  | > 48 h and ≤ 14 d | `weather_1h` |
  | > 14 d | `weather_1d` |

  W1's assumption 10 states it plainly: preserve those four bands and their boundaries, or the `ts`
  spacing in a `/span` response changes and **every client's chart resolution changes with it.**

Note which band is load-bearing. Both shipped clients only ever request `/span/{integer}h`, and
`/span/24h` — which falls in the 5-minute band — is the span the website's default view uses. Serving
it from `observations_1h` would quarter the number of points in the dominant chart; serving it from
`observations_1m` would multiply them by five and make the response five times larger.

**Decide this, with the reasoning written down.** D-11's assumption 16 already licenses the obvious
answer — a `observations_5m` level derived from `observations_1m`, which is contiguous and therefore
constructible — but it is a level D-11's text does not list, so taking it means appending an
amendment per the protocol rather than quietly adding a view. If you instead decide the bands change,
that is a deviation from the frozen contract (**D-37**) and W6 needs it in the ledger as one. Either
way this is not a detail: it is the one place where the design as merged does not reproduce what
ships.

---

## 1. The decisions W0 left you

All three are recorded in `DESIGN.md` as W4's to make. Record each as a decision with its reasoning,
and where a measurement settles it, measure.

**OQ-3 — sum-and-count, or Toolkit `stats_agg`?** Both were verified bit-identical on the mean
against a direct-from-raw control (D-13). `stats_agg` is one column instead of two and yields
variance and standard deviation free; sum-and-count is plain SQL that anyone reading the schema can
reproduce. W0 recommended **sum-and-count for the metrics the frozen API serves**, `stats_agg` only
where a dispersion statistic is actually wanted, because D-37 means the public API's numbers must be
reproducible and a `stats_agg` state is opaque binary. W2 says both are already expressible and the
choice is **one line in `[defaults]`** — so decide, set it, regenerate, and commit.

**OQ-4 — is the 1-minute aggregate compressed, retention-bounded, or both?** At production width it
is ~42 GB for a decade, *larger than the compressed raw table it derives from* (D-18). Compression
measured 4.58× on a 398-column materialisation, which puts a decade at ~9 GB. W0 recommended
**compress it and keep it unbounded**, because bounded retention on the minute level reintroduces, at
one remove, exactly the hole D-4 exists to close. W0 also attached a condition it could not test:
**confirm that compressed-aggregate query latency is acceptable for the `/span` bands before
committing.** Measure that. It is the one number standing between this recommendation and a decision.

**OQ-5 — which metrics does each level carry?** Read **OQ-W2-1 before deciding**: W2 found OQ-5 is
*not constructible as stated*. A level reads the level below it, so membership can only shrink away
from raw; OQ-5 wanted a narrow minute level under wide hourly and daily levels, which has no source
column to roll up. W2 offers three replacements — (a) contiguous, minute level carries everything
(what ships today, 627 expressions per level, cost managed by OQ-4's compression); (b) two sources at
the hour level, which recovers the saving and reintroduces a partial raw scan plus a new registry
concept; (c) shrink upward, which is contiguous but saves nothing where the saving was wanted. W2
recommends **(a)**. If you pick (b), say explicitly that it is a W2 follow-up rather than something
you work around here.

## 2. The tables W2 does not generate

The generator owns metric columns. Everything below is yours, and none of it is in the tree yet.

- **`stations`** — a dimension table with a surrogate `station_id int`. The Go schema repeats
  `stationname text` and `stationtype text` on every row: ~20 bytes instead of 4 on every row of a
  hundred-million-row table, and it weakens `compress_segmentby`.
- **`station_metrics`** — which metrics each station actually emits, so the API can describe a
  station's capabilities without probing data. The Go tree has a TODO for exactly this at
  `internal/weatherstations/grpcreceiver/station.go:64`.
- **`almanac_cache` and `snow_totals_cache`** — these must **continue to exist as precomputed
  tables** (W1 assumption 12). `/almanac` and `/snow` read them directly, and `/almanac` returns `{}`
  rather than computing anything when the cache is empty. W3 §2a.7's `snow:*` components write the
  latter. Define their schemas; the components that fill them are W3's.
- **An `extra jsonb` escape hatch** for unmodelled vendor fields: sparse, never aggregated. Say what
  gets in and what the promotion path to a real registry metric is.
- **Every index beyond the hypertable's own time index** (W2 assumption 9). The hot path does not
  touch the database at all (W3 §2a), so index for the `/span` bands and the cache tables, not for
  `/latest`.
- **`period_rain`** exists as an aggregate-level column with no raw counterpart (W1 assumption 11).
  Confirm it survives as such.

**OQ-W2-6 is yours too:** decide whether `stationname` and `stationtype` leave the raw table entirely
once `station_id` exists, joining the station registry at read time. They are already excluded from
the aggregates, so the change is contained. W2 left them in only because removing a column from the
bootstrapped registry would mean deprecating something that never existed.

## 3. Correctness constraints handed to you by name

Each of these is an assumption another work item has already written down about you. Violating one is
findable with a grep; getting one wrong is silent.

**Unreported metrics are NULL, and `windd` still serialises as `0`.** W3 assumption 18 takes
OQ-W1-2 option (c) on your behalf: unreported metrics are NULL, and the circular-average finaliser is
**NULL-safe while still emitting `0` rather than `null`** for wind direction. W3 §2.5's writer is what
makes W1-BUG-1 live and §8.2 carries the regression test. This reproduces a shipped bug deliberately
— a non-optional Swift `Double` fails the *entire* decode when its key is absent — so do not fix it
here. `generated/sql/010_functions.sql` already has `mw_circular_mean`; verify its NULL behaviour
against that requirement rather than assuming it.

**The raw table must be idempotent over a replayed prefix.** W3 assumption 22: `ON CONFLICT
(station_id, time) DO UPDATE` semantics, or an equivalent unique constraint, because §2.3's
cancel-safe delivery replays a prefix after a cancellation. Decide which, and note that a unique
constraint on a compressed hypertable is not free — check what TimescaleDB 2.31 actually permits here
and report it.

**The 1-minute aggregate must be readable fast enough for `window:rollup`.** W2a assumption 14 and
W3 assumption 21: a bucket must be readable within `window:rollup`'s **10-second deadline of the
minute boundary**. Specify the refresh policy that achieves that and **measure the lag**. If it
cannot be met, say so — `window:rollup`'s period has to grow and W2a's `Phase::Aligned` needs
revisiting, which is a finding, not a workaround.

**Collector-written rows legitimately contain NULL.** W3's open-question entry 2 decided that W2's
generator emits proto3 `optional` on every metric field so that a satellite and its collector hold
the same data instead of the collector storing `0.0` for a metric never reported. Expect NULLs from
that path.

## 4. The rebuild procedure

You own it (W2 assumption 8); `090_rebuild_aggregates.sql` is the shape, not the procedure.

**Rebuilds run with relaxed commit durability (D-17).** A full refresh of a 1-minute aggregate over
36 M raw rows took 306 s against 2.4 s for the identical aggregation as a plain query. The overhead is
WAL fsync, not CPU, it is not fixed by batching into smaller ranges (299 s), and relaxing commit
durability for the refresh session brought it to **25.6 s**. This is safe for exactly the reason
aggregates are disposable: a crash mid-rebuild costs a re-run, not information. **Ingest keeps full
durability** — this is a property of the rebuild path only.

**OQ-W2-2 is yours in part.** Adding a column is 11.6 ms, but adding a metric that any level carries
forces a drop-and-recreate of that level and every level above it, which at ten years of history is
an hour-scale operation. W2's recommendation, and your deliverable: **support refreshing a bounded
time range first and backfilling older ranges as a background operation**, so a new metric is
queryable at recent resolution in seconds rather than hours. (The other half — correcting the
"adding a metric is cheap" claim in `DESIGN.md` — is an amendment you propose, not one you apply.)

Also document the plain-`pg_dump`-plus-refresh backup procedure's restore side (D-6): a restore of
raw plus a rebuild is a complete recovery, and at ten years that rebuild is the hour-scale operation
above rather than an instant one.

## 5. CI

**The `db-migrate-twice` gate is yours.** `ci/gates.toml` names W4 as its owner, and it armed itself
when W2 committed `migrations/`. It currently passes trivially — there is only one migration, so
"empty → HEAD" and "previous → seeded → HEAD" are the same path, and the script says so. Make it
meaningful: seed rows, exercise a second migration, and compare schema-and-aggregate equivalence
rather than dump equality.

One thing to know before you debug it: the runner's `/usr/bin/pg_dump` is a postgresql-common wrapper
that resolves to a *stopped local PostgreSQL 16 cluster*, so `pr.yml` puts
`/usr/lib/postgresql/17/bin` on `PATH` to get binaries that match the PG 17 service. If you touch
that job, keep it.

**Assert the minimum server version (D-42).** `ci/sql/assert-timescaledb-version.sql` already does,
and the CI service is `timescale/timescaledb-ha:pg17` with timescaledb 2.31 and the toolkit present.
Note that **production's version is still unknown** — W0's OQ-1 could not reach it, and three spike
answers are version-gated. That is W9's and the owner's problem, not yours, but your document should
say which of your decisions change if production turns out to be PostgreSQL 14.

## 6. Measurements this document must contain

Opinions are not findings. Report the command alongside each number, in the style of
`docs/spike-findings.md`.

1. Compressed-aggregate query latency for each `/span` band (settles OQ-4).
2. Refresh lag at the minute boundary against the 10-second deadline.
3. Full-rebuild time per level with and without relaxed durability, at a stated row count.
4. Whether a unique constraint or `ON CONFLICT` target is permitted on the compressed hypertable, and
   at what cost.
5. The size on disk of each level, compressed and not, at a stated row count — so the ~42 GB and
   ~9 GB figures in D-18 are either confirmed or corrected.

Use a throwaway TimescaleDB container matching CI's image. **Do not touch the production database**;
it is unreachable from a runtime in any case.

---

## Do not

- **Do not change the registry format or the generator.** That is W2. If a decision here needs a
  generator change — OQ-3's `[defaults]` line, a fourth level, a dropped `attribute` column — make it
  through `cargo xtask`, regenerate, commit the output, and keep `cargo xtask codegen --check` green.
  Hand-edited generated SQL fails CI and deserves to.
- **Do not design the migration of existing production data.** That is W9: 366 days of raw at 19 GB,
  plus the decision about ~294 days of rollup with no raw behind it. Reconciliation by row count and
  spot values before the old database is decommissioned is W9's too.
- **Do not redesign the component model (W2a) or the service architecture (W3).** If one is wrong,
  raise it against that document.
- **Do not rename the generator's table constants casually.** `observations`, `observations_1m/_1h/_1d`
  are generator constants; changing them is a one-line change plus a regeneration (W2 assumption 10),
  so do it deliberately or not at all.
- **Do not amend a `DESIGN.md` decision in place.** Propose amendments in your closing section and
  carry on under the existing decision — particularly for §0, where you will likely be appending to
  D-11.

## Done when

- `docs/database-schema.md` ends with the four required sections from the output contract in
  `docs/rewrite/README.md`: **Decisions** (numbered one-liners), **Open questions** (with
  recommendations), **Assumptions about other work items**, and **Proposed amendments to
  `DESIGN.md`**. W6 consumes these; a document without them cannot be reconciled.
- §0's four-versus-three-level contradiction is resolved, with the reasoning and the consequence for
  `/span/24h` stated, and either an amendment to D-11 or a recorded deviation from D-37.
- OQ-3, OQ-4 and OQ-5 are each decided and recorded, with OQ-4's decision backed by the latency
  measurement it was made conditional on.
- The migrations apply from empty against the CI image, and `cargo xtask gate db-migrate-twice`
  passes **and is no longer trivial** — show the before and after.
- `cargo xtask codegen --check` is clean, proving every registry-affecting decision went through the
  generator.
- Every measurement in §6 has a number and the command that produced it.
