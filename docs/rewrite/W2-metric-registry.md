# W2 — Metric registry and codegen spine

**Status: COMPLETE.** Delivered as `docs/metric-registry.md`, `metrics/registry.toml`,
`metrics/snapshot.toml`, `xtask/` and `generated/`. This file is the prompt that was run, kept
for the record; the outline it replaced is in git history.

---

**Depends on:** W0 — merged. `docs/DESIGN.md` and `docs/spike-findings.md` are on `main`; read both
before writing anything. Runs in parallel with W1, W2a and W8.
**Produces:** `docs/metric-registry.md`, the registry file itself, a working `cargo xtask codegen`,
its committed output, and a committed schema snapshot.
**This item writes real code.** It is not design-only.

## Context

A metric in the Go version is defined in eight places, and nothing keeps them consistent. Measured
in the read-only `remoteweather` tree:

| Place | Size |
|---|---|
| `internal/types/weather.go` — the `Reading` struct | 215 fields |
| `internal/database/types.go` — `FetchedBucketReading` | 38 fields |
| `internal/storage/timescaledb/sql.go` — `createTableSQL` | 213 columns |
| the same file — `create1mViewSQL`, `create5mViewSQL`, `create1hViewSQL`, `create1dViewSQL` | 2,677 lines total, ~396 aggregate expressions in the 1-minute view alone |
| `protocols/remoteweather/remoteweather.proto` | 241 numbered fields, highest number 225 |
| `internal/controllers/restserver/types.go` — the REST DTOs | 133 JSON tags, 31 `omitempty` |
| `internal/storage/grpcstream/conversion.go` | 276 lines of hand-written mapping |
| `internal/controllers/restserver/weatherapps_grpc.go` | 315 lines of hand-written mapping |

591 lines of that are mapping code that exists only because the same metric is declared twice in
incompatible shapes. The consequences are already visible and already shipped: the source's table
definition has drifted to 215 columns while production sits at ~143 (D-8), and the protobuf numbering
is damaged — station metadata wedged in at 188–192 and relays at 193–195 because 1–187 were gone by
the time anyone needed them (D-21).

**Your job is to make that one file, and make every other representation a build artefact.** This is
the work item every other one leans on: W4 generates its DDL from your output, W8's gate is your
drift check, W3 and W5 consume your types, and W1's frozen contract constrains what your REST
representation is allowed to look like.

---

## The four hazards

The spike measured four things that change what a generator must emit. Two of them are **silent when
wrong** — they produce plausible numbers that are not the right numbers — so they are stated first
and they are not optional.

**H-1. Sums accumulate in a wider type than the column. Emit `sum(col::double precision)`, never
`sum(col)` (D-14).** PostgreSQL's `sum(real)` returns `real`, and `float4` addition is not
associative, so a hierarchy adds in a different order than a direct scan and gets a different answer
— measured wrong in 55 of 56 uneven-bucket pairs, by up to 1.55 × 10⁻⁴. Widened, it was bit-identical
in 56 of 56, error exactly zero. The public API is frozen (W1), so "small" is not the relevant axis:
the same question must not return different answers depending on which aggregate level served it.

**H-2. Counts are nulled at zero. Emit `nullif(count(col), 0)`, never bare `count(col)` (D-16).**
`count()` returns 0 and never NULL, so on a 70%-NULL table every count column of a wide aggregate is
fully populated and almost entirely zeroes — which discards exactly the null-bitmap density that
justifies the wide layout. Measured on a 398-column 1-minute view over 30 days: 347 MB plain, 234 MB
nulled, for 0.9 s of extra refresh time.

**H-3. No level stores a finished average (D-12), and each level reads the level below it, not raw
(D-11).** `avg(avg(x))` was wrong in all 56 uneven-bucket pairs, maximum error 0.114. Production's
daily view contains 135 `avg()` columns and all four of its views read raw directly; both facts have
the same cause. Your templates must make the hierarchical form the only form that can be emitted.

**H-4. The column budget is counted against allocated identifiers, not live ones (D-19).** A dropped
column keeps its attribute number forever — after the spike's add/drop experiments a 216-column table
occupied 239 of 1600 slots. Compression mirrors every column including dropped ones and adds five of
its own, so the effective ceiling is ~1595 and a dropped column is charged twice. Separately, a
near-ceiling aggregate cannot be compressed at all: the 1600-column view the spike built was refused
with an estimated row size of 28,852 bytes against a maximum of 8,160. The generator refuses to emit
over-budget SQL with an error naming the ceiling; it does not discover this at deploy time.

---

## 1. The registry format

**The registry is authored in TOML.** Owner decision — "whatever is cleanest and easiest" — resolved
to TOML because a metric list is human-editable declarative data, `serde` parses it trivially, it is
readable by non-Rust tooling, and no recompile is needed to inspect or diff it. The compile-time
validation argument for Rust consts does not buy the safety that actually matters here: the critical
invariants are append-only identifiers and no reuse of column names or field numbers, and a `const`
cannot catch a reused field number either. Those have to be enforced by the generator against the
committed snapshot regardless of authoring format (§2). So: TOML as the source of truth, and the
generator owns invariant enforcement.

Design the entry schema and document it. At minimum an entry carries:

- **stable key** — the registry's own identifier, never reused (D-21)
- **column name** and **SQL type**, allocated once and never reused
- **protobuf field number**, allocated once and never reused
- **label and unit** — user-facing strings; see §6 before you write one
- **tier** (core/extended)
- **aggregation strategies** — which functions apply, including the circular case below
- **per-level inclusion** — which of the aggregate levels carry this metric (OQ-5)
- **introduced-in version**, and a **deprecated** state that retains the identifiers (D-21)
- **legacy REST key(s)** — see §6

Four shapes the format must handle, because getting the schema wrong here is expensive later:

1. **Non-metric columns.** `time`, `stationname`, `stationtype` and the station identifier are
   columns the generated types need and are not metrics. Decide whether they live in the registry as
   a distinct kind or outside it entirely, and say which.
2. **Multi-column strategies.** A mean stored as sum-and-count is two aggregate columns from one
   registry entry; `stats_agg` is one. **W4 chooses between them (OQ-3) — you do not.** The format
   must express both without a rewrite, and the generator must be able to emit either.
3. **The circular average.** Wind direction cannot be averaged arithmetically. The Go implementation
   already contains a correctly composable `circular_avg_state` — sine sum, cosine sum, count,
   finalised with an arctangent — and the spike verified it composes through all three levels to
   within 3 × 10⁻¹² degrees (D-13). **That pattern carries forward.** It is a first-class aggregation
   strategy in the registry, not a special case bolted onto the generator.
4. **Text and enumerated columns**, which have no meaningful aggregate at all. The format must be
   able to say "raw only".

## 2. The committed snapshot, and what is fatal

The registry alone cannot enforce append-only — a human editing TOML can rename anything. The
generator enforces it by diffing the registry against a **committed schema snapshot**, which is the
record of what has ever been allocated.

Fatal, no flag to override:

- a stable key, column name or protobuf field number that was allocated to one metric now appearing
  on another
- an identifier disappearing from the registry without being marked deprecated
- a column budget over the ceiling, counting allocated rather than live identifiers (H-4)
- an aggregate that would emit more expressions than the ceiling permits

Refused by default, overridable only by an explicit and loud flag, because each one is a data-losing
or contract-breaking change rather than a mistake: column rename, SQL type change in either
direction, aggregation strategy change, protobuf type change.

State plainly what the escape hatch is and what it requires of the operator. A generator that
silently renders a destructive change as SQL is worse than one that refuses.

## 3. `cargo xtask codegen`

Generate, from the registry, into one clearly marked directory, committed, each file carrying a
do-not-edit header naming the registry and the generator (R-2):

| Target | Notes |
|---|---|
| table DDL | the raw hypertable, nullable columns, compression settings per D-4/D-7 |
| migration bodies | see §4 |
| the aggregate definitions | one template × registry × level; H-1, H-2, H-3 are properties of the template, not of the caller |
| Rust types | the reading type and its accessors, replacing the 215-field hand-maintained struct |
| `reading.proto` | field numbers from the registry, never renumbered |
| OpenAPI schema | see §6 |
| TypeScript types | plus the label/unit table the management UI renders from |

`--check` **regenerates into a temporary location and diffs** (R-2). It must never write into the
working tree: a check that fixes the problem it is checking for always passes. W8 wires it into the
PR gate; you provide the command and its exit codes.

How many aggregate levels the template is invoked for is D-11's answer — 1-minute from raw, 1-hour
from 1-minute, 1-day from 1-hour. The Go version's 5-minute level is **not** assumed to carry
forward; if someone later wants it, it derives from the 1-minute level.

## 4. The migration generator

Diff the registry against the snapshot and emit a timestamped `sqlx` migration. No DDL at startup,
ever (D-8).

Classify the diff. The one genuinely cheap change is **adding a nullable column**: measured at
11.6 ms against ~1 GB of compressed chunks with every chunk still compressed afterwards, and 93 ms
for twenty columns in one statement (D-9). So "adding a metric is cheap" is confirmed — with one
caveat that belongs in the generated migration rather than in a document nobody reads at 3 a.m.:
the statement takes `AccessExclusiveLock` on the hypertable, every chunk and every compressed mirror
— 63 relations on the spike instance — so it is instant but it queues behind long-running readers and
blocks everything while it waits. **Generated metric migrations set a short lock timeout and retry**
rather than stalling the API.

Aggregate changes are not migrations in the usual sense. Aggregates are disposable derived data
(D-15): the supported way to change one is to drop it, regenerate it and refresh. Emit that, not an
in-place alter. W4 owns the rebuild procedure and its relaxed-durability requirement (D-17); your job
is to emit something whose shape W4 can execute.

## 5. Bootstrapping the registry with real metrics

The registry must ship populated, not as an empty schema with a worked example. This runs into a
problem W0 recorded and could not settle: **the two candidate sources disagree by 72 columns.** The
Go source defines 215 and production has ~143 (D-8, OQ-6).

Populate from the **Go source definition**, and record the gap rather than resolving it. Reasons: it
is the superset, it is readable from the read-only checkout you already have, and production is not
reachable from an agent runtime at all — W0's OQ-1 established that. W9 reconciles the actual
production column list against the source and reports the difference (OQ-6); your deliverable is a
registry that is complete with respect to the source plus an explicit note that the production-side
reconciliation is outstanding and owned by W9.

Do not quietly drop the columns that exist in the source and not the database. A column that the
code writes and the database does not have is a bug in the old system, and the registry is where it
becomes visible.

## 6. Where your output meets the frozen contract and the UI

Two seams you must handle explicitly rather than discover later.

**The legacy REST keys are not yours to choose.** W1 is capturing the frozen external contract in
parallel, and the iOS app and the public website are both frozen against it. The existing key names
are inconsistent and in one case duplicated — `ohum` and `outhumidity` carry the same measurement,
and `otemp`, `bar`, `winds`, `windd` run together while `period_rain` is snake_case. **Record the
quirks; do not fix them.** The registry needs to express a metric's legacy JSON key or keys —
plural, because of the duplication — so that a generated representation can satisfy the frozen
contract without anyone hand-writing a mapping layer again. That is the whole point of retiring
those 591 lines. Where you need W1's answer and do not have it yet, state the assumption in the
closing section so it is greppable.

**Labels are user-facing, and the UI has a rule about that (D-36).** W5 §0 owns the
internal-to-user-facing vocabulary translation table, and an internal term appearing on screen is
treated as a bug. Your `label` field is the thing the UI renders. Write labels a new user would
understand, and record that W5 §0 is authoritative if the two ever disagree.

---

## Do not

- **Do not decide sum-and-count versus `stats_agg`.** That is W4's (OQ-3). Make both expressible.
- **Do not decide which metrics the 1-minute level carries.** W4's (OQ-5). Make it expressible. Note
  for context that the minute level is not free — ~42 GB for a decade at production width, larger
  than the compressed raw table it derives from (D-18).
- **Do not decide aggregate compression or retention.** W4's (OQ-4).
- **Do not design the crate and workspace layout.** W3's (R-4). Put `xtask` in a minimal root
  workspace: if `Cargo.toml` already exists on `main` because W8 landed first, add `xtask` as a
  member; if not, create the smallest root workspace that holds it and say in your document that you
  did, so W3 can restructure around it. Record this as an assumption either way — you and W8 are
  both touching the repository root in the same stage.
- **Do not design the CI workflow.** W8's. You own the `--check` command; W8 owns the gate.
- **Do not enumerate the legacy API surface.** W1's. See §6.
- **Do not write application code** — no drivers, no components, no HTTP handlers. The generator and
  its output, nothing else.
- **Do not touch the production database.** It is unreachable from here anyway.
- **Do not amend a `DESIGN.md` decision in place.** If you find one wrong, propose the amendment in
  your closing section and carry on under the existing decision.

## Done when

- `docs/metric-registry.md` ends with the four required sections from the output contract in
  `docs/rewrite/README.md`: **Decisions** (numbered one-liners), **Open questions** (with
  recommendations), **Assumptions about other work items**, and **Proposed amendments to
  `DESIGN.md`**. W6 consumes these; a document without them cannot be reconciled.
- The registry is committed and populated from the Go source definition, with the ~72-column gap to
  production noted and attributed to W9.
- `cargo xtask codegen` runs, and its output is committed.
- `cargo xtask codegen --check` **passes on the clean tree and fails on a deliberate one-character
  edit to a generated file** — show both runs. A drift check nobody has seen fail is not known to
  work.
- Each fatal invariant in §2 has a demonstrated failure: show the error message for a reused
  protobuf field number, a reused column name, and an identifier deleted without deprecation.
- The generated aggregate SQL is inspected and shown to use the widened sum (H-1), nulled counts
  (H-2), and the hierarchical form reading the level below (H-3). Paste the relevant lines.
- The column budget calculation is in the document, counting allocated identifiers, with the current
  number against the ~1595 effective ceiling.
