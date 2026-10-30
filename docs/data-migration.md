# W9 — Migrating the existing weather data

> **The owner's constraint is "I don't want to lose the data from my existing database." This
> document found a way to lose it that nobody had written down, and it is not the one §4 of the
> prompt warns about.**
>
> **Four things here disagree with the prompt that commissioned them. All four are measured, all
> four are written up as decisions below, and none of them changes a decision owned by another
> work item.**
>
> 1. **The §4 hazard is not limited to a manual refresh. The ordinary refresh *policy* does it
>    too.** `drop_chunks` on the raw hypertable logs an invalidation over the range it dropped, and
>    the next refresh of any level over that range re-materialises it from raw that is no longer
>    there. Measured on TimescaleDB 2.31.0: **one scheduled policy run deleted 41 of 83 daily
>    buckets**, and the range it destroyed was exactly the range whose raw chunks had gone. §1.2,
>    Decision 1.
> 2. **The prompt's "import the remainder as pre-seeded rollup rows" cannot be done inside
>    `observations_1d`.** An `INSERT` into a continuous aggregate is *accepted* on 2.31 — which is
>    worse than being refused — and the rows are then destroyed by any of three ordinary
>    operations, one of which is adding a metric. The archive goes into a plain table. §3,
>    Decision 4.
> 3. **The frozen API can never serve the archived window, so there is no boundary union in the
>    read path.** `/span` takes no time anchor and rejects a span over one year
>    (`docs/legacy-api-contract.md` §2.3), so the furthest back it can see is 365 days — and under
>    D-4 the trailing year always has raw behind it. Measured: wiring `/span`'s daily band to the
>    union view costs 37% more latency and returns **the same 365 rows**. §3.4, Decision 6.
> 4. **§3 step 2's ordering is right and its stated reason is wrong.** "Loading into a hypertable
>    that already has aggregates defined is slower and accumulates invalidation records" does not
>    reproduce: the insert cost of four defined levels is inside the control group's own spread
>    (1,912 ms against 1,815 ms, median of 3), and a whole day of rows written below an advanced
>    invalidation threshold logs **one** record, in a 24 kB table. The step survives for a
>    different and stronger reason. §5.2, Decision 9.
>
> One further item is not a disagreement but is the largest single finding: **the column mapping is
> the identity.** All 215 columns the Go source declares exist in `metrics/registry.toml` under the
> same name and the same type. The transform is not a 215-way mapping to be audited; it is a column
> list to be intersected with what production actually has. §4.1.

**Depends on:** W4 (merged), W0 (merged).
**Produces:** this document; `cargo xtask legacy-import` and the four generated SQL files in
`ci/db/migrate-from-go/generated/`; `ci/db/migrate-from-go/{migrate-from-go.sh,
00-preflight-source.sql, 01-preflight-null-columns.sql, 40-rollup-check.sql,
go-aggregate-views.toml, extract-go-views.sh}`; `ci/db/migrate-from-go/lab/`, which is the harness
every number below came from; the `legacy-import-drift` gate.

Everything measured here was measured against `timescale/timescaledb-ha:pg17` — PostgreSQL 17.11,
TimescaleDB 2.31.0, `timescaledb_toolkit` 1.26.0, the same image the `database` CI job uses and the
same image W4 measured on. §8 says which numbers are load-dependent and which are not, and which
numbers are **not** quoted from the lab on purpose.

---

## 0. What I could do, what I could not, and what that changes

**Production is unreachable, and it was unreachable for W0 too.** OQ-1 records it: production is not
in the homelab Kubernetes cluster and the spike could not reach it. So every instruction in the
prompt that says *measure production*, *confirm whether retention was paused* or *run this query* is
a **deliverable for the owner**, not something this run executed. They are collected in
`ci/db/migrate-from-go/00-preflight-source.sql` as nine labelled reports, P-1 to P-9, each with the
question it settles printed above it and each a `SELECT`. One paste, one output, and the document can
absorb the answer.

**What I did instead, and it is the reason this document says anything new.** A throwaway
TimescaleDB pod, the **old** schema built in it from the Go source's own string constants rather than
retyped, seeded into the *shape* of production — a raw window shortened by an active 365-day
retention policy sitting under a daily rollup that reaches 295 days further back — and then the whole
migration run against it end to end. The lab reproduces the one property that makes this work item
exist:

| | the owner's production (2026-10-27 snapshot) | the lab |
|---|---|---|
| raw `weather` | 2025-10-22 → now, **366 days** | 2025-10-23 → 2026-10-29, **371 days**, 2,388,960 rows |
| `weather_1d` | 2025-01-01 → now, **660 days** | 2025-01-01 → 2026-10-29, **666 days**, 1,983 rows |
| rollup with no raw behind it | **~294 days** | **295 days** (287 for the station with gaps) |
| chunks on raw | 53 (7-day) | 54 (7-day) |

The 295 against 294 is not a coincidence and it is not tuning: both fall out of a 365-day retention
policy running under a daily aggregate that started on 2025-01-01. The lab is 1/26th of production's
row count and that is the part that does not transfer; the *shape* does, and the shape is what every
decision below turns on.

`ci/db/migrate-from-go/lab/` is committed. `run-in-pod.sh up`, `10-seed.sh`, `run-in-pod.sh migrate`.

**The target schema is W4's, and it is not the one §2 of the prompt imagines.** Three corrections
inherited rather than discovered:

- **`stations` is keyed by `station_id text`, not a surrogate `int`.** W4 §2.1 measured the surrogate
  at 0.09 B/row compressed — essentially zero, because `station_id` is the `compress_segmentby` key —
  and declined it. So the transform resolves `stationname` to a *text* `station_id`, and
  `stationname`/`stationtype` **stay** on the row as `role = "attribute"` dimensions. They leave
  later, under OQ-W4-2, and that is a W2 follow-up.
- **Four levels, not three.** W4 added `observations_5m`. The rollup archive therefore has two
  destinations that matter and two that cannot reach it; §3.4.
- **Retention on the lower levels.** 7 days on `observations_1m`, 14 on `_5m`, 90 on `_1h`. This is
  what makes W4 assumption 35 a real constraint and §3 a real decision.

**W4 assumption 34 is already discharged, and the only thing W9 has to do about it is not undo it.**
`migrations/20261029234500_w4_station_dimension_caches_indexes_and_policies.sql` creates
`observations_station_id_time_key` while `observations` is empty, which is exactly what the
assumption asks for. The import then runs *into* a table that already has the unique index — and
that turned out to matter for a reason assumption 34 does not mention. §5.4.

---

## 1. Retention: the question, and the thing that is actually dangerous

### 1.1 Is raw retention still firing? Unknown, and here is what the answer costs

Pausing the production retention policy has been a pre-flight action in `docs/rewrite/README.md`
since 2026-10-27 and is the owner's to perform. Nothing in this runtime can observe production, so
**this document does not assume either way.** The query that settles it:

```sql
SELECT job_id, application_name, schedule_interval, next_start,
       config ->> 'drop_after' AS drop_after
  FROM timescaledb_information.jobs
 WHERE proc_name = 'policy_retention';
```

It is P-2 in the preflight, and P-2b widens it to every policy job with its last run, because a
*disabled* job is a different answer from an absent one.

**The cost of the answer being "no".** Retention on raw is 365 days and fires daily, dropping one
7-day chunk roughly every seven days. Each day it continues converts one more day of raw readings
into rollup-only history. Measured in the lab at the shape above, one day of raw for three stations
at a 40-second cadence is 6,480 rows; at production's four stations and 2-second cadence it is
**172,800 readings per day**, and the daily and hourly buckets over them survive while the readings
themselves do not. The ~294-day window grows by one day per day. At the time of writing, if the
policy was never paused, it is ~297 days.

The 1-minute and 5-minute levels lose their tails faster still — 1 month and 6 months — and those
are *not* recoverable from the archive either, because §3.4 shows they have no destination.

### 1.2 The dangerous thing, which is not retention

**Retention dropping a raw chunk does not invalidate the aggregate buckets above it.** That is why
`weather_1d` holds 660 days on 366 days of raw, and it is the only reason the 294 days still exist.
The prompt knows this and warns against a manual `refresh_continuous_aggregate` over a window where
raw is gone.

The warning is correct and it is too narrow. Measured, in order, on TimescaleDB 2.31.0:

```
-- 10 days of raw, all four levels materialised over it: 29 daily buckets (3 stations, one gap day)
SELECT drop_chunks('weather', older_than => '2026-01-08');   -- what retention does
  -> raw: 8352 rows -> 2304 rows, earliest 2026-01-08
  -> weather_1d:  29 rows, UNCHANGED, bit-identical on outtemp/winddir/period_rain
```

So far so good: this is the production situation, reproduced. Then:

```
-- 1. the hazard as the prompt states it
CALL refresh_continuous_aggregate('weather_1d', '2026-01-01', '2026-01-08');
  -> weather_1d: 29 rows -> 8 rows.  21 buckets DELETED. No error. No warning.
```

and then the part that is not in the prompt:

```
-- 2. a fresh scenario: 28 days of raw, materialised, 83 daily buckets.
SELECT drop_chunks('weather', older_than => '2026-01-15');
  -> weather_1d: 83 rows, unchanged
SELECT add_continuous_aggregate_policy('weather_1d', INTERVAL '10 years',
                                       INTERVAL '1 day', INTERVAL '1 day');
CALL run_job(1000);                       -- the ORDINARY SCHEDULED POLICY, nothing manual
  -> weather_1d: 83 rows -> 42 rows.  41 buckets DELETED.
  -> destroyed range: 2026-01-01 .. 2026-01-14
  -> raw begins:      2026-01-15
```

The destroyed range is *exactly* the range whose raw had been dropped, to the day. The mechanism is
visible in the catalogue and it is the missing half of the prompt's model:

```sql
-- immediately after drop_chunks, before any refresh
SELECT count(*) FROM _timescaledb_catalog.continuous_aggs_hypertable_invalidation_log;
  -> 2
```

**`drop_chunks` logs an invalidation over the range it dropped.** A refresh — by policy or by hand —
processes that invalidation, recomputes the range from a raw table that has nothing in it, and writes
the result, which is no rows. The old `weather_1d` policy's `start_offset` is `INTERVAL '10 years'`,
so its window covers the whole archive.

**This has a consequence for production that the owner should know before doing anything else.** On
2.31 the 294 days could not have survived a single policy run, and they have survived many. So one of
two things is true on production, and the preflight tells them apart:

- **the `weather_1d` refresh policy is not running** — absent, disabled, or failing. P-2b's
  `scheduled` and `last_run_status` columns say which. The history is intact and will stay intact as
  long as nothing starts it.
- **production runs a TimescaleDB old enough that a refresh over a source-less range leaves existing
  buckets alone.** The prompt says the behaviour has varied across versions; this measurement pins
  2.31.0 to the destructive side. P-1 settles the version.

Either way the action is the same and it is the cheap one: **take the archive copy first (§2), and do
not run `refresh_continuous_aggregate` over history on the old database, and do not re-enable or
re-create a refresh policy there.** P-3 reports the pending invalidations directly, and P-3b reduces
it to the question that decides something — *does a pending invalidation reach below where raw
begins?* In the lab, after the seed, it does, for all four levels.

**The same mechanism is the whole of §3's decision**, because it applies to the new database too:
anything pre-seeded into `observations_1d` sits in a continuous aggregate with a refresh policy over
it.

---

## 2. The archive copy

Take it before anything else, including before deciding §3. It is 3 MB and it makes the decision
reversible.

```sql
CREATE TABLE weather_1d_archive AS SELECT * FROM weather_1d;
CREATE TABLE weather_1h_archive AS SELECT * FROM weather_1h;
```

`migrate-from-go.sh archive` runs exactly this, verifies each copy against the view it came from, and
is **the only step in the whole procedure that writes to the source** — which is why it is a
separate, explicitly named step rather than part of `all`.

**Through the view, not through `pg_dump -t`.** The prompt asserts that `pg_dump -t weather_1d`
captures only a view definition. Verified:

```
$ pg_dump -t weather_1d         | grep -cE '^COPY |^INSERT INTO'   ->  0
$ pg_dump -t weather_1d         | grep -oE 'CREATE (MATERIALIZED )?VIEW'  ->  1 CREATE VIEW
$ pg_dump -t weather_1d_archive | grep -cE '^COPY |^INSERT INTO'   ->  1
$ pg_dump -t weather_1d_archive | wc -l                            ->  2431
```

The rows live in `_timescaledb_internal._materialized_hypertable_17`, which `-t weather_1d` does not
name. Copying through the view is also *correct* rather than merely necessary: the old views are
declared `materialized_only = false`, so the view unions the materialised buckets with real-time
aggregation over whatever raw is still present.

**Measured, and this answers the prompt's "`weather_1h` — not measured, measure it":**

| | rows | span | copy size | materialisation size |
|---|---|---|---|---|
| `weather_1d_archive` | 1,983 | 666 days | **3,216 kB** | 4,096 kB |
| `weather_1h_archive` | 47,592 | 666 days | **75 MB** | 82 MB |

The load-independent figure is **1.62 kB per hourly bucket row** at the old view's 399 columns
(and 1.66 kB per daily row). Scaled to production — four stations, hourly, over the 2-year retention
the old `weather_1h` carries — that is 4 × 24 × 730 = 70,080 rows and **~114 MB**. The 3,216 kB for
the daily copy matches the 3.5 MB the prompt measured on production almost exactly, which is a small
piece of evidence that the lab's aggregate shape is faithful.

P-5 in the preflight reports this on production directly, using `hypertable_size` on the
materialisation hypertable rather than `pg_total_relation_size` on the view — the latter reports the
32 kB of catalogue and is the reason an aggregate's size is easy to under-report by three orders of
magnitude.

**One real-time-aggregate trap found while building the lab, worth a paragraph because it is the same
silent-failure class as §5 step 5.** A `materialized_only = false` aggregate whose materialisation is
*empty* looks completely correct: it computes everything in real time from raw and returns the right
answer to every query. It stops looking correct the moment retention removes the raw — at which point
the view silently shortens to the raw window. In the lab this happened to `weather_5m` after an
interrupted `CREATE MATERIALIZED VIEW` rolled back its data, and nothing reported it:

```sql
SELECT user_view_name,
       _timescaledb_functions.to_timestamp(_timescaledb_functions.cagg_watermark(mat_hypertable_id))
  FROM _timescaledb_catalog.continuous_agg;
  weather_1d  | 2026-10-30
  weather_1h  | 2026-10-30
  weather_1m  | 2026-10-30
  weather_5m  | 4714-11-24 BC      <- -infinity: nothing is materialised at all
```

A `-infinity` watermark on a `materialized_only = false` level means the archive copy of that level
would capture a real-time computation rather than stored history. Check the watermarks before
believing an archive copy. P-5 reports them.

---

## 3. The history decision

**The decision is the owner's. Here are the two options, the cost of each, and a recommendation.**

### 3.1 The thing that makes this decision different from how the prompt frames it

The prompt offers "raw only" against "raw plus the rollup archive, **with a union at the boundary for
reads**". The second half of the second option is not available and should not be wanted:

**No endpoint in the frozen contract can ask for the archived window.** `/span/{span}` takes no time
anchor — its only parameters are `station`, an ignored `station_id` and an unhonoured `format`
(`docs/legacy-api-contract.md` §2.1) — so every span is `[now - span, now]`, and a span over one year
is a `400` with `time span exceeds maximum allowed duration of 1 year`. The furthest back `/span` can
look is 365 days. Under D-4 raw is retained indefinitely, so the trailing 365 days will always have
raw behind it and always be served by `observations_1d` proper.

Measured, on the migrated lab database:

| read | buckets | from | to |
|---|---|---|---|
| `observations_1d`, last 366 days | **365** | 2025-10-30 | 2026-10-29 |
| `observations_1d_with_legacy`, last 366 days | **365** | 2025-10-30 | 2026-10-29 |
| `observations_1d_with_legacy`, no time predicate | 667 | 2025-01-01 | 2026-10-29 |

The union returns the same 365 rows for the only band `/span` can ask for, and it costs more to do
it: median of five, 366-day band, every metric finalised — **13.3 ms** against the level alone,
**18.3 ms** against the union view. 37% for zero additional rows.

Note that this was already true of the Go server: it has never been able to serve its own 294-day
archive through `/span` either. The archive has never been reachable by any shipped client.

### 3.2 Option A — raw only

The new database starts with the 366-day raw window and regenerated aggregates over the same window.

**Cost:** the ~294 days of daily and hourly rollup are gone, permanently, and they are the only
record of that period at any resolution. Nothing can recompute them. Nothing ever will.

**What is *not* a cost:** no endpoint loses anything, because of §3.1. Nothing in the frozen contract
changes, no read path changes, and no table is added.

### 3.3 Option B — raw plus the rollup archive, carried but not served

Import the 294-day remainder as pre-computed rows in **`observations_1d_legacy`** and
**`observations_1h_legacy`**: plain tables, same column shape as the levels, a `source` column naming
where the rows came from, and a primary key on `(station_id, bucket)`.

**Cost, measured in the lab and scaled to production:**

| | lab (295 days, 3 stations) | production (~294 days, 4 stations) |
|---|---|---|
| `observations_1d_legacy` | 877 rows, **1,864 kB** | ~1,176 rows, **~2.5 MB** |
| `observations_1h_legacy` | 21,048 rows, **43 MB** | ~28,224 rows, **~58 MB** |
| columns filled of 627 | **451 (72%)** | the same; it is a schema property |
| columns NULL | 176 (28%) | the same |
| read-path change | **none** | none |

Plus one new table per level that must never be truncated and that `ci/db/rebuild-aggregates.sh` must
never touch, and a `w9.legacy_boundary` row per station.

### 3.4 Why it cannot go inside `observations_1d`, measured

The prompt says "imported as **pre-seeded rollup rows**". On TimescaleDB 2.31 that is expressible, and
it is a trap, because the failure is not at insert time:

```
-- 1. an INSERT into a continuous aggregate view is ACCEPTED (rewritten to the
--    materialisation hypertable), and duplicates are accepted too: a materialisation
--    hypertable carries no unique constraint on (bucket, station_id).
INSERT INTO observations_1d (bucket, station_id, outtemp_sum, outtemp_count)
     VALUES ('2025-01-01', 'davis-full', 100, 2);
  -> accepted. SELECT count(*) FROM observations_1d -> 2 (one per insert, no conflict)
  -> mw_mean(outtemp_sum, outtemp_count) = 50.  It even reads correctly.

-- 2. and then
CALL refresh_continuous_aggregate('observations_1d', '2025-01-01', '2025-01-03');
  -> SELECT count(*) FROM observations_1d -> 0.  DELETED.

-- 3. and, independently
DROP MATERIALIZED VIEW observations_1d;      -- what adding a metric does (D-15)
  -> NOTICE: drop cascades to table _timescaledb_internal._hyper_8_1_chunk
```

Three destroyers, all of them routine:

1. **any refresh over the range** — and §1.2 shows that includes the level's own scheduled policy once
   retention has dropped raw underneath, which on the new database it will not have, but which is one
   `refresh_continuous_aggregate` away at any time;
2. **the scheduled refresh policy**, measured destroying 41 of 83 buckets in §1.2;
3. **adding a metric**, which drops and recreates every level carrying it (D-15) and takes the chunk
   with it. W4 §4 records that the 5-minute-level migration already dropped and recreated all four
   levels once.

A plain table survives all three. It also *says in its own name* that its rows were not computed from
raw, which a row inside `observations_1d` does not, and the generated DDL carries a `COMMENT` saying
so and a per-column `-- NULL:` reason on every one of the 176 columns the Go views never computed.

**Which levels can receive the archive at all.** Only the daily and hourly ones. `observations_1m`
and `observations_5m` carry 7-day and 14-day retention (W4 decision 5), so an imported 2025 bucket
would be dropped by a retention job within a week — W4 assumption 35's point, confirmed. And
`weather_1m`/`weather_5m` on production have 1-month and 6-month retention, so for most of the
archived window there is nothing at those resolutions to import.

### 3.5 Recommendation

**Option B, with the archive carried but not served.** Concretely:

1. Take the §2 archive copies **now**, before deciding anything. 3 MB, reversible, and §1.2 says the
   window they protect may be one policy run from gone.
2. Import them into `observations_1d_legacy` and `observations_1h_legacy`.
3. **Leave `/span` reading `observations_1d`.** `observations_1d_with_legacy` exists and is generated,
   and nothing should be pointed at it until there is an endpoint that takes a time anchor — which
   W1 and W3 both defer to a work item that does not exist yet, and which W6 is asked to name.
4. Keep `w9.legacy_boundary` and the disjointness assertion, so that if such an endpoint arrives the
   union is already correct rather than needing to be worked out again.

The reasoning is the asymmetry. Carrying it costs **2.5 MB plus 58 MB and no read-path change**.
Not carrying it costs the only copy of ten months of the owner's weather, with no way back. The
decision the owner is actually being asked to make is whether 60 MB is worth ten months of history
they cannot currently look at but could look at later; and the honest framing of Option A is not
"clean and simple" but "delete it, having noticed that nothing asks for it today".

The one thing worth saying against Option B: 28% of each imported row is NULL and two of the imported
statistics are semantically not what the new column means (§4.3). It is *less* data than a row
produced from raw, and the `source` column is there so nobody mistakes it for one.

---

## 4. The transform

Generated by `cargo xtask legacy-import` into `ci/db/migrate-from-go/generated/`, from two inputs and
no transcription:

- **`metrics/registry.toml`** — the destination (D-20).
- **`ci/db/migrate-from-go/go-aggregate-views.toml`** — the source, extracted from the read-only Go
  tree at `remoteweather c3ec9f7` by `ci/db/migrate-from-go/extract-go-views.sh`.

The committed output is drift-checked by the `legacy-import-drift` gate, for the reason a 629-column
`INSERT` deserves: a registry change between review and cutover would otherwise leave the statement
naming columns the destination no longer has, and the failure would arrive mid-import.

### 4.1 The raw mapping is the identity, and the real risk is width

The registry was bootstrapped from the Go source, and the result is stronger than "mostly matches":

```
columns in the Go createTableSQL:                215
registry columns (dimensions + metrics):         216
in the Go table, absent from the registry:         0
in the registry, absent from the Go table:         1   (station_id)
type disagreements across all 215:                 0   (dateutc int8 == bigint)
```

So there is no column-by-column mapping to audit and **no old column without a registry entry** — the
question the prompt asks about "old columns with no registry entry" has the answer *none, in the Go
definition*. The transform is:

```sql
INSERT INTO observations (station_id, <the 215 columns, by name>)
SELECT <station_id resolved from stationname>, <the same 215 columns>
  FROM weather;
```

**The risk is not the mapping, it is the width.** The Go source declares 215 columns and production
carries about 143 (W0 OQ-6), and nothing in the repository knows which 143. A baked column list would
either fail on a column production does not have, or silently skip one it does. So the emitted SQL
computes its own list at run time:

```sql
w9.import_columns()  =  w9.expected_column  INTERSECT  w9.source_column   -- in destination order
```

`w9.expected_column` is generated from the registry, with an explicit `ord` column rather than a
`ctid` ordering, because the two halves of the streaming COPY must agree on column order exactly and
physical order is not a guarantee. `w9.source_column` is loaded from one read-only query against the
old database.

`w9.gap_report` then classifies every column, and only one of the three classes is benign:

| verdict | meaning | response |
|---|---|---|
| `missing_in_source` | the registry declares it, production does not have it | imports as NULL. One bit in the null bitmap (D-3). Expected for ~72 columns. |
| `unknown_in_source` | production has a column the registry has never heard of | **stops the migration.** Data with no destination. |
| `type_mismatch` | both sides have it and disagree on the type | stops it too. |

`unknown_in_source` has three resolutions and all three are deliberate: add a `[[metric]]` entry,
route it to `observations.extra` (W4 §2.4), or record the decision not to carry it in
`w9.dropped_source_column` **with a reason**, which is a `NOT NULL` column precisely so that
"consciously dropped" leaves a trace. The alternative is deleting a row from `w9.source_column`, and
nobody would ever know.

**Proved at production's likely width.** A second lab run against a source narrowed to 143 real
columns plus one column the registry has never heard of:

```
=== gap
 verdict           | count
 ok                |   143
 missing_in_source |    72
 unknown_in_source |     1
ERROR:  the source has columns with no registry entry: mystery_sensor float4
HINT:  add a [[metric]] entry, route it to observations.extra, or INSERT it into
       w9.dropped_source_column with a reason
```

The assertion fired and stopped the migration. After recording the drop decision, the same artefact
imported **143 columns, 19,440 rows**, and reconciled clean — 375 digest rows `ok`, 18 spot checks
`ok`. One generated import is correct at either width.

**How the 19 GB moves.** A matched pair of COPYs through a pipe, one 7-day slice at a time:

```sh
psql "$SOURCE" -c "$(w9.copy_out_sql t0 t1)" | psql "$TARGET" -c "$(w9.copy_in_sql)"
```

No intermediate dump, no staging table, nothing written to the source. `FORMAT csv` and not `binary`:
binary COPY is faster and is only safe when both ends run the same PostgreSQL major version, which is
exactly what OQ-1 has not established. CSV round-trips `float4` exactly on PostgreSQL 12 and later,
where the default float output is shortest-exact. Slices are 7 days to match the source's chunk
interval, and `w9.import_slice` records which are done, so an interrupted import resumes.

`station_id` is resolved on the source side by a `CASE` built from `w9.station_map`, so the source
needs no join and no temporary object. **The default is `station_id = stationname`**, which is what
keeps `/span?station=<device name>` — a device *name*, frozen, and the only parameter that selects
`/span` data — addressing the history it addressed before. The map is populated explicitly and
asserted complete against the station list actually present in the source, rather than defaulted
silently, because choosing something else is legitimate and means the history is reachable only under
the new id.

`stations` is populated **before** any reading (W4 §2.1 makes it the projection every `/span` join
goes through, and the low-volume tables carry foreign keys to it). `station_metrics` is populated
**after**, in one pass per metric over the imported history — the scan the Go TODO at
`internal/weatherstations/grpcreceiver/station.go:64` never wanted to pay per request. Paying it once
is what the table is for. Measured: 420 (station, metric) pairs, which is 140 populated metrics ×
3 stations, exactly.

### 4.2 The rollup mapping, and the one identity that makes it exact

The old views are **not self-describing**, and this is the single most important detail in the
transform. They name a bucket's mean after the column:

```sql
avg(outtemp) AS outtemp
```

and they name a bucket's *maximum* after the column too, for a different set of metrics:

```sql
max(dayrain) AS dayrain
```

Which is which is in the view definition and nowhere else — not in the column name, not in the
registry. Getting `dayrain` backwards would put a daily maximum into a `_sum` column and a plausible
number would come out of `mw_mean` forever. `go-aggregate-views.toml` exists for exactly this, the
extractor asserts that all four old views are shape-identical apart from the bucket width (so one
mapping serves all four), and `a_mean_is_reconstructed_as_sum_and_count_one_and_an_angle_as_sine_and_cosine`
asserts the `dayrain_max` case by name.

**The mean.** The old view stored a finished average; the new level stores a sum and a count. The
import sets

```sql
<col>_sum   = a.<old>::double precision
<col>_count = CASE WHEN a.<old> IS NULL THEN NULL ELSE 1::bigint END
```

because `mw_mean(avg, 1) = avg`, **exactly**, with no rounding and no reconstruction. The count is not
a lie about how many readings there were: nothing reads a count except the finaliser, nothing rolls
up out of the top level, and the alternative is inventing a reading count the old view never recorded.
The `NULL` branch is D-16 — an empty bucket stores a NULL count, not a zero.

**The angle.** Same identity, three columns: `atan2d(sind(d)/1, cosd(d)/1) = d`.

```sql
<col>_sin_sum   = sind(a.<old>::double precision)
<col>_cos_sum   = cosd(a.<old>::double precision)
<col>_dir_count = CASE WHEN a.<old> IS NULL THEN NULL ELSE 1::bigint END
```

No column holds a finished angle, which is H-3 and is asserted.

**`period_rain`** is the one legacy key that names an aggregate rather than a value (W1 assumption
11): `rainincremental_total = a.period_rain`.

**Verified, not asserted.** R-5, over all 877 imported daily rows in the lab:

| | rows | mean exact | angle exact | extrema exact | total exact |
|---|---|---|---|---|---|
| `observations_1d_legacy` vs `weather_1d_archive` | 877 | **877** | **877** | **877** | **877** |

Every value the old view stored comes back out of the new representation unchanged.

### 4.3 What the archive cannot fill, and what it fills wrongly

**176 of 627 aggregate columns (28%) land NULL**, because W2 gave every metric the full `min`/`max`
and the Go views did not compute them. 451 of 627 have a source. In `(metric, strategy)` terms,
353 of 498 pairs are covered. The numbers are asserted in
`the_columns_with_no_archive_source_are_counted_and_not_invented`, so a registry change that widens
the gap shows up as a failing test and a revised figure rather than as more NULLs. Every unfilled
column carries its reason in the generated DDL:

```sql
    extratemp2_sum              double precision,   -- NULL: the Go views never computed mean(extratemp2)
```

**43 of the old views' 396 aggregate columns are not consumed**, and four of them are semantic rather
than arithmetic:

| old column | what it is | why it has no destination |
|---|---|---|
| `lightning_day` | `sum(lightning_day)` | the registry models `lightning_day` as **max**. Summing an already-cumulative daily strike counter across a day is not a daily strike count. |
| `lightning_hour` | `sum(lightning_hour)` | the same. |
| `lightning_time` | `max(lightning_time)` | `aggregates = []` in the registry — a `timestamptz` the new levels do not carry at all. |
| `min_gdd` | `min(gdd)` | the registry gives `gdd` only `max`. |

The other 39 are `avg(<int column>)::int` — a truncated integer mean of an alarm flag, a battery
status or a relay state — for metrics the registry models as `min`/`max` only.

**None of these four is contract-visible.** `lightning` appears nowhere in
`docs/legacy-api-contract.md`'s 133 `/span` keys and nowhere in the fixtures, and neither does `gdd`.
The sum-versus-max disagreement between the Go view and the registry is therefore an observation and
not a blocker — but it is recorded as an assumption against W2 below, because the two documents
disagree about what the metric means and only one of them can be right if an endpoint ever serves it.

`w9.unconsumed_archive_column` carries all 43 with their reasons **in the database**, so "did anybody
look at all 399 columns?" is answerable with a query rather than from a document.

---

## 5. Order of operations

`ci/db/migrate-from-go/migrate-from-go.sh`. Each step is idempotent and each can be run alone.

```
preflight    report only. Versions, retention state, extents, the loss window, the column gap.
archive      THE ONLY STEP THAT WRITES TO THE SOURCE. §2.
stage        the w9 schema on the target; load both inventories; the gap report.
stations     populate `stations` from w9.station_map. Nothing imports before it.
raw          stream the hypertable, slice by slice, resumably. Then station_metrics.
aggregates   create the four levels and materialise them bottom-up, recent-first.
rollup       import the archived daily/hourly buckets into observations_*_legacy.
reconcile    both digests, the hierarchy comparison, the spot checks.
policies     refresh policies, compression, retention. Last.
```

### 5.1 Export and verify the copy before touching anything

§2. `archive` verifies each copy's row count against the view it came from and warns rather than
fails on a difference, because a real-time tail bucket closing between the two counts explains a
small one and nothing explains a large one.

### 5.2 Create the schema with no aggregates yet — right step, wrong reason

The prompt's reason is that "loading into a hypertable that already has aggregates defined is slower
and accumulates invalidation records". Both halves were measured and neither reproduces.

**Insert cost of four defined levels**, one day of rows per run, three runs each, same data:

| | run 1 | run 2 | run 3 | median |
|---|---|---|---|---|
| four levels + 5 indexes defined | 1,912 ms | 1,954 ms | 1,851 ms | **1,912 ms** |
| no levels, 5 indexes defined | 1,967 ms | 1,785 ms | 1,815 ms | **1,815 ms** |

5% apart, and the control group's own spread is 1,785–1,967 ms. There is no effect to measure here.

**Invalidation accumulation.** With a never-refreshed aggregate the log is empty, because
invalidations below an unadvanced threshold are not logged at all. Advance the threshold with a
refresh and then write a day of rows *below* it — the pessimistic case, and the one a bulk historical
load into a live schema actually is:

```
rows written below the threshold:            25,920
invalidation log rows gained:                     1
invalidation log total size:                  24 kB
insert time:                             1,747 ms  (inside the spread above)
```

TimescaleDB coalesces a statement's modifications into one range entry. The prompt's cost does not
exist at this width.

**The step is still right, for three reasons that do hold.**

1. **A refresh *policy* created with the level is the trap, not the level.** `add_continuous_aggregate_policy`'s
   `start_offset` bounds what the policy will ever refresh. Create the levels and their policies
   before a bulk historical load and the policy will never materialise the history, with no error
   anywhere. This is the same failure as step 5 below, one remove earlier.
2. **A watermark set during a partial load reflects load progress, not data extent.** Creating a
   level `WITH NO DATA` on an empty table and filling it in one controlled bottom-up pass is the only
   path where what is materialised is known.
3. **The Go DDL materialises on create, synchronously, with no way to interrupt it.** The old
   `create*ViewSQL` constants omit `WITH NO DATA`, so `CREATE MATERIALIZED VIEW weather_1h` is a full
   scan of the whole table inside one statement. This cost ten minutes and a rolled-back
   materialisation while building the lab, which is why
   `ci/db/migrate-from-go/lab/emit-old-schema.sh` appends `WITH NO DATA` and says why.

### 5.3 Load and transform

§4. 54 slices, **2,388,960 rows**, matching the source count exactly.

An unexpected result worth recording: **the migrated raw table is 19% smaller than the source,
uncompressed.**

```sh
psql -c "SELECT pg_size_pretty(sum(total_bytes)), pg_size_pretty(sum(table_bytes)),
                pg_size_pretty(sum(index_bytes)) FROM hypertable_detailed_size('weather')"
```

| | heap | indexes | total | index count |
|---|---|---|---|---|
| old `weather` | 1,557 MB | 676 MB | **2,233 MB** | 5 |
| new `observations` | 1,581 MB | 235 MB | **1,817 MB** | 2 |

The heap grows 1.5% — 24 MB, which is the `station_id text` column on every row and matches W4
§6.6b's 8.0 B/row on the uncompressed window almost exactly (2.39 M × 8 B = 19 MB). The indexes fall
65%, because W4 kept two where the Go schema had five. `observations_time_idx` corresponds to
`weather_time_idx`; the other four — `weather_stationname_time_idx`, `weather_time_stationname_idx`,
`weather_stationname_snowdistance_time_idx` and `weather_stationname_solarwatts_idx` — all named
`stationname`, and `observations_station_id_time_key` serves what they served (W4 §2.5: "one index,
two jobs"). Compression is on top of this and is deliberately not measured here; §8.

### 5.4 Create the aggregates

Already created, by the migrations, on an empty table — including
`observations_station_id_time_key`, which discharges W4 assumption 34 with nothing for W9 to do.

**And that index turned out to matter for a reason assumption 34 does not give.** A slice that fails
part-way leaves rows behind, and `COPY` has no `ON CONFLICT`, so the retry dies on the first
duplicate:

```
ERROR:  duplicate key value violates unique constraint
        "_hyper_1_1_chunk_observations_station_id_time_key"
DETAIL:  Key (station_id, "time")=(snowgauge, 2026-10-02 04:06:40+00) already exists.
```

Which is the index doing its job and the import failing to be resumable. `w9.reset_slice` fixes it:
before importing a slice, delete that slice's range. A slice that is planned and not marked done has
either never been imported or was interrupted, the ranges are disjoint by construction, and the
delete normally removes nothing. Verified by leaving 19,339 debris rows in a slice and re-running:

```
NOTICE:  slice 2026-10-01 00:00:00+00 had 19339 rows from an interrupted attempt; removed
  2026-10-01 .. 2026-10-03  -> 19440 rows
```

The alternative — a staging table per slice plus `INSERT ... ON CONFLICT DO NOTHING` — costs a second
write of every row to avoid a delete that normally finds nothing, during an import that owns the
table.

### 5.5 Force a full materialisation explicitly

`ci/db/rebuild-aggregates.sh`, which is W4's and which this work item calls rather than
reimplements. A refresh *policy* will not backfill outside its `start_offset`, so a bulk historical
load otherwise materialises only recent data, and **it fails silently** — the charts look right until
somebody asks for last winter.

Measured in the lab over 2,388,960 raw rows, which is also W4 §6.3b's asymmetry reproduced at a
different scale:

| phase | what | time |
|---|---|---|
| 1 | all four `/span` bands, bottom-up | **1 s total** |
| 2 | `observations_1m`, full history | 13 m 30 s |
| 2 | `observations_5m`, full history | 242 s |
| 2 | `observations_1h`, full history | 25 s |
| 2 | `observations_1d`, full history | 2 s |

Phase 1 makes every `/span` band correct in a second; phase 2 is a background concern. The lab pod
runs on default PostgreSQL settings and 2–8 cores, so the absolute phase-2 figures are slower per row
than W4's tuned 16-core measurements and should not be compared to them; the ratio is the part that
transfers.

**One ordering hazard inside phase 1, found by running it.** The bands are 6 h, 48 h, 14 d and 366 d,
so `observations_1d`'s band is 26× wider than `observations_1h`'s. Bottom-up within phase 1 therefore
has the daily level read an hourly level that is materialised for only 14 days — and cache the hole.
It is corrected by phase 2, because materialising the hourly level invalidates the daily one over the
same range, so the daily level's phase-2 refresh rewrites it. But a `--recent-only` run, which exists
and which §4 of W4 recommends for a metric addition, leaves `observations_1d` wrong beyond 14 days
until a full run happens. Recorded as OQ-W9-3 rather than fixed, because the script is W4's.

### 5.6 Add policies, then compression, then retention

Last, and after reconciliation. All of it is already in
`20261029234500_w4_station_dimension_caches_indexes_and_policies.sql`; the `policies` step reports
state rather than creating it. Compression first-and-then-retention matters for the obvious reason:
compressing a chunk that retention is about to drop is wasted work, and the 7-day compression delay
means nothing compresses during the import anyway.

### 5.7 Reconcile before decommissioning anything

§6. **The old database stays read-only until every check passes**, and the only write it ever takes
is the §2 archive.

---

## 6. Reconciliation

`ci/db/migrate-from-go/generated/30-reconcile.sql` and `ci/db/migrate-from-go/40-rollup-check.sql`.

### 6.1 Row counts and values per metric per year, old against new

One row per `(station, year, metric)` on each side, carrying the row count, the non-NULL count, the
sum, the minimum and the maximum. Both sides into `w9.recon`, compared by `w9.recon_report`, asserted
by `w9.assert_reconciled()`.

**Every sum is `sum(col::numeric)`, never `sum(col::float8)`, and that is the one design decision in
this section.** Floating addition is not associative, so a float sum over 19 GB depends on the order
the rows arrive in and on how many parallel workers the planner chose. The two sides would differ in
the last bits and the operator would be left deciding whether 1e-9 is a migration defect. `numeric`
is exact and order-independent, so the comparison is `IS DISTINCT FROM` with **no tolerance** — which
is the only kind of comparison that cannot be argued with. `float4 → numeric` is exact, so nothing is
given up.

**One sequential pass per side**, not one per metric: a single `GROUP BY` computes 127 metrics × 4
aggregates and a `LATERAL (VALUES ...)` unpivots what it already produced. The metric list is
intersected with `w9.source_column` first, so the same function serves a 143-column production table
and a 215-column one.

Measured in the lab: **1,182 `(station, year, metric)` rows, all `ok`**, both digests in about six
minutes over 2.39 M rows.

### 6.2 Spot checks on values a human remembers

Row counts and sums prove the bytes arrived. An extreme **and its timestamp** prove they arrived
meaning the same thing. Seven metrics in up to two directions, per station per year: hottest and
coldest, highest gust, highest and lowest pressure, wettest day, deepest snow — with the reading
timestamp that produced each.

Measured: **42 of 42 `ok`, values and timestamps both.** This is the check the owner should read
first, because it is the one that answers "was the hottest day of 2025 the day I remember?", and no
constraint can answer that.

### 6.3 The check the others cannot make: the new hierarchy against the old daily view

§6.1 compares raw against raw, which proves the readings moved. This compares `observations_1d` —
built bottom-up through four levels — against `weather_1d`, built in one step directly from raw. It
is therefore the check that catches the class of defect D-11 through D-14 exist for: a mean of means,
a float accumulator no wider than its column, a count that arrived as 0 instead of NULL.

**Twelve of the fourteen statistics must agree bit for bit, and they do.** Worst absolute difference
exactly `0` across all 1,106 buckets, for `out_temp` mean/min/max, `barometer` mean/min/max,
`out_humidity` mean, `in_temp` mean, `snow_depth` mean, `wind_speed` mean/max and `day_rain` max.

**Two cannot agree exactly, and the reason is in the Go schema.**

```sql
-- period_rain
sum(rainincremental) AS period_rain        -- sum(real) RETURNS real
-- winddir
CREATE TYPE circular_avg_state AS (sin_sum real, cos_sum real, accum real);
```

The old view accumulated a day of additions in a 24-bit mantissa, in both cases, and
`circular_avg_final` divides and takes `atan2d` in `real` as well. That is precisely the defect D-14
widened the new accumulator to avoid, so **the new value is the accurate one** and the check bounds
how wrong the old one is allowed to be: `n × 2^-24` relative, the standard worst-case bound for `n`
additions in that width. Every other row gets zero tolerance, and the two that get a bound carry
`tol_model = 'float4_accumulated'` in the table so the distinction is data rather than prose.

| | buckets | worst absolute | worst relative | bound | verdict |
|---|---|---|---|---|---|
| `rain_incremental` total | 1,106 | 0.58 in 185,877 | 3.13e-6 | 2,160 × 2^-24 = 1.29e-4 | ok |
| `wind_dir` circular mean | 1,106 | 6.07e-5 degrees | 1.73e-7 | 2,160 × 2^-24 | ok |

**And the control, because a check that passes for the wrong reason is not a check.** A scalar mean of
the wind direction must *disagree* with the vector mean where the directions straddle north — the
seed data straddles north for exactly this purpose. It disagrees in **1,106 of 1,106** buckets. A
scalar mean sneaking into the hierarchy would also be within 1e-9 of itself.

### 6.4 Negative controls — the reconciliation was proved able to fail

| control | result |
|---|---|
| `w9.recon` empty, then `assert_reconciled()` | **raises**: `w9.recon is empty: nothing was reconciled` — HINT: *a reconciliation that ran against no rows is not a pass* |
| 101 readings deleted from one station on the new side | **125** `row_count` mismatches found |
| one single reading changed by 0.1 | **exactly 1** `sum` mismatch out of 375 |
| a source column with no registry entry | migration **stopped** before any data moved (§4.1) |
| an interrupted import slice re-run | debris removed, slice re-imported to the full count (§5.4) |

---

## 7. Validate by running both

The prompt's §6, and it remains the strongest available check for everything with no test oracle: run
the Go and Rust systems against the same inputs for a period, with the Rust writes going to the new
database, and diff the results. It is possible only because there is a single operator.

Two things this work item can contribute to that plan rather than leaving it as an aspiration.

**What to diff, and what not to.** `contract/README.md` is explicit that conformance is *not* byte
equality — "the values change every two seconds" — and checks status codes, header sets, the exact
JSON key set at every level, every value's JSON type, Go's shortest-round-trip numeric formatting, key
order, trailing newlines and byte-exact error bodies. That is the right comparison for a dual-run and
it is already built. A value-by-value diff of `/latest` between two servers reading different
databases would fail on timing alone.

**Two keys will differ in value, permanently, and it is not a defect.** §6.3's two statistics are
contract-visible on `/span` as `period_rain` and `windd`, and at float32 they differ in **1,106 of
1,106** buckets measured. The new values are more accurate; the old ones were lossy by the width of
the Go accumulator. This is a consequence of D-14 and not a deviation introduced here, it is not a
conformance failure because the suite compares shape and formatting rather than values, and it is
recorded below so that a dual-run diff that *does* compare values knows which two keys to expect.

**A cheap third thing, available before any dual-run.** The migrated database and the old one can be
compared by §6.1 and §6.2 at any time while both exist, because the digests are read-only on both
sides. Running them weekly during the dual-run period costs minutes and catches divergence in the
*ingest* path, which the conformance suite does not look at at all.

---

## 8. The lab, and which numbers transfer

`ci/db/migrate-from-go/lab/`. Pinned to `timescale/timescaledb-ha:pg17`, the image the `database` CI
job uses, so a result here is a result there.

- the old schema is built from the Go source's own string constants by `emit-old-schema.sh`, never
  retyped — a "faithful copy" that was retyped is a measurement of the retyping;
- the new schema is built from `migrations/`, so it is the schema that ships;
- the rollup-only window is produced by the mechanism production used: materialise all four levels
  over the full range, then let retention drop the raw chunks underneath;
- the cadence is **40 seconds, not 60**, which puts 1 or 2 readings in each 1-minute bucket rather
  than exactly one. That is D-12's uneven-bucket condition — the condition under which a mean of
  means is wrong and a sum-and-count hierarchy is not. A 60-second cadence would make §6.3 pass for
  the wrong reason.

**Load-independent, quote freely:** the column mapping and its gaps (215/215, 451/627, 43/396 and the
four semantic ones); every structural result in §3.4 and §1.2; the bytes-per-row figures for the old
aggregate views (1.62 kB hourly, 1.66 kB daily at 399 columns); the per-statistic exactness in §6.3
and the derivation of the two bounds.

**Load-dependent, re-measure before quoting:** every absolute time in §5.5 and §6.1 (the lab pod is
untuned and 2–8 cores against W4's tuned 16); the import throughput, which in this lab is dominated
by two `kubectl exec` hops and not by either database; all table sizes, which scale with row count.

**Not measured here at all, on purpose: compression.** The lab's hold rate is **3.1%**, because a
40-second cadence moves the value model's sinusoid far enough per step that consecutive readings
differ. D-10 measured the same schema between 1.89× and 13.74× with the entire spread explained by
that rate, so any compression figure from this lab would be near the entropy ceiling and would
understate production by up to 7×. OQ-2 remains OQ-2; P-6 in the preflight is the query that settles
it, and it is the owner's to run.

---

## 9. Reproducing every number above

Opinions are not findings. The setup, once:

```sh
REMOTEWEATHER=/path/to/remoteweather ci/db/migrate-from-go/lab/run-in-pod.sh up
ci/db/migrate-from-go/lab/10-seed.sh          # ~25 min: 4.3 M rows, then retention
ci/db/migrate-from-go/lab/run-in-pod.sh migrate preflight
ci/db/migrate-from-go/lab/run-in-pod.sh migrate archive
ci/db/migrate-from-go/lab/run-in-pod.sh migrate all
```

`10-seed.sh` writes the full 2025-01-01 → 2026-10-30 range, materialises all four old levels over it,
and then applies the production retention intervals — which is what produces the 295-day rollup-only
window rather than declaring it.

| # | finding | where | how |
|---|---|---|---|
| M-1 | retention drops raw and the rollup survives, bit-identical | §1.2 | `drop_chunks('weather', older_than => …)` then compare against a baseline table |
| M-2 | a manual refresh over the source-less range deletes 21 of 29 buckets | §1.2 | `CALL refresh_continuous_aggregate('weather_1d', lo, hi)` |
| M-3 | the scheduled policy deletes 41 of 83, range == dropped-chunk range | §1.2 | `add_continuous_aggregate_policy(…, INTERVAL '10 years', …)` then `CALL run_job(<id>)` |
| M-4 | `drop_chunks` logs 2 invalidation rows | §1.2 | `SELECT count(*) FROM _timescaledb_catalog.continuous_aggs_hypertable_invalidation_log` |
| M-5 | `pg_dump -t weather_1d` carries no data; the archive copy does | §2 | `pg_dump -t … \| grep -cE '^COPY \|^INSERT INTO'` |
| M-6 | archive sizes, and the per-row figure for the old 399-column views | §2 | `pg_total_relation_size` on the copies; `hypertable_size` on the materialisations |
| M-7 | an empty materialisation on a `materialized_only = false` level is invisible | §2 | `cagg_watermark` per level |
| M-8 | an `INSERT` into a level is accepted, then destroyed three ways | §3.4 | insert, `refresh_continuous_aggregate`, `DROP MATERIALIZED VIEW` |
| M-9 | the union view returns the same 365 rows and costs 37% more | §3.1 | the same `/span` daily-band query against both relations, median of 5 |
| M-10 | 215/215 columns map by name and type; 0 orphans either way | §4.1 | `cargo test -p xtask legacy::` |
| M-11 | the gap report and the stop-the-migration assertion at 143 columns | §4.1 | a second lab source with 72 columns dropped and one unknown added |
| M-12 | 451 of 627 archive columns fillable, 176 NULL, 43 unconsumed | §4.3 | `cargo test -p xtask legacy::` |
| M-13 | the mean/angle import identities are exact on 877 of 877 rows | §4.2 | `40-rollup-check.sql` R-5 |
| M-14 | insert cost of four defined levels is inside the control spread | §5.2 | three 1-day fills with and without the levels |
| M-15 | a day of rows below the threshold logs 1 invalidation, 24 kB | §5.2 | refresh to advance the threshold, then load an older day |
| M-16 | the migrated raw table is 19% smaller, uncompressed | §5.3 | `hypertable_detailed_size` on both |
| M-17 | phase 1 is 1 s; phase 2 is 13 m 30 s + 242 s + 25 s + 2 s | §5.5 | `PSQL=… ci/db/rebuild-aggregates.sh` |
| M-18 | the retry collision, and the slice reset that fixes it | §5.4 | re-run a slice with rows already present |
| M-19 | 1,182 digest rows `ok`; 42 spot checks `ok` with timestamps | §6.1, §6.2 | `migrate-from-go.sh reconcile` |
| M-20 | 12 of 14 statistics bit-exact; 2 within `n × 2^-24` | §6.3 | `40-rollup-check.sql` R-4 |
| M-21 | scalar and vector wind means disagree in 1,106 of 1,106 buckets | §6.3 | R-4c, the control |
| M-22 | both float4-accumulated statistics differ as float32 in 100% | §7 | R-4d |
| M-23 | five negative controls, all of which failed as intended | §6.4 | §6.4's table |

The environment, for the figures that depend on it: a single `timescale/timescaledb-ha:pg17` pod on
an `emptyDir`, 2 CPU requested and 8 limit, 4–12 GiB, **no tuning at all** — default
`shared_buffers`, default `max_wal_size`, `fsync = on`. Every bulk operation sets
`synchronous_commit = off` where `rebuild-aggregates.sh` does and nowhere else. The import's
throughput additionally crosses two `kubectl exec` hops, which dominate it.

---

# Decisions

1. **`drop_chunks` on the raw hypertable logs an invalidation over the dropped range, so the next
   refresh of any level over that range deletes the buckets whose raw has gone — and the ordinary
   scheduled refresh policy is a refresh.** Measured on TimescaleDB 2.31.0: a manual refresh removed
   21 of 29 daily buckets, and one policy run with `start_offset = 10 years` removed 41 of 83, the
   destroyed range matching the dropped-chunk range to the day. The prompt's §4 hazard is real and
   understated.
2. **Therefore: take the archive copy before anything else, and never run
   `refresh_continuous_aggregate` over history on the old database, or re-enable a refresh policy
   there.** `migrate-from-go.sh archive` is the only step that writes to the source and is a
   separate, explicitly named step for that reason.
3. **The source database is read-only for the duration apart from step 2.** Every other statement
   sent to it is a `SELECT` or a `COPY ... TO STDOUT`.
4. **The rollup archive goes into plain tables — `observations_1d_legacy` and
   `observations_1h_legacy` — and never into a continuous aggregate.** An `INSERT` into a level is
   accepted on 2.31 (and accepts duplicates), and the rows are then destroyed by a refresh over the
   range, by the level's own refresh policy, or by adding a metric, which drops and recreates the
   level (D-15). Three routine operations, no warning from any of them.
5. **Only the daily and hourly levels can receive the archive.** `observations_1m` and
   `observations_5m` carry 7-day and 14-day retention (W4 decision 5), so a 2025 bucket imported
   there is dropped within a week. W4 assumption 35 confirmed.
6. **`/span` keeps reading `observations_1d`. The archive is carried but not served.** `/span` takes
   no time anchor and rejects a span over one year, and raw is permanent (D-4), so the furthest band
   it can ask for always has real data behind it. Measured: the union view returns the same 365 rows
   and costs 37% more (18.3 ms against 13.3 ms). `observations_1d_with_legacy` is generated and ready
   for a time-anchored endpoint that does not exist yet.
7. **Recommendation on the history question: Option B — import the archive.** 2.5 MB daily plus
   58 MB hourly at production scale, no read-path change, against the permanent loss of the only
   record of ~294 days. `w9.legacy_boundary` plus `w9.assert_legacy_disjoint()` keep the two sides
   non-overlapping so a future union cannot double-count a bucket.
8. **The raw column mapping is the identity and is not hand-written.** All 215 columns of the Go
   `createTableSQL` exist in `metrics/registry.toml` under the same name and the same type; the only
   registry column with no counterpart is `station_id`. The import's column list is computed at run
   time as `w9.expected_column INTERSECT w9.source_column`, so one artefact is correct at
   production's ~143 columns and at the source definition's 215 — proved by a second lab run at 143.
9. **Create the schema with no aggregates, load, then create the levels — but not for the stated
   reason.** The insert cost of four defined levels is inside the control group's own spread
   (1,912 ms against 1,815 ms, median of 3) and a day of rows below an advanced invalidation
   threshold logs one record in a 24 kB table. The step holds because a refresh *policy* created with
   the level will never backfill outside its `start_offset`, because a watermark set mid-load records
   load progress rather than data extent, and because the Go DDL materialises on create in one
   uninterruptible statement.
10. **A source column with no registry entry stops the migration.** `w9.assert_no_unknown_source_columns()`
    raises; the three resolutions are a `[[metric]]` entry, `observations.extra`, or a row in
    `w9.dropped_source_column` with a `NOT NULL` reason. Proved able to fail.
11. **The transform is generated from `metrics/registry.toml` plus a committed inventory of the Go
    views, because the Go views are not self-describing.** `avg(outtemp) AS outtemp` and
    `max(dayrain) AS dayrain` are indistinguishable by name, so which old column holds which
    statistic lives only in the view definition. `extract-go-views.sh` extracts it mechanically from
    `remoteweather c3ec9f7` and asserts that all four views are shape-identical apart from the bucket
    width.
12. **An archived mean is imported as `sum = avg, count = 1`, and an archived angle as
    `sin_sum = sind(d), cos_sum = cosd(d), dir_count = 1`.** `mw_mean(avg, 1) = avg` and
    `atan2d(sind(d)/1, cosd(d)/1) = d` are identities, so nothing is approximated. Verified exact on
    877 of 877 rows for mean, angle, extrema and total.
13. **176 of 627 aggregate columns land NULL in an imported row, and each one says why in the DDL.**
    The counts are asserted in a unit test so a registry change that widens the gap is a failing test
    and a revised figure rather than more NULLs.
14. **43 of the old views' 396 aggregate columns are not consumed, and all 43 are recorded in
    `w9.unconsumed_archive_column` with their reasons.** Four are semantic — `lightning_day` and
    `lightning_hour` are `sum()` in the Go view against `max` in the registry, `lightning_time` has
    no aggregate at all, `min_gdd` has no `min` destination — and 39 are truncated integer means.
    None is contract-visible.
15. **`station_id = stationname` is the default station mapping, and it is explicit rather than
    silent.** It keeps `/span?station=<device name>` addressing the history it addressed before.
    `w9.station_map` is populated by hand and asserted complete against the stations actually present
    in the source.
16. **Reconciliation sums in `numeric` and compares with `IS DISTINCT FROM`, with no tolerance.**
    Float addition is not associative, so a float sum over 19 GB depends on row order and worker
    count; `numeric` is exact and order-independent, and `float4 → numeric` loses nothing. A
    reconciliation that ran against zero rows raises rather than passes.
17. **The hierarchy comparison is exact for twelve of fourteen statistics and bounded for two, and
    the bound is derived rather than chosen.** `sum(real)` and the Go `circular_avg_state`'s `real`
    accumulators make exact agreement arithmetically impossible for `period_rain` and `winddir`; the
    bound is `n × 2^-24` relative and the new value is the accurate one. Every other row gets zero
    tolerance and measured exactly zero.
18. **The raw import is resumable at slice granularity, and `w9.reset_slice` is what makes it so.**
    `COPY` has no `ON CONFLICT`, so retrying a half-imported slice collides with
    `observations_station_id_time_key`; deleting the slice's range first makes every slice retryable
    and normally removes nothing.
19. **`stations` is populated before any reading and `station_metrics` after all of them.** The
    second is one pass per metric over the imported history — the scan the Go TODO at
    `grpcreceiver/station.go:64` avoided per request, paid once.
20. **The migration SQL is drift-gated.** `legacy-import-drift` runs `cargo xtask legacy-import
    --check` and is armed by `go-aggregate-views.toml`; it is separate from `codegen-drift` because
    this emitter has a second input describing a frozen tree at a named commit, and `codegen --check`
    should keep meaning exactly "generated/ matches metrics/registry.toml".
21. **No compression figure is quoted from the lab.** The lab's hold rate is 3.1% against production's
    expected ~87%, and D-10 shows the ratio is a property of that rate. OQ-2 stays open.

---

# Open questions

**OQ-W9-1. Which history option, and is 60 MB worth ten months?**

This is the one question only the owner can answer, and §3 is written to be read as a decision.
Option A deletes ~294 days of daily and hourly rollup, permanently, and no endpoint notices. Option B
keeps it for ~2.5 MB plus ~58 MB, with no read-path change and no endpoint able to read it until a
time-anchored endpoint exists.

*Recommendation:* **Option B**, and take the §2 archive copy regardless and immediately, because that
decouples the decision from the deadline. The cost is nearly nothing and the loss is irreversible.

**OQ-W9-2. Is the production `weather_1d` refresh policy running, and what TimescaleDB version?**

These are one question because §1.2 makes them one. On 2.31 an active policy would already have
destroyed the 294 days, and they exist — so either the policy is not running or production predates
the destructive behaviour. P-1, P-2b and P-3b answer it in a minute.

*Options.* (a) Policy absent or disabled: the history is safe; archive and proceed. (b) Policy active
on an older version: the history is safe *on that version* and the archive becomes urgent before any
upgrade touches that instance. (c) Policy active on 2.31+ and the history is nevertheless intact:
something in the model above is wrong and it should be understood before the archive is trusted.

*Recommendation:* run P-1 to P-3b, paste the output in, and **take the archive copy before reading
it.** The copy costs 3 MB and three seconds and is correct under all three answers.

**OQ-W9-3. `rebuild-aggregates.sh` phase 1 leaves `observations_1d` short beyond 14 days.**

The `/span` bands are 6 h, 48 h, 14 d and 366 d, so the daily level's band is 26× wider than the
hourly level's. Bottom-up within phase 1 has the daily level read an hourly level materialised for
only 14 days and cache the hole. A full run fixes it, because phase 2's hourly refresh invalidates
the daily level; a `--recent-only` run does not, and `--recent-only` is what W4 §4 recommends after a
metric addition.

*Options.* (a) Leave it; a metric addition is followed by a full rebuild in practice. (b) Have phase 1
refresh each level over the **widest** band any level above it serves, so the daily level's phase-1
source is complete — one line, and it makes phase 1 a few seconds instead of one. (c) Have
`--recent-only` print a warning naming the levels it left incomplete.

*Recommendation:* **(b), with (c) as well.** The script is W4's and this document does not change it.

**OQ-W9-4. Does `period_rain`'s accuracy improvement need recording as a contract deviation?**

`/span`'s `period_rain` and `windd` will differ from the Go server's values in the 6th significant
figure, because the Go view accumulates in `float4` and D-14 widened the accumulator. At float32 they
differ in 100% of the buckets measured. The conformance suite compares shape and formatting rather
than values, so nothing fails.

*Options.* (a) Record it in W6's ledger as a consequence of D-14 and move on. (b) Reproduce the Go
arithmetic for these two columns to make the values bit-identical — which means a deliberately
narrow accumulator and giving up D-14 for two metrics. (c) Say nothing, since no test compares values.

*Recommendation:* **(a).** (b) trades accuracy for bit-compatibility nothing checks, and (c) leaves a
dual-run value diff looking like a migration defect. **W6 decides** whether this belongs in the
deviation ledger or only in this document.

**OQ-W9-5. Should the `w9` staging schema be dropped after cutover, and when?**

`w9` holds the column inventories, the station map, the slice bookkeeping, the archive landing tables
and the reconciliation results — 81 MB in the lab, mostly the hourly archive staging. Everything in
it is reconstructible except `w9.station_map`, and `observations_1d_legacy` is deliberately **not** in
it.

*Options.* (a) Drop `w9` once reconciliation passes and the source is decommissioned; keep the
reconciliation output as a text artefact first. (b) Keep it indefinitely; 81 MB. (c) Keep only
`w9.station_map`, `w9.dropped_source_column` and `w9.unconsumed_archive_column`, which are the three
tables that record human decisions.

*Recommendation:* **(c).** The decisions are worth keeping and the staging is not. The schema comment
in the generated DDL says exactly this.

**OQ-W9-6. Does `lightning_day`/`lightning_hour` mean a maximum or a sum?**

The Go view computes `sum()`, the registry declares `max`. Nothing serves either, so nothing is
currently wrong — but the two documents disagree about what the metric is, and 39 further columns
have the same shape at lower stakes.

*Options.* (a) Leave it; `max` is the defensible reading of a cumulative counter and the registry is
the source of truth (D-20). (b) Add `total` to the registry entry as well, which costs two columns at
each of four levels and lets a future endpoint serve either.

*Recommendation:* **(a).** A daily strike counter's daily maximum *is* the day's strike count, and
the Go view's sum of a cumulative counter is the bug. **W2 owns the registry entry**; this is recorded
rather than changed.

---

# Assumptions about other work items

**About W4 (database schema)**

1. `observations_station_id_time_key` is created by `20261029234500` on an empty `observations`, which
   discharges W4 assumption 34 completely. W9 does not create it and must not drop it: §5.4 shows the
   import's resumability depends on it existing.
2. The four levels' retention windows are 7, 14, 90 days and unbounded (W4 decision 5). Decision 5
   here depends on it: a different set of windows changes which levels can receive the archive.
3. `observations_1d` is the only level `/span` reads over history, and the other three are read over
   trailing windows inside the raw window (W4 §1.2). Decision 6 depends on this.
4. `stations` is keyed by `station_id text` and carries `name`, `type`, `timezone`, `latitude`,
   `longitude`, `altitude_ft`, `snow_base_distance_mm`, `first_seen`, `last_seen` (W4 §2.1).
   `w9.station_map` carries exactly these and `w9.populate_stations()` writes them.
5. `stationname` and `stationtype` are still columns on `observations`, as `role = "attribute"`
   dimensions. The import copies them. When OQ-W4-2 drops them, the import's column list shrinks
   automatically because it is derived from the registry — but `w9.station_map` becomes the only
   source of a station's name, which is what OQ-W4-2 intends.
6. `station_metrics` is keyed by the registry's stable `key` and not by the column name (W4 §2.2).
   `w9.metric_key` is generated from the registry for this.
7. Every metric column in `observations` is nullable (W4 §3.4), so a column production does not have
   imports as NULL rather than failing.
8. `mw_mean(total, n)` returns NULL for `n = 0 or NULL`, and `mw_circular_mean` returns **0**
   (W4 §3.1, OQ-W1-2 option (c)). §4.2's `count = 1` import relies on the first; the archive's angle
   import relies on the second behaving the same for an imported row as for a computed one.
9. Count columns above the first level are `bigint`, not `numeric` (W4 §3.2). `observations_1d_legacy`
   declares `bigint` to match, so a future union view type-checks.
10. `ci/db/rebuild-aggregates.sh` is W4's and remains so. W9 added one thing to it — a `PSQL`
    override of the connection, so it runs where `psql` cannot reach the database directly — and
    changed nothing about the ordering or the phases. Flagged below.
11. `observations_1d_with_legacy` and `observations_1h_legacy` are **not** inputs to any level and
    must not be refreshed, compressed or retained. If W4's schema assertions grow a "every relation
    named `observations*` is a hypertable or a level" check, these two are the exceptions.

**About W2 (metric registry)**

12. `metrics/registry.toml`'s `column` field is the *old* column name for all 215 Go columns, which is
    what makes §4.1's mapping the identity. If a column is ever renamed (`--allow-breaking`), the
    import breaks and `legacy-import-drift` catches it.
13. `aggregates` and `levels` on a metric determine the archive's destination columns.
    `legacy_aggregate_rest_keys.total = ["period_rain"]` on `rain_incremental` is the one legacy key
    naming an aggregate, and the transform special-cases nothing: it reads the registry.
14. The 39 `avg(<int>)::int` columns and the four semantic ones in §4.3 are consequences of registry
    decisions, not of this transform. OQ-W9-6 asks W2 about two of them.
15. `cargo xtask legacy-import` reads the registry through `xtask/src/registry.rs` and reuses
    `Metric::agg_columns` indirectly via `Registry::level_columns` in its tests, so a change to how a
    strategy expands into columns fails a W9 test rather than silently producing a mismatched
    `INSERT`.

**About W3 (service architecture)**

16. `/span` resolves a device name to a `station_id` through `StationRegistry`/`stations`
    (W3 §1.2, §2.6). Decision 15's `station_id = stationname` default assumes the resolution path
    matches on `stations.name`, which is the column `w9.populate_stations()` fills with the old
    `stationname`.
17. `snapshot:latest` reads `stations` for display name and type rather than reading
    `stationname`/`stationtype` off a reading (W4 assumption 24). The migration populates `stations`
    before any reading for this reason.
18. `ReadingStore::span` reads `observations_1d` and not `observations_1d_with_legacy` (Decision 6).
    If W3 or a later work item points it at the union view, §3.1's 37% is the cost and the 365-row
    result is the benefit.
19. The trailing partial bucket `/span` appends from raw (W4 decision 13, W3's to place) is unaffected
    by anything here: it reads raw over `[last_closed_bucket_end, now)`, which is always inside the
    imported window.

**About W1 (frozen contract)**

20. `/span` takes no time anchor, rejects a span over one year, and `station` is a device name
    (`docs/legacy-api-contract.md` §2.1, §2.3). **Decision 6 rests entirely on this.** If a future
    endpoint takes a time anchor, Decision 6 should be revisited and the union view is already there.
21. Conformance is shape, types and formatting rather than values (`contract/README.md`). §7's two
    differing keys are not conformance failures because of this. If the suite ever compares values,
    `period_rain` and `windd` are the two that will differ.
22. `period_rain` is a `/span`-only key naming a bucket total with no raw counterpart
    (W1 assumption 11). The archive import maps `weather_1d.period_rain` to
    `rainincremental_total`.

**About W6 (reconciliation and implementation plan)**

23. OQ-W9-4 asks W6 whether the `period_rain`/`windd` accuracy change belongs in the deviation
    ledger. It is a consequence of D-14, which is merged, not a deviation introduced here.
24. The time-anchored v2 endpoint that W1 §2.1 and W3 both defer is the only thing that would ever
    read `observations_1d_legacy`. W6 is already asked to record it as a named future work item;
    this is a second caller for it.
25. The pre-flight list in `docs/rewrite/README.md` item 1 should be amended to say *archive first,
    then pause* rather than treating them as one action, because §1.2 shows the archive is correct
    under every answer to OQ-W9-2 and pausing is not what protects the aggregate buckets.

**About W8 (CI and release)**

26. `legacy-import-drift` follows `codegen-drift`'s pattern exactly: declared in `ci/gates.toml` with
    an `arms_when` path, a job in `pr.yml`, a dependency of the single required `gate` job, and the
    same "assert the check left the tree clean" step (R-2).
27. The `actionlint` job's `shellcheck` step now names the five new scripts and
    `ci/db/rebuild-aggregates.sh`, which it did not cover before. All six are clean at
    `-S warning`.

---

# Proposed amendments to `DESIGN.md`

Following the protocol, and following W4's precedent: these are **proposed** here and
`docs/DESIGN.md`'s `## Amendments` section is left untouched. That section is still empty, including
of W4's own A-W4-1 through A-W4-3, so appending to it from this document alone would make it look
like W9 found three problems and nobody else found any. W6 consumes these four sections from every
document and is the right place for the amendments to be applied together.

Nothing below edits a decision in place, each entry names the date, the decision, what was found and
what replaces it, and this document carries on under the existing decisions throughout.

*Date for all three entries: 2026-10-30.*

**A-W9-1 — amend D-4 and D-6.** *Retention dropping a raw chunk invalidates the continuous aggregates
above it, so on TimescaleDB 2.31 the next refresh of any level over that range — including the
level's own scheduled policy — deletes the buckets whose raw has gone.*

D-4 ("raw is retained indefinitely") and D-6 ("`pg_dump` of the hypertable plus a rebuild is the
backup") both rest on aggregates being disposable derived data, which they are *in the new design*
because raw is permanent. The old database is the case where they are not, and the measurement is
specific: a manual refresh removed 21 of 29 daily buckets and one policy run removed 41 of 83, the
destroyed range matching the dropped-chunk range to the day, with `drop_chunks` logging the
invalidation that caused it.

Nothing about the new design changes. The amendment is the mechanism, recorded, because it is the one
way the owner's existing data can still be lost and because "the aggregates are disposable" is the
sentence that makes it sound safe.

**A-W9-2 — append to D-6.** *A continuous aggregate accepts an `INSERT` on TimescaleDB 2.31 and
discards it on the next refresh, on the next policy run, or when the level is dropped and recreated.
Pre-seeded aggregate rows are not a restore mechanism.*

D-6 makes "restore raw, then rebuild the aggregates" the supported path, which is right and which
this adds the converse of: there is no supported path that puts rows *into* an aggregate. Measured —
the insert succeeds, duplicates succeed (a materialisation hypertable has no unique constraint on
`(bucket, station_id)`), the rows read back correctly through `mw_mean`, and then a refresh over the
range leaves zero rows. The failure is entirely at a later time than the mistake, which is what makes
it worth a decision rather than a comment.

**A-W9-3 — append to D-14.** *The Go schema's own aggregates are lossier than D-14 assumed, and
`/span`'s `period_rain` and `windd` will differ from the Go server's values in the 6th significant
figure as a result.*

D-14 widened the new accumulator because `sum(real)` is not associative and a hierarchy adds in a
different order than a direct scan. The old schema does worse than add in a different order: it
accumulates in `float4`. `sum(rainincremental)` returns `real`, and `circular_avg_state` declares
`sin_sum real, cos_sum real, accum real` with a finaliser that divides and takes `atan2d` in `real`.
Measured at 2,160 readings per daily bucket: `period_rain` differs by up to 0.58 in 185,877
(3.13e-6 relative) and `winddir` by up to 6.07e-5 degrees (1.73e-7), and **both round to a different
float32 in 100% of buckets.** At production's 2-second cadence the Go side's error is roughly 20×
larger.

D-14's decision is correct and is strengthened by this. The amendment records that it has a
*visible* consequence on two frozen-contract keys, which D-14 does not mention, and that the new
numbers are the accurate ones.

---

## Generator edits made by this work item, flagged

Two files owned by other work items were changed, both minimally, both named here because an
unflagged edit to someone else's artefact is the thing the output contract exists to catch.

1. **`ci/db/rebuild-aggregates.sh` (W4)** — `PSQL` now overrides the whole `psql` invocation, so the
   script works where `psql` cannot reach the database directly: a container, a bastion, or the W9
   lab, which reaches a pod through `kubectl exec`. `DATABASE_URL` remains required unless `PSQL` is
   given. No change to the ordering, the phases, the bands or the durability setting. W9 calls this
   script rather than reimplementing the rebuild, which is why the edit was preferable to a copy.
2. **`xtask/src/main.rs` (W2/W8)** — one `mod legacy;`, one match arm for `legacy-import`, one usage
   block, and the `legacy_import` function. `gen::all` is untouched and `codegen --check` still means
   exactly "generated/ matches `metrics/registry.toml`"; the W9 emitter is a separate subcommand with
   a separate gate because it has a second input describing a frozen tree at a named commit.
