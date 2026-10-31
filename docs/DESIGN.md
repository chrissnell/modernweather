# ModernWeather — DESIGN

The shared context every later work item reads. It records **decisions as constraints, with
rationale**, so that six agents working in parallel make the same assumptions.

**What this document is not.** It contains no code. No trait definitions, no signatures, no buffer
sizes, no module layout, no DDL. It says *what* and *why*; `docs/service-architecture.md` (W3) says
*how*, and `docs/component-model.md` (W2a) says what a component is. If you came here looking for an
interface, you are in the wrong document.

Decisions are **never edited in place**. See `## Amendments` at the end for the protocol.

**Where to start, as of W6 (2026-10-30).** The design programme is complete and all ten documents are
merged. `# Decisions — the ledger` below is **one numbered ledger of all seventy-one decisions, each
carrying its source**, and it is what an implementation agent should read instead of ten design
documents — the documents are the rationale, the ledger is the specification. `## Amendments` carries
the twenty-three amendments the programme produced, applied under the protocol. The integration pass
that produced both — the contradiction audit, the gap audit and the deviation ledger — is
`docs/reconciliation.md`; the build order is `docs/implementation-plan.md`.

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

**The protocol, unchanged.** This document is the shared context every later work item reads, and it
will be wrong about something. When that happens:

- **A decision is never edited in place.** Not corrected, not softened, not quietly deleted.
- The work item that found the problem appends an entry here recording **the date**, **which
  decision**, **what was found**, and **what replaces it**.
- The original decision **stays where it is, marked superseded**, with a pointer to the amendment.
  Future readers need to know what was tried and why it did not hold; a document that only shows the
  current answer teaches nobody anything and invites the same mistake twice.
- An agent that disagrees with a decision **surfaces the disagreement and does not resolve it.** Write
  the proposed amendment, flag it at the top of your own document, and then carry on under the
  existing decision so that your document stays internally consistent.

Entry format:

> **YYYY-MM-DD — D-nn superseded by W<item>.** What was found. What replaces it. Why the original was
> wrong rather than merely inconvenient.

**Applied by W6, 2026-10-30.** Every document in the programme deliberately left its amendments
*proposed* — W9 said so explicitly ("W6 consumes these four sections from every document and is the
right place for the amendments to be applied together"), and W4 set the precedent. Fourteen
amendment sets from eight documents are applied below, in decision order rather than discovery
order, because a reader arrives here from a decision. Each `Status` cell in the ledger points at its
entry. The derivation of every one is in `docs/reconciliation.md`.

**Nothing here was decided by W6 except where an entry says so.** W6's own additions are *decisions*
(D-47 – D-67), not amendments: they fill holes between documents rather than contradicting anything.
The two exceptions are A-W6-1 and A-W6-2, both of which extend a decision's scope rather than
changing its content.

**Four proposals were additions, not amendments, and became decisions instead.** W7 and W8 each
filed proposals against a decision that *did not exist* — they found a gap in the ledger rather than
an error in an entry — so the amendment protocol does not apply and there is nothing to mark
superseded. They are listed here so a reader following a `Source` cell is not left looking for a
missing `### A-` heading:

| Proposal | Source document | Became |
|---|---|---|
| **A-W8-2** — Linux artefacts are statically linked against musl | `docs/release.md` | **D-43.** §9 covered CI and tooling and said nothing about linkage, and linkage is the measured precondition for the ARMv6 target existing at all |
| **A-W7-1** — a node's position in a topology is a set of components, not a mode | `docs/collector.md` | **D-44.** D-31's rationale names the collector role and stops short of saying what it is, which left W7 to supply it |
| **A-W7-2** — registration carries identity only | `docs/collector.md` | **D-45.** D-33 covers how a secret is *held*, not whether it should be *transmitted* — and the Go implementation transmits four of them in cleartext to a table that nothing reads |
| **A-W7-3** — admission precedes the pipeline | `docs/collector.md` | **D-46.** D-29 specifies the data flow and not where authorisation sits in it; the Go receiver pushes into the distributor at `grpcreceiver/station.go:175` and consults the registry at `:182` |

**A-W7-4** is the one W7 proposal that *is* an amendment — it extends D-30 — and it is folded into
A-W2a-1 below, because D-30 was being split in the same breath and two amendments to a decision that
no longer exists would be worse than one.

W7's own note on A-W7-3 deserves recording, because it was right to raise it: the decision is close
enough to W2a's Decision 46 that W6 could reasonably have recorded it only there. It is kept as
D-46 because D-29 is the decision about the *flow*, and W2a's Decision 46 is about identity
resolution — stating it at the flow level is what makes it apply to a direction nobody has invented
yet.

---

### A-1 — 2026-10-28 — D-3 and D-9 gain a caveat. *Source: W1.*

**What was found.** The density argument for the wide table is that PostgreSQL stores a NULL as one
bit, and D-9 adds a metric as a nullable `ADD COLUMN`. Both are correct and both encourage a writer
to store NULL for a metric a station does not report. Against the `circular_avg` aggregate as the Go
server defines it, a window in which `winddir` is entirely NULL makes `/span` return **500** — the
finaliser divides by a zero accumulator. The shipping Go daemon is only safe because it writes its
whole reading struct, zeros included, so no column it writes is ever NULL. The rewrite is expected
to be more parsimonious and would therefore hit this.

**What replaces it.** Nothing. D-3 and D-9 stand; they gain the caveat that **a NULL-tolerant writer
requires NULL-tolerant aggregate finalisers**, and that D-37 constrains the fix to one that still
emits `0` rather than `null`. **Discharged:** W4 D-8 makes `mw_circular_mean` return `0` for a bucket
with no non-NULL `winddir` (OQ-W1-2 option (c)), and `ci/db/schema-assertions.sql` A-3 asserts it.

**Why this is more than inconvenient.** It is the only place found where two merged decisions combine
into a user-visible 500 that neither decision's own document would predict.

### A-2 — 2026-10-28 — D-37 names its artefacts. *Source: W1.*

**What was found.** D-37 says the legacy contract is frozen and that W1 captures the surface, which
leaves "frozen to what?" answerable only by reading prose.

**What replaces it.** D-37 gains: *the frozen surface is defined by `contract/legacy-v1.json` plus the
70 fixtures in `contract/fixtures/`, and a green conformance run against the Rust daemon is the
acceptance test for any PR that touches a legacy endpoint.*

**Why this is more than inconvenient.** A freeze with no executable definition is a freeze that
nobody can be found to have broken. **W6 note:** the acceptance test does not yet run in CI — see
`docs/reconciliation.md` G-1 and work item N-3.

### A-3 — 2026-10-28 — D-38 records the nine values. *Source: W1.*

**What was found.** D-38 decides that template-generated JavaScript becomes static JavaScript plus a
JSON configuration endpoint, but does not say what that endpoint carries. `weather-app.js.tmpl`
consumes exactly nine of the eleven values the Go handler passes it.

**What replaces it.** D-38 gains a pointer to `docs/legacy-api-contract.md` §9's list. W3 and W5 both
read D-38 and would otherwise each re-derive it from the Go template. **Discharged:** W3 D-80 fixes
the payload as exactly that set with JSON keys equal to the JavaScript property names.

**Why this is more than inconvenient.** It is the one decision in this document that deliberately
changes the website's bytes, so the exact replacement payload is load-bearing rather than detail.

### A-4 — 2026-10-28 — D-32 states its scope. *Source: W1.*

**What was found.** D-32 names `axum`/`tonic` for HTTP and lists no HTTP *client*. The conformance
runner needs a blocking client and uses `ureq`, which is neither a substitution for a listed crate
nor covered by the list.

**What replaces it.** D-32 gains a scope sentence: *it governs the daemon's dependencies; test
harnesses may choose their own, recorded where they are used.*

**Why this is more than inconvenient.** Without it, every piece of test infrastructure either drags
in the daemon's async stack or files an amendment, and W6 reconciles noise.

### A-W2-2 — 2026-10-29 — D-9's cheapness claim is true of the column and false of the aggregates. *Source: W2.*

**What was found.** The `ADD COLUMN` is 11.6 ms as measured. But under D-15 the only supported way to
change an aggregate is drop, regenerate and refresh, and a new metric that any level carries forces a
rebuild of that level and every level above it. At ten years of history D-17 puts that in the hour
range. The migration generator demonstrates it: one added column produces a drop-and-recreate of
every level.

**What replaces it.** A qualification on D-9 — *adding a metric is a millisecond DDL operation plus
an aggregate rebuild whose cost is D-17's and D-18's.* **Discharged in part:** W4 D-23's rebuild
procedure refreshes the `/span` bands first, bottom-up, before backfilling history, measured at
**0.87 s against 467 s** — a 537× difference between "the charts are right" and "everything is
materialised" — so a new metric is queryable at recent resolution in under a second.

**Why this is more than inconvenient.** "Adding a metric is cheap" is cited as the justification for
D-3 and D-4. Both survive the correction — the alternative layouts are worse on this axis too — but
a capacity plan written from the 11.6 ms figure alone schedules a metric addition as a no-op and
discovers an hour-long rebuild in production.

*(W2's A-W2-1 — that OQ-5's recommendation is not constructible under D-11 — is not an amendment to a
decision; it corrected an open question, and W4 D-3 closed it by taking OQ-W2-1 option (a). Recorded
here only so a reader looking for it is not left wondering.)*

### A-W4-1 — 2026-10-29 — **D-11 is superseded: the hierarchy has four levels, not three.** *Source: W4.*

**What was found.** `/span` reads four tables, not three, and the missing one is the 5-minute level
that serves every span between 6 and 48 hours — including `/span/24h`, the public website's default
view. This is not inferred from the Go source: `contract/fixtures/span-24h-five-minute-buckets.json`
puts the bucket spacing at exactly **300,000 ms**. W1 assumption 10 and W3 assumption 19 both already
state four bands, so D-11 was the only document saying three.

**What replaces it.** The hierarchy is 1-minute from raw, **5-minute from 1-minute**, 1-hour from
5-minute, 1-day from 1-hour. D-11's mechanism is untouched — each level still reads the level below
it — and D-11's own assumption 16 already licensed this on the condition that a 5-minute level derive
from the 1-minute level, which it does, exactly five buckets to one.

**Why the original was wrong rather than merely incomplete.** Assumption 16 reads as a hypothetical
("the Go version's 5-minute level is **not** assumed to carry forward"), and under D-37 it is not a
hypothetical — it is a frozen band. A reader implementing D-11 as written ships three levels and then
has to choose which wrong thing `/span/24h` does: quarter the points in the dominant chart, or
quintuple the response size. Neither is reachable without breaking the contract.

### A-W4-2 — 2026-10-29 — **OQ-4's recommendation is unavailable; D-18's figure and D-19's claim are corrected.** *Source: W4.*

**What was found.** Two things, and the second explains the first.

- `compress_chunk` on `observations_1m` fails: `row is too big: size 11720, maximum size 8160`.
  Bisected, **the widest compressible continuous aggregate is 439 columns**; the generated levels are
  629. D-19 located this failure "near the ceiling", citing a 1600-column view rejected at an
  estimated 28,852 bytes. The real threshold is a quarter of the way to the ceiling, which is why
  production's 397-column views compress and these do not. Narrowing is unavailable (membership can
  only shrink away from raw) and `stats_agg` reaches only 502.
- At the generated width the minute level is 4,175 bytes per bucket row — 8.8 GB/year and **87.8 GB
  for a decade** at four stations, **2.1× D-18's figure**, almost exactly the 629/398 column ratio.
  D-18's number was right for its width.

**What replaces it.** OQ-4's other arm: **7-day retention on `observations_1m`, 14-day on `_5m`,
90-day on `_1h`, `_1d` unbounded — 337 MB in place of 107 GB.** And a sharper reason than
"aggregates are disposable": `/span` takes no time anchor, so each level is read over exactly one
trailing window and never over history. D-4's hole is in *raw*, and raw is permanent, so an evicted
minute bucket is an evicted cache entry rather than lost information. D-18's qualitative claim stands
and is strengthened: the minute level is not free, it is *less* free than D-18 says, and the escape
D-18 offered does not exist.

**Why the original was wrong rather than merely inconvenient.** OQ-4's recommendation is conditional
on a measurement ("confirm that compressed-aggregate query latency is acceptable"), which reads as a
performance question with a probable answer. It is not — the configuration cannot be created. An
implementer following OQ-4 would write `ALTER MATERIALIZED VIEW … SET (timescaledb.compress = true)`,
which **succeeds**, and then watch a compression policy job fail silently every day while the minute
level grew unbounded. D-19 is wrong in the direction that hides this: it says the problem is near the
ceiling, so nobody at 629 columns thinks to check.

### A-W4-3 — 2026-10-29 — **D-17's 12× is corrected to 1.89×, and the refresh is not purely fsync-bound.** *Source: W4.*

**What was found.** At 627 expressions over 35,942,400 rows, the same comparison gives **758.5 s with
`synchronous_commit = on` and 401.4 s with it off — 1.89×, not 12×** — while the identical
aggregation as a plain query is 10.0 s single-threaded. Decomposed: ~10 s of arithmetic, +96 s to
materialise 1.2 M 629-column rows into a plain table, +295 s of continuous-aggregate refresh
machinery, +357 s of WAL fsync.

**What replaces it.** A qualification. Relaxing commit durability for a rebuild is still correct and
still worth doing — it halves the longest operation in the system — but **fsync is roughly half the
cost of a durable refresh rather than nearly all of it**, and the residue after relaxing it is 32×
the plain query, not ~10×. "It is WAL fsync, not CPU" should read "it is not CPU: roughly half is WAL
fsync and most of the rest is the refresh's own write path, both of which scale with the
materialisation's width."

**Why the original was wrong rather than merely dated.** The 12× is quoted in
`generated/sql/090_rebuild_aggregates.sql`'s own header, so it is the number an operator reads at the
moment they are deciding whether a rebuild will finish inside a maintenance window. Planning a
15-minute operation from a figure that promises 2 is how a deploy overruns. The decision survives;
the capacity figure does not — and the thing that actually makes a rebuild bearable is not durability
but A-W2-2's ordering, which is 537×.

### A-W2a-1 — 2026-10-28 — **D-30 is superseded by D-30a and D-30b.** *Sources: W2a, extended by W3 (A-W3-2) and W7 (A-W7-4).*

**What was found.** Measured against the code, the rate-limited outputs are not sinks on the reading
stream and cannot be made into correct ones. APRS/CWOP, Weather Underground and PWS Weather are
driven by `time.NewTicker` and poll the database; none subscribes to readings. More importantly they
**must not** send the newest reading: CWOP requires wind direction and speed *averaged* over ~5
minutes with gust as a *peak over ~10 minutes*, and Weather Underground's field names encode the
windows explicitly (`windspdmph_avg2m`, `windgustmph_10m`). Coalescing to the newest reading yields
an instantaneous sample where an average is required — which is exactly the defect the current APRS
packet builder has, taking gust from a 10-minute SQL function but wind direction and speed straight
off the reading. The "latest reading" those uploaders start from is itself
`weather_1m … WHERE bucket > NOW() - INTERVAL '2 minutes' LIMIT 1` with **no `ORDER BY`**
(`internal/database/client.go:121`), so it is an arbitrary bucket up to two minutes stale.

**What replaces it.** **D-30a** and **D-30b** in the ledger, splitting the asymmetry D-30 correctly
identified. W3's A-W3-2 reached the same conclusion from the sink's side and is folded in; W7's
A-W7-4 adds the bounded-spool exception and the `max_lag` invariant that makes it right.

**Why the original was wrong rather than merely inconvenient.** D-30's *intent* — never backfill a
rate-limited service with an hour of history — is correct and is preserved. Its mechanism encodes the
assumption that an output's correct value is a *reading*, and for every rate-limited output in this
system it is a *window*. An implementation built on D-30 as written would transmit instantaneous wind
values to CWOP, which is a protocol violation **that looks like working software**: the packets are
accepted, the numbers are wrong, and nothing reports it. That is the current behaviour, and D-30 as
written would carry it forward.

### A-W3-2 — 2026-10-28 — D-29 names the fan-out point. *Source: W3.*

**What was found.** The shape is right, but "fan-out" is underspecified in exactly the place where
the design is decided. W2a's Decision 61 forbids a source being blocked by a sink and Decision 62
concludes that `Durable` therefore requires a local durable spool. Those two together leave no
freedom: the fan-out point **is** the spool, every durable sink reads it through its own cursor, and
`Latest` and `Lossy` subscribers are notified from the same append. A reader of D-29 alone would
reasonably build a `broadcast` channel and satisfy neither decision.

**What replaces it.** D-29 gains: *the fan-out point is a local durable spool; durable sinks consume
it through independent cursors and acknowledge a prefix, while coalescing and lossy subscribers are
notified after the append commits.*

**Why this is more than inconvenient.** It is the one structural commitment in the pipeline, and it is
currently derivable only by reading two documents and noticing that their conjunction has a unique
solution.

### A-W3-4 — 2026-10-28 — D-31's singular unit is the router and the route table. *Source: W3.*

**What was found.** D-31 makes transport orthogonal to role and the W2a/W3 prompts both phrase the
consequence as "one listener, not seven". Read literally, that moves `WeatherV1` and `WeatherAppsV1`
from their dedicated port onto the website's port, which breaks every machine-to-machine peer at
upgrade. And it is not merely a reading: `grpc_port` is a configuration column defaulting to 50051
(`pkg/config/provider_sqlite.go:231`) and the management API **validates that it differs from the
HTTP and HTTPS ports** (`handlers_controllers.go:501`, `:506`), so a separate socket is the shipped,
validated behaviour.

**What replaces it.** D-31 gains: *the singular unit is the router and the route table. One
`http:listener` component binds the sockets configuration names — the main port, an optional
redirecting plaintext port, an optional legacy gRPC port for existing peers — and serves one router
on all of them.*

**Why this is more than inconvenient.** Seven listener-owning sites, each with its own TLS setup and
shutdown path, is the defect. One component binding two sockets is not that defect, and conflating
the two would force an unnecessary compatibility break on W7 and make the rewrite's first operational
event a fleet-wide flag day.

### A-W3-1 and A-W8-3 — 2026-10-28 — D-32's crate table gains seven rows. *Sources: W3, W8, W5.*

**What was found.** D-32 lists the default crate for each named idiom and says a *substitution* must
be recorded as an amendment. Seven idioms the design requires are not named at all, so seven agents
would each pick one. That is precisely the divergence D-32 exists to prevent, in the one form the
decision does not cover. W8's A-3 independently filed the TLS row, noting it is load-bearing on
ARMv6 and affects `sqlx`'s own feature flags, which name the provider explicitly.

**What replaces it.** Nothing is replaced; the table gains rows:

| Concern | Crate | Why this one |
|---|---|---|
| TLS, and the SNI resolver | `rustls` + **`ring`**, `rustls-pemfile` | answers W8's OQ-1; smaller than `aws-lc-rs`, no `cmake` build dependency, and already on the ARMv6 allowlist |
| HTML templating | `askama` | compile-time checked, so a missing field is a build error rather than a request-time one |
| Static asset embedding | `rust-embed` | one static binary per target, matching what the Go version does with `embed.FS` |
| Protobuf parsing at build time | **`protox`** + `prost-build` | no `protoc` binary on any of four cross targets or on a Pi |
| Logging and metrics | `tracing`, `tracing-subscriber`, `metrics`, `metrics-exporter-prometheus` | one logging path, and a metrics surface that is not logs |
| systemd readiness | `sd-notify` | `Type=notify` without linking `libsystemd`, which a static musl build cannot |
| Management OpenAPI derivation | `utoipa` | the schema is derived from the Rust handlers rather than hand-maintained (W5 D-37) |

**Why this is more than inconvenient.** W1's A-4 already asked D-32 to state that it governs the
daemon rather than test infrastructure. The same decision needs the converse: it should be complete
for the daemon, or it is a list that answers some questions and silently delegates others.

### A-W3-3 — 2026-10-28 — D-38 records *how* the config payload is delivered. *Source: W3.*

**What was found.** W1's A-3 already asked D-38 to name the nine values. Implementing it surfaces a
second gap: "a JSON configuration endpoint" read literally means the module does a `fetch` at
initialisation, which introduces an extra round trip and an asynchronous boundary into a module that
is synchronous today — in a page that is supposed to be visually unchanged. The page already contains
the right pattern: `radar-config` is delivered as an inline `<script type="application/json">` block.

**What replaces it.** D-38 gains: *the configuration is delivered inline as a
`<script id="mw-config" type="application/json">` block, following the existing `radar-config`
precedent; `GET /config.json` exists as an additive endpoint for other consumers. The payload is the
nine values listed in `docs/legacy-api-contract.md` §9, with JSON keys equal to the JavaScript
object's property names.*

**Why this is more than inconvenient.** The difference between an inline block and a `fetch` is the
difference between "visually unchanged" and "a flash of unstyled content and an ordering bug".

### A-W5-1 — 2026-10-29 — D-35 says what "one administrator" means about sessions. *Source: W5.*

**What was found.** D-35 says "one administrator, no roles, no multiple accounts", which settles
*identities*. It is silent on *sessions*, and the two readings diverge materially: "one identity,
many revocable sessions" (a laptop and a phone both signed in, each revocable, with "sign out
everywhere") versus "one session at a time". The second would make the session table pointless and
remove the revocation feature D-35's own rationale asks for — but it is a reading the words permit,
and a later reader economising on the session table would not be obviously wrong.

**What replaces it.** D-35 gains: *"One administrator" constrains identities, not sessions. The single
administrator may hold several concurrent sessions — a laptop and a phone — each independently
revocable, with a "sign out everywhere" action. This is required by the decision's own rationale: a
session that cannot be revoked independently of the credential is the defect being removed.*

### A-W5-2 — 2026-10-29 — D-36 names its enforcement mechanism. *Source: W5.*

**What was found.** D-36 says an internal term on screen "is treated as a bug, not a documentation
gap", which is the right standard with no mechanism behind it. Every team that has written that
sentence has also shipped the term.

**What replaces it.** D-36 gains: *Enforcement is mechanical: user-facing strings live in a single
copy module, and a CI gate fails the build on a banned internal term. Deliberate exceptions are
marked explicitly and the gate prints the current exception list on every run.* **W5 §6.6 owns the
mechanism** — about thirty lines of `xtask`. **W6 note:** that gate (`ui-copy`) is now declared in
`ci/gates.toml`, deferred; without the declaration the decision degrades into a code-review
preference, which is the failure this amendment exists to prevent.

### A-W5-3 — 2026-10-29 — D-33 names the management API's transport defaults. *Source: W5.*

**What was found.** D-33 covers secret *handling* — a redacting wrapper type and constant-time
comparison — and cites the current token's logging and cookie defects. It does not mention two
adjacent defects on the same surface that are equally measured and equally one-line fixes:
`Access-Control-Allow-Origin: *` on the admin API
(`internal/controllers/management/controller.go:306`) and a default bind of `0.0.0.0`.

**What replaces it.** D-33 gains: *The management API's transport defaults are part of this decision:
CORS is an exact-match allowlist defaulting to empty (never `*`, never a reflected `Origin`), and the
default bind address is `127.0.0.1`. **W5 §3.5 owns both.** The public website's bind address is
unaffected.*

### A-W8-1 — 2026-10-28 — the W8 prompt asks for union-view assertions that D-5 rejected. *Source: W8.*

**What was found.** The canonical W8 prompt asks §2 for "the union-view assertions W4 defines", which
contradicts D-5. No union views exist to assert on.

**What replaces it.** No decision changes. The migration gate compares schema dumps,
continuous-aggregate catalog entries including view definitions, and surviving data between the two
paths. The prompt line is corrected so W4 is not sent looking for a scheme the design rejected.
**W6 note:** W9 does generate `observations_1d_with_legacy`, but W9 D-6 keeps it out of the read path
and W9 assumption 11 keeps it out of the hierarchy, so D-5 stands.

### A-W8-4 — 2026-10-28 — D-40's "the emulators stay in Go" means *at a pinned commit*. *Source: W8.*

**What was found.** D-40 and the capability inventory both treat the emulators as fixed external test
infrastructure, which is right, but "fixed" is doing load-bearing work: the reference repository is
read-only to this programme and is **not frozen**, so an unrelated upstream commit can turn this
repository's CI red.

**What replaces it.** D-40's rationale gains: *the emulators are consumed at a pinned commit of the
reference repository, recorded in this repository, and bumping it is a reviewed change.* No decision
is reversed. **Discharged:** W8 D-23.

### A-W9-1 — 2026-10-30 — D-4 and D-6 gain the mechanism by which the *existing* data can still be lost. *Source: W9.*

**What was found.** Retention dropping a raw chunk invalidates the continuous aggregates above it, so
on TimescaleDB 2.31 the next refresh of any level over that range — **including the level's own
scheduled policy** — deletes the buckets whose raw has gone. Measured: a manual refresh removed 21 of
29 daily buckets and one policy run with `start_offset = 10 years` removed 41 of 83, the destroyed
range matching the dropped-chunk range to the day, with `drop_chunks` logging the invalidation that
caused it.

**What replaces it.** Nothing about the new design changes — D-4 and D-6 both rest on aggregates
being disposable derived data, which they are *because raw is permanent*. The old database is the
case where they are not. The amendment is the mechanism, recorded, because it is **the one way the
owner's existing data can still be lost** and because "the aggregates are disposable" is the sentence
that makes it sound safe.

**Consequence for the pre-flight list:** **archive first, then pause.** Pausing retention is not what
protects the aggregate buckets; the archive copy is correct under every answer to OQ-W9-2.
`docs/rewrite/README.md` pre-flight item 1 is corrected accordingly.

### A-W9-2 — 2026-10-30 — D-6 gains its converse. *Source: W9.*

**What was found.** A continuous aggregate **accepts** an `INSERT` on TimescaleDB 2.31 and discards it
on the next refresh, on the next policy run, or when the level is dropped and recreated. Measured:
the insert succeeds, duplicates succeed (a materialisation hypertable has no unique constraint on
`(bucket, station_id)`), the rows read back correctly through `mw_mean`, and then a refresh over the
range leaves zero rows.

**What replaces it.** D-6 makes "restore raw, then rebuild the aggregates" the supported path, which
is right. It gains the converse: *there is no supported path that puts rows **into** an aggregate.
Pre-seeded aggregate rows are not a restore mechanism.*

**Why this is more than a comment.** The failure is entirely at a later time than the mistake.
**Discharged:** W9 D-4 puts the rollup archive into plain tables (`observations_1d_legacy`,
`observations_1h_legacy`) and never into a level.

### A-W9-3 — 2026-10-30 — D-14 has a visible consequence on two frozen-contract keys. *Source: W9.*

**What was found.** D-14 widened the new accumulator because `sum(real)` is not associative. The old
schema does worse than add in a different order: it accumulates in `float4`. `sum(rainincremental)`
returns `real`, and `circular_avg_state` declares `sin_sum real, cos_sum real, accum real` with a
finaliser that divides and takes `atan2d` in `real`. Measured at 2,160 readings per daily bucket:
`period_rain` differs by up to 3.13 × 10⁻⁶ relative and `winddir` by 1.73 × 10⁻⁷, and **both round to
a different float32 in 100% of buckets.** At production's 2-second cadence the Go side's error is
roughly 20× larger.

**What replaces it.** D-14's decision is correct and is strengthened by this. The amendment records
that it has a *visible* consequence on two frozen-contract keys (`/span`'s `period_rain` and `windd`),
which D-14 does not mention, and that **the new numbers are the accurate ones**. Carried in
`docs/reconciliation.md` §3 as **DEV-4**, which is what W9's OQ-W9-4 asked W6 to decide: it belongs in
the deviation ledger, because during the side-by-side validation period an unrecorded value diff on
two frozen keys looks exactly like a migration defect.

### A-W6-1 — 2026-10-30 — D-25's "no component-owned timer" does not reach inside a `Service`'s loop. *Source: W6.*

**What was found.** D-25 says a component declares a trigger and does not own a timer. W2a's Decision
7 states that a `Service` component owns time *inside* its loop — read deadlines, reconnect backoff —
and that this is not a D-25 violation; W3 assumption 5 relies on it to keep `Service` out of the
timer heap entirely. Three documents agree, and D-25 as written does not say so.

**What replaces it.** D-25 gains: *this governs **scheduling**. A long-running `Service` component owns
time inside its own loop — read deadlines, reconnect backoff, keepalives — which is not a schedule
and not a second clock. The supervisor owns when a component runs, not what it waits on while
running.*

**Why this is worth recording.** It is the one place where a decision stated as an absolute has a
universally-agreed exception, and an implementer reading D-25 alone would either put `Service` in the
timer heap or file this amendment themselves.

### A-W6-2 — 2026-10-30 — D-8 governs the configuration store too. *Source: W6.*

**What was found.** D-8 says schema changes happen solely through migrations, with no DDL at startup
and no `IF NOT EXISTS` idiom, and its rationale is the Go schema frozen at first-run shape by exactly
that idiom. Every document read it as being about the weather database. W5 assumption 39 is the only
place that applies it to the configuration store — "the 'migrations only' rule applied to the
configuration store" — and assigns the work to W4, which scopes itself to PostgreSQL throughout. The
result is nine-plus SQLite tables assumed into existence by four documents with no DDL and no
migration mechanism.

**What replaces it.** D-8 gains: *this governs **every** schema this system owns, including the SQLite
configuration store. Configuration migrations live in `migrations/config/` and are applied by name,
exactly as the weather migrations are.*

**Why this is more than inconvenient.** The Go configuration database is the actual instance of the
defect D-8 was written about: `CREATE TABLE IF NOT EXISTS` at startup, frozen at first-run shape. A
rewrite that applied D-8 to the weather schema and reinvented startup DDL for the configuration would
reproduce the original defect in the only place it ever happened. See `docs/reconciliation.md` G-2 and
work items N-4 and N-5.

---

# Decisions — the ledger

**One numbered ledger, every entry carrying its source.** This is the single place to answer *"why is
it like this"*, and it is **the thing an implementation agent reads instead of ten design
documents**. The documents remain the rationale; this is the specification.

Read the `Status` column. A decision is **never edited in place** (see `## Amendments`):

- *(blank)* — stands as written.
- **extended** — the decision stands and an amendment adds to it. Read the amendment too.
- **corrected** — the decision stands and an amendment fixes a figure or a claim inside it. **The
  number in the decision's own text is the wrong one.**
- **superseded** — do not implement this as written. Read the amendment.

| # | Decision | Source | Status |
|---|---|---|---|
| **D-1** | The implementation language is Rust, on the tokio runtime; the Go version stays deployable throughout. | W0 | |
| **D-2** | The deployment target is PostgreSQL 17 with TimescaleDB 2.31 and `timescaledb_toolkit`, pinned rather than floating. | W0 | |
| **D-3** | Storage is a wide table, one column per metric — not narrow/EAV; measured 7–9× better density with no crossover. | W0 §2 | **extended** → A-1 |
| **D-4** | Raw readings are retained indefinitely, with compression enabled from day one, segmented by station and ordered by descending time. | W0 §2 | **extended** → A-W9-1 |
| **D-5** | The versioned-aggregate and union-view scheme is rejected as unnecessary once retention is removed. | W0 §3 | |
| **D-6** | Backup and restore is `pg_dump` of the raw hypertable plus an aggregate refresh; no bespoke tool is built. | W0 §2 | **extended** → A-W9-1, A-W9-2. **The mechanism had no owner and no schedule** — now N-19, and load-bearing once the database is a single-replica local-PV StatefulSet (W6 §7.5) |
| **D-7** | Compression is configured when the hypertable is created, not retrofitted later. | W0 §2 | |
| **D-8** | Schema changes happen solely through migrations; there is no DDL at startup and no `IF NOT EXISTS` idiom. | W0 §2 | **extended** → A-W6-2 (it governs the configuration store too) |
| **D-9** | Adding a metric is a nullable `ADD COLUMN` — 11.6 ms against ~1 GB of compressed chunks — and the migration takes a short lock timeout and retries. | W0 §2 | **extended** → A-1; **corrected** → A-W2-2 |
| **D-10** | The 10–12× compression figure is an estimate pending one hold-rate measurement on production; no capacity decision depends on it. | W0 §2 | |
| **D-11** | Aggregates are hierarchical: 1-minute from raw, 1-hour from 1-minute, 1-day from 1-hour. | W0 §3 | **superseded** → A-W4-1 (**four** levels) |
| **D-12** | Nothing in the aggregate hierarchy stores a finished average; `avg(avg(x))` was measured wrong in 658 of 868 cases. | W0 §3 | |
| **D-13** | Aggregates store composable state — `min`/`max`/`sum` natively, a mean as sum-and-count or as a two-step aggregate state. | W0 §3 | |
| **D-14** | Sums are accumulated in a type wider than the column being summed; `sum(real)` is not order-independent. | W0 §3 | **extended** → A-W9-3 |
| **D-15** | Aggregates are disposable derived data — the supported way to change one is drop, regenerate, refresh. | W0 §3 | |
| **D-16** | Generated aggregates emit nullable counts, because `count()` returns 0 and would otherwise fill a third of the view with zeroes. | W0 §3 | |
| **D-17** | Aggregate rebuilds run with relaxed commit durability; a full refresh is fsync-bound and 12× faster without it. | W0 §3 | **corrected** → A-W4-3 (**1.89×**, not 12×) |
| **D-18** | The 1-minute aggregate is sized and retained deliberately — at production width it is larger than the compressed raw table it derives from. | W0 §3 | **corrected** → A-W4-2 (2.1× larger than stated, and it **cannot** be compressed) |
| **D-19** | The generator enforces the 1600-column ceiling, counting allocated rather than live identifiers. | W0 §3 | **corrected** → A-W4-2 (the *compression* ceiling is **439**, not "near 1600") |
| **D-20** | One registry, authored in TOML, is the single source of truth for every metric; generated output is committed and CI-checked for drift. | W0 §4 | |
| **D-21** | Column names and protobuf field numbers are allocated once and never reused; removal deprecates rather than frees. | W0 §4 | |
| **D-22** | The registry format and generator shape are W2's decisions, not this document's. | W0 §4 | discharged — W2 D-1 |
| **D-23** | The word "controller" is retired in favour of a single component abstraction. | W0 §5 | |
| **D-24** | A component is a cancellable task owning its resources, supervised as a set; stopping is cancellation plus ownership, not a method. | W0 §5 | |
| **D-25** | A component declares a trigger and does not own a timer; the supervisor owns all scheduling. | W0 §5 | **extended** → A-W6-1 (a `Service` owns time *inside* its loop) |
| **D-26** | Dependencies between components are declared as required and provided capabilities; the supervisor resolves startup order. | W0 §5 | |
| **D-27** | A missing dependency means waiting, not refusing to construct. | W0 §5 | |
| **D-28** | Configuration reaches components by watch or broadcast, never through shared mutable state. | W0 §5 | |
| **D-29** | Data flows sources → fan-in → transform/enrich → fan-out → sinks, with an explicit overflow policy per sink. | W0 §5 | **extended** → A-W3-2 (the fan-out point **is** the spool) |
| **D-30** | Sinks are composable and carry per-sink delivery policy: durable sinks never lose data, rate-limited upload sinks coalesce to the newest reading. | W0 §5 | **superseded** → A-W2a-1 (split into **D-30a** and **D-30b**) |
| **D-30a** | A durable sink must not lose data. Because a source is never blocked by a sink, a durable sink is served by a **local durable spool** at the fan-out point rather than an in-memory channel; a slow sink falls behind on disk, never upstream. **A durable sink must not lose data *while the spool has room*** — the spool is bounded, and at the bound the policy is to refuse at the edge, counted, logged and visible as a component failure. `max_lag` must be reachable before the ceiling at the node's observed rate; a configuration where it is not is a startup warning. | W2a (A-W2a-1), extended by W7 (A-W7-4) | |
| **D-30b** | Rate-limited outputs are **not stream sinks.** They are scheduled producers with a declared period, and each declares the **windowed values** it needs as `(Metric, Window, Agg)`, resolved by rolling up aggregate buckets. Rate limiting is therefore structural — a component that does not wake more often than allowed needs no queue, no buffer and no coalescing. | W2a (A-W2a-1) | |
| **D-31** | Transport is orthogonal to role; gRPC and REST are codecs, not kinds of component. | W0 §5 | **extended** → A-W3-4 (the singular unit is the **router and route table**, not the socket) |
| **D-32** | The crate list in §6 is the default choice for each named idiom; substitutions are allowed but must be recorded as amendments. | W0 §6 | **extended** → A-4 (it governs the daemon, not test infrastructure), A-W3-1 (seven additions) |
| **D-33** | Secrets use a redacting wrapper type and credentials are compared in constant time. | W0 §7 | **extended** → A-W5-3 (the management API's transport defaults) |
| **D-34** | Errors are typed per module and preserve their cause chain. | W0 §7 | |
| **D-35** | Administrative authentication is a username and password — one administrator, no roles, Argon2id, sessions distinct from credentials, no default password; W5 §3 owns the design. | W0 §7 | **extended** → A-W5-1 (one *identity*, several revocable sessions) |
| **D-36** | Internal vocabulary is not user-facing vocabulary; an internal term on screen is a bug, and W5 §0 owns the translation table. | W0 §8 | **extended** → A-W5-2 (enforcement is a CI gate, not review) |
| **D-37** | The legacy external contract is frozen and new endpoints are additive only; W1 captures the surface. | W0 §8 | **superseded in scope** → **D-68** (five iOS endpoints are frozen; the rest is coupled). **extended** → A-2 (named artefacts), and see `docs/reconciliation.md` §3 for the six recorded deviations |
| **D-38** | The public weather website is carried forward as-is, except that template-generated JavaScript becomes static JavaScript plus a JSON config endpoint. | W0 §8 | **extended** → A-3 (the nine values), A-W3-3 (how the payload is delivered) |
| **D-39** | A new v2 API is out of scope for this design program; W6 records it as a named future work item. | W0 §8 | discharged — `docs/reconciliation.md` §4, **N-1** |
| **D-40** | The ancillary Go tools are not ported — the provisioner stays in Go and is updated, the emulators stay in Go deliberately as independent test implementations, the rest retire. | W0 §9 | **extended** → A-W8-4 (**at a pinned commit**); the provisioner update is **N-2** |
| **D-41** | A PR gate running build, test, format, lint and concurrency-checked tests exists from the first commit. | W0 §9 | |
| **D-42** | The required database version is asserted both in CI and at startup. | W0 §9 | **extended** → D-62 (the startup half had no owner) |
| **D-43** | **Linux release artefacts are statically linked against musl, for every architecture.** A glibc ARMv6 cross build from Debian's toolchain produces an ARMv7-tagged binary that faults on a Raspberry Pi 1/Zero; one linkage for all four architectures also means one packaging rule and no runtime library skew. Consequence: musl's allocator and resolver differ from glibc's. | W8 (A-W8-2) | |
| **D-44** | **A node's position in a topology is the set of components its configuration produces, not a mode it is in.** A collector is a node whose components include an ingest endpoint; a satellite is one whose components include an uplink; a node with both is the default case. There is no `role`, `mode` or `is_collector` field in the configuration. | W7 (A-W7-1) | |
| **D-45** | **Registration between nodes carries identity and station metadata only. A node never transmits a third-party credential to another node.** A collector that uploads on behalf of the fleet selects which origins it covers and uses its own credentials. | W7 (A-W7-2) | |
| **D-46** | **Admission control for an inbound reading happens before the reading reaches the stream, and an unauthorised sender is a counted rejection.** There is no path by which a reading is stored and then found to be unauthorised. | W7 (A-W7-3) | |
| **D-47** | **The uplink's reading filter is `NotFrom(peer)`, not `NotFrom(self)`.** `admit` sets `path = [self]`, so every envelope in a node's own spool carries `self`; `NotFrom(self)` matches everything and the uplink would send nothing. | W6 (C-W2a-25) | |
| **D-48** | **A `503` from an unmounted route can never be observed on a frozen path, and that is a tested invariant.** `api:legacy` declares only `LatestSnapshot` as a hard requirement, and `snapshot:latest` starts successfully with `ReadingStore` unavailable, so the legacy route table mounts unconditionally. With the database down, every frozen route returns its contract response — never `503`. | W6 (C-W2a-28) | |
| **D-49** | **Every management API route is `/api/v1/*`.** W5 versions the management surface; W3's `/api/logs` spelling is superseded by `/api/v1/logs`. | W6 (C-W3-40) | |
| **D-50** | **`WindowedView` resolves `Mean`, `Min`, `Max`, `Sum` and `CircularMean` from the registry's declared strategies, mapping `Sum` onto the registry's `total`. There is no `Agg::Last`** — an output needing a metric's current value reads `LatestSnapshot`, not a window. | W6 (C-W2a-10) | |
| **D-51** | **`Provision` carries `ProvisionDetail::Routes(RouteManifest)`**, so a duplicate route is a `PlanError` naming both owners rather than silent last-registration-wins. | W3 D-91, accepted by W6 (C-W3-2) | |
| **D-52** | **`ComponentControl` also carries the runtime `tracing` reload handle** — one consumer, same root provider, and a thirteenth capability for one consumer is noise in a graph whose only jobs are ordering and `Waiting`. | W3 D-90, accepted by W6 (C-W3-2) | |
| **D-53** | **`ComponentControl` also exposes `applied_generation` per component and the newest committed `generation`.** It is supervisor state by construction, so computing it elsewhere means a second source of truth for "is this change live". W5 §2.2 is unimplementable without it. | W5 assumption 4, accepted by W6 (C-W3-2) | |
| **D-54** | **`/span`'s response cache holds the closed-bucket prefix only; the trailing partial bucket is appended per request.** W3 D-40's 60-second TTL is lossless of the materialised prefix and *not* of W4 D-13's tail, which changes at the raw cadence. The tail costs a measured 0.44–6.89 ms. | W6 (C-W3/W4-1) | |
| **D-55** | **The trailing-bucket tail query lives in `mw-store`**, so `ReadingStore::span` returns materialised buckets plus the tail and no caller can forget it. `/span/24h` means "24 hours of 5-minute buckets", and the last one is still filling. | W6 (OQ-W4-6, with D-54 as a second reason) | |
| **D-56** | **`Ingress::admit` upserts `stations` on identity resolution; `writer:timescale` maintains `stations.last_seen` and `station_metrics`.** They are read-side projections and nothing else populates them at run time. | W6 (C-W4-20) | |
| **D-57** | **The management OpenAPI schema and its TypeScript client get their own drift gate (`mgmt-schema-drift`), not `codegen-drift`.** `cargo xtask codegen --check` must keep meaning exactly "`generated/` matches `metrics/registry.toml`"; a second emitter with a different input gets a second gate, following `legacy-import-drift`'s precedent. | W6 (C-W5-36, after W9 D-20) | |
| **D-58** | **Node and pnpm are build-time requirements of a release machine, not runtime requirements of the artefact.** `cargo xtask dist` builds the UI bundle once, natively, **before** the cross matrix, and fails if the bundle is missing or stale. The shipped artefact is still one static binary. | W6 (C-W5-45) | |
| **D-59** | **An envelope's `path` is an ordered `SmallVec<[OriginId; 8]>`, not a bitset.** Three of the six receive-check clauses are positional, and a set cannot distinguish "added by my sender's `forward`" from "here two hops ago". At `MAX_PATH = 8` a bitset is an optimisation with no benefit and an overflow case to guard. | W7 assumption 12, accepted by W6 (C-W7-12) | |
| **D-60** | **`Ingress` has a second entry point, `accept()`, which never reassigns provenance.** `admit` assigns `seq`; running it on a forwarded envelope renumbers it in the receiver's coordinate space and destroys the `(origin, seq)` identity the cumulative ack and the dedup gate both depend on. | W7 assumption 13, accepted by W6 (C-W7-13) | |
| **D-61** | **Provenance is not a column on the raw hypertable.** Nothing reads it — idempotency lives at the spool gate and the natural key — and `origin` would be a `[[dimension]]`, which has no deprecation path, making the column a one-way door. If it is wanted later, **N-14**'s `metadata` role is a prerequisite. | W6 (C-W7-26) | |
| **D-62** | **The database version is asserted at startup as well as in CI**, in the binary's boot sequence after the pool opens and before `plan()` runs, failing with a message naming the required and found versions — the same comparison `ci/sql/assert-timescaledb-version.sql` makes. | W6 (C-W8-6), discharging the other half of D-42 | |
| **D-63** | **The codegen interface is `cargo xtask codegen [--check]`, implemented in `xtask/src/gen.rs`.** There is no `mw-codegen` package and no `xtask/src/codegen.rs`; `docs/release.md`'s five references to them describe an interface that was dropped at merge. `ci/gates.toml` has always declared the correct command. | W6 (C-W8-7) | |
| **D-64** | **A gate's `arms_when` must name a path that some work item's done-criteria require it to create.** A gate armed on a path nobody owns reports green forever; a gate armed on a path that already exists fails immediately. G-1 is the first failure mode, observed. | W6 (G-1) | |
| **D-65** | **The configuration store's station table is `config.stations`; the PostgreSQL read-side projection is the `observations`-side `stations`.** One is authoritative and the other is a cache, and no document or comment may write "the stations table" unqualified. | W6 (G-2) | |
| **D-66** | **Per-station third-party service settings normalise into a `station_service` table** rather than sitting on the device row beside the hardware configuration. W5's OQ-W5-4 deferred this to W4 on the grounds that normalising costs a migration; since the configuration schema is now written from scratch (**N-4**), there is no in-place migration to avoid. | W6 (OQ-W5-4) | |
| **D-68** | **The frozen surface is the five endpoints the iOS client calls** — `/latest`, `/span/{hours}h`, `/snow`, `/stationinfo`, `/forecast/{hours}`. Everything else the server exposes is **coupled, not frozen**: the public website's JavaScript ships *inside the server binary* (`rust-embed`, D-76), so server and website are one artefact and one release, and a response change there is a refactor that must touch both halves in one PR rather than a contract break. **The test is "can the client be updated atomically with the server?"**, which has an answer for any future client. Supersedes D-37's scope; D-37's mechanism — an executable definition in `contract/` — is unchanged. | Owner, 2026-10-31 (W6 §6) | |
| **D-69** | **`/alerts`, `/api/remote-stations` and `format=msgpack` are not carried forward.** None is called by either shipped client; `legacy-v1.json` records that no client sends `msgpack`, and `/api/remote-stations`' table is retired by D-45 anyway. This also disposes of W1-BUG-2 by deleting its subject. They become the v2 deprecation list's first closed entries rather than open candidates. | Owner, 2026-10-31 (W6 §6.3) | |
| **D-70** | **Production's rollup-only history was already destroyed before the programme measured it**, so W9's legacy-archive apparatus has no input. **No endpoint could have served it anyway** — `/span` returns `400 time span exceeds maximum allowed duration of 1 year`, measured against the live server, which is why the loss had no symptom (W6 §8.1b). `observations_1d_legacy`, `observations_1h_legacy`, `w9.legacy_boundary`, `w9.assert_legacy_disjoint()` and `observations_1d_with_legacy` are **kept, not removed**: they are built, tested and harmless, and they are the only route by which that history could return if an older `pg_dump` of pitcairn surfaces. Measured 2026-10-31 — 301 days of empty chunk ranges on `_materialized_hypertable_5` (`2025-01-01` to `2025-10-29`), with `n_tup_del` of 121 and 50 on the two boundary chunks as direct deletion evidence. The number of those days that held readings is **not** recoverable from the surviving database; `almanac_cache` independently proves readings existed back to 2025-01-22. | W6 §8, measured | |
| **D-71** | **Raw retention on the source is paused, not removed** (`alter_job(1004, scheduled => false)`), and `archive` must succeed before `pause-source-retention` will run. The two protect different things and the order is enforced by the script rather than documented. | Owner-authorised, executed 2026-10-31 (W6 §8) | |
| **D-67** | **The four plaintext third-party credentials are not migrated.** `remote_stations`' `aprs_password`, `wu_api_key`, `aeris_client_secret` and `pws_password` crossed the wire in cleartext and sat in a second database where nothing read them. **N-5** drops the columns; the owner re-enters the rotated secrets once. Everything else in the configuration is migrated, not re-typed. | W6 (G-2, after A-W7-2) | |

**Seventy-one decisions, plus D-30a/D-30b replacing D-30.** D-68 and D-69 are the owner's, taken
2026-10-31 and recorded in `docs/reconciliation.md` §6. Twenty-one are this work item's, and every
one of those is a *resolution* of something two documents left between them rather than new design —
see `docs/reconciliation.md` §1 and §2 for each one's derivation. One further decision (**D-72**,
retiring `remoteweather:docs/rewrite/`) is **proposed and not applied**, because applying it means
writing to the read-only reference repository: see `docs/reconciliation.md`'s *Proposed amendments*.

---

# Open questions

Stated as decisions for the owner, with a recommendation each.

> **W6, 2026-10-30 — status of this section.** Three of the six are closed by a later document and are
> kept here, marked, rather than deleted, so the reasoning stays findable: **OQ-3** (sum-and-count
> versus `stats_agg`) → W4 D-2, sum-and-count everywhere; **OQ-4** (compress or bound the minute
> level) → W4 D-4/D-5, compression proved *impossible* at 629 columns and retention taken instead
> (A-W4-2); **OQ-5** (per-level membership) → W4 D-3, every level carries every aggregatable metric,
> after W2's A-W2-1 showed OQ-5's recommendation was not constructible under D-11.
>
> **OQ-1, OQ-2 and OQ-6 are still open and all three need a query against production**, which is not
> reachable from an agent runtime. They are carried as **O-1**, **O-3** and **O-4** in
> `docs/reconciliation.md` §5, alongside nine other questions the programme surfaced. **That
> consolidated list is the one to read**; this section is kept for its rationale.

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
