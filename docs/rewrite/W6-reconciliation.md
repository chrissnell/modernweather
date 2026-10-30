# W6 — Reconciliation and implementation plan

**Depends on:** every design work item having landed (W0, W1, W2, W2a, W3, W4, W5, W7, W8, W9).
**Produces:** `docs/reconciliation.md`, an updated `docs/DESIGN.md`, and `docs/implementation-plan.md`.
**Writes no implementation code.**

## Why this exists

The design documents were written by separate agents that never spoke to each other. Each was explicitly
told to stay in its lane and not redesign its neighbours' work. That is the right instruction for
producing the documents, and it guarantees two problems:

1. **Contradictions.** Two documents make incompatible assumptions and nobody notices until
   implementation, which is the expensive place to find out.
2. **Seams nobody owned.** Every prompt has a "do not" list. The union of those lists is a set of
   questions no document was responsible for answering.

This work item is the integration pass. **Its job is to reconcile, not to redesign.** Where two
documents conflict, surface it and propose the minimal resolution; do not quietly rewrite one of
them, and do not introduce a third approach.

## 0. Five things this prompt could not know

All ten documents are merged. `main` carries `docs/DESIGN.md`, `spike-findings.md`,
`legacy-api-contract.md`, `metric-registry.md`, `component-model.md`, `service-architecture.md`,
`database-schema.md`, `management-ui-design.md`, `collector.md`, `release.md` and
`data-migration.md`, each with the four required closing sections, plus real code: `metrics/`,
`generated/`, `migrations/`, `xtask/`, `contract/`, `ci/` and `.github/workflows/`. The PR gate is
live and green.

Five things happened during the programme that change your inputs. None is a redesign; they are
findings you would otherwise spend your first hours rediscovering, or deliverables this prompt
forgot to ask for.

**1. You owe a deviation ledger, and §3 never asks for one.** Two documents reference "W6's deviation
ledger" by name as though it were already a deliverable. W3 produced three entries — an added `ETag`
on `/latest`, `/snow` and the assets; case-insensitive `Host` matching; `404` instead of a directory
index — each a deviation from **D-37**'s frozen contract, however small, and W3 says explicitly that
they are "listed again in the assumptions for W6's ledger." W4's assumption 39 states, equally
explicitly, that it has **no** entry. W9's OQ-W9-4 asks whether `period_rain`'s accuracy improvement
belongs in one. So: **add a deviation ledger to `docs/reconciliation.md`** — every place a design
knowingly changes a byte a shipped client could observe, with who decided it, the reasoning, and
whether it is additive or behavioural. This is the artefact that answers "did we break the iOS app,
and if so where and on purpose."

**2. One CI gate is green by deferral, not by running — and it guards a completed deliverable.**
Work through this chain yourself, then decide who fixes it:

- `ci/gates.toml` declares gate `conformance`, owned by W1, with `arms_when =
  "tests/conformance/manifest.toml"` and `cmd = ["cargo", "xtask", "conformance",
  "--target-binary", "target/debug/modernweather"]`.
- `tests/conformance/manifest.toml` does not exist and nothing is going to create it: W1 shipped its
  runner as a standalone crate at **`contract/conformance/`** with 70 golden fixtures, per W0's R-3.
- There is no `conformance` subcommand in `xtask` and no `modernweather` binary.
- The CI job named *"frozen contract conformance"* runs `cargo xtask gate conformance`, which reads
  the manifest, finds it absent, prints `DEFERRED` and **exits 0**.

So a job reports green on every PR while W1's suite — whose own "done when" required it to pass
against the Go server — never runs in CI. This is precisely what your §2 means by "verify the PR gate
actually exists and is green, not just designed." Note the contrast with `codegen-drift`, where the
same mechanism worked exactly as intended: W2 landed `metrics/registry.toml`, the gate armed itself,
and it has been running since. The mechanism is sound; this one gate's `arms_when` and `cmd` point at
an interface nobody built. `driver-emulators` is also `DEFERRED`, but legitimately — no Rust drivers
exist yet.

**3. The workspace root on `main` is partly a merge resolution, not any document's decision.** W2 and
W8 ran in the same stage and both created a root `Cargo.toml` and an `xtask` crate, because each was
told to create one if none existed. At merge the resolution was: keep W8's root manifest (W2's was an
explicit placeholder), fold both module sets into **one** `xtask` crate, and drop W8's placeholder
`xtask/src/codegen.rs` — it shimmed out to a separate `mw-codegen` package, while `ci/gates.toml`
already declared the drift gate as `cargo xtask codegen --check`, which is what W2 implemented. W2's
assumption 13 then says W3 "will restructure the minimal root workspace this work item created," and
W3 did decide the crate layout. **Nothing guarantees the tree and W3's decision agree.** Check
`Cargo.toml`, `crates/` and `xtask/src/` against W3 §1 and record the result; `contract/conformance/`
declares its own empty `[workspace]`, which W1 left to W3 as well.

**4. Four named W2 follow-ups are unowned.** W4's open questions each end in a recommendation
addressed to W2, and W2 is finished: OQ-W4-1 (`affected_levels` drops every level when one level's
membership changed), OQ-W4-2 (dimensions have no deprecation path, so `stationname`/`stationtype`
cannot leave the raw table), OQ-W4-3 (no `jsonb` SQL type, so `observations.extra` sits outside the
registry), OQ-W4-4 (`cargo xtask codegen` writes stale files and never prunes them, so `codegen`
leaves a tree `--check` rejects). Feed these into §2 as gaps with an owner, not into §4 as questions
for the owner — they are small, decided, and need a work item rather than a decision.

**5. Four things need the owner and cannot be resolved by anyone here.** Keep them in §4 as a
separate group, because they are not design questions and they have been outstanding for the whole
programme: **pre-flight 1** — whether the production 365-day raw retention policy has been paused,
still unconfirmed, and every day it fires converts another day of raw into unrecoverable rollup-only
history; **OQ-1** — production's PostgreSQL and TimescaleDB version, which W4 §5.2 says moves several
of its decisions and which, below TimescaleDB 2.9, makes a major-version upgrade part of W9;
**OQ-2** — the compression hold-rate query, which brackets a capacity figure that has already
propagated into planning documents; **OQ-6** — the ~72-column gap between the Go source's 215-column
definition the registry was bootstrapped from and production's ~143 columns. **Production is not
reachable from an agent runtime** (W0 OQ-1), so all four are queries for the owner to run. Do not
treat them as answerable here.

**One piece of bookkeeping for §2.** W2's and W4's prompts were outlines when the programme started
and were written mid-flight from merged output; W1, W3 and W9 each had a `§0` appended for the same
reason. The consequence is that `remoteweather:docs/rewrite/` — the second "canonical copy" the
master issue describes — is now stale across the board, while `modernweather:docs/rewrite/` is
current. W0's **R-1** argues for referencing the Go repo by URL and nothing more, which argues for
retiring those copies. Record it as a gap with a recommendation; it is the owner's call, and
`remoteweather` is read-only reference for every other purpose.

---

## Inputs

Read all of these, and read them *together* rather than in sequence:

- `docs/DESIGN.md` and its `Amendments` section (W0)
- `docs/spike-findings.md` (W0)
- `docs/legacy-api-contract.md` plus `contract/` (W1)
- the metric registry design and generator (W2)
- `docs/component-model.md` (W2a)
- `docs/service-architecture.md` (W3)
- the database schema and migration design (W4)
- `docs/management-ui-design.md` (W5)
- `docs/collector.md` (W7)
- the CI workflows and `docs/release.md` (W8)
- `docs/data-migration.md` and the migration scripts (W9)

Every one of those documents ends with the four required sections from the output contract in
`docs/rewrite/README.md`: **Decisions**, **Open questions**, **Assumptions about other work items**,
and **Proposed amendments**. Those four sections are your primary input — the prose is background.

## 1. Contradiction audit

The **Assumptions about other work items** sections are the detector. For each assumption one
document made about another, check it against what that document actually decided.

Produce a table: the assumption, who made it, what the other document actually says, and whether it
holds. For each that doesn't hold, propose the minimal resolution and say which document has to
change. Flag any where the resolution is a genuine design choice rather than a clerical fix — those
go to the owner, not to you.

Known likely fault lines, as starting points rather than an exhaustive list:

- whether `Component::tick` is a single method across all six triggers, or `Service` got its own
  entry point (W2a decides; W3's supervisor section assumes something)
- whether `HttpPoll` ended up a transport or a `Trigger::Every` wrapper (W2a §4a), and whether W3's
  HTTP layer assumed the same
- the ingest-versus-read-time decision for each derived value (W3 §2) against what the schema and
  aggregates actually provide (W4), and against the windowed values the outputs need (W2a §4b)
- whether the legacy compatibility adapter's zero-filling (W1) stays sealed behind the boundary W3
  §1 specified, given the nullable domain types the registry generates (W2)
- whether the running-state surface W5 needs matches the lifecycle states W2a defined, field for field
- whether the OpenAPI schema W5's generated client consumes is actually produced by W2's generator

## 2. Gap audit

Collect every "do not" line from all ten prompts and check whether anything in the union is
unowned. For each gap: state it, say which document should own it, and either resolve it inline if
it is small or raise it as a new work item if it is not.

Specific ones to check, since the prompts deliberately deferred them:

- the public website's three Go templates — W3 §7 owns the serving mechanism; verify the decided
  `weather-app.js.tmpl` replacement (static JS + JSON config endpoint) was actually specified
- migration of the existing `weather_v2_0_0` data — owned by W9; verify it landed and that the
  history decision was presented to the owner rather than assumed
- the release and packaging story — owned by W8; verify every target in its matrix was produced or
  consciously dropped, and that the armv6 answer fed back into the SQLite-driver choice if needed
- CI — owned by W8; verify the PR gate actually exists and is green, not just designed
- **the new v2 API surface** — W1 and W3 both defer to "a later work item," but no design work item
  owns it. Confirm it is deliberately post-design scope, or raise it as a named work item
- **the config store and config migration** — W9 covers weather data only. The new config schema is
  W3/W4 territory, but migrating (or deliberately hand-recreating) the owner's existing SQLite
  config database is unowned. Resolve it or raise it; "re-enter by hand" is acceptable only as a
  recorded decision
- the four dead Go tools pending the owner's delete confirmation

## 3. Decision ledger

Merge the `Decisions` sections of all documents into one numbered ledger in `DESIGN.md`, each entry
carrying its source document. This becomes the single place to answer "why is it like this," and the
thing an implementation agent reads instead of ten design documents.

Apply the accumulated `Proposed amendments` using the protocol: append to `Amendments`, mark the
superseded decision, never edit a decision in place.

## 4. Open questions for the owner

One consolidated list, deduplicated, each with: the question, why it matters, the options, and your
recommendation. The four questions that were open when this prompt set was first drafted (registry
format, `weather-app.js.tmpl`, UI technology, dead Go tools) are **all settled** — see the master
issue's "Decisions resolved" table — so do not re-open them; this list is for whatever the design
documents surfaced since.

Keep this list short and decision-shaped. If it exceeds a dozen items, the design documents left too
much open and that is itself a finding worth stating.

## 5. The implementation plan

`docs/implementation-plan.md`. Ordered work items with dependencies, each sized to be reviewable in
one sitting, each with its own done-criteria and test strategy.

Constraints on the ordering:

- the **metric registry and its generator come first**, because everything else is downstream of
  generated types
- the **frozen-contract conformance suite from W1 must be green before any legacy endpoint is
  reimplemented**, not after
- the **first station driver should be WeatherLink Live** — UDP and HTTP, so it can be iterated
  without serial hardware, and it is the best-covered driver in the Go tree at 360 test lines
- **CI exists from the first commit**, not retrofitted
- each milestone must leave the system in a state where the Go version is still the one in production
  and the Rust version is independently verifiable

Say explicitly where the two implementations run side by side and how they are diffed, because that
is the validation story for everything that has no test oracle.

## Do not

- Redesign anything. If a document is wrong, say so and propose the minimal change.
- Resolve a contradiction that is a genuine design choice — escalate those to the owner.
- Write implementation code.
- Let the implementation plan exceed what the design documents actually decided. Gaps are findings,
  not invitations to invent.

## Done when

- Every assumption in every document's **Assumptions about other work items** section is checked and
  recorded as holding or not.
- Every unowned gap is either resolved inline or raised as a named work item.
- `DESIGN.md` carries one numbered decision ledger with sources, and amendments are applied via the
  protocol.
- The open-questions list is consolidated, deduplicated, and each entry has a recommendation.
- The implementation plan is ordered, dependency-aware, and satisfies the five ordering constraints
  above.
