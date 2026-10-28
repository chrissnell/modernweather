# ModernWeather — DESIGN

The shared context every later work item reads. It records **decisions as constraints, with
rationale**, so that six agents working in parallel make the same assumptions.

**What this document is not.** It contains no code. No trait definitions, no signatures, no buffer
sizes, no module layout, no DDL. It says *what* and *why*; `docs/service-architecture.md` (W3) says
*how*, and `docs/component-model.md` (W2a) says what a component is. If you came here looking for an
interface, you are in the wrong document.

Decisions are **never edited in place**. See `## Amendments` at the end for the protocol.

---

## Read this first — what the spike changed

`docs/spike-findings.md` (W0 Part 1) measured the assumptions this document asserts. Four of them
needed correcting, and the corrections are folded into the decisions below. They are called out here
because later work items that skim this document will otherwise inherit the uncorrected version from
the planning discussion.

| | Planned | Measured | Where |
|---|---|---|---|
| 1 | Aggregates store `sum` and `count` | Necessary but insufficient — `sum()` over a `real` column returns `real`, and the hierarchy then disagrees with a direct scan in 55 of 56 cases. Accumulators must be widened to `double precision` | D-14 |
| 2 | Compression is 10–12× | 1.89× to 13.74× depending only on how often consecutive readings repeat. 11.2× reproduced under a realistic sensor model, so the decision stands; the capacity *number* is unverified until someone measures production | D-10 |
| 3 | Aggregates are disposable and rebuilding is cheap | True, but a full refresh costs ~125× the query it runs and is fsync-bound. Rebuilds must run with relaxed commit durability | D-17 |
| 4 | Wide layout is cheap because NULLs cost one bit | True of raw. **Not** true of aggregates — `count()` returns 0, never NULL, and at production width the count columns are a third of the materialised view | D-16 |

And the one that mattered most, confirmed rather than corrected: **adding a nullable column to a
hypertable with ~1 GB of compressed chunks took 11.6 ms and decompressed nothing.** The entire "adding
a metric is cheap" premise, on which the wide layout and the indefinite retention both rest, holds.

---

## 1. Language and runtime

**D-1. The implementation language is Rust, on the tokio runtime.**

Greenfield. The Go implementation stays deployable and in production throughout the rewrite; nothing
in this program may require taking it down.

**D-2. The deployment target is PostgreSQL 17 with TimescaleDB 2.31 and `timescaledb_toolkit`, and
the version is pinned, not floating.**

Rationale: three of the six load-bearing spike answers are version-sensitive. Nullable
`ADD COLUMN` on compressed chunks without decompression, continuous aggregates built on other
continuous aggregates, and `stats_agg`/`rollup` all require recent TimescaleDB, and the last requires
the Toolkit extension, which is absent from the plain `timescale/timescaledb` images. A deployment
that silently lands on an older server loses the premise of the storage design rather than merely
performing worse. The required minimum must be asserted at startup and in CI, not assumed.

---

## 2. Storage

**D-3. Storage is a wide table — one column per metric — not a narrow/EAV
`(time, station, metric_id, value)` design.**

This is counterintuitive enough to be worth writing down properly, because it will be re-proposed
otherwise.

PostgreSQL does not store a NULL value. It stores one bit in the row's null bitmap. A wide row
therefore costs roughly `42 + 4 × populated` bytes **regardless of how many columns are declared** —
a declared-but-NULL column is a bit, not a value. A narrow row costs roughly 44 bytes *per metric*,
because every metric pays for a fresh 24-byte tuple header plus a repeated timestamp, station id and
metric id. Narrow measured 7–9× worse on density, **with no crossover point**: there is no column
count or sparsity at which narrow wins.

The spike confirms the shape of this: a 216-column table with 30% of columns populated measured
~400 bytes per row, which at four stations on a 2-second cadence is 24.7 GB/year — the same order as
production's measured 19 GB/year on a narrower table.

The wide schema's genuine problems are maintainability, not bytes: a metric is defined in many
places, and keeping them consistent by hand does not work. That is solved by generation (D-11), not
by changing the storage layout.

**D-4. Raw readings are retained indefinitely. Compression is enabled from day one, segmented by
station and ordered by descending time.**

Rationale and consequence, in that order, because the consequence is the part that shapes everything
else.

Production currently runs a 365-day retention policy on raw readings, which fires daily. Roughly 294
days of history already exist only as rollups with no raw data behind them, and that number grows by
one every day. Retention is the direct cause.

Once raw data is permanent, **the continuous aggregates hold no information that does not exist
upstream.** They stop being data and become a cache. That is what makes adding a metric cheap: add a
column, regenerate the aggregate, refresh. It is also what retires an entire category of complexity
(D-5).

**D-5. The versioned-aggregate and union-view scheme is rejected.**

It was considered as a way to add metrics without losing the history held in old aggregate versions.
With raw retained indefinitely there is no such history to lose, so the scheme buys nothing and costs
a permanent increase in query and schema complexity. Recorded explicitly so it is not reinvented.

**D-6. Backup and restore is `pg_dump` plus an aggregate refresh. No bespoke tool is built.**

Because raw is permanent and aggregates are derived, a dump of the raw hypertable plus a refresh on
restore is a complete backup. The Go `weather-backup`/`weather-restore` pair is retired rather than
ported: it was hard-coded to a single unqualified select against one table and never covered the
continuous aggregates at all, so it was an incomplete backup even for the schema it was written for.
The procedure is documented; the tool is not rebuilt.

**D-7. Compression is configured on the raw hypertable at creation, not added later as an
optimisation.**

Production has compression configured nowhere. Enabling it on an existing multi-gigabyte hypertable
is a migration; enabling it on an empty one is a line of DDL. The spike measured compression
throughput at ~150 MB of input per second, so retrofitting is possible but there is no reason to
choose it.

**D-8. Schema changes happen solely through migrations. There is no DDL at startup.**

Rationale: the Go version does both, and because its startup path uses `CREATE TABLE IF NOT EXISTS`
and `CREATE MATERIALIZED VIEW IF NOT EXISTS`, its schema has been frozen at first-run shape ever
since. 58 weather migrations exist and not one of them alters the hypertable or an aggregate. The
source's table definition has drifted to 215 columns while the production table sits at ~143 — the
extra columns exist in the code and not in the database, and nothing reports the discrepancy. The
`IF NOT EXISTS` idiom is what made that silent.

**D-9. Adding a metric is an `ALTER TABLE ... ADD COLUMN` of a nullable column, and migrations that
do it take a short lock timeout and retry.**

Measured: 11.6 ms against ~1 GB of compressed chunks, with every chunk still compressed afterwards
and pre-existing rows reading back NULL (or the declared default) correctly. Twenty columns in one
statement took 93 ms.

The caveat is the lock, not the work. The statement takes `AccessExclusiveLock` on the hypertable,
every chunk and every compressed mirror — 63 relations on the spike instance — for the duration. It
is instant but it queues behind long-running readers and blocks everything while it waits. A metric
migration must therefore be prepared to fail fast and retry rather than stall the API.

**D-10. The planning figure of 10–12× compression is treated as an estimate pending one measurement
on production, and no capacity decision depends on it.**

The spike measured the same schema, the same 30% column population and the same row count four times
with different value dynamics and got 1.89×, 3.36×, 11.24× and 13.74×. The only variable that
explained the spread was the fraction of consecutive readings that are identical to their
predecessor — 0%, 33%, 86.5%, 87.7% respectively — which is precisely what TimescaleDB's float
encoding exploits.

A realistic held-sensor model reproduces the planning number, so the decision is sound. What is not
established is the *figure*: against production's 19 GB/year, ten years of raw is ~14–17 GB at the
expected hold rate and ~100 GB at the entropy ceiling. **Both are affordable**, which is why this
qualifies a number rather than reopening a decision. Nobody should write "16 GB for ten years" into a
capacity plan until the hold-rate query in `docs/spike-findings.md` Q3 has been run against
production.

---

## 3. Aggregates

**D-11. Aggregates are hierarchical: 1-minute from raw, 1-hour from 1-minute, 1-day from 1-hour.**

Rebuilding an upper level then rescans a small input rather than all of raw. Measured at 217 days of
history: rebuilding the daily level from the hourly level took 0.037 s against 1.92 s from raw — 52×,
and the ratio holds as retention grows because both sides scale linearly. With raw retained forever,
that ratio is the difference between a rebuild being routine and being a maintenance event.

Production does the opposite: all four of its aggregate views read directly from the raw table, so
every rebuild rescans everything.

**D-12. Nothing in the aggregate hierarchy stores a finished average.**

`avg(avg(x))` is wrong whenever bucket counts differ, which happens as soon as a reading is missing.
This is measured, not asserted: over 868 day/station pairs the averaged-average disagreed with the
direct average in 658 of them, and in a window with deliberately uneven bucket counts (2 to 23
readings per minute) it was wrong in **all 56**, maximum error 0.114.

Production's daily view contains 135 `avg()` columns and 3 `sum()` columns. Those averages cannot be
rolled up, which is the mechanical reason its views must each read from raw.

**D-13. Aggregates store composable state. `min`, `max` and `sum` compose natively; a mean is stored
as a sum and a count, or as a two-step aggregate state.**

Both options were verified against a direct-from-raw control. Reconstructing the mean from a rolled-up
`sum` and `count` was bit-identical in 56 of 56 uneven-bucket pairs. TimescaleDB Toolkit's
`stats_agg` with `rollup` was bit-identical on the mean in 868 of 868 pairs, exact on the count, and
within 6 × 10⁻¹⁴ on the standard deviation — and it yields variance and standard deviation for free
from one column instead of two. Either is correct. W4 chooses, and records which.

The circular-average pattern the Go implementation already contains — accumulate the sine sum, the
cosine sum and a count; finalise with an arctangent — was verified through all three levels against a
direct computation and agreed to within 3 × 10⁻¹² degrees. **That pattern carries forward.** Its
plain `avg` columns do not.

**D-14. Sums are accumulated in a type wider than the column being summed.**

This is the spike's first correction. PostgreSQL's `sum(real)` returns `real`, and `float4` addition
is not associative, so a hierarchy adds the same numbers in a different order than a direct scan does
and the two results differ — in 55 of 56 uneven-bucket pairs, by up to 1.55 × 10⁻⁴. The error is
small but it is *visible*: the same question returns a different answer depending on which aggregate
level served it, which a frozen public API cannot tolerate.

Widening the accumulator eliminates it rather than reducing it. A `float4` carries 24 bits of
mantissa and a `float8` carries 53, so summing `float4` inputs into a `float8` accumulator is exact
until the bucket holds on the order of 2²⁹ readings — far beyond anything this system will see. The
measured result was bit-identical in 56 of 56, error exactly zero.

**D-15. Aggregates are disposable derived data. The supported way to change one is to drop it,
regenerate it and refresh.**

This follows from D-4 and is the reason D-4 matters. There is no in-place aggregate migration, no
versioning, no backfill tooling. If an aggregate is wrong, it is rebuilt.

**D-16. Generated aggregates emit nullable counts, not raw `count()`.**

The spike's fourth correction, and the one most likely to be missed. `count()` returns 0, never NULL.
On a table that is 70% NULL, every count column in a wide aggregate is therefore fully populated and
almost entirely zeroes — which throws away exactly the null-bitmap density that justifies the wide
layout in the first place. Measured on a 398-column 1-minute view over 30 days: 347 MB with plain
counts, 234 MB with counts nulled at zero, 159 MB with no count columns at all. The middle option
costs 0.9 s of extra refresh time and saves a third of the view.

**D-17. Aggregate rebuilds run with relaxed commit durability.**

The spike's third correction. A full refresh of a 1-minute aggregate over 36 million raw rows took
306 s; the *identical aggregation* run as a plain query took 2.4 s. The ~125× difference is not CPU —
the same query takes 2.39 s with parallelism disabled — and it is not fixable by refreshing in
smaller ranges, which measured 299 s. It is WAL fsync: `pg_stat_activity` showed the refresh waiting
on `IO / WalSync` on every sample, and relaxing commit durability for the refresh session brought
306 s down to **25.6 s**.

Relaxing durability here is safe for exactly the reason the aggregates are disposable: the data is
derivable from raw, so a crash mid-rebuild costs a re-run, not information. This is a property of the
rebuild path specifically — ingest keeps full durability.

**D-18. The 1-minute aggregate is sized and retained deliberately. It is not free.**

At production width a 1-minute aggregate materialises ~4.2 GB/year uncompressed — roughly 42 GB for a
decade, which is *larger than the compressed raw hypertable it derives from*. "Aggregates are free
because raw is retained" is true of the hourly and daily levels and false of the minute level. The
minute level must therefore either be compressed (measured 4.58× on a 398-column materialisation) or
carry bounded retention, and it should contain only metrics something actually queries at 1-minute
resolution.

**D-19. The generator enforces the column ceiling; it does not discover it at deploy time.**

PostgreSQL permits 1600 columns per table, confirmed by probe on this build. A continuous aggregate
can therefore emit at most 1598 aggregate expressions plus its two grouping columns; 1599 fails with
a column-limit error and anything past 1662 fails earlier with a target-list error. Production's
397-column views sit at a quarter of that, and four aggregates per metric over ~200 metrics needs
~790 columns, so there is real headroom — but the failure mode is a deploy-time error on generated
SQL, which is the worst place to find it.

Three interactions bind tighter than the raw number and all three belong in the generator's rules:

- **Dropped columns never return their slot.** After the spike's add/drop experiments the table had
  216 live columns occupying 239 of the 1600 slots. Only a full table rewrite reclaims them, which on
  a compressed hypertable of this size is not an operation anyone performs. The budget must be
  counted against *allocated* identifiers, not live ones.
- **Compression mirrors every column, including the dropped ones, and adds five of its own.** A
  compressed hypertable's effective ceiling is ~1595, and a dropped column is charged twice.
- **A near-ceiling aggregate cannot be compressed at all.** Compression packs each column's batch
  into a single row, so a wide view's compressed row overflows PostgreSQL's ~8 kB row limit long
  before its column count reaches 1600. The 1600-column view the spike built was rejected for
  compression with an estimated row size of 28,852 bytes against a maximum of 8,160.

---

## 4. The metric registry

**D-20. One registry, authored in TOML, is the single source of truth for every metric.**

DDL, migrations, aggregate SQL, Rust types, protobuf, OpenAPI and TypeScript types are all generated
from it. Generated output is committed to the repository and CI-checked for drift.

Rationale: in the Go version a metric is defined in roughly eight places, and nothing keeps them
consistent. The 215-column source definition against the ~143-column production table (D-8) is the
same failure in a different form.

**D-21. Column names and protobuf field numbers are allocated once and never reused. Removing a
metric marks it deprecated; it does not free its identifier.**

Rationale, and there are now two of them.

The original: the Go version's protobuf numbering is already damaged. Station metadata is wedged in at
fields 188–192 and relays 8–10 at 193–195, because fields 1–187 were consumed by the time anyone
needed them. Reusing a number silently reinterprets historical data on any client that has not been
rebuilt.

The one the spike added: PostgreSQL enforces this whether the registry does or not. A dropped column
keeps its attribute number forever (D-19), so "freeing" an identifier is not something the database
will cooperate with. The registry is encoding a constraint that already exists.

**D-22. The registry format itself, and the shape of the generator, are W2's decisions, not this
document's.**

Recorded so that nobody designs it here.

---

## 5. Runtime shape

These are constraints on the component model, not the model itself. **W2a defines what a component
is; W3 defines the mechanics.** Nothing in this section names a type or a method.

**D-23. The word "controller" is retired.**

It names a layer rather than a behaviour, which is how three unrelated shapes accumulated behind one
interface. It is replaced by a single component abstraction in which *when something runs* and *what
it needs* are declarative.

**D-24. A component is a cancellable task that owns its resources, and a set of components is
supervised. Stopping is cancellation plus ownership, not a stop method.**

Rationale: the Go `Controller` interface has exactly one method, to start. There is consequently no
way to stop anything — removing a controller can only delete a map entry and log that it will stop on
the next application restart, while its listeners and goroutines leak.

**D-25. A component declares a trigger. It does not own a timer.**

Of eleven long-running components in the Go version, only two are driven by the arrival of a reading.
The rest are periodic or long-running services, none of them subscribe to readings, and every one
polls the database on its own hand-rolled ticker. Moving scheduling to the supervisor makes "when did
this last run" inspectable state rather than a local variable inside a loop.

**D-26. Dependencies between components are declared, not implicit. Components state the capabilities
they require and provide, and the supervisor resolves a startup order.**

Rationale: today the forecast and alerts endpoints silently depend on the Aeris component; sunrise and
sunset depend on a precompute buried in the REST controller's constructor; APRS depends on the station
registry; and the management API depends on the application object that owns it, which is a genuine
cycle. None of these is written down anywhere.

**D-27. A missing dependency means waiting, not refusing.**

Four Go controllers validate the TimescaleDB configuration and refuse to construct without it, so a
transient database problem at boot disables the uploaders until the process is restarted by hand. A
component whose capability is unavailable sits in a waiting state and starts when that capability
becomes healthy.

**D-28. Configuration reaches components by watch or broadcast, never through shared mutable state.**

Rationale: the Go REST controller holds the website list, device list, device-name map,
devices-by-website map and TLS configuration as plain fields with no mutex, and the management API
replaces them wholesale while in-flight requests read them. In Go a concurrent map read and write is
an unrecoverable fatal error — so saving a website while serving traffic can kill the process. This is
a live data race in production, not a theoretical one.

**D-29. Data flows sources → fan-in → transform/enrich → fan-out → sinks, with an explicit policy for
what happens when a sink cannot keep up.**

Rationale: the Go reading distributor performs a blocking, unguarded send per engine, so a stalled
database back-pressures the serial device read loops and can hang shutdown. "What happens when a sink
is slow" must be a stated policy per sink rather than an emergent property of an unbuffered channel.

**D-30. Sinks are composable and each carries its own delivery policy. Durable sinks must not lose
data; rate-limited upload sinks coalesce to the newest reading.**

The asymmetry is deliberate and worth stating: backfilling Weather Underground with an hour of
readings after an outage is wrong — the service wants current conditions, not history — whereas
dropping a reading from the database is never acceptable.

**D-31. Transport is orthogonal to role. gRPC and REST are codecs, not kinds of component.**

Rationale: in the Go version, sending readings to a peer is implemented as a *storage engine* and
receiving readings from a peer is implemented as a *weather station*. The collector role — the thing
that actually exists — has no name anywhere in the codebase.

---

## 6. Crates

**D-32. The following crates are the default choice for the idioms below. A substitution is fine but
must be recorded as an amendment.**

Recorded so that six agents reach for the same ones rather than each choosing. Divergence here is
precisely what W6 would otherwise have to reconcile.

| Concern | Crate |
|---|---|
| Async runtime, cancellation, framing | `tokio`, `tokio-util` |
| Sink composition, rate limiting | `tower` |
| HTTP and gRPC on one listener | `axum`, `tonic` |
| Both databases | `sqlx` |
| One schema for REST-JSON and gRPC-protobuf | `prost`, `pbjson` |
| Errors | `thiserror` per module, `anyhow` at the binary boundary |
| Secret wrapper types | `secrecy` |
| Constant-time comparison | `subtle` |
| Password hashing | `argon2` |
| Fixed-layout binary packet decode | `zerocopy` |
| Published snapshots | `arc-swap` or `tokio::sync::watch` |

Two of these are load-bearing rather than taste. Generating REST-JSON and gRPC-protobuf from one
schema replaces roughly 591 hand-written mapping lines across two conversion files, which is 591
opportunities for the two surfaces to disagree. And the Davis LOOP packet is a fixed little-endian
structure currently decoded through reflection-based binary reads; it is a struct, and decoding it as
one is both faster and checkable.

---

## 7. Secrets, errors, authentication

**D-33. Secrets use a wrapper type whose debug and display representations redact. Credential
comparison is constant-time.**

Rationale: the current management token is a UUID compared with string equality, printed in cleartext
at startup *and* on every failed authentication attempt, with the session cookie set to the token
itself.

**D-34. Errors are typed per module and preserve their cause chain.**

Rationale: the Go version formats wrapped errors with `%v` in 767 of its 1,174 error-construction
sites, which flattens the error to a string and destroys the chain. Every one of those is a place
where a root cause cannot be inspected programmatically.

**D-35. Administrative authentication is a username and password. One administrator, no roles, no
multiple accounts.**

Owner decision. Argon2id password storage; sessions distinct from credentials; a defined first-start
password-setup process; a local CLI recovery path; and no default password, ever. **W5 §3 owns the
design.**

Rationale for replacing what exists rather than carrying it forward: the current scheme sets the
session cookie to the API credential itself, so a browser session cannot be revoked without rotating
the credential every integration uses.

This touches **only** the management API. The public API is unauthenticated today and is frozen by W1,
so there is no contract risk here.

---

## 8. Vocabulary and external surface

**D-36. Internal vocabulary is not user-facing vocabulary.**

The component model's names are chosen for the code. The management UI maintains its own
plain-language vocabulary with an explicit translation table (**W5 §0**), and an internal term
appearing on screen is treated as a bug, not a documentation gap.

Rationale: the owner reports that "controller" and "storage backend" are already not obvious to new
users. The new internal names are chosen for precision in code and would be *worse* on screen, not
better.

**D-37. The legacy external contract is frozen. The iOS app and the existing weather website keep
working unchanged. New endpoints are additive only.**

**W1 captures the exact surface.** Do not enumerate it here or anywhere else.

**D-38. The public weather website is carried forward as-is, not redesigned**, with one deliberate
exception: the JavaScript currently generated by Go templating is replaced by static JavaScript plus
a JSON configuration endpoint.

**D-39. A new v2 API is out of scope for this design program.**

W1 and W3 both defer it to "a later work item". That work item does not exist in this set, which is
deliberate — but W6 must record it as a named future item rather than leaving a dangling forward
reference.

---

## 9. Tools and CI

**D-40. The ancillary Go tools are not ported.**

Only three of twelve command binaries were ever shipped. Specifically: the provisioner stays in Go and
needs updating for the new schema; the migrator retires with the Go daemon; the four
emulators/simulators **stay in Go deliberately**, as independent-implementation test infrastructure
for the Rust drivers; the remainder are unused and belong in scratch.

The emulators' value is precisely that they are not Rust. A driver tested against a simulator written
in the same language from the same understanding tests that understanding against itself.

**D-41. A PR gate exists from the first commit: build, test, format, lint, and concurrency-checked
tests.**

Rationale: the Go repository's only workflow is release-on-tag. That is why 45 files fail the standard
formatter and why both data races described in D-28 and D-29 went unnoticed for the life of the
project. The gate is not a late-stage quality initiative; it is the thing that would have caught what
is already broken.

**D-42. The required database version is asserted in CI and at startup.**

Follows from D-2. Three spike answers that the storage design depends on are version-gated, and a
server that silently predates them turns a design premise into a runtime surprise.

---

## 10. Repository setup (W0 Part 2 — recommendations only)

The owner has decided on a new Multica project and a new repository rather than a branch of the Go
tree. That is the direction; it is not re-argued here. **No repository was created by this work item.**
The remaining questions are narrower.

**R-1. Reference the Go repository by documented URL. No submodule, no required sibling checkout.**

Recommended because it is the simplest thing that works and the only access anyone needs is read
access. A submodule pins a commit, which has to be bumped, drifts when it is not, and makes every
clone of the new repository drag in the old one. A required sibling checkout silently breaks anyone
who does not have it. The work-item prompts need to *read* Go source; a URL and a commit reference in
prose satisfies that. The one thing worth adding is a short "where things are in the Go tree" map in
the new repository, so that six agents are not each rediscovering which file holds the schema.

**R-2. Generated code lives in-tree under a clearly marked directory, is committed, and is
regenerated and compared — not rebuilt — by CI.**

Recommended because the drift check only works if the generated artefact is in the tree to compare
against. Each generated file carries a do-not-edit header naming the registry and the generator. The
CI check regenerates into a temporary location and diffs; it must not write into the working tree,
because a check that fixes the problem it is checking for always passes. Keeping it in one directory
rather than scattered beside its sources makes the drift check a single path and makes review
obvious — a PR that touches generated code and not the registry is wrong on its face. W8 owns the
gate; this is the layout it needs.

**R-3. The frozen-contract fixtures live in the new repository and are run against both binaries.**

Recommended because the fixtures are the specification of the surface the new system must reproduce,
and the specification belongs with the implementation being specified. The Go repository is read-only
reference and would need a code change to host a test it does not otherwise need.

The requirement that they run against *both* binaries is a property of the harness, not of where the
files sit: the conformance runner takes a base URL. Pointing it at the Go daemon proves the fixtures
describe reality; pointing it at the Rust daemon proves the rewrite matches. Both runs belong in CI,
with the Go target as the fixture-validity check. W1 owns the fixture format and the runner.

**R-4. The crate and workspace layout is W3 §1's decision, not this document's.** Recorded only so
that nobody settles it here by accident.

---

## Amendments

**Empty, deliberately. The protocol matters more than the content.**

This document is the shared context every later work item reads, and it will be wrong about
something. When that happens:

- **A decision is never edited in place.** Not corrected, not softened, not quietly deleted.
- The work item that found the problem appends an entry here recording **the date**, **which
  decision**, **what was found**, and **what replaces it**.
- The original decision **stays where it is, marked superseded**, with a pointer to the amendment.
  Future readers need to know what was tried and why it did not hold; a document that only shows the
  current answer teaches nobody anything and invites the same mistake twice.
- An agent that disagrees with a decision **surfaces the disagreement and does not resolve it.** Write
  the proposed amendment, flag it at the top of your own document, and then carry on under the
  existing decision so that your document stays internally consistent. A single agent silently
  overriding a shared decision is the specific failure that produces six documents which each make
  sense alone and do not cohere.

Entry format:

> **YYYY-MM-DD — D-nn superseded by W<item>.** What was found. What replaces it. Why the original was
> wrong rather than merely inconvenient.

---

# Decisions

1. The implementation language is Rust, on the tokio runtime; the Go version stays deployable throughout.
2. The deployment target is PostgreSQL 17 with TimescaleDB 2.31 and `timescaledb_toolkit`, pinned rather than floating.
3. Storage is a wide table, one column per metric — not narrow/EAV; measured 7–9× better density with no crossover.
4. Raw readings are retained indefinitely, with compression enabled from day one, segmented by station and ordered by descending time.
5. The versioned-aggregate and union-view scheme is rejected as unnecessary once retention is removed.
6. Backup and restore is `pg_dump` of the raw hypertable plus an aggregate refresh; no bespoke tool is built.
7. Compression is configured when the hypertable is created, not retrofitted later.
8. Schema changes happen solely through migrations; there is no DDL at startup and no `IF NOT EXISTS` idiom.
9. Adding a metric is a nullable `ADD COLUMN` — 11.6 ms against ~1 GB of compressed chunks — and the migration takes a short lock timeout and retries.
10. The 10–12× compression figure is an estimate pending one hold-rate measurement on production; no capacity decision depends on it.
11. Aggregates are hierarchical: 1-minute from raw, 1-hour from 1-minute, 1-day from 1-hour.
12. Nothing in the aggregate hierarchy stores a finished average; `avg(avg(x))` was measured wrong in 658 of 868 cases.
13. Aggregates store composable state — `min`/`max`/`sum` natively, a mean as sum-and-count or as a two-step aggregate state.
14. Sums are accumulated in a type wider than the column being summed; `sum(real)` is not order-independent.
15. Aggregates are disposable derived data — the supported way to change one is drop, regenerate, refresh.
16. Generated aggregates emit nullable counts, because `count()` returns 0 and would otherwise fill a third of the view with zeroes.
17. Aggregate rebuilds run with relaxed commit durability; a full refresh is fsync-bound and 12× faster without it.
18. The 1-minute aggregate is sized and retained deliberately — at production width it is larger than the compressed raw table it derives from.
19. The generator enforces the 1600-column ceiling, counting allocated rather than live identifiers.
20. One registry, authored in TOML, is the single source of truth for every metric; generated output is committed and CI-checked for drift.
21. Column names and protobuf field numbers are allocated once and never reused; removal deprecates rather than frees.
22. The registry format and generator shape are W2's decisions, not this document's.
23. The word "controller" is retired in favour of a single component abstraction.
24. A component is a cancellable task owning its resources, supervised as a set; stopping is cancellation plus ownership, not a method.
25. A component declares a trigger and does not own a timer; the supervisor owns all scheduling.
26. Dependencies between components are declared as required and provided capabilities; the supervisor resolves startup order.
27. A missing dependency means waiting, not refusing to construct.
28. Configuration reaches components by watch or broadcast, never through shared mutable state.
29. Data flows sources → fan-in → transform/enrich → fan-out → sinks, with an explicit overflow policy per sink.
30. Sinks are composable and carry per-sink delivery policy: durable sinks never lose data, rate-limited upload sinks coalesce to the newest reading.
31. Transport is orthogonal to role; gRPC and REST are codecs, not kinds of component.
32. The crate list in §6 is the default choice for each named idiom; substitutions are allowed but must be recorded as amendments.
33. Secrets use a redacting wrapper type and credentials are compared in constant time.
34. Errors are typed per module and preserve their cause chain.
35. Administrative authentication is a username and password — one administrator, no roles, Argon2id, sessions distinct from credentials, no default password; W5 §3 owns the design.
36. Internal vocabulary is not user-facing vocabulary; an internal term on screen is a bug, and W5 §0 owns the translation table.
37. The legacy external contract is frozen and new endpoints are additive only; W1 captures the surface.
38. The public weather website is carried forward as-is, except that template-generated JavaScript becomes static JavaScript plus a JSON config endpoint.
39. A new v2 API is out of scope for this design program; W6 records it as a named future work item.
40. The ancillary Go tools are not ported — the provisioner stays in Go and is updated, the emulators stay in Go deliberately as independent test implementations, the rest retire.
41. A PR gate running build, test, format, lint and concurrency-checked tests exists from the first commit.
42. The required database version is asserted both in CI and at startup.

---

# Open questions

Stated as decisions for the owner, with a recommendation each.

**OQ-1. What TimescaleDB version does production actually run?**

The spike could not reach the production database — it is not in the homelab Kubernetes cluster, and
the only version reference anywhere in the Go repository is a `docker-compose` example naming a
PostgreSQL 14 image, which is documentation rather than evidence. Three spike answers are
version-gated.

*Recommendation:* run the two queries in `docs/spike-findings.md` Q1 on production and record the
result in W9. If it is PostgreSQL 14, W9 is a major-version PostgreSQL upgrade as well as a schema
migration, which it does not currently assume. One minute of work that changes the shape of a work
item.

**OQ-2. What is production's real compression ratio?**

Measured 1.89× to 13.74× on synthetic data, with the spread fully explained by the
consecutive-identical rate. Synthetic data cannot settle this.

*Recommendation:* run the hold-rate query from `docs/spike-findings.md` Q3 against a week of
production readings — that alone brackets the answer. If an exact figure is wanted, dump one month of
the raw table to a throwaway instance, compress it with the same settings and read the stats view.
Either way, do it before any capacity figure is published. The decision does not depend on the answer
(D-10); only the numbers in the planning documents do.

**OQ-3. Sum-and-count, or Toolkit `stats_agg`?**

Both are correct — verified to bit-identical means against a direct-from-raw control. `stats_agg` is
one column instead of two, yields variance and standard deviation for free, and halves the column
pressure against the ceiling. Sum-and-count is plain SQL with no extension dependency, is trivially
inspectable, and works on any PostgreSQL.

*Recommendation:* **sum-and-count for the metrics the frozen API serves, `stats_agg` only where a
dispersion statistic is actually wanted.** The frozen contract (D-37) means the public API's numbers
must be reproducible by anyone reading the schema, and `stats_agg` states are opaque binary. The
extension dependency is also a deployment constraint (D-2) worth minimising rather than deepening.
**W4 decides and records.**

**OQ-4. Is the 1-minute aggregate compressed, or retention-bounded, or both?**

At production width it is ~42 GB for a decade, larger than the compressed raw table (D-18).
Compression measured 4.58× on a 398-column materialisation.

*Recommendation:* **compress it and keep it unbounded.** Compressed, a decade of the minute level is
~9 GB, and keeping it unbounded preserves the property that makes this whole design simple — that
every level can be rebuilt from the one above it without the raw scan. Retention on the minute level
would reintroduce, at one remove, exactly the hole that D-4 exists to close. But confirm that
compressed-aggregate query latency is acceptable for the `/span` endpoints before committing; that
measurement was not in this spike's scope. **W4 decides.**

**OQ-5. How many aggregates per metric, and which metrics get 1-minute resolution?**

The ceiling (D-19) is not the binding constraint at four aggregates per metric; refresh cost is.
Refresh scales with raw rows scanned times the number of aggregate expressions — going from 4
expressions to 396 over the same scan cost 54× — and with output rows times commit cost.

*Recommendation:* the 1-minute level carries only the metrics something queries at 1-minute
resolution, with the hourly and daily levels carrying everything. The measured request mix supports
this: `/latest` is ~93% of traffic and does not read an aggregate at all, and `/span/24h` is under 1%.
**W4 decides; W2 must make per-level inclusion expressible in the registry.**

**OQ-6. Does anything depend on the ~143-to-215 column gap between the production table and the Go
source definition (D-8)?**

Columns exist in the source and not in the database. Any code path writing one of the missing columns
is failing, or silently not writing.

*Recommendation:* W9 reconciles the actual production column list against the Go definition as part
of migration planning, and reports the difference. Not W0's to fix, but somebody must look, because
the metric registry has to be populated from *something* and the two candidate sources disagree by 72
columns.

---

# Assumptions about other work items

Every assumption this document makes about a document it did not write. **This is the drift
detector** — if one of these is wrong, it is findable with a grep rather than during implementation.

**About W2a (component model)**

1. A component's trigger is declarative and the supervisor owns scheduling, so a component has no
   timer of its own (D-25). If W2a gives components their own clock, D-25 is contradicted.
2. Lifecycle includes a waiting state distinct from failed, entered when a required capability is
   unavailable and exited when it becomes healthy (D-27).
3. Capabilities are named, declared in both directions (required and provided), and resolvable into a
   startup order by the supervisor (D-26).
4. Cancellation plus resource ownership is the only stop mechanism; there is no stop operation in the
   component abstraction (D-24).

**About W3 (service architecture)**

5. W3 owns the crate and workspace layout, which this document deliberately does not fix (R-4).
6. W3 defines the per-sink overflow policy mechanism that D-29 and D-30 require as a concept.
7. W3 treats gRPC and REST as codecs over a shared listener rather than as component kinds (D-31),
   and names the collector role that the Go version lacks.
8. W3 owns how the public weather website is served, including the static-JavaScript-plus-config-
   endpoint replacement in D-38.

**About W2 (metric registry)**

9. The registry can express, per metric: the column name, the protobuf field number, the allocated-
   but-deprecated state (D-21), which aggregate levels include it (OQ-5), and which aggregate
   functions apply.
10. The generator emits `sum` over a widened accumulator (D-14) and nullable counts (D-16) rather
    than the naive forms. **These two are easy to get wrong and silent when wrong.**
11. The generator refuses to emit an aggregate view over 1598 expressions, with a clear error naming
    the ceiling (D-19), and counts allocated rather than live identifiers.
12. Generated output lands in one marked directory, committed, each file carrying a do-not-edit
    header naming the registry and generator (R-2).

**About W4 (database schema)**

13. W4 decides sum-and-count versus `stats_agg` (OQ-3) and records which, because W3 and W5 both need
    to know what a stored mean looks like.
14. W4 owns the aggregate rebuild procedure, including the relaxed-durability requirement in D-17,
    and treats a full rebuild as a ~1-hour maintenance operation at ten years of history rather than
    an instant one.
15. W4 compresses the 1-minute materialisation or bounds its retention (D-18, OQ-4).
16. The hierarchy is exactly 1-minute from raw, 1-hour from 1-minute, 1-day from 1-hour (D-11). The
    Go version's 5-minute level is **not** assumed to carry forward; if it does, it should derive from
    the 1-minute level.

**About W1 (frozen contract)**

17. W1 enumerates the legacy surface; this document deliberately does not (D-37).
18. The public API is unauthenticated today and stays that way, so D-35's authentication work touches
    only the management API.
19. W1's fixtures live in the new repository and their runner accepts a base URL, so the same fixtures
    run against both the Go and the Rust binary (R-3).

**About W5 (management UI)**

20. W5 §0 owns the internal-to-user-facing vocabulary translation table (D-36).
21. W5 §3 owns the administrative authentication design within D-35's constraints — one
    administrator, Argon2id, sessions distinct from credentials, first-start setup, CLI recovery, no
    default password.

**About W8 (CI and release)**

22. W8's drift check regenerates into a temporary location and diffs, and never writes into the
    working tree (R-2).
23. W8's gate includes concurrency-checked tests from the first commit (D-41) and asserts the minimum
    database version (D-42).

**About W9 (data migration)**

24. W9 reconciles the production column list against the Go source definition and reports the gap
    (OQ-6).
25. W9 accounts for the possibility that the migration is also a major-version PostgreSQL upgrade
    (OQ-1).

**About W6 (reconciliation)**

26. W6 records the v2 API as a named future work item rather than a dangling reference (D-39).
27. W6 treats the four closing sections of each document as the input and the prose as background.

---

# Proposed amendments to `DESIGN.md`

This work item authors the baseline, so there is nothing yet to amend — the amendment log is empty by
construction and the section above explains the protocol for keeping it that way.

What belongs here instead is an honest record of the four places where the **planning decisions this
document was asked to write down** did not survive measurement. They are already incorporated into the
decisions above rather than appended as amendments, because a baseline should not ship pre-contradicted.
They are listed here so that a reader comparing this document against the W0 prompt or the design
discussion can see exactly what moved and why.

**A-1 — "Aggregates store `sum` and `count`" → "and the accumulator is wider than the column".**
*Found:* a hierarchy of `sum(real)` disagreed with a direct scan in 55 of 56 uneven-bucket pairs, by
up to 1.55 × 10⁻⁴, because PostgreSQL's `sum(real)` returns `real` and `float4` addition is not
associative. *Replaced by:* D-14. *Why this is a real defect and not a rounding nit:* the public API
is frozen, and this makes the same question return different answers depending on which aggregate
level served it.

**A-2 — "Compression is 10–12×, so ten years is ~16 GB" → "the ratio is entropy-driven; the decision
holds, the figure is unverified".** *Found:* 1.89× to 13.74× on the same schema and column
population, with the spread fully explained by the consecutive-identical rate. *Replaced by:* D-10 and
OQ-2. *Why it matters:* the figure has already propagated into two planning documents, and the
decision it supports survives even at the pessimistic end — so this needs correcting without being
mistaken for a reason to revisit retention.

**A-3 — "Aggregates are disposable, so rebuilding is cheap" → "rebuilding is affordable only with
relaxed commit durability".** *Found:* a full refresh cost 306 s against 2.4 s for the identical
aggregation as a plain query; the overhead is WAL fsync, is not fixed by batching, and falls 12× when
commit durability is relaxed. *Replaced by:* D-17, plus D-18 on the minute level's true cost. *Why it
matters:* "disposable" is load-bearing for D-4, D-5 and D-15. It remains true, but a ten-year rebuild
is an hour-scale maintenance operation that has to be planned for rather than assumed away.

**A-4 — "The wide layout is cheap because PostgreSQL stores a NULL as one bit" → "true of raw, false
of aggregates".** *Found:* `count()` returns 0, never NULL, so on a 70%-NULL table the count columns
of a wide aggregate are fully populated and nearly all zeroes — a third of a 398-column view's size.
*Replaced by:* D-16. *Why it matters:* the NULL-density argument is the central justification for D-3,
and applying it to aggregates without noticing this would quietly overshoot every aggregate size
estimate by ~50%.
