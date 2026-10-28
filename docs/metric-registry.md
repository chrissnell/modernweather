# W2 — Metric registry and codegen spine

**Produces:** `metrics/registry.toml`, `metrics/snapshot.toml`, `xtask/`, and the committed
output under `generated/`.

> **Two proposed amendments to `DESIGN.md` are flagged here at the top, per the protocol in
> `docs/rewrite/README.md`. Neither is applied; this document carries on under the existing
> decisions.**
>
> **A-W2-1 — OQ-5 cannot be answered as written.** `DESIGN.md` OQ-5 recommends that "the
> 1-minute level carries only the metrics something queries at 1-minute resolution, with the
> hourly and daily levels carrying everything." That is not constructible under D-11. A level
> reads the level below it, so a metric the minute level does not carry has no source column
> for the hourly level to roll up. Level membership can only shrink *upward* — away from raw —
> and OQ-5 wants it to shrink downward. The generator refuses the non-contiguous case with an
> error rather than emitting SQL that references a column its source does not have. See
> **OQ-W2-1** for the three ways out and a recommendation.
>
> **A-W2-2 — "adding a metric is cheap" is true of the column and false of the aggregates.**
> D-9's 11.6 ms is the `ADD COLUMN`. But under D-15 an aggregate is changed by dropping,
> regenerating and refreshing it, and a new metric that any level carries forces a rebuild of
> that level and every level above it. At ten years of history D-17 puts that in the hour
> range, not the millisecond range. Both decisions are correct; the sentence that joins them
> is not. See **OQ-W2-2**.

---

## 0. What this replaces

A metric in the Go implementation is defined in nine places and nothing keeps them
consistent (the prompt counts eight; `FetchedBucketReading` is the ninth). Measured in the read-only `remoteweather` tree at `c3ec9f7`:

| Place | Size |
|---|---|
| `internal/types/weather.go` — `Reading` | 215 fields |
| `internal/storage/timescaledb/sql.go` — `createTableSQL` | 215 columns |
| the same file — the four aggregate views | 2,677 lines; **348 aggregate expressions over 144 source columns in each of the four** |
| `internal/database/types.go` — `FetchedBucketReading` | 38 fields |
| `protocols/remoteweather/remoteweather.proto` | 225 numbered fields, highest number 225, **none free below it** |
| `internal/controllers/restserver/types.go` — the REST DTO | 88 JSON tags |
| `internal/storage/grpcstream/conversion.go` | 276 lines of hand-written mapping |
| `internal/controllers/restserver/weatherapps_grpc.go` | 315 lines of hand-written mapping |

Four of those figures differ from the ones in the W2 prompt, which cites 213 columns, ~396
aggregate expressions in the 1-minute view, 133 JSON tags and 241 protobuf fields. Re-measuring
at `c3ec9f7` gives 215, 348, 88 and 225. The corrections do not change anything the prompt asks
for, but the protobuf one matters: 225 allocated with no holes is a different problem from 241
fields with gaps, and it is the reason §5 says renumbering is not an available option.

The `Reading` struct and `createTableSQL` agree exactly — 215 columns each, no column in one
and not the other — which is worth saying because it is the *database* that has only ~143 of
them (D-8). The drift is not between two files in the source; it is between the source and
production, and nothing reports it.

After this work item there is one file, `metrics/registry.toml`, and every one of those nine
places is a build artefact.

```
metrics/registry.toml ──┬─> generated/sql/020_observations.sql        table DDL + compression
   (source of truth)    ├─> generated/sql/0{30,40,50}_observations_1{m,h,d}.sql
                        ├─> generated/sql/010_functions.sql           mean + circular-mean finalisers
                        ├─> generated/sql/090_rebuild_aggregates.sql  the shape W4 executes
                        ├─> generated/proto/reading.proto
                        ├─> generated/rust/{reading,metrics,aggregates}.rs
                        ├─> generated/openapi/reading.json
                        ├─> generated/ts/{reading,metrics}.ts
                        └─> migrations/<timestamp>_<name>.sql         via `cargo xtask migrate`
metrics/snapshot.toml ──── what has ever been allocated; the only thing that makes
                           append-only enforceable
```

---

## 1. The registry format

TOML, per D-20. Three kinds of entry.

### 1.1 Metrics

```toml
[[metric]]
key = "out_temp"              # stable key — the registry's own identity, never reused
column = "outtemp"            # allocated once, never reused
sql_type = "float4"
field_number = 7              # protobuf, allocated once, never reused
proto_name = "outsideTemperature"
proto_type = "float"
label = "Temperature"         # user-facing; see §6.2
unit = "degF"                 # the unit the column is STORED in; see §6.1
tier = "core"
aggregates = ["mean", "min", "max"]
levels = ["1m", "1h", "1d"]   # per-level inclusion (OQ-5)
legacy_rest_keys = ["otemp"]  # frozen JSON key(s); plural deliberately, see §6.1
introduced_in = "0.1.0"
```

Optional: `deprecated` / `deprecated_in`, `mean_representation` (overrides the default),
`legacy_read_time_computed`, `legacy_aggregate_rest_keys`, `note`.

`key` and `column` are **two namespaces, not one.** The key is how the registry, the Rust
types, the TypeScript types and the OpenAPI schema name the metric; the column is how
PostgreSQL names it and is constrained by what the old database already calls it. The
bootstrapped registry has `out_temp`/`outtemp`, `in_humidity`/`inhumidity`,
`extra_temp1`/`extratemp1` — because the Go column names were written without separators and
there is no reason to inherit that into Rust. Both are append-only; the generator checks
uniqueness across the union of metrics and dimensions, because they land in the same table and
the same protobuf message.

### 1.2 Dimensions — the non-metric columns

**They live in the registry, as a distinct kind.** They are not metrics: they have no unit, no
tier and no aggregation strategy. But the generated types need them, the DDL needs them, the
aggregates need two of them as grouping columns, and — decisively — they consume column
identifiers and protobuf field numbers from the same namespaces as metrics. Keeping them
outside the registry would mean the budget check and the uniqueness check both had a blind
spot.

```toml
[[dimension]]
key = "station_id"
column = "station_id"
sql_type = "text"
role = "partition"
field_number = 221
```

`role` does real work:

| `role` | In raw | In aggregates | Meaning |
|---|---|---|---|
| `time` | `NOT NULL` | becomes `time_bucket(...) AS bucket` | the hypertable's time dimension |
| `partition` | `NOT NULL` | the `GROUP BY` key | `station_id` |
| `attribute` | nullable | **absent** | `stationname`, `stationtype` |

Two notes on that table. `station_id` is new: the Go table keys readings by station *name*.
Protobuf 221 was already allocated to `station_id` upstream and means the same thing, so it is
carried over rather than reallocated. And the attribute dimensions are deliberately kept out
of the aggregates — the Go views group by `stationname`, which means renaming a station splits
its buckets. With `station_id` as the key, the name is a station-registry attribute joined at
read time.

### 1.3 Levels

```toml
[[level]]
name = "1m"
bucket = "1 minute"
source = "raw"              # only the first level may say this
materialized_only = true
```

The generator validates the chain: the first level reads `raw`, every later level reads the
level immediately below it (D-11). The Go version's 5-minute level is absent; if someone wants
it back it is a `[[level]]` with `source = "1m"`.

`materialized_only` is a per-level knob rather than a generator constant because it is a
query-latency decision and W4 owns those. `true` is required of any level that is itself the
source of another.

### 1.4 The four awkward shapes

**Non-metric columns** — §1.2.

**Multi-column strategies.** One registry entry expands to one or more aggregate columns, and
the expansion is the generator's, not the author's. `mean` is the case the prompt names: with
`mean_representation = "sum_count"` it emits `<col>_sum double precision` and `<col>_count
bigint`; with `"stats_agg"` it emits one `<col>_stats`. **W4 chooses (OQ-3); both are
expressible and both are emittable today.** The switch is `[defaults].mean_representation`,
overridable per metric so that "sum-and-count for the metrics the frozen API serves,
`stats_agg` only where a dispersion statistic is wanted" — OQ-3's recommendation — needs no
generator change.

**The circular average.** A first-class strategy, `circular_mean`, not a special case bolted
on. It emits three columns — `<col>_sin_sum`, `<col>_cos_sum`, `<col>_dir_count` — which is
the pattern the spike verified composing through all three levels to within 3 × 10⁻¹² degrees
(D-13). Finalised by `mw_circular_mean()`. No column holds a finished angle, exactly as no
column holds a finished average.

The Go implementation's `circular_avg` custom aggregate is **not** carried forward as an
aggregate. Its *idea* is; its implementation accumulates into a composite type whose members
are `real`, which is H-1 in a different costume, and a custom aggregate cannot be rolled up
through a continuous-aggregate hierarchy without a matching combine function on every level.
Three plain columns do the same arithmetic in `double precision` and roll up with `sum()`.

**Text and enumerated columns.** `aggregates = []`, and the generator rejects a non-numeric
column with any aggregate at all. 18 of the 212 metrics are raw-only: the ten `extratext`
fields, `tz`, the four timestamps (`stormstart`, `sunrise`, `sunset`, `lightning_time`),
`dateutc`, and the two console forecast codes.

### 1.5 The strategy vocabulary

Five strategies, chosen so that every one of them composes.

| Strategy | Columns emitted | Raw level | Rolled up |
|---|---|---|---|
| `mean` (`sum_count`) | `_sum`, `_count` | `sum(c::double precision)`, `nullif(count(c), 0)` | `sum(c_sum)`, `nullif(sum(c_count), 0)` |
| `mean` (`stats_agg`) | `_stats` | `stats_agg(c::double precision)` | `rollup(c_stats)` |
| `min` | `_min` | `min(c)` | `min(c_min)` |
| `max` | `_max` | `max(c)` | `max(c_max)` |
| `total` | `_total` | `sum(c::double precision)` | `sum(c_total)` |
| `circular_mean` | `_sin_sum`, `_cos_sum`, `_dir_count` | `sum(sind(c::double precision))`, `sum(cosd(c::double precision))`, `nullif(count(c), 0)` | `sum(...)`, `sum(...)`, `nullif(sum(...), 0)` |

There is no `avg` and no `last`. `avg` is absent because D-12 forbids it. `last` is absent
because `last(value, time)` is not a composable state — the Go views use `max()` for the
monotonic counters (`dayrain`, `monthrain`, `yearrain`, `stormrain`, the ET totals, `gdd`) and
so does the registry, which is correct for a counter that only rises within its bucket.

---

## 2. The committed snapshot, and what is fatal

The registry alone cannot enforce append-only: a human editing TOML can rename anything. The
generator enforces it by diffing the registry against `metrics/snapshot.toml`, which is written
only by `cargo xtask migrate` and is the record of every identifier that has ever been
allocated.

### 2.1 Fatal — no flag

There is no override because there is no correct way to proceed. The identifier is already in a
deployed protobuf or already consumed as a PostgreSQL attribute number.

1. **A stable key, column name or protobuf field number allocated to one metric appearing on
   another** — checked both within the registry and against the snapshot.
2. **An identifier disappearing from the registry** without `deprecated = true`.
3. **A column budget over the ceiling**, counted against allocated identifiers rather than live
   ones (H-4).
4. **An aggregate that would emit more expressions than the ceiling permits** — 1598.

Plus the structural checks that would otherwise produce broken SQL: a level chain that skips a
level, a metric carried at `1h` but not at `1m` (§A-W2-1), an aggregate on a `text` column,
`mean` and `total` on the same metric (both emit a sum), a protobuf number in the reserved
19000–19999 range, a deprecated metric with no `deprecated_in`, and one legacy JSON key
claimed by two metrics.

### 2.2 Refused by default — one loud flag

Column rename, SQL type change in either direction, protobuf type change, aggregation strategy
change, mean-representation change, and un-deprecating a retired metric. Each of these is a
real operation an operator may genuinely need, so each is refused rather than impossible.

**The escape hatch is `cargo xtask migrate --allow-breaking`, and what it requires of the
operator is that they read the migration.** The flag does not apply the change; it writes the
destructive statements into a migration whose header names every one of them, and leaves the
decision to run it where it belongs. Concretely:

- a **column rename** is emitted as `ALTER TABLE ... RENAME COLUMN`. The attribute number stays
  where it is, the old name is never reusable, and every generated artefact changes with it.
- a **SQL type change** is emitted as `ALTER COLUMN ... TYPE`, with a comment saying that on a
  compressed hypertable this requires decompressing every chunk holding the column and
  recompressing afterwards, and that it should not be run against a live API.
- a **protobuf type change** produces no SQL and a prominent comment, because the damage is
  entirely on the client side.
- an **aggregation strategy change** produces a drop-and-recreate of every affected level.

`cargo xtask codegen` has **no** such flag. Generated output must always match the snapshot, so
a registry the snapshot refuses cannot be rendered into the tree at all.

---

## 3. `cargo xtask codegen`

```
cargo xtask codegen            regenerate generated/
cargo xtask codegen --check    regenerate into a temporary directory and diff; exit 1 on drift
cargo xtask migrate            diff registry against snapshot, emit a migration, update snapshot
cargo xtask validate           run every invariant and exit
cargo xtask budget             print the column budget against the ceiling
```

Exit codes, which is what W8 wires into the gate: **0** success or no drift, **1** `--check`
found drift, **2** an invariant was violated or usage was wrong.

Thirteen files land under `generated/`, each carrying a do-not-edit header naming the registry,
the snapshot and the generator (R-2):

| Target | File | Notes |
|---|---|---|
| support functions | `sql/010_functions.sql` | `mw_mean`, `mw_circular_mean` — the only places a mean or an angle is computed |
| table DDL | `sql/020_observations.sql` | nullable metric columns, hypertable, compression per D-4/D-7, **no retention policy** per D-4 |
| aggregates | `sql/030_observations_1m.sql`, `040_..._1h.sql`, `050_..._1d.sql` | one template × registry × level |
| rebuild | `sql/090_rebuild_aggregates.sql` | bottom-up refresh with `synchronous_commit = off`; the shape W4 executes |
| Rust types | `rust/reading.rs` | the reading type and a `get(MetricKey) -> Option<f64>` accessor, replacing the 215-field struct |
| Rust metadata | `rust/metrics.rs` | `MetricKey`, and the key/column/label/unit/tier/field-number/legacy-key table |
| Rust aggregates | `rust/aggregates.rs` | what each level actually holds, for W3's query layer and W5's charts |
| protobuf | `proto/reading.proto` | numbers from the registry; deprecated metrics become `reserved` |
| OpenAPI | `openapi/reading.json` | `Reading` by stable key, `LegacyWeatherReading` by frozen JSON key |
| TypeScript | `ts/reading.ts`, `ts/metrics.ts` | the interface, and the label/unit table the management UI renders from |

`--check` regenerates into `$TMPDIR/mw-codegen-check-<pid>` and compares; it never writes into
the working tree, because a check that fixes the problem it is checking for always passes
(R-2). It reports the first differing line of each file, and also reports a file present in
the tree that the generator does not produce.

**The generated Rust is compiled, not merely written.** `generated/rust/metrics.rs` and
`generated/rust/aggregates.rs` depend on nothing outside themselves, so `xtask`'s own test
suite `include!`s them in a test module. That turned up two real generator bugs during
development — a level named `1m` produced `const 1M_COLUMNS`, which is not a Rust identifier,
and module-level `//!` doc comments are illegal in a macro-expanded item list. Both would
otherwise have landed on W3. `generated/rust/reading.rs` is not compiled here because it needs
`chrono` and W3 owns which crate the application types live in (R-4).

Five of `xtask`'s eighteen tests assert properties of the output rather than of the input:
that no aggregate column name ends in `_avg` or `_mean`, that every level but the first reads
the level below it, that every `mean` sum column is `double precision`, that the metadata table
matches the registry entry for entry, and that the emitted aggregate SQL contains no `avg(`, no
unwidened `sum(` and no `count(` outside a `nullif(` — scanning the generated text rather than
the model, because the model being right and the template being right are two different claims.

---

## 4. The migration generator

`cargo xtask migrate` diffs the registry against the snapshot, classifies every difference, and
emits one timestamped `sqlx` migration plus an updated snapshot. **No DDL at startup, ever**
(D-8) — and no `IF NOT EXISTS`, which is the idiom that made the Go version's drift silent.

| Class | What it emits |
|---|---|
| `add nullable column` | `ALTER TABLE ... ADD COLUMN`, all of them in one statement, inside a retry block |
| `deprecate` | the metric leaves the aggregates; the column and its identifiers stay |
| `rebuild aggregates` | drop top-down, recreate bottom-up, `WITH NO DATA` |
| `BREAKING` | §2.2, only with `--allow-breaking` |

### 4.1 The retry, and why it is in the migration

D-9's measurement is 11.6 ms against ~1 GB of compressed chunks, 93 ms for twenty columns in
one statement, nothing decompressed. The caveat is the lock: `AccessExclusiveLock` on 63
relations — the hypertable, every chunk and every compressed mirror — for the ~12 ms it runs.
Instant, but it queues behind long-running readers and blocks everything while it waits.

So the generated migration does this, rather than a document saying it should:

```sql
SET LOCAL lock_timeout = '3s';

DO $migration$
DECLARE
    attempt int := 0;
BEGIN
    LOOP
        BEGIN
            ALTER TABLE observations
                ADD COLUMN lightning_energy float4 NULL;
            EXIT;
        EXCEPTION WHEN lock_not_available THEN
            attempt := attempt + 1;
            IF attempt >= 10 THEN
                RAISE;
            END IF;
            RAISE NOTICE 'lock_timeout on attempt %, retrying', attempt;
            PERFORM pg_sleep(attempt);
        END;
    END LOOP;
END $migration$;
```

The `BEGIN ... EXCEPTION` block is a subtransaction, so a `lock_not_available` rolls back only
the failed `ALTER` and releases whatever it had acquired before sleeping. Ten attempts with
linear backoff, then it gives up and raises rather than holding the API hostage.

### 4.2 Aggregate changes are not migrations in the usual sense

Aggregates are disposable derived data (D-15), so the generator emits drop-and-recreate and
never an in-place alter. The drops are top-down and the creates bottom-up, because each level
reads the level below it.

The **refresh** is deliberately not in the migration. `refresh_continuous_aggregate` cannot run
inside a transaction and `sqlx` wraps a migration in one, and D-17 requires the session to
relax commit durability — 306 s against 25.6 s on the measured rebuild, the cost being WAL
fsync rather than CPU. So the migration recreates the views `WITH NO DATA` and
`generated/sql/090_rebuild_aggregates.sql` is the script W4's rebuild procedure runs:

```sql
SET synchronous_commit = off;

CALL refresh_continuous_aggregate('observations_1m', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1h', NULL, NULL);
CALL refresh_continuous_aggregate('observations_1d', NULL, NULL);
```

Bottom-up, for the same reason the creates are: refreshing top-down would materialise an upper
level from a stale lower one. **W4 owns the procedure** — whether it runs per level or per
time range, how it is supervised, and what it does about the hour-scale cost at ten years of
history. This file is only the shape.

---

## 5. Bootstrapping, and the ~72-column gap

The registry ships populated from the **Go source definition**: 215 columns, of which 3 are
dimensions (`time`, `stationname`, `stationtype`), plus `station_id` which is new. **212
metrics and 4 dimensions, 216 allocated columns.**

| | |
|---|---|
| `float4` | 127 |
| `int4` | 69 |
| `text` | 11 |
| `timestamptz` | 4 |
| `int8` | 1 |
| core tier / extended tier | 87 / 125 |
| aggregated / raw-only | 194 / 18 |
| carrying at least one legacy REST key | 70 |

Strategy usage across the 194 aggregated metrics: `max` 191, `min` 178, `mean` 127,
`circular_mean` 1 (`winddir`), `total` 1 (`rainincremental`).

**The ~72-column gap to production is recorded, not resolved.** The Go source defines 215
columns and production has ~143 (D-8, OQ-6). The registry is populated from the source because
it is the superset, because it is readable from the checkout, and because production is not
reachable from an agent runtime at all (OQ-1). **No column was dropped to make the numbers
agree.** A column the code writes and the database does not have is a bug in the old system,
and the registry is where it becomes visible rather than where it gets quietly tidied away.

**W9 owns the reconciliation** (OQ-6): read the actual production column list, diff it against
`metrics/registry.toml`, and report which of the 212 metrics have never had a database column
behind them. That report is a prerequisite for the data migration, not for this work item.

One more thing the bootstrap found, which is not the same gap. **The protobuf has no free field
numbers below its maximum.** All 225 numbers from 1 to 225 are allocated, with no holes — the
damage D-21 describes (station metadata at 188–192, relays 8–10 at 193–195) is not a sparse
numbering with gaps to reclaim; it is a dense numbering that was simply filled in the wrong
order. The next metric gets 226, and that is fine. It is worth recording because "we can tidy
the numbering later" is not an option that exists.

### 5.1 Labels, units and strategies: how they were assigned

Mechanically, from the column name, with an override table for the singletons. Twenty numbered
sensor families (`extratemp{n}`, `soilhum{n}`, `batleak{n}`, …) got templated labels and a
family unit; the ~80 one-off columns got hand-written labels. Stable keys are
`snake_case` of the Go field name, which is why they differ from the column names.

Where the Go views already express an aggregation intent, the registry follows it, with two
deliberate departures:

- **`lightning_day` and `lightning_hour` use `max`, not `sum`.** The Go views emit
  `sum(lightning_day)`, which double-counts: these are counters the station resets daily and
  hourly, so summing them across a bucket adds the same strikes repeatedly. Neither column is
  on the frozen REST surface (`internal/controllers/restserver/types.go` has no lightning
  fields), so there is no contract risk in getting it right. Recorded rather than silent.
- **Enumerated and state columns get `min`/`max` and never `mean`.** The arithmetic mean of a
  relay state or an alarm code is not a number anybody wants. 55 metrics are in this shape.

`solarjoules` keeps the Go view's `mean`/`min`/`max` even though a joule total would arguably
be `total`; see **OQ-W2-3**.

---

## 6. Where this meets the frozen contract and the UI

### 6.1 The legacy REST keys

**Recorded, not fixed.** The registry carries `legacy_rest_keys` as a list, and the list is
plural because the shipped surface is. Both duplications in the Go DTO are captured:

| Measurement | Column | Frozen JSON keys |
|---|---|---|
| outdoor humidity | `outhumidity` | `ohum`, `outhumidity` — both assigned from `r.OutHumidity` at `transform.go:44` and `:52` |
| rainfall today | `dayrain` | `dayrain`, `rainday` — both assigned from `r.DayRain` |

The inconsistency is likewise recorded and not touched: `otemp`, `bar`, `winds`, `windd`,
`windch`, `heatidx`, `itemp`, `ihum` are abbreviated and run-together while `period_rain` is
snake_case and `rainincremental` and `potentialsolarwatts` are neither. 70 metrics carry at
least one legacy key; 76 keys in total are generated into the `LegacyWeatherReading` schema.

Two seams the registry records explicitly because they are not simple renames:

**`legacy_read_time_computed`.** Four keys are computed by the frozen handler rather than read
from their column: `windch` and `heatidx` are recalculated from `OutTemp` plus wind speed or
humidity on every request (`transform.go:18`, `:21`), and `aqi_pm25_aqin` / `aqi_pm10_aqin`
read the column only when it is greater than zero and compute otherwise. The flag says "the
column and the JSON key are not guaranteed to be the same number"; **W1 owns which one the
contract actually promises.**

**`legacy_aggregate_rest_keys`.** The frozen `/span` surface calls the bucketed rainfall total
`period_rain`, not `rainincremental_total`. The convention the generator applies is that a
legacy key names the metric's *mean* at aggregate levels and its raw value at raw level; this
is the one exception, and it is expressed as data rather than hard-coded.

What the registry deliberately does **not** record: the unit conversions. `snowdistance` and
`snowdepth` are stored in millimetres and served in inches through `mmToInches` with a 0.1-inch
noise threshold (`restserver/types.go:190`). `unit` in the registry is the unit the *column*
holds. Making the conversion generatable is **OQ-W2-4**; until then it belongs to W1's contract
and the compat layer.

**Where W1's answer is needed and was not available:** the key lists here are read out of the
Go DTO and `transform.go`, which is evidence about the current implementation rather than about
the contract. If W1's capture finds a key this document missed, or finds that one of the four
computed values is contractually the column rather than the recomputation, the registry needs
the correction and the generated `LegacyWeatherReading` schema changes with it. Stated as an
assumption in the closing section so it is greppable.

### 6.2 Labels are user-facing

`label` is what the UI renders, and it is written for someone who has never seen the schema:
`outtemp` → "Temperature", `barometer` → "Pressure", `soilhum3` → "Soil humidity 3". **W5 §0
owns the internal-to-user-facing vocabulary translation table and is authoritative where the
two disagree** (D-36). The generated `ts/metrics.ts` carries the label/unit table and says so
in its header; if W5 decides "Pressure" should read "Barometric pressure", the change lands in
the registry and regenerates.

The 25 `unit` values are a closed vocabulary (`degF`, `percent`, `inHg`, `mph`, `deg`, `in`,
`in_per_hour`, `W_per_m2`, `J_per_m2`, `mm`, `mi`, `ppm`, `ug_per_m3`, `aqi`, `index`, `cbar`,
`V`, `count`, `degree_days`, `state`, `code`, `text`, `timestamp`, `ms_epoch`, `unitless`).
They are not currently validated against a list; see **OQ-W2-5**.

---

## 7. The column budget

Counted against **allocated** identifiers, not live ones (H-4). A dropped PostgreSQL column
keeps its attribute number forever — the spike's table had 216 live columns occupying 239 of
1600 slots — and a compressed hypertable's shadow table mirrors every column including the
dropped ones and adds five of its own, so a dropped column is charged twice.

```
$ cargo xtask budget
Column budget — counted against allocated identifiers, not live ones.
A dropped PostgreSQL column keeps its attribute number forever, and a
compressed hypertable's shadow table mirrors every column including the
dropped ones and adds 5 of its own.

  raw hypertable
    allocated identifiers      216
    live columns               216
    effective ceiling         1595  (PostgreSQL's 1600 minus 5 compression metadata columns)
    headroom                  1379

  aggregates                metrics  expressions  ceiling  headroom
    observations_1m            194          627     1598       971
    observations_1h            194          627     1598       971
    observations_1d            194          627     1598       971
```

**216 of 1,595 on the raw table and 627 of 1,598 on each aggregate.** Allocated equals live
today because nothing has been deprecated yet; the two numbers diverge the first time something
is, and the ceiling is checked against the larger one.

The aggregate figure is the one with less room than it looks. 627 expressions over 194 metrics
is 3.2 columns per metric, so the headroom is not "roughly triple the metric count" — it is
roughly 300 more metrics at the current strategy mix, or the same metrics with two more
strategies each. Switching `mean` from `sum_count` to `stats_agg` (OQ-3) would *reduce* the
1-minute level from 627 to 500 expressions, which is a point in `stats_agg`'s favour that D-13
mentions and OQ-3 does not quantify.

Neither ceiling is discovered at deploy time. Both are generator errors:

```
FATAL: aggregate '1m' would emit 1600 aggregate expressions, over the ceiling of 1598.
       A continuous aggregate's two grouping columns also occupy slots, so 1598 + 2 = 1600 is
       the widest view PostgreSQL will create. Reduce the metrics carried at this level or
       their aggregation strategies.

FATAL: the raw hypertable would occupy 1602 allocated columns, over the effective ceiling of 1595.
       PostgreSQL's hard limit is 1600 columns, and a compressed hypertable's shadow table
       mirrors every column — dropped ones included — and adds 5 of its own. Dropped columns
       are charged twice and are reclaimed only by a full table rewrite.
```

Both are asserted by tests (`an_over_budget_aggregate_is_fatal_before_postgresql_ever_sees_it`,
`an_over_budget_raw_table_is_fatal`), so neither error is one nobody has seen fire.

The spike's fourth interaction — a near-ceiling aggregate cannot be compressed at all, because
compression packs each column's batch into one row and the 1600-column view's estimated
compressed row was 28,852 bytes against a maximum of 8,160 — binds well below the column count
and is **W4's**, since W4 decides whether the minute level is compressed (OQ-4). At 627
expressions the measured 398-column case (4.58×) is the relevant data point, not the 1600-column
failure.

---

## 8. Demonstrations

Everything below was run against the committed tree.

### 8.1 The hazards, in the generated SQL

H-1 (widened sum), H-2 (nulled count) and H-3 (hierarchical, no finished average) in
`generated/sql/030_observations_1m.sql`, reading raw:

```sql
CREATE MATERIALIZED VIEW observations_1m
WITH (timescaledb.continuous, timescaledb.materialized_only = true) AS
SELECT
    time_bucket(INTERVAL '1 minute', time) AS bucket,
    station_id,
    -- Temperature [out_temp]
    sum(outtemp::double precision) AS outtemp_sum,
    nullif(count(outtemp), 0) AS outtemp_count,
    min(outtemp) AS outtemp_min,
    max(outtemp) AS outtemp_max,
```

and in `generated/sql/040_observations_1h.sql`, reading the level below it rather than raw:

```sql
CREATE MATERIALIZED VIEW observations_1h
WITH (timescaledb.continuous, timescaledb.materialized_only = true) AS
SELECT
    time_bucket(INTERVAL '1 hour', bucket) AS bucket,
    station_id,
    -- Temperature [out_temp]
    sum(outtemp_sum) AS outtemp_sum,
    nullif(sum(outtemp_count), 0) AS outtemp_count,
    min(outtemp_min) AS outtemp_min,
    max(outtemp_max) AS outtemp_max,
...
FROM observations_1m
GROUP BY 1, 2
WITH NO DATA;
```

The circular mean, raw and rolled up:

```sql
-- 030_observations_1m.sql
    -- Wind direction [wind_dir]
    sum(sind(winddir::double precision)) AS winddir_sin_sum,
    sum(cosd(winddir::double precision)) AS winddir_cos_sum,
    nullif(count(winddir), 0) AS winddir_dir_count,

-- 040_observations_1h.sql
    -- Wind direction [wind_dir]
    sum(winddir_sin_sum) AS winddir_sin_sum,
    sum(winddir_cos_sum) AS winddir_cos_sum,
    nullif(sum(winddir_dir_count), 0) AS winddir_dir_count,
```

The rainfall total, which is the `total` strategy and the one metric with a legacy aggregate
key:

```sql
-- 030_observations_1m.sql
    sum(rainincremental::double precision) AS rainincremental_total,
-- 040_observations_1h.sql
    sum(rainincremental_total) AS rainincremental_total,
```

No non-comment line of the generated aggregate SQL contains `avg(`, a `sum(` that is neither
widened nor a roll-up of a widened accumulator, or a `count(` outside a `nullif(` — asserted by
`the_generated_aggregate_sql_contains_no_naive_form`, which scans the emitted text rather than
the model. Verified by mutation: replacing one `sum(c::double precision)` with `sum(c)` in the
generator makes it fail with
`generated/sql/030_observations_1m.sql:36: a sum that is neither widened nor a roll-up (D-14): sum(barometer) AS barometer_sum,`.

### 8.2 `--check` passes clean and fails on one character

```
$ cargo xtask codegen --check
codegen --check: 13 generated files match metrics/registry.toml
$ echo $?
0

$ sed -i 's/nullif(count(outtemp), 0)/nullif(count(outtemp), 1)/' generated/sql/030_observations_1m.sql
$ cargo xtask codegen --check
codegen --check: generated output does not match metrics/registry.toml
  differs: generated/sql/030_observations_1m.sql
      line 52:
        in the tree:     nullif(count(outtemp), 1) AS outtemp_count,
        generated:       nullif(count(outtemp), 0) AS outtemp_count,

Generated files are build artefacts. Edit metrics/registry.toml and run
`cargo xtask codegen`, then commit both. Regenerated into /tmp/mw-codegen-check-3636 for
the diff; the working tree was not touched.
$ echo $?
1

$ grep -n outtemp_count generated/sql/030_observations_1m.sql | head -1
52:    nullif(count(outtemp), 1) AS outtemp_count,      # the check did not repair it
```

That last line is the point of regenerating into a temporary directory: the edit is still there
after the check ran.

### 8.3 Each fatal invariant, with its message

**A reused protobuf field number, inside the registry.** `out_temp` given barometer's 4:

```
$ cargo xtask codegen
FATAL: protobuf field number 4 is allocated to two registry entries: 'barometer' and 'out_temp'.
       Identifiers are allocated once and never reused (D-21). Allocate a new one for 'out_temp'.
$ echo $?
2
```

**A reused column name, inside the registry.** `out_temp` given `intemp`:

```
$ cargo xtask codegen
FATAL: column name 'intemp' is allocated to two registry entries: 'in_temp' and 'out_temp'.
       Identifiers are allocated once and never reused (D-21). Allocate a new one for 'out_temp'.
$ echo $?
2
```

**An identifier moved to a different metric, caught against the snapshot.** Field numbers 4 and
5 swapped between `barometer` and `in_temp` — legal within the file, fatal against the
allocation record:

```
$ cargo xtask codegen
FATAL: protobuf field number 4 was allocated to 'barometer' and now appears on 'in_temp'.
       Reusing a field number silently reinterprets historical data on every client that has
       not been rebuilt (D-21). Allocate the next free number instead.
FATAL: protobuf field number 5 was allocated to 'in_temp' and now appears on 'barometer'.
       Reusing a field number silently reinterprets historical data on every client that has
       not been rebuilt (D-21). Allocate the next free number instead.
$ echo $?
2
```

**An identifier deleted without deprecation.** The `snow_depth` entry removed:

```
$ cargo xtask codegen
FATAL: 'snow_depth' (column 'snowdepth', protobuf field 83) is in the snapshot and gone from
       the registry.
       Removing a metric does not delete its entry: set deprecated = true and
       deprecated_in = "<version>" so the identifiers stay allocated (D-21).
$ echo $?
2
```

**The same metric deprecated instead of deleted** — allowed, identifiers retained:

```
$ cargo xtask validate
validate: invariants hold; 1 pending change(s):
  [deprecate] snow_depth: snowdepth deprecated; identifiers retained
$ echo $?
0
```

### 8.4 A breaking change, refused

`barometer` retyped from `float4` to `int4`:

```
$ cargo xtask migrate
REFUSED: the registry asks for changes that destroy data or break a shipped contract.
  - barometer: SQL type changed float4 -> int4

Each of these is a real operation, not a mistake, so there is one flag:

    cargo xtask migrate --allow-breaking

It will emit the migration with the destructive statements written out and a header naming
every one of them. Read the migration before you run it: a column rename abandons the old
column's data where it is, a SQL type change rewrites every chunk, and a protobuf type change
breaks every client that has not been rebuilt. `codegen` has no such flag — generated output
must always match the snapshot.
$ echo $?
2
```

### 8.5 Adding a metric

A new `lightning_energy float4` on field 226:

```
$ cargo xtask migrate --dry-run
migrate: 1 change(s)
  [add nullable column] lightning_energy: new metric lightning_energy (lightning_energy float4),
                        protobuf field 226

--dry-run: would write migrations/20261028170150_add_lightning_energy.sql
```

The emitted migration is the retry block from §4.1 followed by a drop-and-recreate of all three
aggregate levels, ending `-- 628 aggregate expressions over 195 metrics`. The three-level
rebuild for one added column is what **A-W2-2** is about.

### 8.6 The gate W8 inherits

```
$ cargo fmt --all --check && cargo clippy -p xtask --all-targets && cargo test -p xtask && \
  cargo xtask codegen --check
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
codegen --check: 13 generated files match metrics/registry.toml
```

Zero clippy warnings, formatted, eighteen passing tests. **W8 owns the workflow; this is the
command.**

---

# Decisions

1. The registry is `metrics/registry.toml`, authored in TOML, and is the single source of truth for every metric, dimension and aggregate level.
2. `key`, `column` and `field_number` are three append-only namespaces, and the generator checks uniqueness across the union of metrics and dimensions because they share a table and a protobuf message.
3. A metric's stable key is distinct from its column name: the key names the metric in Rust, TypeScript and OpenAPI, the column names it in PostgreSQL, and the bootstrapped registry deliberately differs between the two (`out_temp` / `outtemp`).
4. Non-metric columns live in the registry as `[[dimension]]` entries, not outside it, because they consume identifiers from the same namespaces and must be counted against the same budget.
5. A dimension's `role` is `time`, `partition` or `attribute`; attribute dimensions (`stationname`, `stationtype`) are present in the raw table and the reading types and **absent from the aggregates**, which also fixes the Go views' station-rename bucket split.
6. `station_id` is a new `NOT NULL` partition column and the aggregates group by `(bucket, station_id)` only; protobuf 221 is carried over from the Go proto because it already meant `station_id`.
7. Aggregate levels are registry data (`[[level]]`), and the generator validates that the first reads `raw` and every later level reads the level immediately below it.
8. Level membership must be contiguous from the first level; a metric carried at `1h` but not `1m` is a fatal error, because there would be no source column to roll up. **This is why OQ-5 cannot be answered as `DESIGN.md` states it — see A-W2-1 and OQ-W2-1.**
9. One registry entry expands to one or more aggregate columns, and the expansion belongs to the generator; `mean` is `_sum` + `_count` or one `_stats`, selected by `mean_representation` at the registry or per-metric level, so **W4 decides OQ-3 without a generator change**.
10. `circular_mean` is a first-class strategy emitting `_sin_sum`, `_cos_sum` and `_dir_count` in `double precision`; the Go `circular_avg` custom aggregate's idea carries forward, its `real`-typed composite-state implementation does not.
11. The strategy vocabulary is `mean`, `min`, `max`, `total`, `circular_mean`. There is no `avg` (D-12 forbids it) and no `last` (it is not a composable state).
12. H-1, H-2 and H-3 are properties of the generator's templates, not of the caller: there is one place that writes `sum(c::double precision)`, one that writes `nullif(count(c), 0)`, and one that chooses between the raw and the roll-up form.
13. Means and angles are finalised at query time by `mw_mean()` and `mw_circular_mean()`; no stored column holds a finished average or a finished angle.
14. `metrics/snapshot.toml` is the allocation record, written only by `cargo xtask migrate`, and is what makes append-only enforceable rather than merely documented.
15. Four conditions are fatal with no override: an identifier appearing on a different entry, an identifier disappearing without deprecation, the raw column budget over 1,595 allocated identifiers, and an aggregate over 1,598 expressions.
16. Column rename, SQL type change, protobuf type change, aggregation strategy change, mean-representation change and un-deprecation are refused by default and permitted only by `cargo xtask migrate --allow-breaking`, which writes the destructive statements out with a header naming each one. `codegen` has no such flag.
17. Generated output is thirteen committed files under `generated/`, each with a do-not-edit header naming the registry, the snapshot and the generator.
18. `cargo xtask codegen --check` regenerates into `$TMPDIR` and diffs; exit 0 clean, 1 on drift, 2 on an invariant violation. **W8 owns the gate; this is the command and these are the codes.**
19. The generated Rust metadata and aggregate tables are compiled by `xtask`'s own test suite, so "the generator emits Rust" is verified rather than assumed.
20. Migrations come only from `cargo xtask migrate`; there is no DDL at startup and no `IF NOT EXISTS` anywhere in the generated output.
21. A generated metric migration sets `lock_timeout` and retries with backoff inside a subtransaction, so it fails fast rather than stalling the API behind 63 `AccessExclusiveLock`s.
22. Aggregate changes are emitted as drop-and-recreate `WITH NO DATA`, dropped top-down and created bottom-up; the refresh is a separate non-transactional script with `synchronous_commit = off`, because `refresh_continuous_aggregate` cannot run inside a transaction and D-17 requires relaxed durability.
23. The registry ships populated from the Go source definition: 212 metrics, 4 dimensions, 216 allocated columns. Nothing was dropped to make it agree with production.
24. The ~72-column gap between the source definition and the production table is recorded here and owned by W9 (OQ-6).
25. The Go protobuf has **no free field numbers below its maximum** — all of 1–225 are allocated with no holes — so the next metric gets 226 and renumbering is not an available option.
26. `lightning_day` and `lightning_hour` use `max` rather than the Go views' `sum`, which double-counts a counter the station resets; neither is on the frozen REST surface.
27. Enumerated and state columns (55 metrics) get `min`/`max` and never `mean`.
28. `legacy_rest_keys` is a list, and both of the shipped surface's duplications are recorded as data: `outhumidity` → `["ohum", "outhumidity"]` and `dayrain` → `["dayrain", "rainday"]`. The quirks are recorded and not fixed.
29. `legacy_read_time_computed` marks the four keys the frozen handler computes rather than reads (`windch`, `heatidx`, `aqi_pm25_aqin`, `aqi_pm10_aqin`); W1 owns which value the contract promises.
30. `legacy_aggregate_rest_keys` expresses the one legacy key that names an aggregate rather than a raw value (`rainincremental.total` → `period_rain`); the convention otherwise is that a legacy key names the mean at aggregate levels.
31. `unit` is the unit the **column** holds, not the unit the frozen API serves; the `mm` → inch conversions on the snow metrics belong to W1's contract and the compat layer (OQ-W2-4).
32. `label` is user-facing and written for a reader who has never seen the schema; **W5 §0 is authoritative** where its vocabulary table and these labels disagree (D-36).
33. The column budget is counted against allocated identifiers and reported by `cargo xtask budget`: **216 of 1,595** on the raw table and **627 of 1,598** on each aggregate.
34. `xtask` lives in a minimal root workspace created by this work item, holding nothing but `xtask`. **W3 §1 (R-4) owns the real layout and is expected to restructure around it.**

---

# Open questions

**OQ-W2-1. OQ-5 is not constructible as stated. Which of three replacements?**

A level reads the level below it (D-11), so level membership can only shrink away from raw.
OQ-5 wants the opposite — a narrow minute level under wide hourly and daily levels — and that
has no source column to roll up. The generator refuses it; see A-W2-1.

- **(a) Contiguous membership, minute level carries everything.** What ships today: 627
  expressions at every level. The minute level's cost is managed by compression, which is
  OQ-4's recommendation anyway (~9 GB for a decade compressed, against ~42 GB uncompressed).
  Costs nothing new, decides nothing new.
- **(b) Two sources at the hour level.** `observations_1h` reads `observations_1m` for the
  metrics the minute level carries, and a second aggregate reads raw for the rest. Gets OQ-5's
  saving and reintroduces a raw scan for part of the rebuild. Needs a new registry concept
  (per-metric source level) and roughly doubles the hour level's definition.
- **(c) Shrink upward instead.** Carry everything at `1m`, drop the long tail at `1h` and
  `1d`. Contiguous, so it works today with no generator change — but it saves nothing where
  OQ-5 wanted the saving and loses history at the levels that are cheap to keep.

*Recommendation:* **(a).** The minute level is the expensive one, and compressing it (OQ-4's
own recommendation) addresses that directly without a structural exception. OQ-5's premise —
that the minute level is the thing to narrow — is right; its mechanism is the one that does not
work. **W4 decides**, and if it picks (b) that is a W2 follow-up, not a W4 workaround.

**OQ-W2-2. What is the real cost of adding a metric, end to end?**

Adding a column is 11.6 ms (D-9). Adding a metric that any level carries also forces a
drop-and-recreate of that level and every level above it, and at ten years of history D-17 puts
a full rebuild in the hour range. The generated migration today rebuilds all three levels for
one added column — correct under D-15, and not what "adding a metric is cheap" suggests.

*Recommendation:* keep the behaviour and fix the claim. Specifically: (i) W4's rebuild procedure
should support refreshing a bounded time range first and backfilling older ranges as a
background operation, so that a new metric is queryable at recent resolution in seconds rather
than hours; (ii) `DESIGN.md` should carry A-W2-2 so nobody plans around the millisecond figure.
**W4 owns (i); the owner owns (ii).**

**OQ-W2-3. Is `solarjoules` a mean or a total?**

The Go views average it. A joule figure that accumulates per reading should be summed over a
bucket; one that is already an instantaneous rate scaled to a period should be averaged.
Determining which requires reading `pkg/solar`, which is W7's territory.

*Recommendation:* leave it as `mean`/`min`/`max`, matching current behaviour, and have **W7**
confirm against `pkg/solar` when it specifies the collector. One line in the registry if it
turns out to be a total. The cost of being wrong is a wrong number in a chart, not a schema
change.

**OQ-W2-4. Should the registry carry the legacy unit conversion?**

`snowdistance` and `snowdepth` are stored in millimetres and served in inches with a 0.1-inch
noise threshold. Right now `unit` records the stored unit and the conversion lives nowhere
generated, which means someone hand-writes it — exactly the 591 lines this work item exists to
retire.

*Recommendation:* add `legacy_unit` and `legacy_scale` to the metric schema once **W1** has
confirmed the exact behaviour, including the threshold and the rounding, and generate the
conversion into the compat layer. Not done speculatively: a conversion generated from a guess
is worse than one written by hand from the contract. **W1 first, then a small W2 follow-up.**

**OQ-W2-5. Should `unit` be a closed enumeration in the generator?**

25 unit strings are in use and nothing validates them, so a typo (`degf` for `degF`) reaches
the UI as a rendered label.

*Recommendation:* yes, as a `[[unit]]` table in the registry with a display string and a
dimension, validated by the generator. It is ten lines of generator and it belongs with W5 §0's
vocabulary work rather than being guessed here. **Low priority, W5-adjacent.**

**OQ-W2-6. Who owns the `[[dimension]]` `attribute` columns in the long run?**

`stationname` and `stationtype` are in the raw table because the Go table has them, and they
are denormalised duplicates of station-registry data now that `station_id` exists. They cost
two columns and two text values per row.

*Recommendation:* **W4** should decide whether to drop them from the raw table entirely and
join the station registry at read time. They are already excluded from the aggregates, so the
change is contained. Left in for now because removing a column from the bootstrapped registry
would mean deprecating something that has never existed.

---

# Assumptions about other work items

**About W1 (frozen contract)**

1. The legacy JSON key lists in this registry, read out of `restserver/types.go` and
   `transform.go`, match the contract W1 captures. If W1 finds a key this missed, the registry
   needs it and `generated/openapi/reading.json` changes.
2. `ohum`/`outhumidity` and `dayrain`/`rainday` are genuinely duplicated in the contract and
   both must keep working, rather than one of each pair being vestigial.
3. `windch`, `heatidx`, `aqi_pm25_aqin` and `aqi_pm10_aqin` are contractually the handler's
   computed value, not the stored column. The registry marks the seam; W1 states which side the
   contract is on.
4. The `mm` → inch conversion on the snow metrics, with its 0.1-inch threshold, is part of the
   contract W1 captures and not an implementation detail that may be changed (OQ-W2-4).
5. The derived values that are not metrics — `rainfall24h`/`48h`/`72h`, `rainfallstorm`,
   `windgust`, `windcard`, the formatted `sunrise`/`sunset` strings, the four `moon*` fields,
   and everything in `/snow` — are W1's and W3's, and are deliberately absent from the
   generated schemas.

**About W4 (database schema)**

6. W4 decides `sum_count` versus `stats_agg` (OQ-3) and records it; both are already
   expressible and emittable, and the choice is one line in `[defaults]`.
7. W4 decides per-level inclusion (OQ-5) **within the contiguity constraint in Decision 8**, and
   reads OQ-W2-1 before doing so.
8. W4 owns the rebuild procedure and the relaxed-durability requirement (D-17);
   `generated/sql/090_rebuild_aggregates.sql` is the shape, not the procedure.
9. W4 owns aggregate compression and retention (OQ-4), refresh policies, and every index beyond
   the hypertable's own time index. None of those is generated.
10. W4 accepts `observations` as the raw table name and `observations_1m` / `_1h` / `_1d` as the
    aggregate names. These are generator constants; if W4 wants different ones it is a
    one-line change and a regeneration.
11. W4 accepts a 7-day `chunk_time_interval` and a 7-day compression policy on the raw table,
    both of which are the spike's values rather than measured optima.
12. W4 reads OQ-W2-2 and OQ-W2-6.

**About W3 (service architecture)**

13. W3 owns the crate and workspace layout (R-4) and will restructure the minimal root
    workspace this work item created, adopting `generated/rust/*.rs` into whichever crate the
    application types live in.
14. `generated/rust/reading.rs` uses `chrono::DateTime<Utc>` for timestamp columns. If W3 picks
    `time::OffsetDateTime` instead, that is one line in the generator — but it must be W3's
    choice, recorded, because `sqlx` supports both.
15. W3's query layer reads `generated/rust/aggregates.rs` rather than re-deriving which columns
    an aggregate level holds.
16. W3 knows that reading a mean requires `mw_mean(c_sum, c_count)` and a wind direction
    requires `mw_circular_mean(c_sin_sum, c_cos_sum, c_dir_count)`, because no stored column
    holds either finished value.

**About W5 (management UI)**

17. W5 §0 owns the user-facing vocabulary table (D-36) and is authoritative over the `label`
    values here; corrections land in the registry.
18. W5 renders from `generated/ts/metrics.ts` rather than maintaining its own label/unit list.
19. W5 reads OQ-W2-5.

**About W8 (CI and release)**

20. W8's gate runs `cargo xtask codegen --check` and treats exit 1 as drift and exit 2 as an
    invariant violation, reporting them differently — a drift is "regenerate and commit", an
    invariant violation is "your registry change is not allowed".
21. W8's gate also runs `cargo test -p xtask`, which is where the output-property assertions and
    the two budget-ceiling tests live.
22. W8 either already added a root `Cargo.toml` or will find the one this work item created with
    `xtask` as its only member. **We are both touching the repository root in the same stage;
    whoever lands second should merge rather than replace.** The `.cargo/config.toml` alias
    `xtask = "run --package xtask --quiet --"` is what makes `cargo xtask` work and must survive
    any restructuring.
23. W8 asserts the minimum database version (D-42). The generated SQL depends on TimescaleDB
    features the spike verified at 2.31 — continuous aggregates over continuous aggregates, and
    `stats_agg`/`rollup` from `timescaledb_toolkit` if W4 picks `stats_agg`.

**About W9 (data migration)**

24. W9 reconciles the production column list against `metrics/registry.toml` and reports which
    of the 212 metrics have never had a column behind them (OQ-6).
25. W9 does not need the registry to be trimmed first. The superset is deliberate.

**About W2a and W7**

26. W7 confirms `solarjoules` against `pkg/solar` (OQ-W2-3).
27. Neither W2a nor W7 needs a registry change to add a metric a new driver reports: it is one
    `[[metric]]` entry, `cargo xtask migrate`, `cargo xtask codegen`.

**About W6 (reconciliation)**

28. W6 reads A-W2-1 as a contradiction between D-11 and OQ-5 that must be resolved before W4
    ships, not as a W2 implementation detail.

---

# Proposed amendments to `DESIGN.md`

Neither is applied. Both are flagged at the top of this document, and this document carries on
under the existing decisions.

**A-W2-1 — OQ-5's recommendation is not constructible under D-11.**

*Which:* OQ-5 ("How many aggregates per metric, and which metrics get 1-minute resolution?"),
whose recommendation reads "the 1-minute level carries only the metrics something queries at
1-minute resolution, with the hourly and daily levels carrying everything."

*What was found:* D-11 makes each level read the level below it. A metric the 1-minute level
does not carry therefore has no source column for the 1-hour level to roll up, so the
recommended configuration cannot be generated — level membership can only shrink away from raw,
and OQ-5 wants it to shrink toward raw. The generator refuses the non-contiguous case rather
than emitting SQL that references a column its source does not have.

*What replaces it:* OQ-W2-1, with three constructible options and a recommendation of (a) —
contiguous membership with the minute level carrying everything and compressed per OQ-4.

*Why this is wrong rather than merely inconvenient:* the two statements are both load-bearing
and they contradict each other. W4 would otherwise discover it while writing DDL, and the
obvious local fix — having the hour level read raw — silently discards D-11's 52× rebuild
advantage, which is the measured reason the hierarchy exists at all.

**A-W2-2 — "adding a metric is cheap" is true of the column and false of the aggregates.**

*Which:* D-9, and the premise it supports in D-3 and D-4.

*What was found:* the `ADD COLUMN` is 11.6 ms as measured. But under D-15 the only supported way
to change an aggregate is to drop, regenerate and refresh it, and a new metric any level carries
forces a rebuild of that level and every level above it. At ten years of history D-17 puts that
in the hour range. The migration generator demonstrates this: one added column produces a
drop-and-recreate of all three levels.

*What replaces it:* a qualification on D-9 — adding a metric is a millisecond DDL operation plus
an aggregate rebuild whose cost is D-17's and D-18's — plus OQ-W2-2's recommendation that W4's
rebuild procedure make a new metric queryable at recent resolution immediately and backfill
older ranges in the background.

*Why this is wrong rather than merely inconvenient:* "adding a metric is cheap" is cited as the
justification for the wide layout (D-3) and for indefinite retention (D-4). Both survive the
correction — the alternative layouts are worse on this axis too — but a capacity or operations
plan written from the 11.6 ms figure alone will schedule a metric addition as a no-op and
discover an hour-long rebuild in production.

---

## Reproducing everything in this document

```sh
cargo xtask validate          # every invariant
cargo xtask budget            # §7
cargo xtask codegen --check   # §8.2
cargo test -p xtask           # §8.1's output assertions and §7's ceiling tests
cargo fmt --all --check && cargo clippy -p xtask --all-targets
```

The bootstrap script that built `metrics/registry.toml` from the Go source is not committed: it
ran once, and `metrics/registry.toml` is the source of truth from here on. Its inputs were
`internal/types/weather.go`, `internal/storage/timescaledb/sql.go`,
`protocols/remoteweather/remoteweather.proto`, `internal/controllers/restserver/types.go` and
`internal/controllers/restserver/transform.go` in `remoteweather` at `c3ec9f7`.
