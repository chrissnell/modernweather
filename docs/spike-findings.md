# W0 Part 1 — Spike findings

Measured answers to the six de-risking questions in `docs/rewrite/W0-spike-and-decisions.md`.
Every number below was produced by the command shown next to it, on the instance described under
**Test rig**. Where a number depends on something the spike cannot know — chiefly the entropy of real
sensor data — that is said plainly rather than papered over with an average.

---

## Findings that contradict planned decisions

Four findings change what `DESIGN.md` must say. Two of them are corrections to a decision, two are
constraints the planning did not account for. The remaining decisions are confirmed.

1. **`sum()` over a `float4` column returns `float4`, and a hierarchical sum of `float4` is not
   order-independent.** The plan says aggregates must store `sum`/`count` rather than a finished
   `avg`. That is necessary but not sufficient: with a `real` accumulator, the mean reconstructed
   through raw → 1m → 1h → 1d matched the mean aggregated directly from raw in only **1 of 56**
   day/station pairs, with a maximum discrepancy of **1.55 × 10⁻⁴**. Widening the accumulator to
   `double precision` made it **bit-identical in 56 of 56** (error exactly 0), as did `numeric`.
   The registry must therefore emit `sum(col::double precision)`, not `sum(col)`. Full numbers in
   Q4.

2. **The 10–12× compression assumption is only true if real readings repeat as often as this
   spike assumed.** On the same schema and the same 30% column population, the measured ratio ranged
   from **1.89× to 13.74×** across four defensible data models, and the single variable that
   explained the spread was the fraction of consecutive readings that are *identical* — 0% → 1.89×,
   33% → 3.36×, 87% → 11.2–13.7×. The planning number is reproduced by a realistic held-sensor model,
   so the decision survives, but the capacity figure does not transfer until someone runs the
   one-line hold-rate query (given in Q3) against production. At the pessimistic end ten years of raw
   is ~100 GB rather than ~16 GB, which is still affordable — so this changes the sizing, not the
   decision.

3. **`refresh_continuous_aggregate` costs ~125× the query it runs, and the overhead is fsync, not
   CPU.** Refreshing a 1-minute aggregate over 36 M raw rows took **306 s**; the identical
   aggregation as `CREATE TABLE AS SELECT` took **2.4 s**. `pg_stat_activity` showed
   `wait_event = IO / WalSync` on every sample. Setting `synchronous_commit = off` for the refresh
   session brought it to **25.6 s** — a 12× improvement. Batching the refresh into 32 seven-day
   windows did **not** help (299 s). "Aggregates are disposable derived data that can be rebuilt on
   demand" is true, but only affordable if rebuilds run with `synchronous_commit = off`, which is
   safe precisely because the data is derivable. This belongs in `DESIGN.md` as a constraint, and in
   W4 as the rebuild procedure.

4. **`count()` never returns NULL, so a wide aggregate loses the NULL-density advantage that
   justifies the wide raw table.** A 398-column 1-minute aggregate (`sum`/`count`/`min`/`max` over 99
   metrics) materialised **347 MB per 30 days**; dropping the count columns gave 159 MB, and keeping
   them as `nullif(count(x), 0)` gave **234 MB** at no measurable time cost. On a 70%-NULL table the
   count columns are a third of the aggregate's size and almost all of it is zeroes. The registry
   should emit nullable counts.

Confirmed without qualification:

- `ALTER TABLE ... ADD COLUMN <nullable>` on a hypertable with ~1 GB of compressed chunks is a
  catalog-only operation — **11.6 ms**, no decompression, every chunk still compressed afterwards.
  The "adding a metric is cheap" goal holds (Q2).
- Hierarchical continuous aggregates work three levels deep on this version, and rebuilding an upper
  level from the level below is **52× cheaper** than rebuilding it from raw at 217 days of history,
  and the gap grows linearly with retention (Q4, Q5).
- `avg(avg(x))` is wrong exactly as predicted: incorrect in **56 of 56** day/station pairs once
  bucket counts differ, maximum error **0.114** (Q4).
- TimescaleDB Toolkit's `stats_agg` / `rollup` is a correct composable alternative: mean bit-identical
  in 868 of 868 pairs, standard deviation within 6 × 10⁻¹⁴ (Q4).
- The circular-average pattern the Go implementation already uses composes through three aggregate
  levels to within 3 × 10⁻¹² degrees (Q4).

---

## Test rig

| | |
|---|---|
| Image | `timescale/timescaledb-ha:pg17` (927,756,674 bytes), pulled 2026-10-28 |
| Host | Kubernetes pod, 24-vCPU / 64 GiB node, `emptyDir` on node-local filesystem |
| `shared_buffers` | 8 GB (image default for this node) |
| `max_wal_size` | 16 GB (raised from the 1 GB default) |
| Disk `fdatasync` | **1351 ops/sec, 740 µs/op** (`pg_test_fsync -s 3`) |

The fsync figure matters: it is the bottleneck in Q5, so Q5's numbers are storage-dependent and will
improve on hardware with a faster write barrier. Everything else here is CPU- or catalog-bound and
transfers directly.

### Dataset

The schema is the real one: `createTableSQL` from `internal/storage/timescaledb/sql.go` in the Go
repository, verbatim, plus a `station_id text NOT NULL` column so that `segmentby = station_id` could
be tested as specified. **216 columns** — 127 `real`, 69 `integer`, 14 `text`, 5 `timestamptz`,
1 `bigint`.

| | |
|---|---|
| Rows | 35,078,400 for Q2/Q3; 36,045,124 for Q4/Q5 |
| Stations | 4 |
| Interval | 2 s, matching production's observed reading cadence |
| Span | 2025-01-01 → 2025-08-05 (217 days) |
| Chunk interval | 7 days, matching production |
| Columns populated | **65 of 216 (30%)** — so 70% NULL, as specified |
| Uncompressed size | **13 GB** (≈ 400 bytes/row), i.e. 24.7 GB/year against production's measured 19 GB/year |

Values are slowly-varying floats quantised to one decimal place. Days 203–217 were loaded with
pseudo-random gaps (40% of readings kept, **2 to 23 readings per minute** against a regular 30), so
that the Q4 correctness tests run against *uneven* bucket counts — which is the only condition under
which `avg(avg(x))` can be caught.

> **Note on the schema the DDL describes.** The Go `createTableSQL` declares 215 columns today, but
> the measured facts in `docs/rewrite/README.md` record the production table at ~143. That gap is
> itself a finding: because the Go version issues `CREATE TABLE IF NOT EXISTS` and never alters the
> hypertable, every column added to that constant since first run exists in the source and not in the
> database. The spike used the source's 215 (+1) because that is the wider, more demanding case.

---

## Q1 — Version

```sql
SELECT extversion FROM pg_extension WHERE extname = 'timescaledb';
```

| | |
|---|---|
| PostgreSQL | **17.11** (Ubuntu 17.11-1.pgdg22.04+2) |
| `timescaledb` | **2.31.0** |
| `timescaledb_toolkit` | **1.26.0** |

This is the version the rewrite should deploy on: PostgreSQL 17 with TimescaleDB 2.31, from the `-ha`
image, which is also what makes Toolkit's `stats_agg` available (see Q4).

**Production's version was not measured, and this is the one gap in the spike.** The production
database is not reachable from the agent runtime — there is no TimescaleDB or weather workload in the
homelab Kubernetes cluster, and the only version reference anywhere in the Go repository is
`timescale/timescaledb:latest-pg14` in a `docker-compose` example in `README.md`. That is a documented
example, not evidence about the deployed instance.

**Owner action (one minute).** Run on production:

```sql
SELECT extversion FROM pg_extension WHERE extname = 'timescaledb';
SELECT version();
```

**Which answers are version-sensitive:**

| Finding | Sensitivity |
|---|---|
| Q2 `ADD COLUMN` on compressed chunks | **High.** Nullable `ADD COLUMN` without decompression arrived in the 2.11 line and was extended since. On an older server this answer can flip to "decompress everything". |
| Q4 hierarchical CAGGs | **High.** CAGG-on-CAGG arrived in 2.9. Not available before that. |
| Q4 `stats_agg` / `rollup` | **High.** Requires `timescaledb_toolkit`, which is not in the plain `timescale/timescaledb` image. |
| Q3 compression ratio | Low. Algorithms are stable; entropy dominates (see Q3). |
| Q5 refresh cost | Low on version, **high on storage**. |
| Q6 column ceiling | None. 1600 is a PostgreSQL compile-time constant. |

If production is on PG14 / TimescaleDB 2.x-early, that is not an obstacle to the rewrite — the new
system brings its own server version — but it does mean the migration in W9 is a major-version
PostgreSQL upgrade as well as a schema change, which W9 currently does not assume.

---

## Q2 — The highest-risk unknown: `ADD COLUMN` on compressed chunks

**Answer: it succeeds, in milliseconds, without decompressing anything.**

Setup — the hypertable was fully compressed first, so the test ran against ~1 GB of compressed chunks
as specified:

```sql
ALTER TABLE weather SET (timescaledb.compress,
                         timescaledb.compress_segmentby = 'station_id',
                         timescaledb.compress_orderby   = 'time DESC');
SELECT compress_chunk(c) FROM show_chunks('weather') c;     -- 87.6 s for 30 chunks / 13 GB
```

State at the moment of the test: **35,078,400 rows, 30 of 30 chunks compressed, 995 MB compressed**
(13 GB before).

```sql
ALTER TABLE weather ADD COLUMN spike_new_metric float4 NULL;
```

| Operation | Time | Chunks still compressed | Hypertable size |
|---|---|---|---|
| `ADD COLUMN float4 NULL` | **11.6 ms** | 30 / 30 | unchanged |
| `ADD COLUMN float4 NULL DEFAULT 1.5` | 38.3 ms | 30 / 30 | unchanged |
| `ADD COLUMN float4 NOT NULL DEFAULT 0` | 20.5 ms | 30 / 30 | unchanged |
| 20 nullable columns in one statement | 93.3 ms | 30 / 30 | unchanged |
| `DROP COLUMN` | 23.8 ms | 30 / 30 | unchanged |

Correctness checks after the fact:

```sql
SELECT spike_def, spike_nn, spike_new_metric, count(*) FROM weather GROUP BY 1,2,3;
--  1.5 | 0 | <null> | 35078400
```

All 35 M pre-existing compressed rows read back `NULL` for the plain added column and the *declared
default* for the ones with defaults — PostgreSQL's attmissingval fast-default path works through
TimescaleDB's compressed chunks. A subsequent `INSERT` carrying a value for the new column into a
time range covered by a compressed chunk succeeded and read back correctly.

**The one caveat worth designing around: the lock.**

```sql
BEGIN;
ALTER TABLE weather ADD COLUMN lockprobe float4;
SELECT mode, count(*) FROM pg_locks WHERE pid = pg_backend_pid() AND locktype = 'relation' GROUP BY 1;
--  AccessExclusiveLock | 63
--  AccessShareLock     |  1
ROLLBACK;
```

The statement takes `AccessExclusiveLock` on **63 relations** — the hypertable, all 32 chunks, and all
30 compressed mirrors — for the ~12 ms it runs. It is instant but not lock-free: it queues behind any
long-running reader and blocks all of them while it waits. Migrations that add metrics should set a
short `lock_timeout` and retry rather than block the API.

---

## Q3 — Real compression ratio

**Answer: 13.74× on this dataset — but the ratio is a property of the data, not the schema, and it
ranges from 1.89× to 13.74× across four plausible models of the same 216-column table.**

```sql
SELECT pg_size_pretty(before_compression_total_bytes),
       pg_size_pretty(after_compression_total_bytes),
       round(before_compression_total_bytes::numeric / after_compression_total_bytes, 2)
FROM hypertable_compression_stats('weather');
```

Headline, on the main dataset (35,078,400 rows, `segmentby = station_id`, `orderby = time DESC`):

| | |
|---|---|
| Before | **13 GB** (12 GB heap + 652 MB index) |
| After | **995 MB** (59 MB heap + 933 MB TOAST + 82 kB index) |
| **Ratio** | **13.74×** |
| Compression time | 87.6 s for 30 chunks / 13 GB (≈ 150 MB/s of input) |

### Why one number is not an honest answer

The same schema, the same 30% column population and the same row count were loaded four times with
different value dynamics. The single variable that predicts the ratio is the share of consecutive
readings from one station that are *bit-identical* to the previous reading — which is what
TimescaleDB's Gorilla encoding for floats actually exploits.

```sql
-- the measurement that explains the spread, and the one to run on production
SELECT round(100.0 * sum((outtemp = prev)::int) / count(*), 1) AS pct_unchanged
FROM (SELECT outtemp, lag(outtemp) OVER (PARTITION BY station_id ORDER BY time) AS prev
      FROM weather WHERE time >= '2025-01-01' AND time < '2025-01-08') z
WHERE prev IS NOT NULL;
```

| Data model | Consecutive readings identical | Before | After | **Ratio** |
|---|---|---|---|---|
| Uniform random per reading (entropy ceiling) | 0.0% | 473 MB | 251 MB | **1.89×** |
| Random walk, ±0.1 every reading | 33.4% | 473 MB | 141 MB | **3.36×** |
| Held sensor (one update per 5 readings) + diurnal cycle | 86.5% | 473 MB | 42 MB | **11.24×** |
| Smooth periodic (the main dataset) | 87.7% | 13 GB | 995 MB | **13.74×** |

The held-sensor model is the realistic one, and it reproduces the planning assumption almost exactly:
a Davis ISS transmits roughly every 2.5 s while the daemon logs every 2 s, so a large majority of
consecutive rows genuinely carry the same value, and the many monotonic counters
(`dayrain`, `monthrain`, `yearrain`, `dayet`…) are constant for hours at a time.

### What this means for sizing

Production measures **19 GB/year uncompressed**, so ten years of raw is ~190 GB before compression:

| If production's hold rate is… | Ten years of raw |
|---|---|
| ~87% (expected, Davis-like) | **~14–17 GB** |
| ~33% | ~56 GB |
| ~0% | ~100 GB |

**The decision to retain raw indefinitely is robust across the entire range** — even the entropy
ceiling costs 100 GB for a decade, which is not a constraint on any machine this system would run on.
Only the number quoted in planning documents is at risk. Run the hold-rate query above on production
before anyone writes "16 GB for ten years" into a capacity plan.

A second, cheap confirmation is available to the owner and would settle this outright: `pg_dump` one
month of the production `weather` table, restore it to a throwaway instance, enable compression with
the same settings, and read `hypertable_compression_stats`.

---

## Q4 — Hierarchical continuous aggregates

**Answer: supported, three levels deep. Composable state rolls up exactly — provided the accumulator
is wider than the column. `avg(avg(x))` is wrong, as predicted and now measured.**

### Does the version support a CAGG built from another CAGG?

Yes. The chain raw → `weather_1m` → `weather_1h` → `weather_1d` was created without complaint on
2.31.0, each level reading only the level above:

```sql
CREATE MATERIALIZED VIEW weather_1h WITH (timescaledb.continuous) AS
SELECT time_bucket(INTERVAL '1 hour', bucket) AS bucket, station_id,
       sum(outtemp_sum) AS outtemp_sum, sum(outtemp_count) AS outtemp_count,
       min(outtemp_min) AS outtemp_min, max(outtemp_max) AS outtemp_max,
       rollup(outtemp_stats) AS outtemp_stats
FROM weather_1m GROUP BY 1, 2 WITH NO DATA;
```

### Numerical correctness

Two paths were built over identical input: the hierarchy above, and a control aggregating the same
day buckets **directly from raw**. They were compared over the window with deliberately uneven bucket
counts (2–23 readings per minute), which is the condition under which an averaged average diverges.

```sql
SELECT count(*) AS pairs,
  count(*) FILTER (WHERE h.n  = d.n)  AS count_identical,
  count(*) FILTER (WHERE h.sn = d.sn) AS numeric_sum_identical,
  count(*) FILTER (WHERE h.s8 = d.s8) AS float8_sum_identical,
  count(*) FILTER (WHERE h.s4 = d.s4) AS float4_sum_identical,
  max(abs(h.s8/h.n - d.s8/d.n))       AS max_mean_err_float8,
  max(abs(h.s4/h.n - d.s4/d.n))       AS max_mean_err_float4,
  max(abs(h.a - d.a))                 AS max_mean_err_avg_of_avg,
  count(*) FILTER (WHERE h.a <> d.a)  AS avg_of_avg_wrong
FROM w1d_t h JOIN w1d_t_direct d USING (bucket, station_id);
```

| Rolled-up state | Identical to direct aggregation | Max error in the reconstructed mean |
|---|---|---|
| `count(x)` | **56 / 56** | — |
| `sum(x::numeric)` | **56 / 56** | 0 |
| `sum(x::float8)` | **56 / 56** | **0** |
| `sum(x)` where `x` is `real` | **1 / 56** | 1.55 × 10⁻⁴ |
| `avg(avg(avg(x)))` | **0 / 56** | **0.1137** |

Two separate results are hiding in that table, and both matter.

**`avg(avg(x))` is wrong, confirmed empirically.** Over the full 868 day/station pairs it disagreed
with the direct average in **658** of them, maximum error 0.1137 — and in the uneven-bucket window it
was wrong in **all 56**. The claim in the plan is not merely theoretical; it is reproducible with one
join. Nothing in the aggregate hierarchy may store a finished `avg`.

**Storing `sum` and `count` is not sufficient on its own.** PostgreSQL's `sum(real)` returns `real`,
and `float4` addition is not associative, so the hierarchy's addition order differs from the direct
scan's and the two disagree at the 10⁻⁴ level. The errors are small, but they are visible — the same
query against the same data returns a different number depending on which aggregate level answered
it, which is exactly the inconsistency a frozen public API cannot have.

Widening the accumulator fixes it completely: summing `float4` inputs into a `float8` accumulator is
*exact* (a `float4` has 24 bits of mantissa; `float8` has 53, so no rounding occurs until roughly
2²⁹ readings), which is why `float8_sum_identical` is 56/56 with an error of exactly 0 rather than
merely a small one. **The registry must emit `sum(col::double precision)`.**

### Toolkit `stats_agg` is a correct alternative, and gives more

```sql
SELECT count(*) FILTER (WHERE average(h.outtemp_stats) = average(d.outtemp_stats)),
       max(abs(stddev(h.outtemp_stats) - stddev(d.outtemp_stats))),
       max(abs(num_vals(h.outtemp_stats) - num_vals(d.outtemp_stats)))
FROM weather_1d h JOIN weather_1d_direct d USING (bucket, station_id);
```

Over all **868** day/station pairs: mean **bit-identical in 868/868**, count exact, standard deviation
within **5.95 × 10⁻¹⁴**. One `stats_agg` column replaces a `sum`+`count` pair and additionally yields
variance, standard deviation and skew for free. The cost is a varlena state per bucket and a hard
dependency on `timescaledb_toolkit` (hence the `-ha` image). W4 owns the choice; both are correct.

### The circular-average pattern composes

The Go implementation's `circular_avg_state` idea — accumulate `sum(sin)`, `sum(cos)` and a count,
finalise with `atan2` — was tested through all three levels against a direct computation from raw:

| | |
|---|---|
| Pairs compared | 868 |
| Counts identical | 868 / 868 |
| Max error in the circular mean | **3 × 10⁻¹² degrees** |

The pattern carries forward exactly as the plan says. Its plain `avg` columns do not.

### What hierarchy buys (the reason for all of this)

Same data, same output, `synchronous_commit = off` throughout:

| Rebuild | Input scanned | Time |
|---|---|---|
| 1d **from 1h** | 20,832 rows | **0.037 s** |
| 1d **from raw** | 36,045,124 rows | 1.92 s |
| 1h **from 1m** | 1,249,920 rows | **0.93 s** |
| 1m **from raw** | 36,045,124 rows | 25.6 s |

Rebuilding the daily level from the hourly level instead of from raw is **52× cheaper** at 217 days of
history (0.037 s against 1.92 s), and because both sides scale linearly with retention the ratio
holds: at ten years it is roughly 0.6 s against 32 s. The equivalent comparison for the hourly level
was not measured — 1h-from-raw was not built — but it reads the same 36 M-row input as 1m-from-raw,
so the same order of magnitude applies.

---

## Q5 — Refresh cost

**Answer: 25.6 s per 13 GB of raw for the 1-minute level with `synchronous_commit = off`, 306 s with
it on. The dominant cost is neither the scan nor the aggregation — it is WAL fsync in the refresh
machinery.**

All timings from `CALL refresh_continuous_aggregate(<view>, NULL, NULL)` over the full 217 days
(36,045,124 rows, 13 GB of logical raw data, ~1 GB on disk compressed).

| Refresh | Output rows | `synchronous_commit = on` | `= off` |
|---|---|---|---|
| 1m from raw, 11 aggregates incl. `stats_agg` | 1,249,920 | **321.4 s** | — |
| 1m from raw, 4 aggregates | 1,249,920 | **306.4 s** | **25.6 s** |
| 1m from raw, 4 aggregates, 32 × 7-day calls | 1,249,920 | **299.0 s** | — |
| 1d from raw, 4 aggregates | 868 | **2.0 s** | **1.92 s** |
| 1h from 1m | 20,832 | 9.19 s | **0.93 s** |
| 1d from 1h | 868 | 0.165 s | **0.037 s** |

Normalised to the question as asked — per GB of raw:

| | Per GB of logical raw |
|---|---|
| 1-minute level, `synchronous_commit = on` | **23.6 s/GB** |
| 1-minute level, `synchronous_commit = off` | **2.0 s/GB** |
| 1-day level direct from raw | 0.15 s/GB |

The 1-day level is barely affected by `synchronous_commit` (2.0 s → 1.92 s) because it writes only 868
rows; the setting matters exactly in proportion to output cardinality.

### Where the time actually goes

Three measurements, in order of what they rule out.

**It is not the aggregation.** The identical aggregation, run as a plain query:

```sql
CREATE UNLOGGED TABLE ctas_1m AS
SELECT time_bucket(INTERVAL '1 minute', time) AS bucket, station_id,
       sum(outtemp::float8) s, count(outtemp) n, min(outtemp) mn, max(outtemp) mx
FROM weather GROUP BY 1, 2;        -- 2.4 s, 1,249,920 rows
```

**2.4 s** against the refresh's 306 s. The CAGG machinery costs ~125× the query it runs.

**It is not parallel-query loss.** The same `CREATE TABLE AS` with
`max_parallel_workers_per_gather = 0` took **2.39 s** — within noise of the parallel run. The
aggregation is 2.4 s serial.

**It is WAL fsync.** Sampling `pg_stat_activity` every 3 s through a refresh returned
`wait_event_type = IO`, `wait_event = WalSync` on 11 of 12 samples. `synchronous_commit = off`
reproduces the hypothesis: 306 s → **25.6 s**, a 12× improvement, and the remaining 25.6 s is within
an order of magnitude of the 2.4 s floor. `pg_test_fsync` on this volume reports 1351 `fdatasync`
ops/sec, so the refresh is issuing on the order of hundreds of thousands of commits.

Batching the refresh into 32 seven-day windows made no difference (299 s vs 306 s), so this is not a
"refresh in smaller ranges" problem — the commit frequency is per internal batch, not per call.

### The production-shaped case

Production's aggregates have ~397 columns, not 4. A 398-column 1-minute aggregate
(`sum`/`count`/`min`/`max` over 99 metrics), 30 days, `synchronous_commit = off`:

| | |
|---|---|
| Refresh time | **31.8 s** for 30 days |
| Output | 172,800 rows, **347 MB** |
| Extrapolated to 217 days | ~230 s |
| Extrapolated to 10 years | **~64 min** |
| Extrapolated materialisation size | ~4.2 GB/year, **~42 GB for 10 years** uncompressed |

For comparison, the 398-column aggregate at *day* granularity over the full 217 days took **108.5 s**
— so going from 4 aggregate expressions to 396 over the same scan costs ~54×. The refresh cost model
is therefore roughly:

> (raw rows scanned × number of aggregate expressions) + (output rows × commit cost)

Both terms are actionable. The first is why the 1-minute level should carry only the metrics anything
actually queries at 1-minute resolution. The second is why rebuilds run with
`synchronous_commit = off`.

Two consequences for W4:

1. The 1-minute materialisation at 398 columns is ~42 GB for a decade — **larger than the compressed
   raw hypertable it derives from**. It must either be compressed (4.58× measured, see Q6) or keep a
   bounded retention, and "aggregates are free because raw is retained" is not true of the 1-minute
   level at production width.
2. A full rebuild of all four levels for ten years is roughly an hour, not a minute. That is still
   "disposable", but it is a maintenance window, and the procedure needs to be written down.

---

## Q6 — Column ceiling headroom

**Answer: 1600 columns per table on this build, confirmed by probe. A continuous aggregate can emit
at most 1598 aggregates plus its 2 grouping columns, and dropped columns consume the budget
permanently.**

### The hard limit

```sql
-- add columns in a loop until it fails
DO $$ DECLARE i int := 0; BEGIN LOOP BEGIN
  EXECUTE format('ALTER TABLE collimit ADD COLUMN c%s float4', i);
EXCEPTION WHEN others THEN RAISE NOTICE '... %', SQLERRM; EXIT; END; i := i + 1; END LOOP; END $$;
-- NOTICE:  failed adding column #1599 (table would have 1600 cols): tables can have at most 1600 columns
-- max_columns_reached: 1600
```

**1600**, exactly as documented. There is a second, higher limit on query target lists — 1664 — which
a very wide aggregate hits with a different error message.

### The aggregate ceiling, probed precisely

| CAGG shape | Result |
|---|---|
| 1598 aggregates + `bucket` + `station_id` = 1600 columns | **created** |
| 1599 aggregates + 2 = 1601 columns | `ERROR: tables can have at most 1600 columns` |
| 1662 aggregates + 2 = 1664 columns | `ERROR: target lists can have at most 1664 entries` |

So the usable ceiling for an aggregate view is **1598 aggregate expressions**.

### Headroom against the real design

| | |
|---|---|
| Raw table as tested | 216 columns |
| Metrics available for aggregation (`real` + `integer`) | 196 |
| 196 metrics × 4 aggregates (`sum`/`count`/`min`/`max`) | 784 + 2 = **786 columns** — comfortable |
| 196 metrics × 7 aggregates | 1372 + 2 = 1374 — tight but legal |
| 196 metrics × 8 aggregates | 1568 + 2 = 1570 — within 30 columns of the wall |

Production's 397-column aggregates sit at a quarter of the ceiling. Four aggregates per metric leaves
room to roughly double the metric count; the registry should nonetheless refuse to generate a view
over 1598 expressions with a clear error rather than letting PostgreSQL produce the one above.

### Three interactions the planning did not account for

**1. Dropped columns never give their slot back.** After the Q2 add/drop experiments:

```sql
SELECT count(*) FROM information_schema.columns WHERE table_name = 'weather';          -- 216
SELECT max(attnum) FROM pg_attribute WHERE attrelid = 'weather'::regclass;             -- 239
SELECT count(*) FROM pg_attribute WHERE attrelid='weather'::regclass AND attisdropped; -- 23
```

216 live columns consuming 239 of the 1600 slots. The budget is only reclaimed by a full table
rewrite, which on a compressed multi-terabyte hypertable is not a thing one does. This is an
independent, mechanical reason for the decision that **identifiers are allocated once and never
reused**: PostgreSQL enforces it whether the registry does or not. It also means the registry must
count *allocated* identifiers against the ceiling, not live ones.

**2. Compression mirrors every column, including the dropped ones, and adds five.** Each compressed
chunk has a shadow table:

| | |
|---|---|
| `_hyper_1_10_chunk` attnums | 239 |
| `_hyper_1_10_chunk_compressed` attnums | **244** |
| Added metadata columns | `_ts_meta_count`, `_ts_meta_min_1`, `_ts_meta_max_1`, `_ts_meta_v2_first_time`, `_ts_meta_v2_last_time` |

So a compressed hypertable's effective ceiling is ~1595, and dropped columns are charged twice.

**3. A near-ceiling aggregate cannot be compressed at all.** Enabling compression on the 1600-column
aggregate:

```
WARNING:  compressed row size might exceed maximum row size
DETAIL:   Estimated row size of columnstore-enabled hypertable is 28852. This exceeds the maximum
          size of 8160 and can cause conversion of chunks to columnstore to fail.
```

Compression packs each column's batch into one row, so a wide table's compressed row overflows the
8160-byte limit long before the column count does. At production width this is still fine — the
398-column 1-minute materialisation compressed **4.58×** (347 MB → 76 MB in 2.3 s) — but it is a real
ceiling that sits well below 1600 and it binds on aggregates, not on the raw table.

**4. `count()` columns are the expensive part of a wide aggregate.** Same 30-day window, same
99 metrics, 1-minute buckets:

| Aggregate set | Columns | Refresh | Materialised |
|---|---|---|---|
| `sum`, `count`, `min`, `max` | 398 | 31.8 s | **347 MB** |
| `sum`, `nullif(count(x),0)`, `min`, `max` | 398 | 32.7 s | **234 MB** |
| `sum`, `min`, `max` | 297 | 22.4 s | 159 MB |

`count()` returns 0, never NULL, so on a 70%-NULL table all 99 count columns are fully populated and
almost entirely zeroes. `nullif(count(x), 0)` restores the NULL-bitmap saving that justifies the wide
layout in the first place — **113 MB per 30 days, ~32% of the view** — for 0.9 s of extra refresh
time. The registry should emit nullable counts.

---

## Reproducing this

Everything above runs against a throwaway container. The production database was not touched; it was
not even reachable (see Q1).

```
timescale/timescaledb-ha:pg17   # PostgreSQL 17.11 + TimescaleDB 2.31.0 + Toolkit 1.26.0
```

1. `createTableSQL` from `internal/storage/timescaledb/sql.go`, plus `station_id text NOT NULL`.
2. `create_hypertable('weather', 'time', chunk_time_interval => INTERVAL '7 days')`.
3. Load 2-second readings for 4 stations with 65 of 216 columns populated, values quantised to one
   decimal place. 217 days ≈ 36 M rows ≈ 13 GB.
4. Compress with `segmentby = station_id`, `orderby = time DESC`; `compress_chunk` every chunk.
5. Run the queries quoted in each section above.

The one piece that needs care is step 3: as Q3 shows, the value dynamics you choose determine the
compression ratio you measure, so any rerun should report its consecutive-identical rate alongside
its ratio or the number means nothing.
