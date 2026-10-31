# W4 — Database schema, migrations, and continuous aggregates

> **Three things in this document disagree with a decision or recommendation that is already merged.
> All three are flagged here, all three are written up as proposed amendments in the closing section,
> and none of them is applied by editing someone else's decision.**
>
> 1. **D-11 lists three aggregate levels. There are four.** `/span` reads a 5-minute table today and
>    the website's default view is the 24-hour span that falls in that band. D-11's own assumption 16
>    licenses the fourth level on the condition that it derives from the 1-minute level, which it
>    does. §0, amendment **A-W4-1**.
> 2. **OQ-4's recommendation — "compress the 1-minute aggregate and keep it unbounded" — is not
>    available.** `compress_chunk` on a 629-column aggregate fails outright: *row is too big: size
>    11720, maximum size 8160*. Bisected, the widest compressible aggregate is **439 columns**, and
>    no configuration of this schema gets the minute level under that. The levels are
>    retention-bounded instead, which is both cheaper and, on examination, safe for a reason OQ-4 did
>    not consider. §1.2, amendment **A-W4-2**.
> 3. **D-17's 12× relaxed-durability figure does not reproduce at this width, and D-18's size figure
>    is about half what the generated schema actually costs.** Both are corrected with numbers rather
>    than disputed. §6 measurements 3 and 5, amendment **A-W4-3**.
>
> A fourth item is a plain defect rather than a disagreement, and it was fixed here because its
> symptom was *three of `/span`'s four bands returning an error*: the roll-up expression for a count
> column produced `numeric` above the first level while the generated Rust declared `bigint`. §3.2.

**Depends on:** W0, W2, W2a, W3 — all merged.
**Produces:** this document; `migrations/20261029231656_five_minute_level_contract_safe_finaliser_and_bigint_counts.sql`
(generated) and `migrations/20261029234500_w4_station_dimension_caches_indexes_and_policies.sql`
(hand-authored); `ci/db/schema-assertions.sql`, `ci/db/seed.sql`, `ci/db/verify-seed.sql`,
`ci/db/rebuild-aggregates.sh`; and `ci/db/measure/`, which is the harness every number below came
from.

Everything measured here was measured against `timescale/timescaledb-ha:pg17` — PostgreSQL 17.11,
TimescaleDB 2.31.0, `timescaledb_toolkit` 1.26.0, the same image the `database` CI job uses — on a
16-core container with `shared_buffers = 8GB`, `maintenance_work_mem = 2GB`, `max_wal_size = 1GB`,
`fsync = on`, and PGDATA on node-local disk. The dataset is 35,942,400 raw rows: 4 stations at a
2-second cadence over 208 days, 140 of the 216 columns populated, with a measured
consecutive-identical ("hold") rate of **76.87%**. Production is ~143 columns and measures 19 GB/year;

> **Corrected 2026-10-31 by measurement against production (W6 §5 Group A).** Two numbers in this
> paragraph are wrong. Production's `weather` has **215 columns**, not ~143 — exactly the Go source
> definition, so the "~72-column gap" (OQ-6) does not exist. And production's hold rate is **59.6%**,
> not 76.87%, so **the 5.3× compression measured here is optimistic for this data**; W0's spike
> established the ratio is driven almost entirely by that rate. The 19 GB/year figure is confirmed,
> and it is *uncompressed* — production has zero compressed chunks. No decision moves (D-10), but
> every capacity figure derived from 5.3× should be read as a floor.
this load is 44 GB/year uncompressed, so it is roughly 2.3× production's volume at a comparable
width. §6 states which figures are load-dependent and which are not.

---

## 0. Three levels or four

`DESIGN.md` **D-11** says the hierarchy is 1-minute from raw, 1-hour from 1-minute, 1-day from
1-hour. W2 generated exactly that. W1 §2.3 measured what `/span` does and found four tables:

| Span | Table read today | Level |
|---|---|---|
| ≤ 6 h | `weather_1m` | `observations_1m` |
| > 6 h and ≤ 48 h | **`weather_5m`** | *missing* |
| > 48 h and ≤ 14 d | `weather_1h` | `observations_1h` |
| > 14 d | `weather_1d` | `observations_1d` |

**Decided: the 5-minute level exists, derived from the 1-minute level.** `metrics/registry.toml`
gained a `[[level]]` and `observations_1h` now reads `observations_5m`; the change went through
`cargo xtask migrate`, so it is in the generated SQL, the generated Rust, the snapshot and a
migration rather than hand-written anywhere.

Three reasons, in descending order of how much they would have cost to get wrong.

**It is contract-visible, and the fixtures are not ambiguous about it.** The captured bucket spacing
in `contract/fixtures/` is 60,000 ms for `span-1h-one-minute-buckets`, **300,000 ms** for
`span-24h-five-minute-buckets`, and 86,400,000 ms for `span-720h-daily`. That is not a reconstruction
from source; it is the shipped server's output.

**The 24-hour span is the dominant chart.** Both shipped clients only ever build `/span/{integer}h`,
and `/span/24h` is the website's default view. Serving it from `observations_1h` gives 24 points
instead of 288 — a visibly coarser chart. Serving it from `observations_1m` gives 1,440 points, a
five-fold larger response for a chart nobody asked to be finer. Either substitution changes `ts`
spacing in the band that matters most, which is exactly what W1 assumption 10 exists to prevent.

**W3 already assumed four.** W3 assumption 19 reads "`/span`'s **four** aggregate levels and their
exact span boundaries are preserved", and §8.2 names the boundaries "≤ 6 h, ≤ 48 h, ≤ 14 d, > 14 d".
Shipping three levels would have put this document in conflict with a merged one.

**This is an amendment to D-11, not a deviation from D-37.** D-11's assumption 16 says the Go
version's 5-minute level "is *not* assumed to carry forward — adding that if it does, it should derive
from the 1-minute level". It does carry forward, and it does derive from the 1-minute level: a
5-minute bucket is exactly five 1-minute buckets, so the level is contiguous and constructible, and
nothing about D-11's mechanism changes. The frozen contract is preserved rather than deviated from,
so there is nothing for W6's deviation ledger — only amendment **A-W4-1** appending the level to
D-11's list.

**What this costs.** One more level to refresh and retain. Measured: the 5-minute level is 4,175
bytes per bucket row, 4.8 MB/day at four stations, and its share of a full rebuild is 59 s against
the minute level's 401 s (§6 measurement 3). It is the cheapest of the four refreshes that reads
another level.

---

## 1. The decisions W0 left me

### 1.1 OQ-3 — sum-and-count, not `stats_agg`

**Decided: `sum_count` everywhere, with no per-metric override.** `[defaults].mean_representation`
is already `"sum_count"` and stays there; the decision is recorded rather than changed.

W0's recommendation was "sum-and-count for the metrics the frozen API serves, `stats_agg` only where
a dispersion statistic is actually wanted". The second half has no instances. The frozen contract
serves no variance, no standard deviation and no dispersion statistic of any kind — not on `/latest`,
not on `/span`, not on `/almanac`, which serves extrema and their timestamps. There is nothing to
switch, and switching a metric speculatively would be worse than useless: a `mean_representation`
change is classified `BREAKING` by the snapshot differ and needs `--allow-breaking`, so
`stats_agg` adopted now and regretted later costs a drop-and-recreate of every level. Adopted later
for a metric that genuinely needs dispersion, it is one line and a rebuild of the levels that carry
that metric. The asymmetry points one way.

Two further reasons, one of which only appeared once the schema was running.

**The public API's numbers must be reproducible from the schema (D-37).** A `sum` and a `count` are
two numbers anybody can divide; a `statssummary1` is opaque binary that requires the Toolkit to
interpret. For a frozen contract whose correctness arguments have to survive being re-derived by
somebody who was not here, that matters more than a column.

**It does not buy what it would have to buy to be worth it.** The one quantitative argument for
`stats_agg` is column pressure: one column per mean instead of two, 127 fewer columns at each level.
That would have mattered enormously if it had brought the levels under the 439-column
compressibility ceiling found in §1.2 — it would have turned OQ-4's recommendation from impossible
into merely expensive. It does not: 627 expressions minus 127 is 500, still past 439. The one
argument that could have outweighed D-37 fails on a measurement.

`ci/sql/assert-timescaledb-version.sql` warns rather than fails when `timescaledb_toolkit` is absent,
on the grounds that it would fail "once W4 records that the schema uses `stats_agg`". It does not, so
the warning stays a warning. The extension is still present in the deployment image (D-2) and the
assertion file needs no change.

### 1.2 OQ-4 — the minute level is retention-bounded, because it cannot be compressed

**Decided: no compression on any aggregate level; 7-day retention on `observations_1m`, 14-day on
`observations_5m`, 90-day on `observations_1h`, and `observations_1d` unbounded.**

This is the opposite of W0's recommendation, and the reason is that the recommendation's premise does
not hold.

```
$ compress_chunk on observations_1m
ERROR:  row is too big: size 11720, maximum size 8160
```

A compressed chunk holds one row per segment per batch and turns every column of the source relation
into a compressed array. Those arrays are TOASTable; the pointers to them are not, and roughly 19
bytes of pointer per column have to fit in an 8,160-byte heap tuple however little data they point
at. Bisected with `ci/db/measure/66-compressible-width.sh`:

| Aggregate columns | `compress_chunk` | Reported row size |
|---|---|---|
| 100 | compressed | — |
| 300 | compressed | — |
| 430 | compressed | — |
| **439** | **compressed** | — |
| **440** | **refused** | **8,168** |
| 500 | refused | 9,320 |
| 627 | refused | 11,720 |

D-19 anticipated this failure — "a near-ceiling aggregate cannot be compressed at all" — but placed
it near PostgreSQL's 1600-column ceiling, citing a 1600-column view rejected with an estimated row
size of 28,852. The real threshold is **439**, a quarter of the way to the ceiling, and the generated
levels are 629. Production's 397-column views sit just under it, which is why nobody has met this
before.

**Narrowing the minute level is not available.** Level membership can only shrink *away* from raw
(OQ-W2-1), so the minute level is necessarily the widest of the four; a 439-column minute level means
194 metrics become about 60 at *every* level, including the daily level that is cheap to keep wide.
And as §1.1 notes, `stats_agg` only gets to 502. There is no configuration of this schema in which
the minute level compresses.

So OQ-4's other arm. W0 rejected it on the grounds that bounded retention on the minute level
"reintroduces, at one remove, exactly the hole D-4 exists to close". **It does not, and the reason is
sharper than "aggregates are disposable".**

`/span/{span}` takes no time anchor. Its only parameters are `station`, a `station_id` the handler
ignores, and a `format` this handler does not honour (`docs/legacy-api-contract.md` §2.1), so every
span is `[now - span, now]` and the maximum span is one year. Each level is therefore read over
exactly one trailing window, and never over history:

| Level | Read over | Retention | Why that figure |
|---|---|---|---|
| `observations_1m` | last 6 h | **7 days** | its band, plus `observations_5m`'s 2-hour refresh window, plus a week of margin for a late replay |
| `observations_5m` | last 48 h | **14 days** | its band, plus `observations_1h`'s 2-day refresh window |
| `observations_1h` | last 14 d | **90 days** | its band, plus `observations_1d`'s 14-day refresh window |
| `observations_1d` | last 1 year | **unbounded** | the only level read over history, and 18 kB/day |

D-4's hole is in *raw*, and raw is permanent. An evicted minute bucket is not lost information; it is
a cache entry, and `ci/db/rebuild-aggregates.sh --from … --to …` reconstructs any range of it from
raw on demand. The difference between that and production's 365-day raw retention — which is the hole
D-4 closes — is the difference between discarding a derivation and discarding the thing it was
derived from.

**What it is worth.** Measured, an aggregate bucket row is 4,175 bytes at 629 columns, which is
load-independent. At four stations:

| Level | Per day | Unbounded, 10 years | With the retention above |
|---|---|---|---|
| `observations_1m` | 24.0 MB | **88 GB** | 168 MB |
| `observations_5m` | 4.8 MB | 17 GB | 67 MB |
| `observations_1h` | 402 kB | 1.5 GB | 36 MB |
| `observations_1d` | 18 kB | 66 MB | 66 MB |
| **total** | | **107 GB** | **337 MB** |

D-18 put the unbounded minute level at ~42 GB for a decade and expected compression to take it to
~9 GB. At the generated width it is 88 GB and compression is unavailable, so the unbounded option is
worse than D-18 feared and the compressed option does not exist. Retention replaces 107 GB of
uncompressible data that nothing reads with 337 MB. That is the whole of the decision.

**The condition OQ-4 attached, discharged.** OQ-4 made its recommendation conditional on confirming
that compressed-aggregate query latency was acceptable for the `/span` bands. There is no compressed
aggregate to measure, so the condition is answered by the configuration not existing. What is
measured instead is the latency of the configuration that ships — 7 to 17 ms across all four bands,
§6 measurement 1 — and the latency of the raw table, which *is* compressed at 5.3× and is read for
the trailing bucket at 0.44–6.89 ms (§6 measurement 1b).

**Raw compression is unaffected and stays exactly as D-7 configured it.** 216 columns is far inside
the threshold; measured 25.3 GB → 4,552 MB, **5.3×** at a 76.87% hold rate, compressing at 151 MB/s.

### 1.3 OQ-5 — option (a), contiguous, every level carries everything

**Decided: OQ-W2-1 option (a).** Every level carries all 194 aggregatable metrics, 627 expressions
each. No registry change, no W2 follow-up, nothing to work around.

OQ-5 wanted a narrow minute level under wide hourly and daily levels and W2 found that
unconstructible: a level reads the level below it, so membership can only shrink away from raw.
W2 recommended (a) on the grounds that compression would manage the minute level's cost instead. That
grounds is now gone — §1.2 — and the recommendation survives it, because what OQ-5 was really after
was not narrowness but the minute level not being expensive, and retention achieves that more
completely than narrowing would have.

The arithmetic is worth stating because it settles the question rather than deferring it. OQ-5's
mechanism, had it worked, would have cut the minute level's width by whatever fraction of metrics are
not queried at minute resolution. Retention cuts its *size* by 208/7 — a factor of 30 on this
dataset, and a factor that grows without bound as history accumulates while the narrowing factor
stays fixed. Narrowing also costs something retention does not: a metric absent from the minute level
is absent from every level above it, so `/span/14d` would lose metrics that `/span/6h` has, and the
response shape would vary by band. The frozen contract emits all 133 keys on every element of every
`/span` response.

**Option (b) is explicitly not taken.** Two sources at the hour level — one reading `observations_1m`
and one reading raw — would recover a saving that no longer needs recovering, at the cost of a new
registry concept and a partial raw scan in the rebuild path. If a future need for it appears, it is a
W2 follow-up and not something to approximate here.

---

## 2. The tables W2 does not generate

All of this is in
`migrations/20261029234500_w4_station_dimension_caches_indexes_and_policies.sql`, which is
hand-authored and says so in its first line. The split is clean: the generator owns metric columns
and the aggregate expressions over them, and nothing in that file is something a registry entry could
have expressed.

### 2.1 `stations` — a projection, not a registry

```sql
CREATE TABLE stations (
    station_id            text PRIMARY KEY,
    name                  text NOT NULL,
    type                  text NOT NULL,
    timezone              text NOT NULL DEFAULT 'UTC',
    latitude              double precision,
    longitude             double precision,
    altitude_ft           double precision,
    snow_base_distance_mm double precision,
    first_seen            timestamptz NOT NULL DEFAULT now(),
    last_seen             timestamptz
);
```

**What it is.** `config:sqlite`'s `StationRegistry` is authoritative for a station's existence,
credentials and configuration (W3 §1.2 and §3.1). This table is the subset a *reading-side* query
needs, maintained by the ingest path when it resolves a station in `Ingress::admit`, so that `/span`
can reach a station's display name through a join instead of a column repeated on every row of a
hundred-million-row table. Saying which copy is authoritative is the point of the table: without
that, two station registries in two databases is a defect rather than a design.

`timezone` is here and not only in the config store because every calendar rollover is computed in
the station's zone (W3 assumption 23), and `snow_base_distance_mm` is here because W3 §2.5's enrich
stage computes `snowdepth` from it at ingest, which puts it in the reading path.

**On the surrogate integer.** The prompt asks for a surrogate `station_id int`, and this table is
keyed by the `text` station id that arrives on the wire instead. The saving a surrogate is after is
real, and it is smaller than it looks. Measured on two otherwise identical hypertables with the same
172,800 rows (§6 measurement 6b):

| | `station_id text` | `station_id int4` | delta |
|---|---|---|---|
| uncompressed | 105.1 B/row | 97.0 B/row | **8.0 B/row** |
| compressed | 9.67 B/row | 9.58 B/row | **0.09 B/row** |

The compressed delta is the one that matters, and it is essentially zero — because `station_id` is
the `compress_segmentby` key, so a compressed batch stores it once rather than once per row. The
8 bytes apply only while a chunk is uncompressed, which under D-7's policy is seven days: at four
stations that is 1.4 MB/day of standing overhead, about 10 MB at any moment, and it does not grow.

Against 10 MB, a surrogate costs a protobuf type change on field 221 — which remoteweather's
satellites already send as a string — plus server-side allocation and a lookup inside a writer that
W3 §2.3 does not have one in. The denormalisation the prompt is actually objecting to is
`stationname` and `stationtype` repeated per row, and that is OQ-W4-2 below.

**No foreign key from `observations`, and not for cost reasons.** Measured, the constraint is accepted
on a hypertable with 30 compressed chunks and costs 6.3% of bulk insert time; at the production
cadence of four rows every two seconds it is unmeasurable (§6 measurement 6). The reason is the failure
direction. W3 §2.6 resolves the station in `Ingress::admit` before a reading can reach the writer at
all — that is the order that makes an unauthenticated reading unstorable — so the invariant is
already enforced, and a second enforcement here would have exactly one new effect: *rejecting a
reading* because a projection row was missing. Readings are the thing that must not be lost (D-30).
It would also force a choice between `ON DELETE CASCADE`, which lets one `DELETE` take a hundred
million readings with it, and `RESTRICT`, which makes a decommissioned station unremovable. The
low-volume tables below do carry the reference, where the check is free and an orphan is a real
failure mode.

### 2.2 `station_metrics` — capability without probing

```sql
CREATE TABLE station_metrics (
    station_id text NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    metric_key text NOT NULL,
    first_seen timestamptz NOT NULL DEFAULT now(),
    last_seen  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (station_id, metric_key)
);
```

The Go tree has a TODO for exactly this at
`internal/weatherstations/grpcreceiver/station.go:64`. The alternative it is a TODO instead of —
asking the raw table which columns are non-NULL for a station — is a scan of every chunk that station
appears in, which is why it never got written.

Keyed by the registry's stable `key`, not by the PostgreSQL column name. The two are different
namespaces on purpose (`docs/metric-registry.md` §1.1): `out_temp` is what the Rust types, the
OpenAPI schema and the TypeScript types call the metric, and `outtemp` is an artefact of what the old
database happened to name a column. An API describing a station's capabilities should return the
former. There is deliberately no foreign key to a metrics table, because there is no metrics table:
`metrics/registry.toml` is the source of truth (D-20) and a second copy in the database would be a
second source of truth.

### 2.3 `almanac_cache` and `snow_totals_cache` — the contract's only data source

W1 assumption 12: these are not an optimisation that could be removed. `/almanac` and `/snow` read
them directly, and `/almanac` returns `{}` rather than computing anything when its cache is empty.
W3 §2a.7's `snow:*` components write the second one.

```sql
CREATE TABLE almanac_cache (
    station_id  text NOT NULL REFERENCES stations (station_id) ON DELETE CASCADE,
    record      text NOT NULL,          -- the frozen JSON key, CHECKed against all 17
    value       real NOT NULL,
    observed_at timestamptz NOT NULL,
    wind_dir    real,
    computed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (station_id, record)
);
```

**One row per record, not seventeen column triplets.** `AlmanacData` has seventeen `*AlmanacRecord`
pointer fields, every one `omitempty`, so an absent row is an absent key and no rows at all is `{}` —
the shipped empty-cache behaviour, reproduced by the table's shape rather than by a branch in a
handler. Seventeen nullable triplets would have needed that branch, and the branch is where
`/almanac` returning `{"high_temp": {"value": 0, ...}}` instead of `{}` would come from.

The `CHECK` on `record` lists all seventeen keys. A typo in a cache writer would otherwise silently
drop a field from a response, which is the failure mode this endpoint's `omitempty` makes invisible.

`value` and `wind_dir` are `real`, not `double precision`, because the contract serialises them as Go
`float32` and §6.1 of the contract document pins the digits Go emits. Widening the column changes
those digits for any value not exactly representable in 24 bits of mantissa, and a frozen API cannot
do that.

```sql
CREATE TABLE snow_totals_cache (
    station_id     text PRIMARY KEY REFERENCES stations (station_id) ON DELETE CASCADE,
    snowdepth      real NOT NULL DEFAULT 0,
    snowtoday      real NOT NULL DEFAULT 0,
    snowlast24     real NOT NULL DEFAULT 0,
    snowlast72     real NOT NULL DEFAULT 0,
    snowseason     real NOT NULL DEFAULT 0,
    snowstorm      real NOT NULL DEFAULT 0,
    snowfallrate   real NOT NULL DEFAULT 0,
    computed_at    timestamptz NOT NULL DEFAULT now(),
    source_through timestamptz
);
```

Every number `NOT NULL DEFAULT 0`, because the contract emits all seven unconditionally and a
configured-but-never-reporting gauge returns an all-zero object with status 200 (fixture
`snow-device-never-reported`). Zero is the correct value here, not a missing one.

**No TTL, no retention, no recency filter, deliberately.** `/snow` has none either, and that is
contract rather than oversight: an offline gauge returns its last known depth indefinitely (fixture
`snow-device-stale`, Q-16, and the owner's currently-down hardware does this today). `computed_at`
and `source_through` exist so that staleness is *observable* without being acted on — the moment
something filters on them, a shipped behaviour changes.

Values are inches, which is what `/snow` serves — not the millimetres `snowdepth` carries on
`/latest` (Q-4). The cache stores what its endpoint returns; the unit seam is W1's and W3's, and
putting it in a column comment is how it stops being a surprise.

### 2.4 `extra jsonb` — the escape hatch, and the boundary that keeps it one

```sql
ALTER TABLE observations ADD COLUMN extra jsonb;
ALTER TABLE observations ADD CONSTRAINT observations_extra_is_small_object CHECK (
    extra IS NULL OR (jsonb_typeof(extra) = 'object' AND length(extra::text) <= 2048));
```

Sparse, never aggregated, never indexed.

**What gets in:** a flat JSON object of scalars, written by a driver that received a field it has no
metric for. **What does not:** anything a `[[metric]]` entry could hold, and anything large. The
2 kB cap — about forty unmodelled scalars — is what stops the hatch from quietly becoming the
storage layer, and it is a `CHECK` rather than a convention because a convention would lose.

**The promotion path is the point of the column, and it is three steps.** Add a `[[metric]]` entry;
`cargo xtask migrate`, which is 11.6 ms of nullable `ADD COLUMN` (D-9); then a one-off
`UPDATE observations SET <col> = (extra->>'<field>')::float4` over the range that has the key, after
which the driver writes the column and stops writing the key. The levels pick the metric up on their
next rebuild. Nothing in `extra` is ever aggregated, so there is no aggregate to migrate and no
history to lose.

**It is outside `metrics/registry.toml` on purpose.** The registry is the source of truth for
*modelled* metrics, and this column exists for fields that are by definition not modelled yet;
putting it in the registry would also mean allocating it a protobuf field number and a generated Rust
type, neither of which means anything for a field nobody has named. The registry also has no `jsonb`
SQL type today — see OQ-W4-3, which is a small W2 follow-up if the escape hatch is ever wanted on the
wire.

**No GIN index on it, now or later.** A GIN index on a jsonb column of a hundred-million-row
hypertable is the single thing most likely to turn an escape hatch into the schema, because it makes
querying `extra` fast enough that nobody bothers promoting a field.

### 2.5 Indexes

The hot path does not touch the database at all — `/latest` is ~93% of traffic and is served from the
in-memory snapshot (W3 §2a) — so these index the two things that do read SQL: `/span`'s four bands,
and the caches.

```sql
CREATE UNIQUE INDEX observations_station_id_time_key ON observations (station_id, time DESC);
CREATE INDEX observations_1m_station_bucket_idx ON observations_1m (station_id, bucket DESC);
CREATE INDEX observations_5m_station_bucket_idx ON observations_5m (station_id, bucket DESC);
CREATE INDEX observations_1h_station_bucket_idx ON observations_1h (station_id, bucket DESC);
CREATE INDEX observations_1d_station_bucket_idx ON observations_1d (station_id, bucket DESC);
```

**One index, two jobs.** `observations_station_id_time_key` is the unique constraint W3 assumption 22
needs so a cancelled prefix can be replayed with `ON CONFLICT (station_id, time)`, and it is also the
access path every `/span` and every cold single-station read wants: station equality, then a
descending time range. Measured: 4.43 s to build over 35.9 M rows with every chunk
compressed, 360 kB compressed and 34 MB with the seven-day uncompressed window in place, and 0.3% of
insert time (§6 measurements 4 and 6). `DESC` matches `compress_orderby` and the direction every
reader scans.

The hypertable's own `observations_time_idx` stays: it is the access path for a cross-station time
range, which a `(station_id, …)` index cannot serve.

The per-level composite indexes exist because `/span`'s predicate is `(station_id, bucket range)` and
TimescaleDB's generated index on a materialisation hypertable is not composite. These matter most
precisely where `/span` reads, since the levels are never compressed and so never get segment
skipping.

**Nothing on `station_metrics.metric_key`.** The reverse question — which stations emit this metric —
has no caller in the frozen contract, and four stations by ~200 metrics is a sequential scan of under
a thousand rows if one appears.

### 2.6 `period_rain` survives as an aggregate-level column with no raw counterpart

Confirmed (W1 assumption 11). The raw column is `rainincremental`; `period_rain` is the frozen
`/span` key naming that column's *bucket total*, which exists only at aggregate level. The registry
expresses it as `[metric.legacy_aggregate_rest_keys] total = ["period_rain"]` — the one legacy key
that names an aggregate rather than a value. `ci/db/verify-seed.sql` V-6 asserts that the total rolls
up to the same number at all four levels, which a mean would not.

---

## 3. The correctness constraints handed to me by name

### 3.1 Unreported metrics are NULL, and `windd` still serialises as `0`

W3 assumption 18 takes OQ-W1-2 option (c) on my behalf. **`mw_circular_mean` did not implement it**,
and I was told to verify rather than assume, which is the only reason this was caught:

```sql
-- before
SELECT CASE WHEN n IS NULL OR n = 0 THEN NULL
            ELSE (atan2d(sin_sum / n, cos_sum / n) + 360)::numeric % 360 END

-- after
SELECT CASE WHEN n IS NULL OR n = 0 OR sin_sum IS NULL OR cos_sum IS NULL THEN 0
            ELSE (atan2d(sin_sum / n, cos_sum / n) + 360)::numeric % 360 END
```

The generated finaliser was NULL-*safe* — it does not divide by zero the way
`circular_avg_final` does, so W1-BUG-1's 500 was already fixed — but it returned `null`, which is
OQ-W1-2 option **(b)**, the option W1 explicitly calls a contract change. `windd` is a non-optional
Swift `Double`, and `Foundation.JSONDecoder` fails the decode of the *entire* `/span` response when a
non-optional key is absent. Option (b) therefore turns a 500 into a client-side total failure for
every snow gauge and air-quality monitor, which is not an improvement.

`0` is also exactly what ships: the Go daemon writes its whole struct including zeros, so a station
with no vane has `winddir = 0` on every row, and `atan2d(0, count)` is `0`. Returning `0` reproduces
the shipped bytes rather than approximating them.

**The asymmetry with `mw_mean` is deliberate and is the whole of OQ-W1-2.** `mw_mean(NULL, NULL)`
stays `NULL`: a missing temperature is absent, a missing direction is `0`, because only the direction
has a non-optional client-side type whose decode fails on absence. `ci/db/schema-assertions.sql` A-3
and A-4 assert both halves, and A-3 also asserts the converse — that a real direction is not
flattened to zero — because a finaliser that returned `0` unconditionally would pass the first half.

The fix is in the generator (`xtask/src/gen.rs::functions_sql`), not in
`generated/sql/010_functions.sql`. Hand-editing generated SQL fails `cargo xtask codegen --check` and
deserves to.

**And the finalisers now reach the database.** `010_functions.sql` was emitted only by the bootstrap
migration, so a change to a finaliser's *body* — which changes no identifier and therefore produces
no snapshot diff — reached `generated/` and never reached a deployed database. `cargo xtask migrate`
now emits the finalisers in every migration; they are `CREATE OR REPLACE` and no continuous aggregate
inlines them, so re-asserting them is idempotent. This is the second of the two generator edits this
work item makes, and both are flagged in the closing section.

### 3.2 The defect that only running the query finds

```
ERROR:  function mw_mean(double precision, numeric) does not exist
```

That is `/span` on any band except `≤ 6 h`.

`sum(bigint)` returns `numeric` in PostgreSQL. The generated roll-up expression was
`nullif(sum(c_count), 0)`, so every level above the first declared its 128 count columns `numeric`
while `generated/rust/aggregates.rs` declared `bigint` at all four levels. Two consequences, both
silent in the SQL and neither visible without a live instance:

- **`mw_mean` is `(double precision, bigint)` and `numeric` does not implicitly narrow to `bigint`.**
  The call fails to resolve. Three of `/span`'s four bands — including the 24-hour default view —
  return an error rather than a wrong number, which is the good version of this bug.
- **`sqlx` would decode the column into the wrong Rust type.** W3 assumption 15 has the query layer
  read `aggregates.rs` for what a level holds rather than re-deriving it, so the generated file being
  wrong is the generated file being trusted.

Fixed in `xtask/src/registry.rs` by casting the roll-up back to the declared type —
`nullif(sum(c_count), 0)::bigint` for `_count` and `_dir_count` — regenerating, and folding it into
the same migration as the 5-minute level, since both drop and recreate every level anyway. A count
also has no business being variable-length: `numeric` is wider on disk than `bigint` across 128
columns and 1.2 M rows per level.

`ci/db/schema-assertions.sql` A-1 asserts every `_count`/`_dir_count` is `bigint` and every
`_sum`/`_total` is `double precision` at every level; A-2 resolves both finalisers against each
level's own column types, which catches a signature change A-1 would miss. Both run on both migration
paths in CI.

### 3.3 Idempotency over a replayed prefix

W3 assumption 22 needs `ON CONFLICT (station_id, time) DO UPDATE` or an equivalent, because §2.3's
cancel-safe delivery replays a prefix after a cancellation. **Decided: a unique index on
`(station_id, time DESC)`, with `ON CONFLICT DO UPDATE`,** and TimescaleDB 2.31 permits the whole of
it, which was the part in doubt.

Measured (§6 measurement 4):

| | |
|---|---|
| `CREATE UNIQUE INDEX` over 35.9 M rows, 30/30 chunks compressed | **4.43 s**, accepted |
| Index size, all chunks compressed / with the newest decompressed | 360 kB / **34 MB** — ~29 B per row on the uncompressed window only |
| `ON CONFLICT DO UPDATE`, 30 rows, uncompressed chunk | **50.7 ms** |
| `ON CONFLICT DO UPDATE`, 30 rows, **compressed** chunk | **281.4 ms**, chunk still compressed |
| Insert cost of the index, median of 3 × 43,200 rows | 3,356 ms against 3,345 ms — **+0.3%** |

The constraint is legal because a unique index on a hypertable must include the partitioning column
and `time` is it. The compressed case works and leaves the chunk compressed, which is the answer the
prompt asked to have reported: it was not true of earlier TimescaleDB versions and it is not
something to assume. It is also 5.5× the uncompressed case, so the guidance is to keep replays
inside the uncompressed window — which §2.3's cancel-safe delivery does naturally, since it replays
a prefix of the batch it was mid-send on.

**One ordering consequence for W9.** The index is created in this migration, when `observations` is
empty. Creating it later, over imported history, is the 4.0 s above plus whatever lock it takes on a
live table — W9 should import raw first and create the index as part of the import rather than
discovering it afterwards.

### 3.4 Collector-written rows legitimately contain NULL

Noted and designed for. W3's open-question entry 2 has the generator emit proto3 `optional` on every
metric field so a satellite and its collector hold the same data instead of the collector storing
`0.0` for a metric never reported. Every metric column in `observations` is nullable, every count
column is `nullif(count(c), 0)` so an empty bucket stores NULL rather than 0 (D-16), and §3.1's
finalisers are the only place a NULL is turned back into a number. `ci/db/seed.sql`'s `seed-nowind`
station is a NULL-throughout station and `ci/db/verify-seed.sql` V-4 asserts its behaviour at all
four levels.

### 3.5 The trailing partial bucket — a contract requirement this schema cannot meet alone

This is the one correctness constraint that lands outside my lane, and it was not in the prompt.

All four levels are `materialized_only`. Three of them have no choice: TimescaleDB cannot build a
continuous aggregate on top of a real-time one, and `observations_1m`, `_5m` and `_1h` are each the
source of another level. `observations_1d` is uniform with them by decision rather than necessity —
its real-time tail would read `observations_1h`, so it would be stale by up to an hour anyway, which
buys an inconsistency across bands in exchange for nothing.

A `materialized_only` level never emits the bucket that is still filling. **The shipped server does.**
Measured from the fixtures, against each one's own `captured_at`:

| Fixture | Captured | Last bucket | Last bucket is |
|---|---|---|---|
| `span-1h-one-minute-buckets` | 16:56:42 | 16:56:00 | the current minute, 42 s in |
| `span-72h-hourly-with-gap` | 16:56:42 | 16:00:00 | the current hour, 56 min in |
| `span-720h-daily` | 16:56:42 | 00:00:00 | today, 17 h in |

So the Go views are real-time aggregates, and the last element of every `/span` response is the
in-progress bucket. Dropping it would shorten every response by one element and make the "now" end of
every chart lag by up to a bucket width — a day on the `> 14 d` band. W1's hazard table does not list
this, because it is only visible by comparing a fixture's last `ts` against its capture time.

**The read path must append one bucket aggregated from raw over
`[last_closed_bucket_end, now)`.** That is a `/span` query-layer requirement, not a schema one, and it
is squarely inside W3's own rule: R2 in §2.5 says SQL computes a window only for `/span`. It is also
cheap, which is the part worth measuring so that W3 is not deciding blind — one bucket, one station,
against a *compressed* chunk, which is the pessimistic case because the newest chunk is uncompressed
for seven days:

| Level | Raw rows in the tail | Median |
|---|---|---|
| `observations_1m` | 30 | **0.44 ms** |
| `observations_5m` | 150 | **0.45 ms** |
| `observations_1h` | 1,800 | **0.68 ms** |
| `observations_1d` | 43,200 | **6.89 ms** |

Against 7–17 ms for the materialised part of the same query, the tail is a rounding error at three of
the four bands and 40% at the daily one. Recorded as an assumption against W3 so it is greppable.

---

## 4. The rebuild procedure

`ci/db/rebuild-aggregates.sh`. `generated/sql/090_rebuild_aggregates.sql` is the shape it executes.

**Why it is a script and not a migration.** `refresh_continuous_aggregate` cannot run inside a
transaction and `sqlx` wraps every migration in one, so a migration can create a level but never
materialise it. Every migration that touches a level leaves it `WITH NO DATA`, and this script is what
fills it. A deploy that migrates and forgets leaves `/span` empty for every affected band with no
error anywhere, which is why `--check-watermarks` exists.

**Recent ranges first, oldest last — OQ-W2-2's half that is mine.** The script runs in two phases,
both bottom-up:

1. **The `/span` bands.** Each level over exactly the window `/span` can read it over — 6 h, 48 h,
   14 d, 366 d — bottom-up. Measured at 35.9 M raw rows: §6 measurement 3b. This is the phase that
   has to finish before the charts are right.
2. **Everything older**, bottom-up again, as a background concern.

The asymmetry is the whole argument. A full rebuild of the minute level over 35.9 M rows is minutes;
the last six hours of it is well under a second. Adding a metric forces a drop-and-recreate of every
level that carries it (D-15), so without this ordering, "adding a metric" means a long window of
empty charts at ten years of history — which is A-W2-2's point, restated as a procedure instead of a
warning. With it, the charts are correct in seconds.

Bottom-up is **required**, not merely preferable: each level reads the level below it, so a range
materialised top-down has the upper level reading a hole and caching it. The generated rebuild script
says this and the procedure enforces it per phase.

**Relaxed commit durability (D-17).** Every refresh session sets `synchronous_commit = off`. Safe for
exactly the reason aggregates are disposable — a crash mid-rebuild costs a re-run, not information —
and scoped to these sessions only. Ingest keeps full durability. §6 measurement 3 reports what it is
actually worth at this width, which is less than D-17 says and still worth having.

**A note on what the generated migration does that the procedure then has to undo.** The migration
for the 5-minute level drops and recreates all four levels, including `observations_1m`, whose
definition did not change. `snapshot::affected_levels` keys off which metrics changed rather than
which level definitions changed, and every metric's `levels` list changed. Correct under D-15 and
expensive in exactly the way OQ-W2-2 describes: an intact minute level would have let the new
5-minute level be built from it in 77 s instead of forcing a full raw rescan. Recorded as OQ-W4-1
rather than fixed, because `affected_levels` is W2's.

**Backup and restore (D-6).** `pg_dump` of the raw hypertable plus a rebuild is a complete backup,
and the restore side is where the cost sits. Restore raw, then run this script: phase 1 brings
`/span` back in seconds and phase 2 is the long part. At ten years of history phase 2 is an
hour-scale operation rather than an instant one — but note that under §1.2's retention only
`observations_1d` has ten years of history to rebuild, and the daily level rebuilds from the hourly
level in 0.38 s at 208 days. The three lower levels only ever rebuild their retention window. The
restore procedure is therefore *faster* than D-6 implies, and the reason is the retention decision,
not the dump.

---

## 5. CI

### 5.1 `db-migrate-twice` was trivially green. Here is the before and after.

**Before**, run against `main`'s `ci/` and `migrations/` on the CI image:

```
=== asserting the server version (DESIGN.md D-42)
NOTICE:  PostgreSQL 17 / timescaledb 2.31.0 / toolkit t — ok
=== path A: empty -> HEAD
  20261028165610_initial_schema.sql
=== path B: previous schema -> seeded rows -> HEAD
  20261028165610_initial_schema.sql
    note: ci/db/seed.sql does not exist; W4 owns the seed fixture. The upgrade
          path runs against an empty table until it does.
  20261028165610_initial_schema.sql (already applied)
=== comparing
=== both paths produced the same schema
exit=0
```

Both paths applied the same single file, nothing was seeded, nothing was asserted beyond a dump
comparison between two identical schemas, and it exited 0 — which is the only thing a gate
communicates.

**After**, same image, same command:

```
=== path A: empty -> HEAD
  20261028165610_initial_schema.sql
  20261029231656_five_minute_level_contract_safe_finaliser_and_bigint_counts.sql
  20261029234500_w4_station_dimension_caches_indexes_and_policies.sql
=== path B: previous schema -> seeded rows -> HEAD
  20261028165610_initial_schema.sql
    seeding
  20261029231656_five_minute_level_contract_safe_finaliser_and_bigint_counts.sql
  20261029234500_w4_station_dimension_caches_indexes_and_policies.sql
=== comparing
=== schema assertions, path A
schema assertions: all passed
=== schema assertions, path B
schema assertions: all passed
=== verifying seeded data survived
seed verification: all passed
=== path B applied 1 migration(s), then 2 more to reach HEAD
    path B exercised a real upgrade over seeded tables.
=== both paths produced the same schema
exit=0
```

**After**, four changes, in descending order of what they catch:

1. **`ci/db/schema-assertions.sql`, run on both paths.** Nine assertions about properties a dump
   comparison cannot see, because both paths are equally wrong about them. Three of the nine are
   there because they were violated: A-1 (count columns became `numeric`), A-3 (the finaliser
   returned `null` where the contract needs `0`), A-8 (retention on the levels). The others are A-2
   finaliser resolution, A-4 `mw_mean`'s NULL, A-5 the hierarchy reads levels and never raw, A-6
   source levels are `materialized_only`, A-7 the unique constraint exists, A-9 every level has a
   refresh policy and the minute level's is phase-locked.
2. **`ci/db/seed.sql`**, the fixture the script asks W4 for, so path B's second apply runs over rows.
   Three stations, three minutes, twelve rows, and every shape that has broken something: uneven
   bucket counts (D-12's condition), a station that never reports `winddir` (W1-BUG-1's trigger),
   directions straddling north where a scalar mean gives 180 and a vector mean gives 0, a bucket with
   exactly one reading, and a bucket with none. Anchored to *yesterday at noon* rather than a fixed
   date, because three of the four levels now carry retention and a fixture dated in the past would
   be dropped by a retention job firing mid-gate — and because `refresh_continuous_aggregate`
   materialises only complete buckets, so a fixture written *today* would never appear in
   `observations_1d` and every assertion reading that level would pass vacuously.
3. **`ci/db/verify-seed.sql`**, which is where D-12, D-13 and D-14 stop being spike results and
   become assertions the build runs. The mean reconstructed from the *top* of the hierarchy is
   compared to a direct scan of raw with `IS DISTINCT FROM`, not a tolerance, because D-14 widened
   the accumulator specifically so the two agree exactly. The circular mean is compared to a direct
   computation *and* asserted to disagree with a scalar `avg()`, so a scalar mean sneaking in fails
   rather than passing within a tolerance.
4. **A strength report.** The script now prints how many migrations each path applied and says
   `WEAK:` on stderr when path B was a second path A or when HEAD adds no migration. A gate that is
   green for a reason nobody checked is the thing this gate existed to stop being.

**A second migration now exists**, so path B is a genuine delta: previous → `initial_schema`, seed,
then the two W4 migrations. The upgrade it exercises is the real one — a drop-and-recreate of all
four levels and a set of new tables and policies over existing rows.

**And the assertions were proved able to fail**, which is the only thing that separates them from
comments. Two negative controls:

```
# the same assertions against a database without the W4 migration
ERROR:  observations has no unique index on (station_id, time); the writer cannot be
        idempotent over a replayed prefix

# mw_circular_mean reverted in a live database to the NULL-returning form
ERROR:  mw_circular_mean(NULL,NULL,NULL) = <NULL> — W1-BUG-1 is live again

# restored
schema assertions: all passed
```

### 5.3 Three generator behaviours now have Rust-level tests, and all three were proved to fail

`ci/db/` needs a database. `cargo test -p xtask` does not, and W8's gate already runs it (W2
assumption 21), so the two generator behaviours this work item made load-bearing are asserted there
as well:

| Test | Asserts |
|---|---|
| `a_rolled_up_count_is_cast_back_to_bigint` | every `_count`/`_dir_count` above the first level is `bigint` **and** its roll-up expression ends in `::bigint` |
| `the_circular_mean_finaliser_returns_zero_for_an_empty_bucket` | `mw_circular_mean` contains `THEN 0` and no `THEN NULL`, **and** `mw_mean` still contains `THEN NULL` — both halves of OQ-W1-2's asymmetry |
| `every_migration_reasserts_the_finalisers` | a non-bootstrap migration contains both `CREATE OR REPLACE FUNCTION` statements |

Each was run against a deliberately reverted generator and each failed:
`test result: FAILED. 22 passed; 3 failed`. A test that cannot fail proves nothing, which is
especially true of a test asserting the presence of a substring.

One existing test was also widened rather than left alone.
`the_generated_aggregate_sql_contains_no_naive_form` filtered its file list on
`path.contains("observations_1")`, which covered `_1m`, `_1h` and `_1d` and silently stopped covering
anything the moment a level was added whose name does not contain `1` — which `observations_5m` does
not. It now matches on the generator's own naming.

**`/usr/bin/pg_dump` is left alone.** `pr.yml` puts `/usr/lib/postgresql/17/bin` on `PATH` because
the runner's `/usr/bin/pg_dump` is a postgresql-common wrapper resolving to a stopped local
PostgreSQL 16 cluster. Nothing here touches that job's `PATH`.

### 5.2 The minimum server version, and what changes if production is PostgreSQL 14

`ci/sql/assert-timescaledb-version.sql` already asserts PostgreSQL ≥ 17 and TimescaleDB ≥ 2.31 and
warns on a missing Toolkit. No change needed: §1.1 declines `stats_agg`, so the Toolkit warning stays
a warning rather than becoming an error.

Production's version is unknown — W0's OQ-1 could not reach it and the only reference in the Go
repository is a `docker-compose` example naming PostgreSQL 14. That is W9's and the owner's problem,
not mine, but these are the decisions in this document that change if it turns out to be 14:

| If production is PostgreSQL 14 / an older TimescaleDB | Consequence for this document |
|---|---|
| **Continuous aggregates on continuous aggregates** need TimescaleDB ≥ 2.9 | The entire four-level hierarchy is unconstructible below 2.9. Every level would have to read raw, which is what production does today and what D-11 exists to stop. This is the one that breaks the design rather than a decision in it. |
| **Nullable `ADD COLUMN` on compressed chunks without decompression** needs ≥ 2.11 | D-9's 11.6 ms becomes a decompress-and-recompress of every chunk holding the column. Adding a metric stops being cheap and §4's phase-1-first rebuild stops being enough, because the `ADD COLUMN` itself becomes the long part. |
| **`ON CONFLICT` against a compressed chunk** — §3.3 verified it at 2.31 | W3 assumption 22's idempotency would need the writer to decompress explicitly, or to confine replays to the uncompressed window. The 7-day compression delay makes the second option viable, but it becomes a constraint on the writer rather than a property of the table. |
| **`add_retention_policy` on a continuous aggregate** | §1.2's decision needs it. Without it, retention is a cron job calling `drop_chunks` on each materialisation hypertable — the same operation, less supervised. |
| **`initial_start` / fixed-schedule policy jobs** | §6 measurement 2's 3-second lag depends on the phase being deterministic. Without fixed schedules, the minute level's refresh fires at whatever phase the deploy happened at and `window:rollup`'s 10-second deadline is met by luck. A-9 asserts the fixed schedule for this reason. |
| Everything else | The column-width compressibility ceiling, the row sizes, the bucket arithmetic and both finalisers are plain PostgreSQL and do not move. |

The upgrade is not optional under any reading: a 2.9-or-older server cannot build this hierarchy at
all.

---

## 6. Measurements

Opinions are not findings. Every number below has the command that produced it, in the style of
`docs/spike-findings.md`. The harness is `ci/db/measure/`, committed so these can be reproduced or
disputed; it is not wired into any gate, because it writes tens of gigabytes and takes about an hour.

**The setup, once.** A throwaway `timescale/timescaledb-ha:pg17` pod — PostgreSQL 17.11, TimescaleDB
2.31.0, `timescaledb_toolkit` 1.26.0 — on a 16-core node with PGDATA on node-local disk and the
image's own `timescaledb-tune` settings: `shared_buffers = 8GB`, `effective_cache_size = 24GB`,
`work_mem = 16MB`, `maintenance_work_mem = 2GB`, `max_wal_size = 1GB`, `wal_compression = off`,
`fsync = on`, `synchronous_commit = on`. Nothing was tuned for these measurements; a faster
`max_wal_size` in particular would have flattered measurement 3.

```sh
# the schema under test
psql -1 -f migrations/20261028165610_initial_schema.sql
psql -1 -f migrations/20261029231656_five_minute_level_contract_safe_finaliser_and_bigint_counts.sql
psql    -f ci/db/measure/00-harness.sql

# 208 days x 4 stations at 2 s = 35,942,400 rows, 8 concurrent sessions over disjoint ranges
#   CALL mw_measure.fill(4, '<from>', '<to>');
# 7 m 57 s wall clock.
psql -c "SELECT count(*) FROM observations"          --> 35942400
psql -c "SELECT mw_measure.hold_rate()"              --> 0.7687
```

**The hold rate is quoted with every compression figure, and that is not decoration.** D-10 measured
the same schema and row count at 1.89×, 3.36×, 11.24× and 13.74×, with the entire spread explained by
the fraction of consecutive readings identical to their predecessor. A generator using `random()`
would measure the entropy ceiling and report a number no production instance will ever see, so
`mw_measure.value_expr` is a slow sinusoid quantised to a sensor's resolution, which is what makes
consecutive 2-second readings repeat. The rate achieved is **76.87%**, which sits between D-10's 33%
and 86.5% points — so the compression figures below should be read as conservative against the
held-sensor model D-10 says production resembles.

### 6.1 Compressed-aggregate query latency per `/span` band — settles OQ-4

OQ-4's condition was "confirm that compressed-aggregate query latency is acceptable for the `/span`
bands before committing". **There is no compressed aggregate to measure**: `compress_chunk` refuses
every level in this schema (6.5 below). The condition is answered by the configuration not existing,
and what is measured instead is the latency of the configuration that ships.

```sh
psql -f ci/db/measure/30-span-latency.sql
psql -c "SELECT * FROM mw_measure.span_all()"
```

The query shape is built from each view's own columns rather than transcribed, and it finalises
**every** metric the level carries — 333 output expressions against the frozen `/span` element's 133
keys — so these are upper bounds on the real read, not approximations of it. `md5(q::text)` forces
every output column to be constructed.

| Level | Band tested | Rows out | First run | Median of 5 |
|---|---|---|---|---|
| `observations_1m` | 6 h | 360 | 16.52 ms | **11.55 ms** |
| `observations_5m` | **24 h** (the default view) | 288 | 11.71 ms | **9.51 ms** |
| `observations_5m` | 48 h | 576 | 17.01 ms | **16.81 ms** |
| `observations_1h` | 14 d | 336 | 13.87 ms | **10.55 ms** |
| `observations_1d` | 200 d | 200 | 11.85 ms | **7.19 ms** |

Every band is inside 17 ms at a third more width than the contract needs, and the bands are
flat in cost because that is what the band boundaries are *for*: each returns a few hundred rows by
construction. `/span` is 0.9% of the measured request mix (W3 §2a.8), so the first-run column is the
more honest one, and it is within 5 ms of the median everywhere.

### 6.1b The trailing partial bucket from raw — §3.5

```sh
psql -f ci/db/measure/35-trailing-bucket.sql
psql -c "SELECT * FROM mw_measure.tail_all()"
```

One bucket, one station, aggregated from raw. Measured twice: with the newest chunk compressed, and
after `decompress_chunk` on it. In production the newest chunk is uncompressed for seven days and the
tail is always in the newest chunk, so the uncompressed column is the real one and the compressed
column is the bound.

| Bucket width | Raw rows | Median, chunk compressed | Median, chunk uncompressed |
|---|---|---|---|
| 1 minute | 30 | 0.44 ms | 0.44 ms |
| 5 minutes | 150 | 0.45 ms | 0.45 ms |
| 1 hour | 1,800 | 0.68 ms | 0.67 ms |
| 1 day | 43,200 | 6.89 ms | 6.88 ms |

The two columns are indistinguishable, which is itself the finding: at this row count the work is
tuple construction and arithmetic, not I/O, so compression does not enter. Against 6.1's 7–17 ms the
tail is a rounding error at three bands and 40% at the daily one.

### 6.2 Refresh lag at the minute boundary against the 10-second deadline

W2a assumption 14 and W3 assumption 21 give `window:rollup` 10 seconds from the minute boundary to
read that minute's bucket.

```sh
psql -f ci/db/measure/40-refresh-lag.sql
psql -c "SELECT add_continuous_aggregate_policy('observations_1m',
             start_offset => INTERVAL '30 minutes', end_offset => INTERVAL '1 second',
             schedule_interval => INTERVAL '1 minute',
             initial_start => date_trunc('minute', now()) + INTERVAL '1 minute 3 seconds')"
# two concurrent sessions, 9 minutes
psql -c "CALL mw_measure.live_insert(INTERVAL '9 minutes')"      # 4 stations at 2 s, at now()
psql -c "CALL mw_measure.watch_buckets(INTERVAL '8 minutes 30 seconds')"  # polls 5x/second
psql -c "SELECT * FROM mw_measure.refresh_lag"
```

The lag is `first_seen - (bucket + 1 minute)`: wall-clock instant the bucket became readable, minus
the instant it closed. Measured, not inferred from the job's schedule.

| Bucket | Lag |
|---|---|
| 23:47 | 3.22 s |
| 23:48 | 3.06 s |
| 23:49 | 3.09 s |
| 23:50 | 3.09 s |
| 23:51 | 3.12 s |
| 23:52 | 3.15 s |
| 23:53 | 3.18 s |
| 23:54 | 3.21 s |

**Median 3.14 s, range 3.06–3.22 s, against a 10-second budget.** 18 job runs, 0 failures; the
refresh itself takes 28–53 ms, so essentially all of the lag is the three-second `initial_start`
phase offset and there is another two seconds available by reducing it. (A ninth sample, 32.19 s, is
discarded: it is the watcher's own start artefact — the first bucket it saw had already closed before
it began polling.)

**W3 assumption 21 holds, with margin, and it holds because of `initial_start`.** A policy created
without it gets whatever phase the deploy happened at, and the same measurement would have returned
anything from 3 s to 63 s. `ci/db/schema-assertions.sql` A-9 asserts the fixed schedule for exactly
that reason. `window:rollup`'s period does not have to grow and W2a's `Phase::Aligned` does not need
revisiting.

### 6.3 Full-rebuild time per level, with and without relaxed durability

At **35,942,400 raw rows**, 627 aggregate expressions per level, all four levels dropped and
recreated `WITH NO DATA` before each pass.

```sh
PSQL=... ci/db/measure/10-recreate-aggregates.sh
psql -v durability=on  -f ci/db/measure/20-rebuild-timing.sql
PSQL=... ci/db/measure/10-recreate-aggregates.sh
psql -v durability=off -f ci/db/measure/20-rebuild-timing.sql
```

| Level | Reads | `synchronous_commit = on` | `= off` | Speedup |
|---|---|---|---|---|
| `observations_1m` | raw | 758.5 s | **401.4 s** | 1.89× |
| `observations_5m` | `_1m` | 152.6 s | **59.2 s** | 2.58× |
| `observations_1h` | `_5m` | 12.76 s | **6.23 s** | 2.05× |
| `observations_1d` | `_1h` | 0.66 s | **0.39 s** | 1.69× |
| **all four** | | **924.5 s** (15 m 25 s) | **467.2 s** (7 m 47 s) | **1.98×** |

**This does not reproduce D-17's ratio, and the decomposition says why.** D-17 measured 306 s → 25.6 s
(12×) on a 398-column materialisation and concluded "the cost is WAL fsync, not CPU". Controls on the
same data and the same aggregation:

```sh
psql -f ci/db/measure/25-plain-aggregation.sql
```

| | |
|---|---|
| The identical aggregation as a plain query, `count(*)` only, parallel | **12.5 s** |
| …the same, `max_parallel_workers_per_gather = 0` | **10.0 s** |
| …the same, materialised into a plain table, `synchronous_commit = off` | **106.2 s** (4,680 MB) |
| `refresh_continuous_aggregate`, `synchronous_commit = off` | **401.4 s** |
| `refresh_continuous_aggregate`, `synchronous_commit = on` | **758.5 s** |

So the 758.5 s breaks down as roughly 10 s of arithmetic, 96 s of writing 1.2 M 629-column rows,
295 s of continuous-aggregate refresh machinery, and 357 s of WAL fsync. **D-17 is directionally
right and quantitatively wrong at this width**: fsync is 47% of the durable refresh rather than
nearly all of it, and the residue after relaxing durability is 32× the plain query rather than D-17's
implied ~10×. Relaxing durability is still worth doing — it halves the longest operation in the
system — and it is still not CPU. It is just not the only thing. Amendment A-W4-3.

Also note the generated schema is 627 expressions against the spike's 398, which is most of why the
absolute numbers are 2.5× larger; the ratio is the part that does not transfer.

### 6.3b Bounded-range refresh — the measurement that justifies §4's ordering

Same data, all four levels empty, refreshing only the window `/span` can read each level over.

```sh
PSQL=... ci/db/measure/10-recreate-aggregates.sh
psql -c "SET synchronous_commit = off" \
     -c "CALL refresh_continuous_aggregate('observations_1m', '<now-6h>', '<now>')" \
     -c "CALL refresh_continuous_aggregate('observations_5m', '<now-6h>', '<now>')" \
     -c "CALL refresh_continuous_aggregate('observations_1h', '<now-6h>', '<now>')" \
     -c "CALL refresh_continuous_aggregate('observations_1d', '<now-1d>', '<now>')"
```

| Level | Range | Time |
|---|---|---|
| `observations_1m` | last 6 h | 753 ms |
| `observations_5m` | last 6 h | 82 ms |
| `observations_1h` | last 6 h | 18 ms |
| `observations_1d` | last 1 d | 15 ms |
| **all four** | | **0.87 s** |

**0.87 s against 467 s — 537×.** That is OQ-W2-2's recommendation as a number: a new metric is
queryable at every `/span` resolution in under a second, and the history backfill is a background
concern rather than the thing standing between a migration and a working chart. A-W2-2 said "adding
a metric is cheap" is false of the aggregates; it is true again of the part anyone looks at.

### 6.4 Is a unique constraint or `ON CONFLICT` target permitted on the compressed hypertable?

**Yes, both, and the conflicting rows do not force the chunk out of compression.**

```sh
psql -f ci/db/measure/50-idempotency.sql
```

Against 35,942,400 rows in 30 chunks, all compressed:

| | |
|---|---|
| `CREATE UNIQUE INDEX observations_station_id_time_key ON observations (station_id, time DESC)` | accepted, **4.43 s** |
| Index size, every chunk compressed | **360 kB** across 31 per-chunk indexes |
| Index size with the newest chunk decompressed (≈1.21 M rows) | **34 MB** — so **~29 bytes per row** on the uncompressed window, and near zero once a chunk compresses |
| `INSERT … ON CONFLICT (station_id, time) DO UPDATE`, 30 rows, **uncompressed** chunk | **50.7 ms** |
| the same, against a **compressed** chunk | **281.4 ms** |
| …chunk state afterwards | **still compressed** |

The constraint is legal because a unique index on a hypertable must include the partitioning column
and `time` is it.

Two things in that table are worth more than the headline.

**The index is nearly free on compressed chunks and ~29 B/row on the uncompressed window**, because a
compressed chunk's index is built over the compressed rows — one per segment per batch. At four
stations the standing cost is about 34 MB and it does not grow, since the window is seven days wide
by D-7's policy.

**A replay into a compressed chunk costs 5.5× one into an uncompressed chunk** — 281 ms against
51 ms for 30 rows, roughly 9 ms per row. It works and the chunk stays compressed, which is the
answer the prompt asked to have reported; it was not true of earlier TimescaleDB versions, so W3
assumption 22's idempotency is a property of 2.31 specifically and
`ci/sql/assert-timescaledb-version.sql` is what keeps it true. But the guidance that follows is
**keep replays inside the uncompressed window**, which §2.3's cancel-safe delivery does naturally —
it replays a prefix of the batch it was in the middle of sending, which is seconds old, not days.
A replay reaching a compressed chunk means something else has gone wrong.

### 6.5 Size on disk per level, compressed and not — corrects D-18

```sh
psql -f ci/db/measure/60-sizes.sql
psql -f ci/db/measure/65-compress.sql
PSQL=... ci/db/measure/66-compressible-width.sh 100 300 430 439 440 500 627
```

At 35,942,400 raw rows. `hypertable_size` includes indexes.

| Relation | Rows | Uncompressed | Compressed | Ratio | **Bytes/row** |
|---|---|---|---|---|---|
| `observations` | 35,942,400 | 25.3 GB | **4,552 MB** | **5.3×** | 703 → 133 |
| `observations_1m` | 1,198,080 | 4,770 MB | *refused* | — | **4,175** |
| `observations_5m` | 239,616 | 954 MB | *refused* | — | **4,175** |
| `observations_1h` | 19,968 | 80 MB | *refused* | — | **4,186** |
| `observations_1d` | 832 | 3,656 kB | *refused* | — | **4,500** |

Raw compression throughput: 30 chunks, 158.5 s for ~25 GB — **151 MB/s**, confirming D-7's ~150 MB/s.

**The bytes-per-row column is the load-independent one.** Every level costs ~4.2 kB per bucket row at
629 columns, whatever the station count or cadence. Scaled to four stations:

| Level | Per day | Unbounded, 10 years | Under §1.2's retention |
|---|---|---|---|
| `observations_1m` | 24.0 MB | **87.8 GB** | 168 MB |
| `observations_5m` | 4.81 MB | 17.6 GB | 67 MB |
| `observations_1h` | 402 kB | 1.47 GB | 36 MB |
| `observations_1d` | 18 kB | 66 MB | 66 MB |

**D-18 is corrected rather than confirmed.** D-18 put the 1-minute level at ~4.2 GB/year
uncompressed, ~42 GB for a decade, and expected compression to take it to ~9 GB. At the generated
width it is 8.8 GB/year and 87.8 GB for a decade — **2.1× D-18's figure**, which is almost exactly
the 629/398 column ratio, so D-18's number was right for its width and wrong for this schema. And the
~9 GB compressed figure does not exist at all:

| Aggregate columns | `compress_chunk` | Reported row size |
|---|---|---|
| 100 | compressed | — |
| 300 | compressed | — |
| 430 | compressed | — |
| **439** | **compressed** | — |
| **440** | **refused** | 8,168 |
| 500 | refused | 9,320 |
| 550 | refused | 10,224 |
| 627 | refused | 11,720 |

The reported size grows at ~18.9 bytes per column, which is the TOAST-pointer overhead a compressed
chunk's one-row-per-batch layout cannot avoid: the arrays are TOASTable, the pointers are not, and
they all have to fit in an 8,160-byte heap tuple. **The threshold is 439 columns**, not the
near-1600 D-19 located it at. Amendment A-W4-2.

### 6.6 What the index and the foreign key cost at insert time

```sh
# 43,200 wide rows (6 h x 4 stations at 2 s) per configuration
psql -c "CALL mw_measure.fill(4, '<from>', '<to>')"
```

```sh
psql -f ci/db/measure/55-insert-cost.sql
```

Three repetitions per configuration, 43,200 wide rows each into a fresh time range, because the node
is shared and a single sample is not a measurement. Medians:

| Configuration | min | **median** | max | Rows/s | vs baseline |
|---|---|---|---|---|---|
| no index, no foreign key | 3,341 ms | **3,345 ms** | 3,348 ms | 12,916 | — |
| `UNIQUE (station_id, time DESC)` | 3,343 ms | **3,356 ms** | 3,392 ms | 12,871 | **+0.3%** |
| …plus `FOREIGN KEY (station_id) REFERENCES stations` | 3,440 ms | **3,569 ms** | 3,747 ms | 12,103 | **+6.3%** |

The unique index is free at this precision — its median is inside the no-index configuration's own
spread. The foreign key is 6.3%, and it is also the only configuration whose spread is wide (307 ms
against 7 ms), which is what a per-row referential check against a shared buffer looks like.

`ALTER TABLE observations ADD CONSTRAINT … FOREIGN KEY` was **accepted on a hypertable with 30
compressed chunks**, in 6.0 s. So the foreign key omitted in §2.1 is omitted on failure-direction
grounds and not on cost grounds: at the production cadence of four rows every two seconds, 6.3% of
12,000 rows/s is not a number anyone would notice. Recording that is what makes the decision
reversible by someone who disagrees with the reasoning rather than with the arithmetic.

### 6.6b `station_id text` against a surrogate `int4` — §2.1

```sh
psql -f ci/db/measure/70-station-key-width.sql
```

Two hypertables, identical but for the partition column's type, 172,800 identical rows in each.

| | `station_id text` | `station_id int4` | Delta |
|---|---|---|---|
| uncompressed | 18,153,472 B — 105.1 B/row | 16,769,024 B — 97.0 B/row | **8.0 B/row** |
| compressed | 1,671,168 B — 9.67 B/row | 1,654,784 B — 9.58 B/row | **0.09 B/row** |

The compressed delta is 0.9% of the delta that an argument from `pg_column_size` alone would predict,
because `station_id` is the `compress_segmentby` key: a compressed batch stores it once, not once per
row. The 8 bytes apply only while a chunk is uncompressed — seven days under D-7 — which at four
stations is 1.4 MB/day of standing overhead and about 10 MB at any moment, not growing.

### 6.7 `cargo xtask codegen --check` and the gate

```
$ cargo xtask codegen --check
codegen --check: 14 generated files match metrics/registry.toml
```

Fourteen, not thirteen: the 5-minute level adds `generated/sql/040_observations_5m.sql` and renumbers
the two above it, so `040_observations_1h.sql` and `050_observations_1d.sql` were deleted and
`050_observations_1h.sql` and `060_observations_1d.sql` added. `codegen` writes files but does not
remove stale ones; `--check` reports them as `in the tree but not generated`, which is how they were
found. Noted as OQ-W4-4.


---

# Decisions

1. **The aggregate hierarchy has four levels, not three: `observations_1m` from raw, `observations_5m` from `_1m`, `observations_1h` from `_5m`, `observations_1d` from `_1h`.** `/span` serves four bands today and the website's default 24-hour view falls in the 5-minute one; the fixtures measure its spacing at 300,000 ms. Appended to D-11 as A-W4-1, contiguous and constructible, no deviation from D-37.
2. **A mean is stored as `sum` and `count` everywhere (OQ-3), with no per-metric `stats_agg` override.** The frozen contract serves no dispersion statistic, so the second half of W0's recommendation has no instances; `sum`/`count` is reproducible by anyone reading the schema, which D-37 requires, and `stats_agg`'s column saving does not reach the one threshold that would have made it worth the opacity (decision 4).
3. **Every level carries every aggregatable metric — OQ-W2-1 option (a), 627 expressions per level (OQ-5).** Membership can only shrink away from raw, so narrowing the minute level would narrow every level; retention achieves what OQ-5 wanted more completely, by a factor that grows with history instead of staying fixed.
4. **No compression on any aggregate level, because none is possible.** `compress_chunk` refuses at 440 columns and the levels are 629; bisected, the ceiling is **439**. D-19 located this failure near 1600 columns. `stats_agg` would reach 502 and still fail.
5. **Retention instead: 7 days on `observations_1m`, 14 on `_5m`, 90 on `_1h`, `_1d` unbounded (OQ-4).** `/span` takes no time anchor, so each level is read over exactly one trailing window and never over history. This is against W0's recommendation: 337 MB in place of 107 GB of uncompressible data nothing reads, and no information is lost because raw is permanent (D-4) and a range is rebuildable on demand.
6. **No retention on `observations`, ever, and `ci/db/schema-assertions.sql` A-8 fails the build if one appears.** The failure mode of adding one is silence for exactly as long as the interval.
7. **Raw compression stays exactly as D-7 configured it** — 216 columns is far inside the 439 ceiling. Measured 5.3× at a 76.87% hold rate, compressing at 151 MB/s.
8. **`mw_circular_mean` returns `0`, not `NULL`, for a bucket with no non-NULL `winddir`** (OQ-W1-2 option (c), W3 assumption 18). The generated finaliser returned `NULL`, which is option (b) — the one W1 calls a contract change, because `windd` is a non-optional Swift `Double` whose absence fails the decode of the whole response. `mw_mean` stays `NULL` for an empty bucket; the asymmetry is the decision.
9. **Count columns are `bigint` at every level.** `sum(bigint)` returns `numeric`, so the roll-up now casts: `nullif(sum(c_count), 0)::bigint`. Without it three of `/span`'s four bands failed to resolve `mw_mean` at all and `sqlx` would have decoded the column into the wrong Rust type.
10. **`cargo xtask migrate` emits the finalisers in every migration.** They were emitted only by the bootstrap migration, so a change to a finaliser's body — which changes no identifier and so produces no snapshot diff — reached `generated/` and never reached a deployed database.
11. **The raw table is idempotent over a replayed prefix through `UNIQUE (station_id, time DESC)` and `ON CONFLICT … DO UPDATE`** (W3 assumption 22). Permitted on the compressed hypertable in 2.31 and the chunk stays compressed; 50.7 ms for 30 rows against an uncompressed chunk and 281.4 ms against a compressed one, so replays belong in the uncompressed window. The index costs +0.3% on insert and ~29 B/row on that window only.
12. **All four levels are `materialized_only`**; three have no choice, and `_1d` is uniform with them by decision.
13. **Therefore the `/span` read path must append one bucket aggregated from raw over `[last_closed_bucket_end, now)`.** Every `/span` fixture W1 captured ends with the in-progress bucket, so omitting it shortens every response by one element and lags the "now" end of every chart by up to a bucket width. Measured at 0.44–6.89 ms. This is a requirement on W3, recorded as an assumption.
14. **`stations` is a read-side projection keyed by the wire's `text` station_id, not a surrogate `int`.** The authoritative registry stays in `config:sqlite` (W3 §3.1). A surrogate saves 8 B/row while a chunk is uncompressed and 0.09 B/row after, at the cost of a protobuf type change on field 221 and a lookup inside W3's writer.
15. **No foreign key from `observations` to `stations`** — on failure-direction grounds, not cost. It is accepted on a compressed hypertable at +6.3% insert, and its only new effect would be rejecting a reading because a projection row was missing. The low-volume tables do carry the reference.
16. **`station_metrics` is keyed by the registry's stable `key`, not the PostgreSQL column name**, and has no foreign key to a metrics table because `metrics/registry.toml` is the source of truth (D-20).
17. **`almanac_cache` is one row per record per station, not seventeen column triplets.** An absent row is an absent `omitempty` key and no rows is `{}` — the shipped empty-cache behaviour falls out of the shape instead of needing a branch. A `CHECK` lists all seventeen frozen keys.
18. **`almanac_cache.value`/`wind_dir` and every `snow_totals_cache` number are `real`, not `double precision`**, because the contract serialises them as Go `float32` and widening the column changes the digits a frozen API emits.
19. **`snow_totals_cache` has every number `NOT NULL DEFAULT 0` and no TTL, retention or recency filter.** An offline gauge returning its last known depth indefinitely is contract (Q-16); `computed_at` and `source_through` make staleness observable without being acted on.
20. **`observations.extra jsonb` is the escape hatch, `CHECK`ed to a ≤2 kB flat object, never aggregated and never indexed**, and deliberately outside the registry because the registry is for modelled metrics. The promotion path is a `[[metric]]` entry, `cargo xtask migrate`, and one backfill `UPDATE`.
21. **Indexes: `UNIQUE (station_id, time DESC)` on raw — which doubles as every `/span` and cold-read access path — plus `(station_id, bucket DESC)` on each level.** The hypertable's own time index stays for cross-station ranges. Nothing on `extra`, nothing on `station_metrics.metric_key`.
22. **`period_rain` survives as an aggregate-level column with no raw counterpart** (W1 assumption 11), as `rainincremental_total` exposed under the legacy key, asserted to roll up identically at all four levels.
23. **The rebuild procedure is `ci/db/rebuild-aggregates.sh`, and it refreshes the `/span` bands first, bottom-up, before backfilling history** (OQ-W2-2's half that is W4's). Measured: 0.87 s against 467 s, a 537× difference between "the charts are right" and "everything is materialised".
24. **Every refresh session sets `synchronous_commit = off`; ingest keeps full durability** (D-17). Worth 1.98× at this width rather than D-17's 12×, and still the difference between 15 and 8 minutes.
25. **The `/span` bands are the rebuild's unit of work, and D-6's restore is faster than D-6 implies.** Only `observations_1d` has ten years of history to rebuild, and it rebuilds from the hourly level in 0.39 s at 208 days.
26. **The 1-minute refresh policy is phase-locked with `initial_start`, 3 seconds past each boundary, `end_offset => 1 second`.** Measured lag 3.06–3.22 s against `window:rollup`'s 10-second deadline. A-9 asserts the fixed schedule, because without it the deadline is met by luck.
27. **`db-migrate-twice` is no longer trivial**: a second migration makes path B a real delta, `ci/db/seed.sql` gives it rows, `ci/db/verify-seed.sql` turns D-12/D-13/D-14 into assertions the build runs, `ci/db/schema-assertions.sql` adds nine schema properties checked on both paths, and the script now prints `WEAK:` when a run did not test an upgrade.
28. **The seed fixture is anchored to yesterday at noon, not a fixed date** — inside every retention window, and late enough that the day bucket is closed, since a fixture written today would never appear in `observations_1d` and every assertion reading that level would pass vacuously.

---

# Open questions

**OQ-W4-1. `affected_levels` drops and recreates every level when only one level's membership changed. Is that worth fixing in the generator?**

The migration for decision 1 drops and recreates `observations_1m`, whose definition is byte-identical before and after. `snapshot::affected_levels` keys off which *metrics* changed, and every metric's `levels` list changed, so all four levels are in scope. Correct under D-15 and expensive in exactly the way A-W2-2 describes: an intact minute level would have let the new 5-minute level be built from it in 59 s instead of forcing a 401 s raw rescan, and at ten years of history that difference is hours.

*Options.* (a) Leave it; §4's phase-1-first rebuild makes the user-visible part 0.87 s regardless, so the cost is background CPU. (b) Have `affected_levels` compare each level's rendered definition against the previous one and include only levels whose text changed — about ten lines, and the generator already renders both sides. (c) Have the migration generator emit the refresh ranges too, so the ordering in §4 is in the migration rather than in a script beside it.

*Recommendation:* **(b)**, as a W2 follow-up, not urgent. It is a strictly better signal than the metric-level one and it makes "add a metric to one level" cost one level's rebuild. **Not fixed here**, because `affected_levels` is W2's and the behaviour is wasteful rather than wrong.

**OQ-W4-2. Do `stationname` and `stationtype` leave the raw table, and who makes that possible? (OQ-W2-6)**

**They should.** They are denormalised duplicates of `stations` now that it exists, they are already excluded from the aggregates, and nothing reads them from `observations` that could not join. **It is not expressible today.** `[[dimension]]` has no deprecation path — `snapshot::from_registry` hard-codes `deprecated: false` for dimensions, and removing a registry entry without `deprecated = true` is FATAL with no override (`validate.rs`, `against_snapshot`). So dropping them is not a registry edit; it needs the generator to grow the concept.

*Options.* (a) A new `role` — `metadata` — meaning present in the protobuf and the Rust type, absent from the raw DDL and the aggregates. The station's self-reported name and type stay on the wire, where `Ingress::admit` legitimately uses them to maintain the projection, and stop being columns. (b) Dimension deprecation proper, mirroring metrics: identifiers retained, column kept but unwritten. (c) Leave them.

*Recommendation:* **(a), as a W2 follow-up.** It is the smaller change and it matches what the two values actually are — wire metadata, not measurements. The saving is two text values per row on the uncompressed window, so the priority is low; the reason to do it is that two sources of a station's name is the kind of thing that diverges silently. **Not done here:** the registry change is W2's and the prompt's own `Do not` section says so.

**OQ-W4-3. Should the registry gain a `jsonb` SQL type so `extra` can be a registry entry?**

`observations.extra` is created by the W4 migration and is not in `metrics/registry.toml`, because `SqlType` has no `jsonb` variant and because an escape hatch for unmodelled fields has no business having a protobuf field number. The consequence is a column on a generated table that `generated/sql/020_observations.sql` does not mention — defensible, since migrations are the applied artefact and the generated files are reference, but it is a seam.

*Options.* (a) Leave it. (b) Add `SqlType::Jsonb` and a `[[dimension]]` with the `metadata` role from OQ-W4-2, so `extra` appears in the generated DDL and the Rust type but not the aggregates or the protobuf. (c) As (b), plus a protobuf `map<string, double>` so a satellite can forward unmodelled fields.

*Recommendation:* **(a) until a driver needs it**, then **(c)**. (c) is the only version that makes the hatch useful across a hop, and today no driver reports into it. Worth deciding before W7 writes its first driver that receives an unknown field.

**OQ-W4-4. `cargo xtask codegen` writes stale files but never removes them.**

Adding the 5-minute level renumbered two generated files, leaving `040_observations_1h.sql` and `050_observations_1d.sql` orphaned. `--check` reports them (`in the tree but not generated`) and exits 1, so CI catches it, but `codegen` itself leaves a tree that `--check` rejects — a developer runs `codegen`, commits, and the gate fails on files `codegen` just made stale.

*Options.* (a) Leave it; `--check` catches it. (b) Have `codegen` delete files under `generated/` that it did not write. (c) As (b) behind `--prune`.

*Recommendation:* **(b)**, a W2 follow-up of a few lines. `generated/` is wholly owned by the generator — the header on every file says so — so deleting what it no longer generates is the invariant it already claims.

**OQ-W4-5. What should the retention windows be at production's real cadence and station count?**

Decision 5's figures — 7, 14 and 90 days — are each "the band, plus the level above's refresh window, plus margin", sized at four stations. The *reason* for each is cadence-independent but the *cost* is not: ten stations triples the per-day figures and 337 MB becomes about 850 MB, which is still nothing.

*Options.* (a) As decided. (b) Tighter: 2 days, 7 days, 30 days — still covers every band and every refresh window, at about 120 MB. (c) Make them configuration.

*Recommendation:* **(a).** The margin is what absorbs a replayed day, a paused refresh job or an operator wanting yesterday's minute data for an incident, and the thing it is being traded against is 200 MB. **(c) specifically not**: a retention window that varies per deployment makes "is this level supposed to have last March?" un-answerable from the repository.

**OQ-W4-6. Should the trailing-bucket tail query live in `mw-store` or in `mw-api-legacy`?**

Decision 13 puts a requirement on the read path without saying where in it. The tail is a storage query by shape (it aggregates raw) and a compatibility detail by motivation (it exists because the Go views were real-time aggregates).

*Options.* (a) `mw-store`'s `ReadingStore::span` returns materialised buckets plus the tail, so every caller gets the shipped shape and cannot forget. (b) `mw-api-legacy` asks for both and concatenates, keeping the quirk in the compat layer where §1.7 puts quirks.

*Recommendation:* **(a).** It is not a quirk, it is what the band means: `/span/24h` is "24 hours of 5-minute buckets", and the last one is still filling. A caller that got only complete buckets would be getting a different question answered. **W3 decides**; §1.7's boundary is the thing being weighed, and this document should not decide it from outside.

---

# Assumptions about other work items

**About W1 (frozen contract)**

1. The four `/span` bands and their boundaries in §2.3 are the contract, and `contract/fixtures/`' captured bucket spacing (60,000 / 300,000 / 86,400,000 ms) is the shipped server's output rather than a reconstruction. Decision 1 rests entirely on this.
2. `/span` really takes no time anchor — only `station`, an ignored `station_id` and an unhonoured `format` (§2.1). **Decision 5 rests on this and fails without it**: a `/span` that could ask for an arbitrary window would need the minute level over all history, and the minute level cannot be compressed.
3. `AlmanacData`'s seventeen fields are all `omitempty` pointers and an empty cache really returns `{}` (§3, fixture `almanac-empty`), which is what makes decision 17's row-per-record shape equivalent to the shipped behaviour rather than merely similar.
4. `/snow` has no recency filter and that is contract rather than defect (Q-16, fixture `snow-device-stale`). Decision 19 declines to add one on that basis.
5. `value` and `wind_dir` on `/almanac`, and all seven `/snow` numbers, are Go `float32` on the wire and §6.1's formatting rules apply to them. Decision 18 chose `real` columns to match.
6. OQ-W1-2 option (c) is still W1's preferred answer. Decision 8 implements it; if W1 has since preferred (b), the finaliser is a one-line change and `ci/db/schema-assertions.sql` A-3 inverts.

**About W2 (metric registry and codegen)**

7. A fourth `[[level]]` with `source = "1m"` is exactly the extension `docs/metric-registry.md` §1.3 says it is ("if someone wants it back it is a `[[level]]` with `source = "1m"`"), and a `levels` change is `RebuildAggregates` rather than `BREAKING` — which is what let decision 1 go through `cargo xtask migrate` with no flag.
8. The two generator edits this work item makes are W4's to make because the behaviours they implement are assigned to W4, and W2 will keep them: `functions_sql`'s zero-returning `mw_circular_mean` branch (decision 8) and `registry.rs`'s `::bigint` cast on the count roll-up (decision 9). Both are flagged in `## Proposed amendments`, both are commented in place with the reason, and neither changes the registry *format*.
9. `cargo xtask migrate` emitting the finalisers in every migration (decision 10) is additive and idempotent. If W2 would rather have a dedicated `cargo xtask functions` command, the requirement is only that a finaliser change reach a deployed database by *some* route that a migration-only deploy takes.
10. `generated/rust/aggregates.rs` is what W3's query layer reads for column types (W2 assumption 15), which is why decision 9 fixed the SQL to match the Rust rather than the Rust to match the SQL.
11. `observations` and `observations_1m`/`_5m`/`_1h`/`_1d` are accepted as generator constants (W2 assumption 10). Nothing here renames one.
12. The 7-day `chunk_time_interval` and 7-day raw compression policy are accepted as-is (W2 assumption 11). They are not measured optima and nothing in this document depends on their exact values — except that decision 5's 7-day minute-level retention and the raw compression delay happening to match is a coincidence, not a constraint.
13. OQ-W2-1 option (a) is taken (decision 3) and option (b) is not, so no per-metric source level is needed and no registry concept is added.
14. **A-W2-2 is accepted and acted on, not just noted.** Decision 23 and §6.3b are OQ-W2-2's recommendation (i) implemented; recommendation (ii), the correction to D-9's claim, is the owner's and is restated in A-W4-3.

**About W2a (component and capability model)**

15. `window:rollup`'s 10-second deadline from the minute boundary (assumption 14) is a real deadline and 3.14 s of measured lag is comfortably inside it. `Phase::Aligned` does not need revisiting.
16. `window:rollup` reads 1-minute buckets from `observations_1m` and never needs one older than an hour, so 7-day retention on that level is invisible to it.
17. Daily, monthly and yearly boundaries are computed in the station's zone (assumption 16). `stations.timezone` is where that comes from on the reading side, and no database job anywhere in this schema calls `date_trunc('day', NOW())`.

**About W3 (service architecture)**

18. **`/span` appends one bucket aggregated from raw over `[last_closed_bucket_end, now)`** (decision 13). Without it every `/span` response is one element short of the shipped one and the "now" end of every chart lags by up to a bucket width. The tail costs 0.44–6.89 ms; OQ-W4-6 asks where it lives.
19. `Ingress::admit` resolves the station before a reading reaches the writer (§2.6), which is the invariant decision 15 declines to re-enforce with a foreign key. If that order ever changes, the foreign key argument changes with it.
20. The ingest path maintains `stations` and `station_metrics` as it resolves and writes — upsert on admit, `last_seen` on write. They are projections and nothing else populates them. This is a small piece of writer behaviour §2.5 does not currently name.
21. The writer stores NULL for an unreported metric (§2.5), which is what makes decision 8 necessary rather than theoretical.
22. §2.3's cancel-safe replay uses `ON CONFLICT (station_id, time) DO UPDATE` (assumption 22) against the unique index decision 11 creates, and the prefix it replays is seconds old, so it lands in the uncompressed window. A replay reaching a compressed chunk works but costs 5.5× as much (281 ms against 51 ms for 30 rows) and is a TimescaleDB 2.31 property rather than a general one.
23. `snow:*` writes `snow_totals_cache` in inches (the `/snow` unit), not the millimetres `snowdepth` carries on `/latest` (Q-4). The column comment says so; the conversion is W3's.
24. `snapshot:latest` reads `stations` for display name, type, latitude, longitude and timezone rather than reading `stationname`/`stationtype` off a reading — which is what makes OQ-W4-2's eventual column drop contained.
25. Aggregate refresh duration is observable for §5.10's `mw_aggregate_refresh_seconds` (assumption 24): `timescaledb_information.job_stats` carries per-job run duration, which is what §6.2's 28–53 ms came from.
26. `ReadingStore` queries the levels at the four band boundaries §8.2 names, and the per-level `(station_id, bucket DESC)` indexes in decision 21 are built for that predicate.

**About W5 (management UI)**

27. Any chart W5 adds stays inside the four `/span` bands, or reads `observations_1d`. A W5 view wanting minute resolution over last March would find the minute level retained for 7 days — rebuildable on demand (`ci/db/rebuild-aggregates.sh --from … --to …`), but not instant. This is the one consequence of decision 5 that reaches another work item.

**About W7 (collector)**

28. The collector's writer uses the same `ON CONFLICT` path and the same NULL-for-unreported rule; nothing in this schema distinguishes a collector-written row from a satellite-written one.
29. No driver currently reports a field with no `[[metric]]` entry, so `observations.extra` is unused at first boot. If one does, OQ-W4-3 becomes a decision rather than a question.

**About W8 (CI and release)**

30. `ci/gates.toml`'s `db-migrate-twice` entry is unchanged — same id, same `arms_when`, same `cmd`. What the command *does* grew; the manifest did not need to.
31. `pr.yml`'s `database` job keeps `/usr/lib/postgresql/17/bin` on `PATH`, because the runner's `/usr/bin/pg_dump` is a postgresql-common wrapper resolving to a stopped PostgreSQL 16 cluster. Nothing here touches it.
32. The gate's `psql` can create and drop databases and run `CREATE EXTENSION timescaledb`, which it already does.
33. `ci/sql/assert-timescaledb-version.sql` needs no change: decision 2 declines `stats_agg`, so its Toolkit check stays a warning rather than becoming an error.

**About W9 (data migration)**

34. **Create the unique index as part of the raw import, not after it.** 4.0 s over 35.9 M rows is cheap, but doing it on a live table takes a lock, and `ON CONFLICT` is unavailable until it exists — which W9's import probably wants.
35. W9 decides what to do about ~294 days of rollup with no raw behind it. Under decision 5, importing it into `observations_1m` would be dropped by retention within a week; it belongs in `observations_1d`, or in raw as reconstructed readings, or nowhere. This document does not decide it.
36. If production turns out to be PostgreSQL 14 (OQ-1), the table in §5.2 is the list of this document's decisions that move — and a TimescaleDB older than 2.9 cannot build this hierarchy at all, which makes the upgrade part of W9 rather than an option.

**About W6 (reconciliation)**

37. All three amendments below are proposals. This document carries on under D-11, D-17 and D-18 as written, which is why §0 appends a level rather than rewriting D-11 and why §6.3 reports a ratio that disagrees with D-17 rather than editing it.
38. A-W2-1 is resolved by decision 3 and needs no further action: W2 asked W4 to choose among three options, and option (a) is chosen with the reasoning in §1.3.
39. There is **no entry for W6's deviation ledger**. Decision 1 preserves the frozen contract rather than deviating from it, and decision 13 is the one place where this schema alone does not reproduce the shipped output — closed by a read-path requirement, not by a contract change.

---

# Proposed amendments to `DESIGN.md`

None is applied. All three are flagged at the top of this document, and this document carries on under the existing decisions.

**A-W4-1 — D-11's hierarchy has four levels, not three.**

*Which:* D-11 ("Aggregates are hierarchical: 1-minute from raw, 1-hour from 1-minute, 1-day from 1-hour") and its assumption 16.

*What was found:* `/span` reads four tables, not three, and the missing one is the 5-minute level that serves every span between 6 and 48 hours — including `/span/24h`, the website's default view. This is not inferred from the Go source: the captured fixtures in `contract/fixtures/` put `span-24h-five-minute-buckets`' bucket spacing at exactly 300,000 ms. W1 assumption 10 and W3 assumption 19 both already state four bands, so D-11 is the only document that says three.

*What replaces it:* the hierarchy is 1-minute from raw, **5-minute from 1-minute**, 1-hour from 5-minute, 1-day from 1-hour. D-11's mechanism is untouched — each level still reads the level below it — and assumption 16 already licensed this on the condition that a 5-minute level derive from the 1-minute level, which it does, exactly five buckets to one.

*Why the original was wrong rather than merely incomplete:* assumption 16 reads as a hypothetical ("the Go version's 5-minute level is not assumed to carry forward"), and under D-37 it is not a hypothetical — it is a frozen band. A reader implementing D-11 as written ships three levels and then has to choose which wrong thing `/span/24h` does: quarter the points in the dominant chart, or quintuple the response size. Neither is reachable without breaking the contract, so the hypothetical had to become a decision before anyone wrote the query layer.

**A-W4-2 — OQ-4's recommendation is unavailable, and D-18's size figure is for a narrower schema.**

*Which:* OQ-4 ("compress it and keep it unbounded"), D-18 ("~4.2 GB/year uncompressed — roughly 42 GB for a decade … must therefore either be compressed (measured 4.58× on a 398-column materialisation) or carry bounded retention"), and D-19's third interaction ("a near-ceiling aggregate cannot be compressed at all").

*What was found:* two things, and the second explains the first.

- `compress_chunk` on `observations_1m` fails: `row is too big: size 11720, maximum size 8160`. Bisected, the widest compressible continuous aggregate is **439 columns**; the generated levels are 629. D-19 located this failure "near the ceiling", citing a 1600-column view rejected at an estimated 28,852 bytes. The real threshold is a quarter of the way to the ceiling, which is why production's 397-column views compress and these do not. Narrowing is not available (membership can only shrink away from raw) and `stats_agg` reaches only 502.
- At the generated width the minute level is 4,175 bytes per bucket row — 8.8 GB/year and 87.8 GB for a decade at four stations, **2.1× D-18's figure**, which is almost exactly the 629/398 column ratio. D-18's number was right for its width.

*What replaces it:* OQ-4's other arm. 7-day retention on `observations_1m`, 14-day on `_5m`, 90-day on `_1h`, `_1d` unbounded — 337 MB in place of 107 GB. And a sharper reason than "aggregates are disposable": `/span` takes no time anchor, so each level is read over exactly one trailing window and never over history. D-4's hole is in *raw*, and raw is permanent, so an evicted minute bucket is an evicted cache entry rather than lost information.

D-18's qualitative claim stands and is strengthened: the minute level is not free, it is *less* free than D-18 says, and the escape D-18 offered does not exist.

*Why the original was wrong rather than merely inconvenient:* OQ-4's recommendation is conditional on a measurement ("confirm that compressed-aggregate query latency is acceptable"), which reads as a performance question with a probable answer. It is not — the configuration cannot be created. An implementer following OQ-4 would write `ALTER MATERIALIZED VIEW … SET (timescaledb.compress = true)`, which *succeeds*, and then watch a compression policy job fail silently every day while the minute level grew unbounded. D-19 is wrong in the direction that hides this: it says the problem is near the ceiling, so nobody at 629 columns thinks to check.

**A-W4-3 — D-17's 12× is a property of a narrower materialisation, and the refresh is not purely fsync-bound.**

*Which:* D-17 ("A full refresh of a 1-minute aggregate over 36 million raw rows took 306 s; the identical aggregation run as a plain query took 2.4 s. The ~125× difference is not CPU … It is WAL fsync … relaxing commit durability brought 306 s down to 25.6 s").

*What was found:* at 627 expressions over 35,942,400 rows, the same comparison gives 758.5 s with `synchronous_commit = on` and 401.4 s with it off — **1.89×, not 12×** — while the identical aggregation as a plain query is 10.0 s single-threaded. Decomposed with the same data: ~10 s of arithmetic, +96 s to materialise 1.2 M 629-column rows into a plain table, +295 s of continuous-aggregate refresh machinery, +357 s of WAL fsync.

*What replaces it:* a qualification. Relaxing commit durability for a rebuild is still correct and still worth doing — it halves the longest operation in the system — but fsync is roughly half the cost of a durable refresh rather than nearly all of it, and the residue after relaxing it is 32× the plain query, not ~10×. "It is WAL fsync, not CPU" should read "it is not CPU: roughly half is WAL fsync and most of the rest is the refresh's own write path, both of which scale with the materialisation's width."

*Why the original was wrong rather than merely dated:* the 12× is quoted in `generated/sql/090_rebuild_aggregates.sql`'s own header, so it is the number an operator reads at the moment they are deciding whether a rebuild will finish inside a maintenance window. Planning a 15-minute operation from a figure that promises 2 is how a deploy overruns. The decision survives; the capacity figure does not — and the thing that actually makes a rebuild bearable is not durability but §6.3b's ordering, which is 537×.
