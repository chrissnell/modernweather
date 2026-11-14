# W6 — Reconciliation

This is the integration pass over the ten design documents. It **reconciles; it does not redesign.**
Where two documents conflict, the conflict is surfaced and the minimal resolution proposed. Where a
resolution is a genuine design choice rather than a clerical fix, it is escalated to the owner in §5
rather than decided here.

The input is the four closing sections of every document — **Decisions**, **Open questions**,
**Assumptions about other work items**, **Proposed amendments** — as the output contract in
`docs/rewrite/README.md` requires. The prose was read as background.

| Section | What it is |
|---|---|
| §1 | **Contradiction audit.** Every assumption in every document, checked against what the other document actually decided. 329 assumptions, each recorded as holding or not. |
| §2 | **Gap audit.** The union of all ten prompts' "do not" lists, checked for anything unowned. Each gap resolved inline or raised as a named work item. |
| §3 | **Deviation ledger.** Every place a design knowingly changes a byte a shipped client could observe. |
| §4 | **Work items raised by this document.** The named items §2 raises, in one place. |
| §5 | **Open questions for the owner.** Consolidated, deduplicated, each with a recommendation. |

The numbered decision ledger and the applied amendments are in `docs/DESIGN.md`, not here. The
ordered build sequence is `docs/implementation-plan.md`.

## 0. What this pass found, in one page

**The documents cohere better than the method predicted.** Of **329** assumptions checked,
**208 hold exactly as written**, 64 hold with a narrowing or correction that changes no decision, 8
are forward commitments on artefacts not yet built, and **49 do not hold** — representing **27
distinct defects**, since a defect between two documents is normally reported from both sides.

**None of the 27 is a disagreement about what the system should do.** They fall into four kinds:

1. **Stage-parallel blindness (9 defects).** Two documents written in the same stage could not read
   each other. W5 and W7 ran in parallel, so W5's page inventory has no enrolment-token list; W2 and
   W8 ran in parallel, so W8's references to a `mw-codegen` binary describe an interface W2 did not
   build. Every one of these is clerical.
2. **A request made of a finished document (3 defects, 6 rows).** W3, W5 and W7 each asked an upstream document
   for a small extension — `ProvisionDetail::Routes`, `ComponentControl`'s log level,
   `applied_generation`, `Ingress::accept`, an ordered `path`, `NotFrom(peer)`. The upstream document
   was already merged and could not answer. Each is resolved here on the merits, because each carries
   a stated fallback or a correctness argument, and each becomes a numbered decision in `DESIGN.md`.
3. **A forward requirement nobody executed (2 defects).** The clearest is proto3 field presence: W3 Decision
   89 requires W2's generator to emit `optional` on every metric field, W7's spool format depends on
   it, and `generated/proto/reading.proto` contains the word `optional` **zero** times. W3 itself
   called this "the one decision here that another work item has to execute"; nobody did.
4. **An unverifiable verification.** W2a's deletion of the `sun_times` table is conditional on
   W1's fixtures pinning sunrise to the minute *and the runner comparing against a fresh evaluation*.
   The fixtures pin it (`"sunrise":"7:30 AM"`); the runner compares shape and formatting, never
   values. The check W2a's decision rests on does not exist.

**One contradiction was found that no document flagged**, because it requires holding two decisions
from different documents at once: W3 Decision 40 gives `/span` a 60-second response cache, justified
as "lossless because the 1-minute aggregate advances once a minute", and W4 Decision 13 requires
`/span` to append a trailing bucket aggregated **from raw** over `[last_closed_bucket_end, now)`.
With the tail, the response changes every two seconds, so the cache is no longer lossless. See
C-W3/W4-1.

**The most consequential finding is not a contradiction at all.** One CI gate is green by deferral
while the deliverable it guards is complete and has never run. See G-1; it is the reason §2 comes
before §5 in importance.

**Four things cannot be resolved by anyone with access to this repository.** They are production
queries, they have been outstanding for the whole programme, and they are grouped separately in §5.

---

## 1. Contradiction audit

**Method.** Every numbered entry in every document's *Assumptions about other work items* section,
checked against the *Decisions* section of the document it is about. Verdicts:

- **holds** — the other document decided what was assumed.
- **holds (narrowed)** — the other document decided something compatible but narrower, or took one
  arm of an alternative the assumption itself offered. No decision changes.
- **holds (corrected)** — the substance holds; a figure or a name in the assumption is wrong.
- **forward** — the assumption is about an artefact a later work item must produce. Recorded as a
  commitment, carried into the implementation plan, not a contradiction.
- **FAILS** — the other document decided otherwise, did not decide, or the artefact contradicts it.
  Every one of these is expanded after its table.

Where a verdict rests on something in the tree rather than in a document, the check is named so it
can be re-run.

### 1.1 W0 / `DESIGN.md` — 27 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W2a | Trigger is declarative; supervisor owns scheduling; a component has no timer of its own (D-25) | W2a D-6: the supervisor owns every clock; sixteen hand-rolled clocks become supervisor state | holds |
| 2 | W2a | Lifecycle has a waiting state distinct from failed, entered on an unavailable capability (D-27) | W2a D-26: `Waiting{on}` is a state and carries the capability *and its provider* | holds |
| 3 | W2a | Capabilities are named, declared in both directions, resolvable into a startup order | W2a D-15, D-17, D-18, D-23 | holds (narrowed) — only `Hard` edges form the ordering graph; `Soft` is the cycle escape hatch |
| 4 | W2a | Cancellation plus ownership is the only stop mechanism (D-24) | W2a D-32: stopping is cancellation plus `Drop` | holds |
| 5 | W3 | W3 owns the crate and workspace layout (R-4) | W3 D-1: eighteen crates | holds |
| 6 | W3 | W3 defines the per-sink overflow policy mechanism D-29/D-30 require | W3 D-19: the policy differs per sink kind because the cost of loss differs | holds |
| 7 | W3 | W3 treats gRPC and REST as codecs over a shared listener, and names the collector role | W3 D-63/D-92: one router, N sockets; gRPC keeps its own port. The collector role is named by **W7** (A-W7-1), not W3 | **FAILS** — see C-W0-7 |
| 8 | W3 | W3 owns how the website is served, including D-38's static-JS-plus-config-endpoint | W3 D-79, D-80 | holds |
| 9 | W2 | The registry can express column, field number, deprecated state, level membership, aggregate functions | W2 D-1 – D-9 | holds |
| 10 | W2 | The generator emits `sum` over a widened accumulator (D-14) and nullable counts (D-16) | W2 D-12: one place writes `sum(c::double precision)`, one writes `nullif(count(c), 0)` | holds (corrected) — W4 D-9 added the `::bigint` cast the roll-up needed; without it three `/span` bands could not resolve `mw_mean` |
| 11 | W2 | The generator refuses an aggregate over 1598 expressions, counting allocated identifiers | W2 D-15 | holds |
| 12 | W2 | Generated output lands in one marked committed directory with a do-not-edit header (R-2) | W2 D-17: thirteen files under `generated/` | holds |
| 13 | W4 | W4 decides sum-and-count versus `stats_agg` (OQ-3) and records it | W4 D-2: `sum`/`count` everywhere, no per-metric override | holds |
| 14 | W4 | W4 owns the rebuild procedure including relaxed durability (D-17), treating a full rebuild as ~1 hour at ten years | W4 D-23, D-24 | holds (corrected) — the durability saving is 1.89×, not 12×, and the ordering is worth 537× (A-W4-3) |
| 15 | W4 | W4 compresses the 1-minute materialisation or bounds its retention (D-18, OQ-4) | W4 D-4, D-5: compression is impossible at 629 columns; retention instead | holds (narrowed) — one arm taken, and the other proved unavailable rather than merely worse |
| 16 | W4 | The hierarchy is exactly 1m-from-raw, 1h-from-1m, 1d-from-1h; the Go 5-minute level is not assumed to carry forward (D-11) | W4 D-1: **four** levels. `/span` serves four bands and `span-24h` measures 300,000 ms | **FAILS** — see C-W0-16 |
| 17 | W1 | W1 enumerates the legacy surface; this document deliberately does not (D-37) | W1 D-1: `contract/legacy-v1.json` plus 70 fixtures | holds |
| 18 | W1 | The public API is unauthenticated and stays so, so D-35 touches only the management API | W5 assumption 37; W1 adds no auth | holds |
| 19 | W1 | The fixtures live in the new repository and the runner accepts a base URL (R-3) | `contract/conformance/src/main.rs` takes `--base-url`; 70 fixtures in `contract/fixtures/` | holds (verified in tree) |
| 20 | W5 | W5 §0 owns the vocabulary translation table (D-36) | W5 D-1, D-2 | holds |
| 21 | W5 | W5 §3 owns the admin auth design within D-35's constraints | W5 D-23 – D-35 | holds |
| 22 | W8 | W8's drift check regenerates into a temporary location and never writes the tree (R-2) | W8 D-11, and `pr.yml` additionally asserts `git status --porcelain` is empty | holds (verified in tree) |
| 23 | W8 | W8's gate has concurrency-checked tests from the first commit (D-41) and asserts the DB version (D-42) | W8 D-2, D-13, D-18 | holds |
| 24 | W9 | W9 reconciles the production column list against the Go source definition and reports the gap (OQ-6) | W9 D-8 computes the import's column list at run time as `expected ∩ source`, so one artefact is correct at 143 or 215 columns | holds (narrowed) — the *mechanism* is stronger than asked; the **report** is still owed and needs production. OQ-6 stays open in §5 |
| 25 | W9 | W9 accounts for the migration also being a major-version PostgreSQL upgrade (OQ-1) | W9 OQ-W9-2; W4 assumption 36 names the decisions that move | holds (conditional on OQ-1) |
| 26 | W6 | W6 records the v2 API as a named future work item (D-39) | §4, item **N-1** | holds |
| 27 | W6 | W6 treats the four closing sections as the input and the prose as background | §1's method | holds |

**C-W0-7 — "one listener" is the wrong unit, and the collector role has a different author.**

Two separate failures in one assumption.

*The listener.* W3 D-92 keeps `WeatherV1`/`WeatherAppsV1` on their own socket, because `grpc_port` is
a configuration column defaulting to 50051 whose value the management API *validates* against the
HTTP and HTTPS ports. Folding it onto 443 would invalidate a shipped config column. W2a assumption 5
makes the same assumption as D-31 and fails the same way. **Resolution: A-W3-4, applied.** D-31's
singular unit becomes *the router and the route table*, not the socket. Nothing about the property
D-31 exists to protect — one place that owns listening, one route table, one TLS story, one shutdown
path — changes; the seven listener-owning sites in the Go tree are still the defect being removed.
Clerical.

*The collector role.* D-31's rationale names the collector role as the thing the Go version lacks and
stops short of saying what it is. W3 did not define it; **W7 did** (A-W7-1). **Resolution: A-W7-1,
applied as D-44.** Clerical — the role exists and is well specified, it is simply authored one
document further along than D-31 predicted.

**C-W0-16 — the aggregate hierarchy has four levels.**

D-11 says three. W4 D-1 says four, and the evidence is not inferential: `contract/fixtures/`
records `span-24h-five-minute-buckets` with bucket spacing of exactly 300,000 ms, and
`/span/24h` is the public website's default view. W1 assumption 10 and W3 assumption 19 both
already say four bands, so D-11 is the only document that says three — and D-11's own assumption 16
licensed a 5-minute level on the condition that it derive from the 1-minute level, which it does,
five buckets to one.

**Resolution: A-W4-1, applied.** D-11's mechanism is untouched — each level still reads the level
below it. This is a decision change rather than a clerical fix, so it goes through the amendment
protocol rather than an edit, but it is **not** a design choice needing the owner: under D-37 the
5-minute band is frozen, so three levels is unreachable without breaking the contract.

### 1.2 W1 (frozen contract) — 16 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W3 | W3 owns serving the three templates and the asset trees, treating §9 as the specification | W3 D-74 – D-78: askama with a `GoHtmlEscaper`, embedded assets, a hard-coded mime table | holds |
| 2 | W3 | The JSON config endpoint carries exactly the nine values §9 lists as consumed | W3 D-80: "exactly W1 §9's consumed set", JSON keys equal to the JavaScript property names | holds |
| 3 | W3 | `/latest`'s 10-minute recency window and `/snow`'s absence of one survive the snapshot scheme | W3 D-34, D-38; assumption 28 | holds |
| 4 | W8 | The conformance suite runs in CI against both binaries, with the Go target as the fixture-validity check (R-3) | Neither run exists. The `conformance` gate reports `DEFERRED` and exits 0; no scheduled workflow targets the Go daemon | **FAILS** — see **G-1** |
| 5 | W3 | Multi-hostname routing preserves the *fallback* of Q-18, not just the matching | W3 D-66: the empty-hostname default rule is reproduced verbatim | holds |
| 6 | W2 | The generated REST-JSON schema can express the exact JSON key, `omitempty`, and two keys from one metric | W2 D-28 records both duplications as data. `omitempty` is **not** expressible; W3 D-9 gives it to `mw-api-legacy` | holds (narrowed) — W1's own stated alternative ("or the legacy endpoints are hand-written") is what was taken. The DTO is hand-written in one crate that exclusively owns it |
| 7 | W2 | The registry can express a field whose unit differs between endpoints, or the legacy endpoints are hand-written | W2 D-31: `unit` is the *stored* unit; the conversion lives nowhere generated (OQ-W2-4). W3 D-9 gives the `mm`→inch split to `mw-api-legacy` | holds (narrowed) — same resolution as 6. OQ-W2-4 is the named follow-up, **N-6** |
| 8 | W2 | Identifier allocation does not rename any of the 163 existing JSON keys | W2 D-3 separates key from column; D-28 preserves the JSON keys as `legacy_rest_keys` data | holds |
| 9 | W4 | W4 decides OQ-W1-2, and if metrics are NULL makes the circular finaliser NULL-safe while still emitting `0` | W4 D-8: `mw_circular_mean` returns `0`, not `NULL` — option (c) exactly | holds |
| 10 | W4 | `/span`'s four aggregate levels and their exact span boundaries are preserved | W4 D-1 | holds — and this assumption is what forced C-W0-16 |
| 11 | W4 | `period_rain` exists as an aggregate-level column with no raw counterpart | W4 D-22 | holds |
| 12 | W4 | `almanac_cache` and `snow_totals_cache` continue to exist as precomputed tables | W4 D-17, D-19 | holds |
| 13 | W7 | Whatever writes readings either writes every column or coordinates with W4 on assumption 9 | Coordinated across three documents: W3 D-26 writes NULL, W4 D-8 makes the finaliser safe, W7 assumption 27 carries NULL across a hop | holds |
| 14 | W6 | W6 carries OQ-W1-1, -3, -4 and -5 forward as decisions the owner still owes | §5: OQ-W1-1 as **O-5**; OQ-W1-3 and OQ-W1-5 together as **O-6** (one access-log query answers both). OQ-W1-4 was answered at the owner's request (W3 D-96) | holds (narrowed) |
| 15 | W6 | W6's ledger records OQ-W1-4's outcome as a deliberate deviation from D-37 | §3, ledger entry **DEV-3** | holds |
| 16 | iOS | `RemoteWeather.app` @ `77967ab` is the version that must keep working | No later document changes it; `legacy-v1.json` pins it | holds |

### 1.3 W2 (metric registry) — 28 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W1 | The legacy JSON key lists read out of the Go source match the contract W1 captures | W1 reports no missing key; the surface is `legacy-v1.json` plus 70 fixtures | holds (see note on counts below) |
| 2 | W1 | `ohum`/`outhumidity` and `dayrain`/`rainday` are genuinely duplicated and both must keep working | W1 D-12: both duplications preserved | holds |
| 3 | W1 | `windch`, `heatidx`, `aqi_pm25_aqin`, `aqi_pm10_aqin` are contractually the handler's computed value; W1 states which side | W1 never states the side. **W3 D-21/D-23 closes it another way**: a derived value that is a pure function of one reading is computed at ingest and stored, so both sides are numerically identical by construction | holds (narrowed) — the seam W2 marked is closed, by W3's R1 rather than by W1's statement |
| 4 | W1 | The `mm`→inch conversion with its 0.1-inch threshold is contract, not an implementation detail | W1 D-12 preserves the unit inconsistency as contract | holds — the conversion's *location* is **N-6** |
| 5 | W1 | The derived values that are not metrics are W1's and W3's and are absent from the generated schemas | W3 D-23 places every one of them (stored, pure-function or component-maintained) | holds |
| 6 | W4 | W4 decides `sum_count` versus `stats_agg`; both are expressible, the choice is one line in `[defaults]` | W4 D-2 | holds |
| 7 | W4 | W4 decides per-level inclusion within Decision 8's contiguity constraint, having read OQ-W2-1 | W4 D-3 takes OQ-W2-1 option (a) and says so | holds |
| 8 | W4 | W4 owns the rebuild procedure; `090_rebuild_aggregates.sql` is the shape, not the procedure | W4 D-23: `ci/db/rebuild-aggregates.sh` | holds |
| 9 | W4 | W4 owns aggregate compression, retention, refresh policies and every index beyond the time index; none is generated | W4 D-4, D-5, D-21, D-26 | holds — but W4 made two *generator* edits it flagged; see C-W2-9 |
| 10 | W4 | W4 accepts `observations` and `observations_1m/_1h/_1d` as generator constants | W4 assumption 11 renames nothing; `_5m` is added in the same convention | holds |
| 11 | W4 | W4 accepts a 7-day `chunk_time_interval` and 7-day raw compression policy | W4 assumption 12 accepts both as not-measured-optima | holds |
| 12 | W4 | W4 reads OQ-W2-2 and OQ-W2-6 | W4 D-23 implements OQ-W2-2(i); OQ-W4-2 answers OQ-W2-6 | holds |
| 13 | W3 | W3 restructures the minimal root workspace this item created, adopting `generated/rust/*.rs` | W3 D-2 leaves the root **unchanged** except `[workspace.dependencies]`, and D-5 adopts the generated files into `mw-types` via `#[path]` | holds (narrowed) — satisfied by not needing restructuring, which is the cheaper outcome |
| 14 | W3 | `reading.rs` uses `chrono::DateTime<Utc>`; if W3 wants `time` that is one generator line, but it must be W3's recorded choice | W3 D-7: chrono, recorded, with the reason (`chrono-tz` is needed anyway for `Calendar`) | holds |
| 15 | W3 | W3's query layer reads `generated/rust/aggregates.rs` rather than re-deriving level membership | W3 assumption 13 | holds |
| 16 | W3 | W3 knows a mean needs `mw_mean(...)` and a direction needs `mw_circular_mean(...)` | W3 assumption 13 | holds |
| 17 | W5 | W5 §0 owns the user-facing vocabulary table and is authoritative over `label` | W5 D-2; W5 assumption 34 overrides via a copy-module map, never by editing generated files | holds |
| 18 | W5 | W5 renders from `generated/ts/metrics.ts` rather than maintaining its own list | W5 assumption 33 | holds |
| 19 | W5 | W5 reads OQ-W2-5 (should `unit` be a closed enumeration?) | W5 does not mention it. `grep -n 'OQ-W2-5' docs/management-ui-design.md` is empty | **FAILS** — see C-W2-19 |
| 20 | W8 | W8's gate treats exit 1 as drift and exit 2 as an invariant violation, **reporting them differently** | `xtask/src/gates.rs::run` collapses every non-zero status into one `Err`. Both fail the job; neither is distinguishable | **FAILS** — see C-W2-20 |
| 21 | W8 | W8's gate also runs `cargo test -p xtask` | `pr.yml`'s `test` job runs `cargo test --workspace`, which includes `xtask` | holds (narrowed) — by workspace scope rather than by a named target |
| 22 | W8 | W8 either added a root `Cargo.toml` or will find this one; whoever lands second merges rather than replaces, and the `xtask` alias must survive | The merge kept W8's root manifest and folded both module sets into one `xtask`; `.cargo/config.toml` still carries `xtask = "run --package xtask --quiet --"` | holds (verified in tree) |
| 23 | W8 | W8 asserts the minimum database version; the generated SQL depends on features verified at 2.31 | `ci/sql/assert-timescaledb-version.sql`; W4 assumption 33 keeps the Toolkit check a warning because D-2 declines `stats_agg` | holds |
| 24 | W9 | W9 reconciles the production column list against the registry and reports which metrics never had a column (OQ-6) | Same as W0 assumption 24: the mechanism is stronger, the report needs production | holds (narrowed) — **O-4** |
| 25 | W9 | W9 does not need the registry trimmed first; the superset is deliberate | W9 D-8 | holds |
| 26 | W7 | W7 confirms `solarjoules` against `pkg/solar` (OQ-W2-3) | W7 does not mention `solarjoules` or `pkg/solar` anywhere | **FAILS** — see C-W2-26 |
| 27 | W2a/W7 | Neither needs a registry change to add a metric a new driver reports | W2a and W7 both treat adding a metric as a registry entry plus two commands | holds |
| 28 | W6 | W6 reads A-W2-1 as a contradiction between D-11 and OQ-5 that must be resolved before W4 ships | Resolved by W4 D-3 before W4 shipped; W4 assumption 38 records it | holds |

**A note on key counts.** W1 assumption 8 says "163 existing JSON keys"; W3 D-9 says
`mw-api-legacy` owns "the 133-key DTO". These are different scopes — 133 is `/latest`'s DTO, 163 is
the whole surface — and nothing depends on either figure, so this is recorded rather than resolved.
An implementation agent should take the key set from `contract/legacy-v1.json`, not from a count in
prose.

**C-W2-9 — W4 edited W2's generator twice, and W2 is finished.**

W4 D-10 and D-9 required two changes inside `xtask`: `functions_sql`'s zero-returning
`mw_circular_mean` branch, and the `::bigint` cast on the count roll-up. W4 made both, flagged both,
and recorded (assumption 8) that "W2 will keep them". W2 cannot keep anything; it is merged.

**Resolution: the edits stand and the ownership transfers.** Both implement behaviours
`DESIGN.md` assigns to W4 (the finaliser's contract behaviour, the stored count's type), both are
commented in place with the reason, and both are covered by `cargo test -p xtask` and
`codegen-drift`. Recorded here so that a future reader does not find W4's behaviour inside W2's
artefact and treat it as drift. No decision changes. Clerical.

**C-W2-19 — unit strings are unvalidated and nobody owns validating them.**

OQ-W2-5 observed that 25 unit strings are in use with nothing validating them, so `degf` for `degF`
reaches the UI as a rendered label, and recommended a `[[unit]]` table as "W5-adjacent". W5 never
read it: the recommendation was addressed to a document that had no reason to grep for it.

**Resolution: raised as work item N-7**, a `[[unit]]` table in the registry validated by the
generator. It is ten lines of generator and it is on the path of W5's copy-module work, so the plan
places it with the registry work rather than with the UI. Small, decided, needs a work item rather
than a decision.

**C-W2-20 — the drift gate cannot distinguish drift from an invariant violation.**

W2 D-18 made `codegen --check` exit 1 on drift and 2 on an invariant violation precisely so the two
could be reported differently: a drift means "regenerate and commit", an invariant violation means
"your registry change is not allowed". `gates::run` maps any failure to one error string, so the two
read identically in CI.

**Resolution: raised as work item N-8**, about ten lines in `xtask/src/gates.rs` to carry the child
status and let a gate declare a per-code message. The gate *fails* correctly in both cases today, so
this is a diagnostics defect rather than a correctness one, which is why it is a work item and not a
blocker.

**C-W2-26 — `solarjoules` may be a total rather than a mean, and the question was never asked.**

OQ-W2-3 could not determine whether `solarjoules` accumulates per reading (so a bucket should
`sum` it) or is an instantaneous rate scaled to a period (so a bucket should average it), because the
answer is in `pkg/solar`, which was W7's territory. W7's document never mentions it. The registry
currently declares `mean`/`min`/`max`, matching the Go views.

**Resolution: raised as work item N-9**, one reading of `remoteweather:pkg/solar` at `c3ec9f7` and, if
it is a total, one line in `metrics/registry.toml`. The cost of being wrong is a wrong number in a
chart, not a schema change, and `solarjoules` is not on the frozen REST surface — so this is a
follow-up rather than a gate on anything.

### 1.4 W2a (component model) — 30 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W3 | W3 owns `ReadingStream`'s internals and implements "a source is never blocked by a sink" | W3 D-17: the only thing `admit` can wait on is the local spool's fsync | holds |
| 2 | W3 | W3 implements `Durable` with a local durable spool at the fan-out point, not a channel | W3 D-13: the spool **is** the fan-out point | holds |
| 3 | W3 | W3 accepts that a `Service` owns time inside its loop and that this is not a D-25 violation | W3 assumption 5; `Service` is kept out of the timer heap on that basis | holds |
| 4 | W3 | W3 owns `LatestSnapshot` and honours "the `/latest` handler performs no queries" | W3 D-34: four things, none of them a query, and no handle that could | holds |
| 5 | W3 | W3 mounts every route provider on **one listener**, including both gRPC services | W3 D-92: the gRPC service keeps its own socket | **FAILS** — same failure as C-W0-7; resolved by A-W3-4 |
| 6 | W3 | W3 owns where the supervisor's durable state lives (OQ-D) | W3 D-41: the SQLite pool is a root resource owned by `main`, handing a one-table `ScheduleStore` to the supervisor | holds — answered more cleanly than OQ-D proposed |
| 7 | W3 | The pure APRS encoder lives in a crate with no database or transport dependency | W3 §1.2: `mw-aprs`, forbidden `tokio`, `sqlx`, any transport | holds |
| 8 | W3 | W3 treats the Go emulators as fixed external test infrastructure (D-40) | W3 D-82, assumption 8 | holds (narrowed) — **for the drivers the reference tree has an emulator for**. It has none for WeatherLink Live, which is the first driver the plan builds, so M3-1 arms `driver-emulators` against an emulator in **this** repository. See **O-13**. |
| 9 | W3 | W3 defines `ComponentError` as typed and cause-chain-preserving (D-34) | W3 D-49, D-51 | holds |
| 10 | W2 | The registry's per-metric `agg` is the same field that generates aggregate SQL and resolves an `OutputEncoder`'s `(Metric, Window, Agg)` | W2 D-11: the strategy vocabulary is `mean, min, max, total, circular_mean`, and there is **no `last`** — "it is not a composable state" | **FAILS** — see C-W2a-10 |
| 11 | W2 | `Metric` is a registry-generated type an encoder can name in a const table, and `{Last, Mean, CircularMean, Max, Min, Sum}` is a subset of what the registry expresses | `generated/rust/metrics.rs` gives the type; the `Agg` set is **not** a subset | holds on the type, **FAILS** on the subset claim — folded into C-W2a-10 |
| 12 | W2 | The 1-minute level stores composable state, so a ten-minute mean is arithmetic over ten buckets | W4 D-2: `sum` and `count` everywhere | holds |
| 13 | W2 | 1-minute membership must include every metric an `OutputEncoder` declares with a sub-hour window | W4 D-3: every level carries every aggregatable metric | holds (superset) |
| 14 | W4 | The `WindowedView` ring is refreshed from the 1-minute aggregate within `window:rollup`'s 10-second deadline | W4 D-26: phase-locked 3 s past the boundary; measured lag 3.06–3.22 s | holds |
| 15 | W4 | W4 may absorb `aeris:alerts:gc` as a retention policy (OQ-C) | W4 never mentions alerts or a retention policy for them | **FAILS** — see C-W2a-15 |
| 16 | W4 | Rainfall boundaries are computed in the **station's** zone, not the server's | W4 assumption 17: no job in the schema calls `date_trunc('day', NOW())`; W3 D-24 | holds |
| 17 | W5 | W5 §0 owns the translation table and no term from this document reaches a screen | W5 D-1 (a CI lint, not review), D-2 | holds |
| 18 | W5 | W5 renders `Waiting{on}` as an actionable sentence naming the provider, and `Running{degraded}` as "running, with X unavailable" | W5 D-4, assumption 1 | holds |
| 19 | W5 | W5 consumes `ComponentControl` for state, start/stop and run-now | W5 D-22 offers run-now and restart and deliberately **no general start/stop** — "turning something off is a configuration change" | holds (narrowed) — W5 declines a capability W2a offers; the capability is unchanged |
| 20 | W5 | W5 surfaces the per-component counters this document defines | W5 D-4: every one of W2a's nine states, six triggers and ten counters has a specified rendering | holds |
| 21 | W5 | W5 displays `Freshness` as distinct from component state | W5 D-5, assumption 7 | holds |
| 22 | W7 | W7 owns provenance and `ReadingFilter::NotFrom(origin)` semantics | W7 D-5, D-6; assumption 2 keeps `has_visited` unchanged | holds |
| 23 | W7 | W7's acked resumable spool is the mechanism `writer:timescale` uses, not a collector-only facility | W7 D-10 – D-18; assumption 7: "nothing in §3 is collector-specific" | holds |
| 24 | W7 | Remote-station registration is `FromRegistration` against the general `StationRegistry`, with mTLS | W7 D-32, D-34; assumptions 4, 5 | holds |
| 25 | W7 | The uplink is `Trigger::OnReading { NotFrom(self), Durable }` with no bespoke lifecycle | W7 D-3: `NotFrom(self)` matches **every** envelope in the spool, so the uplink would send nothing. Corrected to `NotFrom(peer)` | **FAILS** — see C-W2a-25 |
| 26 | W1 | W1's fixtures pin what `/latest` returns when the snapshot is empty | `contract/fixtures/latest-no-recent-data.json` exists; W3 §2a.6 matches its bytes | holds (verified in tree) |
| 27 | W1 | W1's fixtures pin sunrise/sunset to the minute **and the runner compares them against a fresh evaluation** | The fixtures record `"sunrise":"7:30 AM"`. The `assert` block is `status, headers, json_shape, json_key_order, numeric_format, numeric_convention, trailing_newline, body_equals, required_substrings` — no fresh-evaluation comparison exists | **FAILS** — see C-W2a-27 |
| 28 | W1 | W1 states whether a `503` on an unmounted route can be observed on a frozen path | W1 does not state it; W1 and W2a ran in the same stage | **FAILS** — see C-W2a-28 |
| 29 | W6 | W6 reconciles the D-30 amendment against whatever W3 assumed about sinks | W3's A-W3-2 proposes the same split from the sink's side; the two are compatible and are applied together as D-30a/D-30b | holds |
| 30 | W6 | W6 records the `sun_times` deletion against the README capability inventory, which now has no owner | `docs/rewrite/README.md`'s capability inventory still assigned *Sun-times precompute* to W2a §4. Corrected in this PR — the row is struck through and annotated with the reason and with N-10's gating | holds |

**C-W2a-10 — `Agg::Last` has no registry counterpart, and W2 forbids one.**

W2a Decision 48 has an `OutputEncoder` declare `&[(Metric, Window, Agg)]`, Decision 49 resolves it by
rolling up 1-minute buckets using the registry's declared strategy, and Decision 11's assumption
claims the `Agg` set used — `Last, Mean, CircularMean, Max, Min, Sum` — is a subset of what the
registry expresses. It is not. W2 D-11 fixes the vocabulary at `mean, min, max, total,
circular_mean` and explicitly excludes `last`, because a last value is not a composable aggregate
state. An encoder declaring `(Metric, Window, Last)` therefore has nothing in the registry to resolve
against.

**Resolution, on the merits and clerical:** `Last` is not a window aggregate and should never have
been in that list. The instantaneous value of a metric is what `LatestSnapshot` holds (W3 D-31 – D-34);
`WindowedView` exists for the windowed values, which are exactly W2's five strategies. So:

- `WindowedView` resolves `Mean`, `Min`, `Max`, `Sum` and `CircularMean`, mapping `Sum` onto the
  registry's `total` (the same strategy under two names — also worth normalising).
- An encoder needing the current value takes it from the snapshot, not from a window.

This changes no decision in either document and removes a dangling variant. Recorded as **D-50** in
`DESIGN.md` so an implementation agent does not go looking for a registry strategy D-11 forbids.
Carried into the plan as part of the rate-limited-output work item.

**C-W2a-15 — `aeris:alerts:gc` has no owner, and both candidates are correct.**

OQ-C offered two homes: W4 absorbs it as a retention policy if the alerts table becomes a hypertable,
or it stays the `Calendar` component W2a specified. W4 never addressed alerts at all.

**Resolution: it stays the `Calendar` component W2a specified.** This needs no decision — OQ-C says
either is correct, and W4 declining to make the alerts table a hypertable settles which arm is
operative by elimination. Recorded so it is not lost in the move, which is the only thing OQ-C was
actually protecting against. Carried into the plan with the Aeris components.

**C-W2a-25 — `NotFrom(self)` matches everything, so the uplink would send nothing.**

W2a §4.1 and W3 assumption 44 both specify the uplink's filter as `NotFrom(self)`. W7 D-3 shows why
that is empty: `admit` sets `path = [self]`, so *every* envelope in a node's own spool has `self` on
its path. The filter that expresses "do not send a peer what it just sent me" is `NotFrom(peer)`.

**Resolution: W7 D-3 is correct and is applied to both documents.** This is a one-identifier fix with
a measured reason, affecting no decision's substance — but it is exactly the kind of defect that
ships as "working software with no readings arriving", so it is recorded as **D-47** rather than left
in W7's prose. W7 asked for it to be "recorded against both documents because W6 will grep for both";
it is.

**C-W2a-27 — the check that justifies deleting `sun_times` does not exist.**

W2a Decision 36 deletes the sun-times table and component in favour of a pure Meeus evaluation, and
OQ-B makes that safe by requiring W1's conformance runner to compare the served sunrise and sunset
against a fresh evaluation. The fixtures do carry the values. The runner never compares values —
by design, per W1 D-3 ("conformance means status, headers, key set, value types, numeric formatting
… not byte equality") and D-4 (formatting is asserted as a rule, not as a recorded literal).

So the decision is sound and its verification is absent. The risk is small and real: the stored Go
table is day-of-year keyed and therefore slightly wrong in non-leap years, so the two **should**
disagree by a minute somewhere, and nothing would report it.

**Resolution: raised as work item N-10** — one fixture-level assertion comparing `/latest`'s
`sunrise`/`sunset` strings against a fresh Meeus evaluation for the fixture's station and date,
exempt from D-3's value-blindness because this is the one field where the value is the thing under
test. It is a precondition on deleting the table, so the plan sequences it before the snapshot work
rather than after.

**C-W2a-28 — whether a `503` can appear on a frozen path was never answered.**

W2a Decision 19 makes `HttpRoutes` additive and late-bindable, so an unmounted declared path returns
`503` naming what it waits on. Both W2a (assumption 28) and W3 (assumption 30) hand the question "can
this ever be observed on a frozen route?" to W1, which ran in the same stage as W2a and could not
have answered it.

**Resolution: it cannot, and the reason is checkable.** W3 §6.2's argument is that `api:legacy`
declares only `LatestSnapshot` as a hard requirement, and `LatestSnapshot` is provided by
`snapshot:latest`, which W3 D-37 makes start successfully even with `ReadingStore` unavailable
(it finishes empty rather than refusing). So the legacy route table mounts unconditionally and a
frozen path is never unmounted. **This is an invariant, not an argument**, so the plan makes it a
test: with the database down, every frozen route must return its contract response (including
`/latest`'s `404 no recent weather data available for this station\n`) and never `503`. Recorded as
**D-48**. Resolvable inline; no owner decision needed.

### 1.5 W3 (service architecture) — 62 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W2a | W2a's model is implemented as written; this document adds mechanics, not variants | True apart from assumptions 2 and 3 | holds |
| 2 | W2a | `Provision` gains `ProvisionDetail::Routes(RouteManifest)`. **Taken, not asked** | W2a is merged and never answered. The stated fallback is detection moving into `http:listener::build` | **FAILS (unanswerable)** — see C-W3-2 |
| 3 | W2a | `ComponentControl` also carries runtime log-level control. **Taken, not asked** | Same; stated fallback is a thirteenth capability | **FAILS (unanswerable)** — see C-W3-2 |
| 4 | W2a | `Durable` is a local durable spool, and W7's spool is that mechanism | W2a D-62; W7 assumption 7 | holds |
| 5 | W2a | A `Service` owns time inside its loop | W2a D-7 | holds |
| 6 | W2a | The supervisor's `Calendar` state lives in the config SQLite database, not through `ConfigStore` | W2a OQ-D's recommendation; W3 D-41 goes one step further and puts the pool in `main` | holds |
| 7 | W2a | The pure APRS encoder is in a crate a golden test can link alone | W2a assumption 7; `mw-aprs` | holds |
| 8 | W2a | The Go emulators are fixed external test infrastructure | D-40; W8 D-23 pins the commit | holds |
| 9 | W2a | `ComponentError` is typed and preserves its cause chain | D-34; W2a D-26 carries it on `Failed` | holds |
| 10 | W2a | `OnReading` components do not declare `Need { ReadingStream }`; the trigger implies the subscription | W2a D-64: components never hold a channel receiver; the supervisor delivers | holds |
| 11 | W2a | `Latest` is a coalescing `watch` and `Lossy` a counted `broadcast` | W2a D-63 | holds |
| 12 | W2 | `generated/rust/{metrics,reading,aggregates}.rs` are adopted unchanged via `#[path]`, with chrono | W2 assumption 14; verified: `generated/rust/reading.rs` carries **214** `Option`-typed fields — the 212 metrics plus the two nullable attribute dimensions — and chrono timestamps | holds (verified in tree) |
| 13 | W2 | The query layer reads `aggregates.rs` and knows means need `mw_mean`, directions `mw_circular_mean` | W2 assumptions 15, 16 | holds |
| 14 | W2 | **The generator emits proto3 `optional` on metric fields** | `grep -c optional generated/proto/reading.proto` → **0** | **FAILS** — see **G-3** |
| 15 | W2 | The registry's per-metric `agg` resolves an `OutputEncoder`'s declaration, so `window:rollup` needs no second table | True for five of six variants; see C-W2a-10 | holds (narrowed) |
| 16 | W2 | `legacy_rest_keys` / `_read_time_computed` / `_aggregate_rest_keys` are sufficient to generate the frozen DTO's key set, and where they are not `mw-api-legacy` hand-writes the difference and says so | W2 D-28 – D-30 supply the data; `omitempty` and the unit split are not expressible, so the hand-written arm applies | holds (narrowed) — W1 assumptions 6 and 7 reach the same place |
| 17 | W2 | W2 restructures nothing when these crates land; `xtask` stays a member and the alias survives | Verified in tree | holds |
| 18 | W4 | W4 takes OQ-W1-2 option (c) | W4 D-8 | holds |
| 19 | W4 | `/span`'s four aggregate levels and span boundaries are preserved | W4 D-1 | holds |
| 20 | W4 | `almanac_cache` and `snow_totals_cache` continue to exist | W4 D-17, D-19 | holds |
| 21 | W4 | The 1-minute refresh policy makes a bucket readable within `window:rollup`'s 10-second deadline | W4 D-26 | holds |
| 22 | W4 | The raw table carries `ON CONFLICT (station_id, time) DO UPDATE` or an equivalent unique constraint | W4 D-11 | holds |
| 23 | W4 | Daily/monthly/yearly boundaries are in the station's zone and no database job contradicts it | W4 assumption 17 | holds |
| 24 | W4 | Aggregate rebuild duration is observable | W4 assumption 25: `timescaledb_information.job_stats` | holds |
| 25 | W1 | Conformance asserts a per-fixture **allowlist** of header names and never enumerates headers | Verified at `contract/conformance/src/lib.rs:320`, and `latest-default-station.json` carries a four-entry `assert.headers` array | holds (verified in tree) |
| 26 | W1 | The recorded HTML bodies are usable as **byte goldens** for the template port | The fixtures' `assert` block includes `body_equals`; the full rendered bodies are recorded | holds (verified in tree) |
| 27 | W1 | The config endpoint carries exactly W1 §9's nine consumed values | W1 assumption 2 agrees; W3 D-80 lists them | holds |
| 28 | W1 | `/latest`'s recency window and `/snow`'s absence of one survive the snapshot scheme | W1 assumption 3 | holds |
| 29 | W1 | Multi-hostname routing preserves the fallback, not just the matching | W1 assumption 5; W3 D-66 | holds |
| 30 | W1 | The `503` on an unmounted route cannot be observed on a frozen path; W1 owns the surface | W1 never states it | **FAILS** — same as C-W2a-28; resolved there as D-48 |
| 31 | W1 | W1's fixtures pin `/latest` on an empty snapshot | `latest-no-recent-data.json` | holds (verified in tree) |
| 32 | W8 | The conformance gate's `arms_when` is corrected to point at `contract/` rather than `tests/conformance/` | It is not; `ci/gates.toml` still names `tests/conformance/manifest.toml` | **FAILS** — see **G-1** |
| 33 | W1 | W1's fixtures pin sunrise/sunset so W2a D-36 is verifiable, with the runner comparing against a fresh evaluation | No value comparison exists | **FAILS** — same as C-W2a-27 |
| 34 | W5 | W5 §0 owns the vocabulary table | W5 D-1, D-2 | holds |
| 35 | W5 | W5 consumes **one** log stream with a `stream` field and renders `gap` as "N entries dropped" | W5 D-40, D-43; assumption 17 | holds |
| 36 | W5 | W5 renders log records from structure, applying the §5.8 palette, with `?format=ansi` as a secondary view | W5 assumption 16; D-? §5.8 | holds |
| 37 | W5 | W5 surfaces this document's additional counters | W5 assumption 32 names `spool_depth_bytes`, `spool_lag_envelopes` and a readable configured capacity | holds |
| 38 | W5 | W5's log-level control uses `set_level(target, level, ttl)` and shows a TTL-less override as standing | W5 D-45; assumption 23 | holds |
| 39 | W5 | W5 displays `Staleness::Degraded` and `Freshness` as distinct from component state | W5 D-5 | holds |
| 40 | W5 | W5's generated OpenAPI client covers the management API, and `/metrics`, `/api/logs` and `/config.json` are part of that surface | **W5 assumption 21 rejects this**: `/config.json` is `mw-web`'s, and `/metrics` is Prometheus text and not in any OpenAPI schema | **FAILS** — see C-W3-40 |
| 41 | W7 | W7 owns `Provenance`'s wire encoding and `OriginId` allocation | W7 D-5 | holds |
| 42 | W7 | `NotFrom(origin)` is `!prov.has_visited(origin)` — a set test, not a name comparison | W7 assumption 2 | holds |
| 43 | W7 | W7's spool is the mechanism §2 specifies; §2.4's sizes are this document's defaults over W7's format | W7 assumption 15 adopts all of them unchanged | holds |
| 44 | W7 | The uplink is `OnReading { NotFrom(self), Durable, 10 s }` wrapped in `SpooledSink` | W7 D-3: `NotFrom(peer)` | **FAILS** — same as C-W2a-25, resolved there as D-47 |
| 45 | W7 | W7 must carry proto3 field presence on the wire | W7 assumption 23 agrees it must; nobody emitted it | **FAILS** — same as G-3 |
| 46 | W7 | A satellite's endpoint does not change at upgrade | W7 assumption 18 | holds |
| 47 | W7 | `station:grpcrx` authenticates with mTLS before any frame is decoded | W7 D-34; renamed `ingest:grpc` (W7 D-2) | holds (renamed) |
| 48 | W8 | `crates/modernweather` produces a binary named `modernweather` with the packaging metadata blocks | `crates/` holds only `.gitkeep`; `ci/release.toml` is armed by `crate_manifest` existing | forward |
| 49 | W8 | The config flag is `--config <path>` | W8 assumption 2; W3 §3.1 | holds |
| 50 | W8 | Readiness notification is implemented, so `Type=notify` stands | W3 D-47 with `sd-notify` | holds |
| 51 | W8 | W3's concurrency tests run under both ThreadSanitizer and `loom` | W8 assumption 5; W3 D-85 specifies two tests, one `pause()`-based | holds |
| 52 | W8 | The TLS crypto provider is `ring` | W8 OQ-1's recommendation, now A-W3-1's table row | holds |
| 53 | W8 | No replacement allocator is adopted | W8 OQ-2's recommendation | holds |
| 54 | W8 | `protox` means no `protoc` on any build host | W8 assumption on toolchain; `platform.yml` needs no new install step | forward (verifiable only when `mw-proto` exists) |
| 55 | W8 | Three gates need their `arms_when` or `cmd` updated as these artefacts land: `layout` (new), `conformance` (path), `db-tests` (new) | None of the three is done; `layout` and `db-tests` are not declared in `ci/gates.toml` at all | **FAILS** — see **G-4** |
| 56 | W8 | `[workspace.dependencies]` is populated by these crates, and pinning there keeps the drift check and the replay comparing like with like | Root manifest's `[workspace.dependencies]` is present and empty, as W8 left it | holds (verified in tree) |
| 57 | W6 | W6 reads the four closing sections and treats the prose as background | §1's method | holds |
| 58 | W6 | W6 carries the three deliberate D-37 deviations forward as a ledger | §3: **DEV-1**, **DEV-2**, **DEV-3** | holds |
| 59 | W6 | W6 records the two corrected measurements against `docs/rewrite/README.md`'s "Measured facts" table, which states 7 for both | `README.md:201` and `:207` both say 7. Corrected in this PR | holds |
| 60 | W6 | W6 records the v2 API as a named future work item, with this document's additive routes as its starting inventory | §4, item **N-1** | holds |
| 61 | W6 | W6 reconciles Decision 89 (proto3 field presence) across W2, W4 and W7 | §2, **G-3** | holds |
| 62 | W6 | W6 should expect no open questions from W3 | W3's Open questions section records ten decided calls with reversal triggers; none is deferred | holds |

**C-W3-2 — three extensions to W2a's model, none of which W2a could answer.**

W3 took two extensions to a merged document and said so (assumptions 2 and 3). W5 then asked for a
third (its assumption 4: `ComponentControl` must expose `applied_generation` per component and the
newest committed `generation`, without which W5 §2.2 is unimplementable). So `ComponentControl` has
accumulated two post-W2a widenings and `Provision` one, from three documents, none reviewed by the
document that owns the model.

Each is small, each has a stated fallback, and **none conflicts with anything W2a decided** — they
are additions to an enumeration, not changes to a rule. The failure is procedural, not technical: a
merged document cannot consent, so "taken, not asked" has no reviewer.

**Resolution: all three are accepted and promoted to numbered decisions** (D-51, D-52, D-53), so that
the component model an implementation agent builds is the one in the ledger rather than W2a's
document plus three appendices. The reasoning for each is the one its author gave:

- `ProvisionDetail::Routes(RouteManifest)` — the alternative is a duplicate route going undetected,
  or failing the one component everything's reachability depends on *after* its siblings started.
- `ComponentControl` carries the `tracing` reload handle — one consumer, same root provider; a
  thirteenth capability for one consumer is noise in a graph whose only jobs are ordering and
  `Waiting`.
- `ComponentControl` exposes `applied_generation` — it is supervisor state by construction (the
  supervisor is what applies a plan diff), so computing it anywhere else means a second source of
  truth for "is this change live".

This is the one place where the "stay in your lane" instruction produced a real cost, and it is worth
naming: **the lane rule has no mechanism for an upstream document to answer a downstream request.**
Recorded as a finding; §5's **O-12** asks whether a future programme of this shape should keep a
nominated owner per document for exactly this.

**C-W3-40 — three additive routes were assigned to the wrong schema, and W5 is right.**

W3 assumption 40 folds `/metrics`, `/api/logs` and `/config.json` into the management API's generated
OpenAPI surface. W5 assumption 21 rejects two of the three on inspection: `/config.json` is the
*public website's* configuration payload, served by `mw-web` (W3's own D-79 says so), and `/metrics`
is Prometheus text exposition, which is not JSON and cannot be in an OpenAPI document — the UI reads
the JSON mirror instead.

**Resolution: W5's narrowing stands.** The management OpenAPI schema covers the management API's JSON
routes and nothing else. `/config.json` belongs to `mw-web`'s surface and to the v2 inventory;
`/metrics` is a Prometheus endpoint with no schema. Clerical; W3's own decisions already imply it.

**A second, smaller divergence on the same surface.** W5 assumption 20 flags that W3 §5.8 names the
terminal log view `GET /api/logs?format=ansi` while W5 versions the whole management API, making it
`GET /api/v1/logs?format=ansi`, and asks W6 to pick one. **Picked: `/api/v1/...`.** W5 owns the
management API surface and versioning was an explicit instruction in its prompt; W3's spelling is one
path string in prose. Every management route is `/api/v1/*`. Recorded as **D-49**.

### 1.6 W4 (database schema) — 39 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W1 | The four `/span` bands and the captured bucket spacing (60,000 / 300,000 / 86,400,000 ms) are the shipped server's output | W1 D-1, D-14; the fixtures carry it | holds (verified in tree) |
| 2 | W1 | `/span` takes no time anchor — only `station`, an ignored `station_id` and an unhonoured `format` | W1 §2.1; **W4 D-5 fails without this** | holds |
| 3 | W1 | `AlmanacData`'s seventeen fields are all `omitempty` pointers and an empty cache returns `{}` | W1 fixture `almanac-empty` | holds (verified in tree) |
| 4 | W1 | `/snow` has no recency filter and that is contract, not defect | W1 D-16; fixture `snow-device-stale` | holds |
| 5 | W1 | `/almanac`'s `value`/`wind_dir` and all seven `/snow` numbers are Go `float32` on the wire | W1 §6 | holds |
| 6 | W1 | OQ-W1-2 option (c) is still W1's preferred answer | W1's own recommendation is (c) | holds |
| 7 | W2 | A fourth `[[level]]` with `source = "1m"` is the extension W2 §1.3 said it was, and a `levels` change is `RebuildAggregates` rather than `BREAKING` | W2 §1.3's own words; the migration went through `cargo xtask migrate` with no flag | holds (verified in tree) |
| 8 | W2 | W2 will keep W4's two generator edits | W2 is merged and cannot | **FAILS (unanswerable)** — see C-W2-9; the edits stand and ownership transfers |
| 9 | W2 | Emitting the finalisers in every migration is additive and idempotent | No conflict with any W2 decision | holds |
| 10 | W2 | `aggregates.rs` is what W3's query layer reads for column types | W2 assumption 15; W3 assumption 13 | holds |
| 11 | W2 | The table names are generator constants and nothing here renames one | W2 assumption 10 | holds |
| 12 | W2 | The 7-day chunk interval and compression policy are accepted as-is | W2 assumption 11 | holds |
| 13 | W2 | OQ-W2-1 option (a) is taken, so no per-metric source level is needed | W4 D-3 | holds |
| 14 | W2 | A-W2-2 is accepted and acted on: D-23 and §6.3b are recommendation (i) implemented; (ii) is the owner's | (ii) is the correction to D-9's claim, **applied here as A-W2-2** rather than carried to the owner | holds |
| 15 | W2a | `window:rollup`'s 10-second deadline is real and 3.14 s of lag is comfortably inside it | W2a assumption 14 | holds |
| 16 | W2a | `window:rollup` never needs a 1-minute bucket older than an hour, so 7-day retention is invisible to it | W2a §4b.3 | holds |
| 17 | W2a | Boundaries are computed in the station's zone, from `stations.timezone` | W2a D-11, assumption 16 | holds |
| 18 | W3 | **`/span` appends one bucket aggregated from raw over `[last_closed_bucket_end, now)`** | W3 ran before W4 and has no such step; W3 D-40 gives `/span` a 60-second cache called "lossless" | **FAILS** — see **C-W3/W4-1**, the one contradiction no document flagged |
| 19 | W3 | `Ingress::admit` resolves the station before a reading reaches the writer | W3 D-27 | holds |
| 20 | W3 | The ingest path maintains `stations` and `station_metrics` as projections — "a small piece of writer behaviour §2.5 does not currently name" | W3 does not name it | **FAILS (unstated)** — see C-W4-20 |
| 21 | W3 | The writer stores NULL for an unreported metric | W3 D-26 | holds |
| 22 | W3 | The cancel-safe replay uses `ON CONFLICT … DO UPDATE` against W4's unique index, landing in the uncompressed window | W3 assumption 22 | holds |
| 23 | W3 | `snow:*` writes `snow_totals_cache` in inches, not the millimetres `snowdepth` carries on `/latest` | W3 D-38 owns the `snow:*` components; the conversion is W3's | holds |
| 24 | W3 | `snapshot:latest` reads `stations` for display metadata rather than off a reading | W3 D-31; W4 assumption 24 | holds |
| 25 | W3 | Aggregate refresh duration is observable for `mw_aggregate_refresh_seconds` | W3 assumption 24 | holds |
| 26 | W3 | `ReadingStore` queries the levels at the four band boundaries, and the per-level indexes are built for that predicate | W3 D-40 | holds |
| 27 | W5 | Any chart W5 adds stays inside the four `/span` bands or reads `observations_1d` | W5 D-51: no chart library; W5 adds no charting | holds |
| 28 | W7 | The collector's writer uses the same `ON CONFLICT` path and the same NULL rule | W7 D-28, assumptions 25, 27 | holds |
| 29 | W7 | No driver reports a field with no `[[metric]]` entry, so `extra` is unused at first boot | W7 confirms nothing reports into it | holds |
| 30 | W8 | `db-migrate-twice`'s manifest entry is unchanged — same id, `arms_when` and `cmd` | Verified in `ci/gates.toml` | holds (verified in tree) |
| 31 | W8 | `pr.yml`'s `database` job keeps `/usr/lib/postgresql/17/bin` on `PATH` | Unchanged | holds (verified in tree) |
| 32 | W8 | The gate's `psql` can create and drop databases and `CREATE EXTENSION timescaledb` | It already does | holds |
| 33 | W8 | `assert-timescaledb-version.sql` needs no change because D-2 declines `stats_agg` | Unchanged | holds |
| 34 | W9 | Create the unique index as part of the raw import, not after it | W9 assumption 1: the index is created by migration `20261029234500` on an empty table, discharging this completely | holds |
| 35 | W9 | W9 decides what to do with ~294 days of rollup with no raw behind it | W9 D-5, D-7: daily and hourly legacy plain tables; the minute and 5-minute levels would be dropped by retention | holds |
| 36 | W9 | If production is PostgreSQL 14, §5.2's table lists the decisions that move, and a TimescaleDB below 2.9 makes the upgrade part of W9 | W9 OQ-W9-2 | holds (conditional on **O-1**) |
| 37 | W6 | All three amendments are proposals; this document carries on under D-11, D-17 and D-18 | Applied here as A-W4-1 – A-W4-3 | holds |
| 38 | W6 | A-W2-1 is resolved by W4 D-3 and needs no further action | Confirmed | holds |
| 39 | W6 | There is **no entry** for W6's deviation ledger from W4 | Confirmed: D-1 preserves the contract rather than deviating, and D-13 is closed by a read-path requirement | holds — see §3's note on W9's `period_rain` question, which is a different thing |

**C-W3/W4-1 — `/span`'s 60-second cache stops being lossless once the trailing bucket is appended.**
*(Found by this pass. Neither document flagged it; it requires holding one decision from each.)*

W3 D-40 gives `/span` a 60-second TTL keyed by `(station, span, snow-join)` and justifies it as
**lossless**, because the 1-minute aggregate advances once a minute — so a cached response can be at
most one bucket behind what a fresh query would return, and for 60 seconds it *is* what a fresh query
would return.

W4 D-13 then requires the `/span` read path to append one bucket aggregated **from raw** over
`[last_closed_bucket_end, now)`, because every `/span` fixture W1 captured ends with the in-progress
bucket. Omitting it shortens every response by one element and lags the "now" end of every chart.

Together: the response now contains a value that changes at the **raw cadence** — every two seconds
in production — inside a cache with a 60-second TTL. The cache is no longer lossless. The last point
of every chart on the public website would be stale by up to a minute, which is a visible behaviour
change against a frozen surface, and the conformance suite would not catch it because it asserts
shape and formatting rather than values.

Neither document is wrong. D-40 was written before D-13 existed; D-13 is required by the captured
fixtures. The conjunction is what fails.

**Resolution — minimal, preserving both decisions exactly: cache the closed-bucket prefix only, and
append the tail per request.** The prefix is what D-40's losslessness argument is true of; the tail is
measured at **0.44–6.89 ms** (W4 §6.1b), which is the cost of the thing D-40 exists to avoid paying
on 100% of requests, now paid on the one part of the response that genuinely changes. `/span` is
under 1% of traffic (W0 OQ-5's measured mix), so the absolute cost is negligible.

Two alternatives were considered and rejected: shortening the TTL to 2 seconds makes the cache
pointless, and accepting up to 60 seconds of staleness on the last bucket is a user-visible change to
a frozen surface for no benefit. This is a clerical reconciliation rather than a design choice, so it
is decided here: recorded as **D-54**, and the cache's key and contents are specified in the plan's
read-path work item.

It also settles **OQ-W4-6** by implication, which asked whether the tail query lives in `mw-store` or
`mw-api-legacy`: it must be in `mw-store`, because it is now part of what `ReadingStore::span`
returns and the cache sits above it. W4's own recommendation (a) was already this, on the separate
ground that "24 hours of 5-minute buckets" means the filling one too. Two independent reasons for the
same placement; recorded as **D-55**.

**C-W4-20 — the `stations` and `station_metrics` projections have no named writer.**

W4 D-14 and D-16 make `stations` and `station_metrics` read-side projections, and W4 assumption 20
says the ingest path maintains them — upsert on admit, `last_seen` on write — while noting this is
"a small piece of writer behaviour §2.5 does not currently name". W3's §2.5 indeed does not, and W9
only populates them at migration time (W9 D-19).

**Resolution: assign it.** `Ingress::admit` already resolves the station (W3 D-27), so the upsert is
on a path that has the data; the `last_seen` and `station_metrics` updates belong to
`writer:timescale`, which is the only component that knows a row was stored. Recorded as **D-56**:
`admit` upserts `stations` on identity resolution, and the writer maintains `stations.last_seen` and
`station_metrics`. Clerical — W4 identified the gap precisely and only lacked the authority to place
it.

### 1.7 W5 (management UI) — 45 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W2a | `Waiting { on }` carries the provider's id, resolvable to a user-facing name | W2a D-26 | holds |
| 2 | W2a | All nine lifecycle states are observable through `ComponentControl`, including their payloads | W2a D-26, D-28 | holds |
| 3 | W2a | `ComponentControl` exposes read state, start/stop, run-now, and the `tracing` reload handle | W2a D-? plus W3 D-90; W5 uses state, run-now, restart and `set_level` and declines start/stop | holds |
| 4 | W2a | **Extension requested:** `ComponentControl` also exposes `applied_generation` and the newest `generation` | W2a is merged and cannot answer | **FAILS (unanswerable)** — see C-W3-2; accepted as D-53 |
| 5 | W2a | The per-component counters are readable **per component**, not only as process-wide series | W2a D-20 defines them per component | holds |
| 6 | W2a | `last_run`, `last_outcome` and `next_run` are supervisor state, not derived by the UI | W2a §1.3; W3 D-44 makes `next_run` a heap peek | holds |
| 7 | W2a | `Freshness { capability, populated_by, expected_max_age }` is readable and distinct from lifecycle state | W2a D-20 | holds |
| 8 | W2a | `plan()` is pure and runnable against a *proposed* configuration with no side effects | W2a D-2, D-25 | holds |
| 9 | W2a | A plan diff reports which components will start, stop and restart, by id, before anything is applied | W2a D-25 | holds |
| 10 | W2a | `PlanError` variants carry enough structure to map onto a form field | W3 D-64's `RouteConflict { scope, pattern, a, b }` is the worked case | holds |
| 11 | W2a | An unchanged component is genuinely untouched, observable through an unchanged `applied_generation` | W2a D-25 | holds (given D-53) |
| 12 | W2a | The `/test/*` checks are `check:<name>` `OnDemand` components, including the APRS one | W2a D-38 | holds |
| 13 | W3 | `Staleness::Degraded { missing, since }` is readable for `snapshot:latest` | W3 D-36 | holds |
| 14 | W2a | Sink delivery policy — durable versus coalescing — is readable **per sink** | D-30/D-30b make it a per-sink property; W3 D-19 types it per sink kind | holds (narrowed) — readable from the sink's kind rather than from a queryable field |
| 15 | W3 | The management API is a `RouteTable` with `Scope::Management`, and late binding keeps the UI reachable during an outage | W3 D-19, D-65; **load-bearing for W5's whole design** | holds |
| 16 | W3 | Log records reach the UI as structure, never formatted text, with the named fields | W3 D-59 | holds |
| 17 | W3 | **One** log stream carries both rings with a `stream: app \| access` field | W3 D-57 | holds |
| 18 | W3 | The synthetic `gap` record appears in the merged stream on overflow | W3 D-58 | holds |
| 19 | W3 | Backfill on connect is the ring's contents followed by live records, through one code path | W3 §5.7 | holds |
| 20 | W3 | **Divergence, flagged.** W3 names `GET /api/logs?format=ansi`; this document versions it to `/api/v1/logs` | W3 §5.8 | **FAILS (flagged)** — picked: `/api/v1/*`, recorded as D-48 |
| 21 | W3 | **Divergence, flagged.** `/config.json` is `mw-web`'s and `/metrics` is Prometheus text; neither is in the management schema | W3 assumption 40 says otherwise | **FAILS (flagged)** — W5 is right; see C-W3-40 |
| 22 | W3 | `set_level(target, level, ttl)` is callable with a TTL and the expiry is enforced by the supervisor | W3 D-56 | holds |
| 23 | W3 | A TTL-less raise is permitted and readable as a standing override | W3 D-56 | holds |
| 24 | W3 | Error cause chains survive to the JSON layer as an array | W3 D-51 | holds |
| 25 | W3 | `disposition` is a closed vocabulary the UI colours and counts | W3 D-61 | holds |
| 26 | W3 | `mw::audit` events are never suppressed by the dedupe layer | W3 D-60 | holds |
| 27 | W3 | `rust-embed` is available to `mw-api-mgmt` as well as `mw-web` | W3 D-76, A-W3-1 | holds |
| 28 | W3 | `mw-api-mgmt` uses `ConfigAdmin` and `ComponentControl` and depends on neither `mw-store` nor `mw-pipeline` | W3 §1.2's forbidden list | holds (given D-52) |
| 29 | W3 | Adding `utoipa` to `mw-api-mgmt` is an addition to D-32's table in A-W3-1's spirit | A-W3-1 adds six rows; this is a seventh | holds — folded into the applied A-W3-1 |
| 30 | W3 | The management API keeps its own socket and port | W3 D-63 binds N sockets from configuration | holds |
| 31 | W3 | `admit_rejected_total{reason}` is attributable to a station | W3 D-62 forbids request-derived labels but station is an allowed label | holds |
| 32 | W3 | `spool_depth_bytes`, `spool_lag_envelopes` and the spool's **configured capacity** are readable | W3 D-18's 2 GiB ceiling is configuration | holds |
| 33 | W2 | `generated/ts/metrics.ts` is the UI's source for labels and units | W2 D-17, assumption 18 | holds (verified in tree) |
| 34 | W2 | §0's vocabulary table overrides a generated `label` via a copy-module map, never by editing the file | W2 D-32 defers to W5 §0; R-2 forbids editing generated files | holds |
| 35 | W2 | `generated/openapi/reading.json` is `$ref`-able from a second OpenAPI document | It is a standalone schema document | holds (verified in tree) |
| 36 | W2 | `cargo xtask codegen` can gain `openapi/management.json` and `ts/mgmt-api.d.ts` targets, covered by the existing `codegen-drift` gate | **W9 D-20 argues the opposite**: `codegen --check` must keep meaning exactly "generated/ matches `metrics/registry.toml`", which is why W9 built a separate gate | **FAILS** — see C-W5-36 |
| 37 | W1 | The frozen contract is the public surface only; the management API is additive and unconstrained | D-37; W1 scopes to the public surface | holds |
| 38 | W1 | The iOS app does not use the management API | W1's crosswalk covers only public endpoints | holds |
| 39 | W4 | `admin_credential` and `admin_session` are **SQLite** config-database tables created by a migration | No SQLite migration mechanism exists; `migrations/` is PostgreSQL only and W4 scopes to Timescale | **FAILS** — see **G-2** |
| 40 | W4 | OQ-W5-4 (normalising per-station service settings off the device row) is W4's call | W4 never addresses it | **FAILS (undecided)** — folded into **G-2** |
| 41 | W7 | The peer registry is readable through `StationRegistry` for `GET /api/v1/peers/stations`, with per-peer registration state visible | W7 D-32, §5.1 | holds |
| 42 | W7 | A peer uplink's spool is a **durable** sink, so "nothing has been lost" is the correct reassurance | W7 D-20, D-21 | holds |
| 43 | W8 | A `ui` job can join `pr.yml`, and `gate ui-bundle` / `gate ui-copy` fit the gate vocabulary | Neither gate is declared | forward — folded into **G-4** |
| 44 | W8 | `cargo xtask dist` can build the UI before the Rust binaries, so a release cannot embed a stale bundle | `xtask/src/dist.rs` has no UI step | forward — carried into the plan's UI work item |
| 45 | W8 | pnpm and a Node toolchain are acceptable **build-time** dependencies, "which is W8's to confirm" | W8 is merged and never confirmed | **FAILS (unanswerable)** — see C-W5-45 |

**C-W5-36 — the management schema must not ride the registry's drift gate.**

W5 assumption 36 folds two new generated targets into `codegen-drift`. W9 D-20 had already decided
the opposite principle for exactly this situation, and gave the reason: `codegen --check` should keep
meaning precisely "`generated/` matches `metrics/registry.toml`", because that is the sentence a
reviewer relies on when a PR touches generated code. A second emitter with a *different input* —
W9's was the Go view inventory; W5's is the Rust handler set via `utoipa` — gets its own gate.

**Resolution: follow W9's precedent.** The management schema gets `mgmt-schema-drift`, armed by the
management crate's manifest, with the same "assert the check left the tree clean" step. Declared in
`ci/gates.toml` in this PR as deferred. Recorded as **D-57**. Clerical, and it is the pattern the
repository already has.

**C-W5-45 — nobody confirmed that a release machine may need Node.**

W5's build is SvelteKit 2 + Vite 6, bundled with `rust-embed` (W5 D-49). That makes pnpm and a Node
toolchain build-time requirements for any machine that produces a release artefact, which W5
correctly flagged as W8's to confirm. W8 was merged two stages earlier.

**Resolution: confirm it here, because the facts are all in the tree and the answer is forced.**
`packaging/docker/Dockerfile` is `FROM scratch` over pre-cross-compiled binaries and `ci/release.toml`
drives `cargo xtask dist`, so the UI bundle must be built **once, natively, before** the
four-architecture cross matrix — it is architecture-independent static files. That is strictly easier
than the alternative (building it inside each `cross` container). So: Node and pnpm join the release
machine's requirements, `cargo xtask dist` builds the UI first and fails if the bundle is missing
or stale, and the shipped artefact is still one static binary with no runtime dependency. Recorded as
**D-58**; the plan's UI work item owns the `dist.rs` change and the `ui` job.

### 1.8 W7 (collector) — 37 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W2a | W2a §4.1's uplink filter is **corrected** to `NotFrom(peer)` | W2a said `NotFrom(self)` | **FAILS (correction accepted)** — C-W2a-25, D-47 |
| 2 | W2a | `NotFrom(origin)` is a membership test over the path and `has_visited` is unchanged under an ordered path | W2a D-63; W3 assumption 42 | holds |
| 3 | W2a | §4a.3's `Transport` → `Trigger` implication table is scoped to **station** components | W2a D-41 states the implication without scoping it | holds (narrowed) — the uplink and `writer:timescale` hold a transport across ticks and are `OnReading`; a useful scoping, no rule changed |
| 4 | W2a | `FromRegistration` is the ingest side and needs no new machinery | W2a D-42, D-45 | holds |
| 5 | W2a | `StationRegistry` is `Write` for `ingest:*` alone, because registration mints keys | W2a §4.1b | holds |
| 6 | W2a | The resolved `PeerId` reaches `accept` through `FrameMeta`, whose fields W2a does not enumerate | W3 assumption 47 routes peer identity through `FrameMeta` | holds |
| 7 | W2a | W2a D-62's spool is this document's spool; nothing in §3 is collector-specific | W2a D-62; W3 D-13 | holds |
| 8 | W2a | The supervisor owns reconnection; §4.3 describes one attempt | W2a D-28, D-29 | holds |
| 9 | W2a | D-50 holds for collector-side uploaders: `for`-selected stations resolve through `WindowedView` | W2a D-50 | holds |
| 10 | W3 | W3 assumption 44's `NotFrom(self)` is corrected, recorded against both documents | — | **FAILS (correction accepted)** — D-47 |
| 11 | W3 | `seq` is per origin, not per `(origin, station)`; W3's `Seq` type and `Provenance`'s privacy are unchanged | W3 D-14 | holds (narrowed) |
| 12 | W3 | `PathSet` becomes an ordered `SmallVec<[OriginId; 8]>` rather than a bitset. **"A request, not a unilateral change"** | W3 owns the in-memory type and is merged | **FAILS (unanswerable)** — see C-W7-12 |
| 13 | W3 | `Ingress` gains `accept()`, which preserves provenance where `admit` would reassign `seq` | Same: a request to a merged document | **FAILS (unanswerable)** — see C-W7-12 |
| 14 | W3 | §2.4's group-commit cap of 64 counts append **calls**, not records | W3 D-18's rationale reads naturally this way; the other reading would contradict the 256-envelope batch in the same table | holds (clarified) |
| 15 | W3 | §2.4's sizes and §2.7's policies are adopted unchanged | W3 D-18, D-28, D-29 | holds |
| 16 | W3 | §3.6's 15 s uplink shutdown grace is the `Draining` budget | W3 D-48 | holds |
| 17 | W3 | D-89 is load-bearing for the spool as well as the wire, because the spool payload **is** the wire message | W7 D-11; and the generator does not emit `optional` | holds as an assumption, **blocked by G-3** |
| 18 | W3 | `grpc_port` keeps its own socket, so `ingest:grpc` is a drop-in for `grpcreceiver` | W3 D-92 | holds |
| 19 | W3 | One schema and one pair of conversions serve the spool too; no spool-specific encoding | W3 D-71 | holds |
| 20 | W3 | Plaintext h2c does not extend to ingest — a narrowing for one route table | W3 D-72; W7 D-35 | holds |
| 21 | W3 | `SpooledSink` wraps the uplink, and a filtered envelope counts toward `Acked(n)` or the cursor deadlocks | W3 D-16; W7 D-9 | holds |
| 22 | W2 | The generator emits `collector.proto`'s `Envelope` with the reading message as a field | `generated/proto/` holds `reading.proto` only | forward (unbuilt) — carried into the plan's collector work item |
| 23 | W2 | **Every metric field is `optional`** — "the one item another work item must execute" | Not emitted | **FAILS** — **G-3** |
| 24 | W2 | `Reading` is `Option`-valued and registry-typed, so an unreported metric is `None` end to end | `generated/rust/reading.rs` carries **214** `Option`-typed fields (212 metrics plus `stationName`/`stationType`) | holds (verified in tree) |
| 25 | W4 | The raw hypertable keeps `UNIQUE (station_id, time)` and the writer keeps `ON CONFLICT DO UPDATE` | W4 D-11 | holds |
| 26 | W4 | `origin` is worth a provenance column; `(origin, seq)` is not a uniqueness key. **W4 decides** | W4 never addresses it; W4 and W7 ran in the same stage | **FAILS (undecided)** — see C-W7-26 |
| 27 | W4 | A collector-written row can be NULL in any metric column, including `winddir` | W4 D-8 makes that safe | holds |
| 28 | W5 | W5 owns the translation of every term here | W5 D-1, D-2 | holds |
| 29 | W7→W5 | §7's per-peer five-number row is available as numbers; W5 owns the rendering | W5's `/data-storage/send` and `/receive` pages show four of the five; peer-reported depth is absent | holds (narrowed) — see C-W7-29 |
| 30 | W5 | W5 surfaces outstanding enrolment tokens and the enrolment log | W5's page inventory has neither | **FAILS** — see C-W7-29 |
| 31 | W5 | W5 surfaces §3.5's configuration advisory and `spool_time_remaining_seconds` | Neither is in W5's inventory | **FAILS** — see C-W7-29 |
| 32 | W5 | `peer_reported_depth_envelopes` is labelled as peer-reported wherever shown | Not shown | **FAILS** — see C-W7-29 |
| 33 | W9 | W9 moves each `remote_stations` row's services into collector-side `[[uploader]]` rows, drops the four plaintext secret columns, and retires the table | W9 covers weather data only; it never touches the config database | **FAILS** — **G-2** |
| 34 | W9 | W9 recommends rotating all four migrated credentials | Not done | **FAILS** — **G-2** |
| 35 | W9 | W9 owns removing the Go compat shim and allocating a `NodeId` per existing node at migration | Not done | **FAILS** — **G-2** and **N-5** |
| 36 | W1 | `POST /v1/ingest` is additive and collides with nothing in W1's `/v1/*` surface | W1's `/v1/*` is `/v1/weather/span/{spanDuration}` and `/v1/weather/latest` | holds (verified against `contract/legacy-v1.json`) |
| 37 | W1 | `WeatherV1` is not modified, so every `google.api.http` annotation W1 pinned is untouched | W7 D-19 | holds |

**C-W7-12 and -13 — two requests to a merged W3, both accepted on correctness grounds.**

W7 was careful to phrase both as requests rather than changes. Both are accepted, and in both cases
the argument is about correctness rather than taste, which is why this document resolves them instead
of escalating:

- **`path` is an ordered `SmallVec<[OriginId; 8]>`, not a bitset.** W7 D-7's receive check has six
  clauses, three of which (`path[0] == origin`, addressed to me, `path[len-2]` is the sending peer)
  are *positional*. A set cannot express them — W7 D-6 puts it exactly: a bare set cannot distinguish
  "you were added by my sender's `forward`" from "you were here two hops ago". At `MAX_PATH = 8` the
  bitset is an optimisation with no measurable benefit and an overflow case to guard. Recorded as
  **D-59**.
- **`Ingress` gains `accept()`.** W3 §2.6's `admit` assigns `seq = seq_table.next(...)`. Running it on
  a *forwarded* envelope would renumber it in the receiver's coordinate space, destroying the
  `(origin, seq)` identity that W7's cumulative ack (D-22) and dedup gate (D-27) both depend on. This
  is not a preference; `admit` is wrong for this input. Recorded as **D-60**.

**C-W7-26 — whether `origin` becomes a column on the raw table is still undecided.**

W7 D-28 adds no columns and says provenance-as-a-column is W4's call. W4 and W7 ran in the same
stage, so W4 could not have read it.

**Resolution: do not add it now, and record why adding it later is not free.** Nothing reads it —
W7 D-27 and D-28 put idempotency at the spool gate and the natural key, so `origin` on the row has no
consumer. More importantly, `origin` would be a `[[dimension]]`, and OQ-W4-2 established that
`[[dimension]]` has **no deprecation path** (`snapshot::from_registry` hard-codes
`deprecated: false`), so an `origin` column added speculatively could not later be removed without
the generator change OQ-W4-2 asks for. Recorded as **D-61**: provenance stays off the raw table;
if it is wanted later, **N-14**'s `metadata` role is a prerequisite. Resolvable inline, and the ordering
matters more than the decision.

**C-W7-29 — W7's observability surface has no UI, because W5 and W7 ran in parallel.**

Four assumptions (29–32) with one cause. W5's page inventory does cover peers — `/data-storage/send`
and `/data-storage/receive` carry peer address, status, unsent-readings depth, last successful
delivery, the peer registry and per-peer last-seen — so the *pages* exist. What is missing is
everything W7 specified after W5 had finished:

- `peer_reported_depth_envelopes`, which must be labelled peer-reported (W7 D-40);
- `spool_time_remaining_seconds` and §3.5's `ceiling < max_lag × rate` advisory, which is the thing
  that makes W7 D-16's ceiling policy watchable rather than a coin flip;
- outstanding enrolment tokens and the enrolment log, because **leaving enrolment open is the mistake
  and it should be visible** (W7 D-33).

**Resolution: raised as work item N-11**, additions to two pages W5 already designed, in W5's own
design language, with W5 §0's vocabulary rules applying. No design question — W7 says exactly which
numbers must be available and W5 says exactly how a number is rendered. The enrolment-token list is
the one with a security argument behind it, so the plan puts it with the collector's admission work
rather than with cosmetic UI work.

### 1.9 W8 (CI and release) — 18 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W3 | W3 creates `crates/modernweather` producing a binary named `modernweather` | W3 §1.2 commits to exactly this; `crates/` is empty | forward |
| 2 | W3 | The daemon accepts `--config <path>` | W3 §3.1 | holds |
| 3 | W3 | The daemon signals systemd readiness, so `Type=notify` stands | W3 D-47 with `sd-notify` | holds |
| 4 | W3 | W3 owns the crate and workspace layout, so this item created no application crate | W3 D-1, D-2 | holds |
| 5 | W3 | W3's concurrency tests are runnable under both ThreadSanitizer and `loom` | W3 assumption 51 answers yes, with the reason | holds |
| 6 | W3 | **W3 asserts the database version at startup** as well as in CI (D-42) | W3 never mentions a startup assertion; `grep -i 'version' docs/service-architecture.md` finds no startup check | **FAILS** — see C-W8-6 |
| 7 | W2 | W2 provides a `mw-codegen` binary taking `--registry` and `--out` | W2 implemented `cargo xtask codegen` in `xtask/src/gen.rs`; there is no `mw-codegen` package and no `xtask/src/codegen.rs` | **FAILS (harmless)** — see C-W8-7 |
| 8 | W2 | W2's generator is deterministic, since the drift check is a byte diff | `codegen-drift` is armed and green | holds (verified in tree) |
| 9 | W1 | W1's runner takes a base URL and is reachable as `cargo xtask conformance --target-binary <path>`; the PR gate runs it against the Rust binary, with the Go run scheduled | The runner takes `--base-url`; there is no `xtask conformance` subcommand and no scheduled Go-target job | **FAILS** — **G-1** |
| 10 | W1 | W1's fixtures are in `tests/conformance/`, with `manifest.toml` arming the gate | They are in `contract/fixtures/`; no `tests/` directory exists | **FAILS** — **G-1** |
| 11 | W4 | W4 creates `migrations/` as `.sql` files in filename order, plus `ci/db/seed.sql` and `verify-seed.sql` | All three exist | holds (verified in tree) |
| 12 | W4 | W4's migrations are idempotent in the `sqlx-migrate` sense, not the `IF NOT EXISTS` sense | W4 D-27; D-8 forbids the idiom | holds |
| 13 | W4 | W4 introduces no union views or versioned aggregates (D-5) | W4 adds none. W9 generates `observations_1d_with_legacy` but W9 D-6 keeps it out of the read path and W9 assumption 11 keeps it out of the hierarchy | holds (narrowed) |
| 14 | W4 | W4's rebuild procedure runs inside CI's budget on a fixture-sized database | W4 D-23's phase-1-first ordering: 0.87 s against 467 s | holds |
| 15 | W2a | The emulator tests connect to a TCP port supplied by the harness | W2a/W3 D-83: `socat` pty pairs for Davis and Campbell; the rest are natively TCP, UDP or HTTP | holds |
| 16 | W2a | WeatherLink Live discovery uses an in-process mDNS client, not `getaddrinfo` of a `.local` name | W2a §4a does not name a discovery mechanism, but nothing contradicts it and the constraint is W8's | forward — carried into the plan's WLL work item as a stated constraint |
| 17 | — | The reference Go repository stays readable at a pinned commit | `remoteweather` @ `c3ec9f7` is readable | holds (verified) |
| 18 | W6 | W6 reads these four closing sections and nothing above them | §1's method | holds |

**C-W8-6 — the startup half of D-42 has no owner.**

D-42 requires the database version to be asserted in CI **and at startup**. W8 implemented the CI
half (`ci/sql/assert-timescaledb-version.sql`) and said so explicitly: "This item covers only the CI
half." W3, which owns the binary and its boot sequence, never picked the other half up.

**Resolution: assign it.** The check belongs in `crates/modernweather`'s startup, after the pool
opens and before `plan()` runs, failing with a message naming the required and found versions — the
same comparison `assert-timescaledb-version.sql` makes, so the two cannot disagree. Recorded as
**D-62** and listed in the plan's bootstrap work item. Clerical; the failure is simply that "CI and
startup" is one decision split across two documents and only one of them had a `§`.

**C-W8-7 — `mw-codegen` never existed, and the gate is right anyway.**

W8 D-12 names the W8–W2 codegen contract as three paths defined in `xtask/src/codegen.rs`, including
`cargo run -p mw-codegen -- --registry <file> --out <dir>`. That file and that package do not exist:
at merge, W8's placeholder `xtask/src/codegen.rs` was dropped because it shimmed out to a separate
`mw-codegen` package, while `ci/gates.toml` already declared the drift gate as
`cargo xtask codegen --check` — which is what W2 actually built in `xtask/src/gen.rs`.

So the *mechanism* is correct and green; only W8's prose describes an interface nobody built.
`docs/release.md` carries five such references.

**Resolution: no code change.** The gate is right, the generator is right, and `codegen-drift` has
been running since W2 landed. What is wrong is documentation, and `DESIGN.md`'s ledger now records the
real interface (**D-63**). The contrast with G-1 is the point worth keeping: the same merge produced
one stale reference that is harmless because the manifest was already correct, and one that is
dangerous because the manifest was not.

### 1.10 W9 (data migration) — 27 assumptions

| # | About | Assumption | What the other document decided | Verdict |
|---|---|---|---|---|
| 1 | W4 | `observations_station_id_time_key` is created by migration `20261029234500` on an empty table, discharging W4 assumption 34 | Verified in `migrations/20261029234500_*.sql` | holds (verified in tree) |
| 2 | W4 | The retention windows are 7, 14, 90 days and unbounded | W4 D-5 | holds |
| 3 | W4 | `observations_1d` is the only level `/span` reads over history | W4 §1.2, D-5 | holds |
| 4 | W4 | `stations` is keyed by `station_id text` and carries the nine named columns | W4 §2.1, D-14 | holds |
| 5 | W4 | `stationname`/`stationtype` are still `attribute` dimensions on `observations`, and the import's column list shrinks automatically when OQ-W4-2 drops them | W4 OQ-W4-2 leaves them in; W2 D-5 | holds |
| 6 | W4 | `station_metrics` is keyed by the registry's stable `key`, not the column name | W4 D-16 | holds |
| 7 | W4 | Every metric column is nullable, so a column production lacks imports as NULL | W4 §3.4; W2's generated DDL | holds |
| 8 | W4 | `mw_mean` returns NULL for `n = 0`; `mw_circular_mean` returns **0** | W4 D-8 | holds |
| 9 | W4 | Count columns above the first level are `bigint`, not `numeric` | W4 D-9 | holds |
| 10 | W4 | `rebuild-aggregates.sh` is W4's; W9 added only a `PSQL` override and changed no ordering | Flagged in W9's own "generator edits" appendix | holds (flagged) |
| 11 | W4 | `observations_1d_with_legacy` and `_1h_legacy` are not inputs to any level and are the exceptions if W4's assertions grow an `observations*` check | W4's assertions have no such check today | forward (conditional) |
| 12 | W2 | The registry's `column` field is the *old* column name for all 215 Go columns, which makes the mapping the identity | W2 D-3, D-23 | holds (verified: `legacy-import-drift` is armed and green) |
| 13 | W2 | `aggregates` and `levels` determine the archive's destination columns, and the transform special-cases nothing | W2 D-9, D-30 | holds |
| 14 | W2 | The 39 truncated-integer-mean columns and the four semantic ones are consequences of registry decisions | W2 D-26, D-27 | holds |
| 15 | W2 | `cargo xtask legacy-import` reuses the registry's own column expansion, so a strategy change fails a W9 test | `xtask/src/legacy.rs` reads through `registry.rs` | holds (verified in tree) |
| 16 | W3 | `/span` resolves a device name to a `station_id` through `StationRegistry`/`stations`, matching on `stations.name` | W3 §1.2, §2.6; W9 D-15's default fills `name` from the old `stationname` | holds |
| 17 | W3 | `snapshot:latest` reads `stations` for display metadata rather than off a reading | W3 D-31; W4 assumption 24 | holds |
| 18 | W3 | `ReadingStore::span` reads `observations_1d`, not the union view | W9 D-6 | holds |
| 19 | W3 | The trailing partial bucket reads raw over a range always inside the imported window | W4 D-13 | holds — and see C-W3/W4-1 for where that bucket is cached |
| 20 | W1 | `/span` takes no time anchor, rejects a span over one year, and `station` is a device name. **W9 D-6 rests entirely on this** | W1 §2.1, §2.3 | holds |
| 21 | W1 | Conformance is shape, types and formatting rather than values, which is why §7's two differing keys are not failures | W1 D-3, D-4 | holds — and the same property is why C-W2a-27 is a real gap |
| 22 | W1 | `period_rain` is a `/span`-only key naming a bucket total with no raw counterpart | W1 assumption 11; W4 D-22 | holds |
| 23 | W6 | OQ-W9-4 asks W6 whether the `period_rain`/`windd` accuracy change belongs in the deviation ledger | Answered in §3: **yes**, as **DEV-4** | holds |
| 24 | W6 | The time-anchored v2 endpoint is the only thing that would read `observations_1d_legacy`; W6 records it as a named work item | §4, **N-1**, with this as a second named caller | holds |
| 25 | W6 | `docs/rewrite/README.md` pre-flight item 1 should say *archive first, then pause*, because the archive is correct under every answer to OQ-W9-2 and pausing is not what protects the aggregate buckets | README item 1 treats them as one action. Corrected in this PR | holds |
| 26 | W8 | `legacy-import-drift` follows `codegen-drift`'s pattern exactly, including the tree-clean assertion | Verified in `ci/gates.toml` and `pr.yml` | holds (verified in tree) |
| 27 | W8 | The `actionlint` job's `shellcheck` step now covers the five new scripts and `rebuild-aggregates.sh` | Verified in `pr.yml`'s `workflows` job | holds (verified in tree) |

### 1.11 Tally

| Verdict | Rows |
|---|---|
| **holds** as written | 208 |
| holds, narrowed / corrected / clarified / verified in tree | 64 |
| forward — a commitment on an artefact not yet built | 8 |
| **FAILS** | **49** |
| **Total assumptions checked** | **329** |

The 49 failing rows are **27 distinct defects**; the inflation is because the output contract works
as designed — a defect that sits between two documents is usually reported from both sides, and
`NotFrom(self)` alone accounts for four rows (W2a-25, W3-44, W7-1, W7-10).

**None of the 27 is a disagreement about what the system should do.** By cause:

| Cause | Defects | Rows | Entries |
|---|---|---|---|
| Written in the same stage, so neither could read the other | 9 | 15 | C-W2-19, C-W2-26, C-W2-9, C-W2a-15, C-W2a-27, C-W2a-28, C-W7-26, C-W7-29, C-W8-7 |
| A request or extension aimed at a document already merged | 3 | 6 | C-W3-2, C-W5-45, C-W7-12/-13 |
| A forward requirement nobody executed | 2 | 4 | **G-3** (proto3 presence), C-W8-6 (startup version assert) |
| An artefact contradicts the manifest that guards it | 2 | 5 | **G-1** (conformance gate), **G-4** (undeclared gates) |
| A decision superseded by measurement | 2 | 2 | C-W0-7, C-W0-16 |
| Naming or schema divergence, flagged by its own finder | 4 | 5 | C-W3-40, D-49, C-W5-36, C-W2a-10 |
| An ownership hole | 5 | 12 | **G-2** (config store), C-W2-20, C-W3/W4-1, C-W4-20, C-W2a-10 |

(Two entries appear twice where a defect has two causes; the row counts do not.)

**The method earned its cost.** Twenty-four of the 27 defects were found by grepping *Assumptions*
sections against *Decisions* sections, which is precisely what the output contract exists for. Two
more were found by checking a document's claim against the tree. **Only one — C-W3/W4-1 — required
reading two *Decisions* sections together and noticing that their conjunction fails**, and it is
correspondingly the only finding here that no author had already half-seen. If the closing-sections
discipline had not been imposed, this pass would have been a re-read of 16,000 lines of prose with
no detector.

**Where the method has a hole.** Six of the 49 rows are a downstream document asking an upstream one
for a small extension, and in every case the upstream document was merged and could not answer. The
"stay in your lane" instruction is right for producing documents and it has **no mechanism for a
reply**. See C-W3-2 and §5's **O-12**.

---

## 2. Gap audit

**Method.** The union of all ten prompts' "do not" lists is 34 deferrals. Each was checked for an
owner. Most are owned: "do not design the metric registry" is W2's, "do not design the pipeline
internals" is W3's, and so on. What follows is everything in the union that is **not** owned, plus
the specific items §2 of the W6 prompt named for verification.

### 2.0 Verification of the named items

| Item | Owner | Verdict |
|---|---|---|
| The public website's three Go templates | W3 §7 | **Landed.** W3 D-74 ports them to askama with a `GoHtmlEscaper` reproducing `html/template`'s `&#34;`/`&#39;`, so the acceptance test is a byte diff against W1's recorded bodies. D-75 makes `portal.html.tmpl` a static asset — it consumes nothing. |
| The `weather-app.js.tmpl` replacement | W3 §7.4 | **Landed, and specified further than D-38 asked.** D-79 delivers the payload inline as `<script id="mw-config" type="application/json">` following the page's existing `radar-config` precedent, with `GET /config.json` as the additive endpoint; D-80 fixes the payload as exactly W1 §9's nine consumed values with matching key names. A-W3-3 records why a literal `fetch` reading of D-38 would have been wrong. |
| Migration of the existing `weather_v2_0_0` data | W9 | **Landed.** `ci/db/migrate-from-go/` with a generated 629-column transform, resumable at slice granularity, reconciled in `numeric` with no tolerance. The history decision **was presented, not assumed**: OQ-W9-1 states Option A and Option B with a recommendation, and W9 D-7 records the recommendation as a recommendation. Carried to the owner as **O-7**. |
| The release and packaging story | W8 | **Landed.** Four Linux targets in `ci/release.toml`, every one static musl; deb, rpm, Arch and `FROM scratch` containers in `packaging/`. Windows amd64 was **consciously dropped** by owner decision (W8 D-32) with the target block and the zip branch removed rather than left dormant. The ARMv6 answer **did** feed back and the feedback was negative: W8 D-27 records that `sqlx`'s bundled SQLite compiles for ARMv6 and runs on an ARM1176, so D-32 stands unamended. |
| CI — does the PR gate exist and is it green? | W8 | **Exists and is green — with one gate green by deferral rather than by running. See G-1.** Thirteen jobs feeding one required `gate` job that fails on `skipped` and `cancelled` as well as failure. `cargo xtask gates` reports four armed and two deferred. |
| **The new v2 API surface** | nobody | **Deliberately post-design scope**, confirmed against three sources: D-39, the master issue's pre-flight item 4, and W1's and W3's matching deferrals. Raised as named work item **N-1**, with its starting inventory assembled below. |
| **The config store and config migration** | nobody | **Unowned. See G-2** — the largest hole this pass found. |
| The four dead Go tools | D-40 | **Settled, and the count is three decisions not four.** D-40: the provisioner stays in Go and needs updating for the new schema; the migrator retires with the Go daemon; the four emulators/simulators stay in Go deliberately as independent test oracles; the remainder are unused and belong in scratch. The master issue's "Decisions resolved" table records the owner's agreement. **One consequence is unowned**: updating the Go provisioner for the new schema is a change to the read-only reference repository and no work item owns it. Raised as **N-2**. |

### G-1 — one CI gate is green by deferral while the deliverable it guards is complete

**This is the most consequential finding in this document**, and it is the one the W6 prompt asked to
be worked through rather than taken on trust. The chain, verified command by command:

```
$ cargo xtask gate conformance
DEFERRED  conformance
  Frozen legacy REST contract replayed against the built binary
  owned by W1 (frozen contract)
  arms itself when tests/conformance/manifest.toml exists; nothing to check until then
$ echo $?
0
```

- `ci/gates.toml` declares gate `conformance` with `arms_when = "tests/conformance/manifest.toml"`
  and `cmd = ["cargo", "xtask", "conformance", "--target-binary", "target/debug/modernweather"]`.
- `tests/` **does not exist** and nothing is going to create it: W1 shipped its runner as a
  standalone crate at `contract/conformance/` with 70 fixtures in `contract/fixtures/`, per R-3 and
  W1 D-7.
- There is no `conformance` subcommand in `xtask` (`xtask/src/main.rs:48-58`) and no `modernweather`
  binary (`crates/` holds only `.gitkeep`).
- `pr.yml`'s job *"frozen contract conformance"* runs `cargo xtask gate conformance`, which reads the
  manifest, finds the path absent, prints `DEFERRED` and **exits 0**.
- That job is a `needs:` of the required `gate` job, so every PR reports a green check named after a
  suite that has never run.

**W1's own "done when" required the suite to pass against the Go server.** It did, locally, during
W1 — `contract/legacy-v1.json` records the server commit and the fixtures carry per-fixture
provenance. But it has never run in CI, against either binary, and W1 D-10 additionally requires a
passing **negative control** (`contract/tools/rig/mangle_proxy.py`, kept in-tree for the purpose)
before a green run is accepted. Neither runs.

**Contrast with `codegen-drift`, where the identical mechanism worked exactly as intended.** W2
landed `metrics/registry.toml`, the gate armed itself, and it has been running and green on every PR
since. The deferred-gate mechanism is sound. What failed here is narrower and worth stating
precisely: **this gate's `arms_when` path and `cmd` describe an interface nobody committed to
building.** W8 wrote them from its prompt-era expectation (`tests/conformance/`, a `--target-binary`
driver); W1 shipped something different and better; W3 §1.8 **noticed and recorded the fix** —
"recommend changing the `arms_when` path in `ci/gates.toml` to `contract/fixtures/`" — and, having
written no code, could not apply it.

**Resolution. Who fixes it, and the fix is not the one W3 recommended.**

W3's recommendation would arm the gate immediately (`contract/fixtures/` exists) and it would then
fail, because `cargo xtask conformance` does not exist and there is no Rust daemon to target. That is
*correct* behaviour under W8 D-9 — "a gate that arms into a missing command fails and names the
owning work item" — but it would turn every PR red for a check that is genuinely impossible today.
Arming a gate on a path that is already present is the mirror of the defect, not its cure.

The real error is that **one gate is modelling two checks with different readiness**, which W1's own
R-3 distinguishes clearly and W8's manifest collapses:

| Check | Target | Possible today? |
|---|---|---|
| **Fixture validity** — do the 70 fixtures describe reality? | the **Go** daemon | Yes. Needs a Go build, a TimescaleDB container and W1's seeding rig, all of which exist in `contract/tools/rig/`. |
| **Rewrite conformance** — does the rewrite match? | the **Rust** daemon | No. There is no daemon. |

So, applied in this PR:

1. **`conformance` is repointed at the artefact whose existence actually makes it runnable** —
   `arms_when = "crates/modernweather/Cargo.toml"`, the daemon's own manifest, which is also what
   arms the release pipeline (`ci/release.toml`'s `crate_manifest`). The `cmd` is unchanged: W3 §1.8
   decided `cargo xtask conformance --target-binary <path>` shells into `contract/conformance` with
   `--locked`, so `--target-binary` is the xtask-level interface and `--base-url` is the inner
   crate's. Both are correct; only the arming path was wrong. The `note` now names the two checks and
   points at the second gate.
2. **A new `conformance-fixtures` gate is declared**, armed by `ci/conformance/go-target.sh` — a path
   that does not exist yet and that **N-3**'s done-criteria require it to create. That is the
   deferred mechanism used as designed: the work item that builds the harness turns the gate on.
3. **N-3 is raised** as a named work item: stand the Go daemon up in CI from the pinned reference
   commit, seed it with W1's rig, replay the 70 fixtures, and run `mangle_proxy.py` as the negative
   control W1 D-10 requires. This is real CI work — a Go toolchain, a TimescaleDB service container,
   the seeding SQL — not a one-line fix, which is why it is a work item and not an edit.

**And one general rule, because this will recur.** `driver-emulators` has the same shape:
`arms_when = "tests/emulators/manifest.toml"`, a path no merged document commits to creating. It is
*legitimately* deferred today — no Rust drivers exist — so it is left alone, but the rule that
prevents a second G-1 is recorded as **D-64**: **a gate's `arms_when` must name a path that some work
item's done-criteria require it to create.** The implementation plan lists
`tests/emulators/manifest.toml` as an explicit deliverable of the first-driver work item for exactly
this reason.

### G-2 — the configuration store and its migration are unowned

The second-largest hole, and unlike G-1 it is a genuine absence rather than a misconfiguration.

**What exists.** `migrations/` is three PostgreSQL files. W4 scopes itself to PostgreSQL and
TimescaleDB throughout and says so. W3 D-41 owns the SQLite *pool* — a root resource in `main`,
handing `ScheduleStore` to the supervisor and the full handle to `config:sqlite` — but not a schema.
W9 migrates weather data and never touches the config database.

**What four documents assume into existence.** At least nine tables, from four authors, with no DDL
and no migration mechanism:

| Table(s) | Assumed by | For |
|---|---|---|
| `component_schedule` | W2a OQ-D, W3 D-41 | `Misfire::RunLatest` correctness across a restart (W2a D-12, D-14) |
| `admin_credential`, `admin_session` | W5 D-23, D-24; assumption 39 | Argon2id credentials and individually revocable sessions |
| `peers`, `peer_origins`, `stations` | W7 §5.1 | The peer registry and `StationIdentity::FromRegistration` |
| the station / website / uploader configuration | W3, W5 throughout | Everything the management UI edits |

W5 assumption 39 states the requirement exactly right — "**SQLite** config-database tables created by
a migration, not by DDL at startup — the 'migrations only' rule applied to the configuration store" —
and assigns it to W4, which never picked it up. D-8 ("schema changes solely through migrations; no
DDL at startup and no `IF NOT EXISTS` idiom") applies to this database just as much as to the weather
one, and D-8's own rationale is the Go schema frozen at first-run shape by exactly that idiom.

**Two things make this worse than a missing schema.**

*A name collision.* W4 D-14 creates a PostgreSQL table called `stations` — a read-side projection
keyed by `station_id text` — and says the authoritative registry stays in `config:sqlite`. W7 §5.1
creates a SQLite table *also* called `stations`, keyed by `station_key` and carrying different
columns. Both are correct in their own document; together, "the stations table" is ambiguous in a
system where one is authoritative and the other is a cache. Recorded as **D-65**: the SQLite table is
`config.stations` and the PostgreSQL projection is `observations`-side `stations`, and no document or
comment may write "the stations table" unqualified.

*Four plaintext secrets with a migration nobody owns.* W7 D-32 and A-W7-2 establish that
registration carries identity only, and that the current `RemoteStationConfig` ships
`aprs_password`, `wu_api_key`, `aeris_client_secret` and `pws_password` on every registration and
`INSERT OR REPLACE`s them in cleartext into the collector's `remote_stations` table — **where nothing
reads them**. W7 assumptions 33 and 34 ask W9 to move each row's enabled services into collector-side
`[[uploader]]` rows, drop the four secret columns, retire the table, and *recommend rotating all four
credentials because they crossed the wire in plaintext and sat in a second database*. W9's prompt
scopes it to weather data; none of this happened.

**Resolution: two work items, not one, because the two halves have different risk profiles.**

- **N-4 — the configuration schema.** Define the SQLite config schema and a migration mechanism for
  it, under D-8: a `migrations/config/` directory applied by name, no startup DDL. It must cover the
  nine-plus tables above and resolve W5's **OQ-W5-4** (whether per-station service settings normalise
  off the device row into a `station_service` table), which W5 deferred to W4 and W4 never answered.
  W5's recommendation was (b), normalise, with the page design unchanged either way — and since the
  config schema is now being written from scratch rather than migrated in place, **(b) is free**:
  there is no in-place migration to avoid. Recorded as **D-66**, and it is the one place where this
  gap being found late makes the answer easier rather than harder.
- **N-5 — the configuration migration.** Move the owner's existing config database onto the new
  schema: station rows, website rows, certificates, the per-service settings, and W7's
  `remote_stations` → `peers`/`peer_origins`/`stations` conversion with the four secret columns
  dropped. Allocate a `NodeId` (UUIDv7) per existing node, **not** reusing the `station_id` UUIDs
  already in `remote_stations`, which are station identities (W7 assumption 35). Carry W7's
  credential-rotation recommendation to the owner as part of the cutover runbook.

**"Re-enter by hand" is not taken, and the reason is specific.** The W6 prompt permits it as a
recorded decision. It is declined because the data is not small and not all of it is re-typable: the
owner would have to re-enter every station's calibration (wind-direction correction, snow base
distance, altitude, IANA time zone), every website's hostname, title and about-text HTML, every TLS
certificate path, and four third-party credential sets — and a mistyped `snow_base_distance_mm`
produces plausible wrong numbers rather than an error. **The credential rotation, however, is
deliberately manual**: N-5 drops those four columns and the owner re-enters the rotated secrets once,
because migrating a credential that crossed the wire in plaintext would carry the exposure forward.
Recorded as **D-67**.

### G-3 — proto3 field presence: decided three times, implemented zero times

`generated/proto/reading.proto` contains the word `optional` exactly **zero** times:

```
$ grep -c optional generated/proto/reading.proto
0
```

Three documents depend on it:

- **W3 D-89** decides it, and W3 assumption 45 flags it as "the one decision here that another work
  item has to implement."
- **W7 assumption 23** calls it "the one item another work item must execute for this document to be
  correct", and W7 assumption 17 shows why it is worse than a wire concern: the spool record's
  payload **is** the wire message (W7 D-11), so without presence a `NULL` becomes `0.0` across a
  **restart** as well as across a hop.
- **W4 D-8** and W3 D-26 make NULL the normal state of an unreported metric, which is what creates
  the values that need distinguishing.

The Rust side is already correct — `generated/rust/reading.rs` carries **214** `Option`-typed fields (212 metrics plus `stationName`/`stationType`) — so the
defect is confined to the protobuf emitter and is a one-line-per-field generator change.

**Why this is not merely untidy.** The failure is silent and lossy in the direction that looks like
working software. A satellite that does not measure humidity forwards a reading with no humidity; the
collector decodes `0.0`; the writer stores `0.0` instead of NULL; the aggregate means include it. The
station appears to report 0% humidity forever, every value is plausible, and nothing reports an error.
The same thing happens to a local node's own spool across a restart.

**Resolution: raised as work item N-12**, a W2 follow-up: emit `optional` on every metric field in
`generated/proto/reading.proto`. It is wire-compatible — same field numbers, same tags, same encoding
— and W3's reversal trigger ("evidence that a deployed non-Rust consumer breaks on explicit
presence") has no known instance, since the only consumers are this daemon and its peers. The plan
sequences it **inside the registry work item**, before anything reads the proto, because retrofitting
it after a collector exists means a fleet-wide flag day for a change that is free today.

### G-4 — three gates named by a merged document and never declared

W3 assumption 55 records that three gates need their `arms_when` or `cmd` updated as its artefacts
land: `layout` (new), `conformance` (path — that is G-1) and `db-tests` (new). W5 assumption 43 adds
two more: `ui-bundle` and `ui-copy`. None of the four new ones is declared in `ci/gates.toml`.

This matters because of what those gates enforce, which is not incidental:

- **`layout`** is the *only* enforcement of W3's eighteen-crate boundary rules — the layer order, the
  forbidden lists, "only `mw-types` mentions `generated/`", "only `mw-http` binds a socket", "only the
  binary depends on `anyhow`", and the rule that a template may not use `safe` or `escape("none")`
  (W3 D-11, D-81). W3 D-1's entire argument for eighteen crates over one is that a boundary is only
  real if `cargo build` says so — and the gate is what makes the *other* half, the layer order and the
  template rule, say so too.
- **`db-tests`** backs W3 D-84: database tests **fail CI if they skip**, so "green because it did not
  run" is unreachable. That is the exact failure mode of G-1, named as a decision in one document and
  undeclared in the manifest that would enforce it.
- **`ui-bundle`** enforces W5 D-50's budget (150 KB gzipped initial route, 280 KB all routes, 60 KB
  largest lazy chunk, 45 KB fonts) against `ui/budget.toml`; **`ui-copy`** is W5 D-1's vocabulary
  lint, which A-W5-2 exists to make mechanical rather than a review preference.

**Resolution: all four declared in `ci/gates.toml` in this PR, deferred, each armed by the artefact
whose owning work item's done-criteria create it** — plus `mgmt-schema-drift` from C-W5-36. Declaring
a gate before its subject exists is what the manifest is *for* (W8 D-8: "nobody has to remember to
turn a gate on"), and leaving them undeclared is how a decision becomes a preference. Each now
appears in the `cargo xtask gates` summary printed into every PR, so the not-yet-armed set is visible
rather than forgotten.

### G-5 — four named W2 follow-ups, and one W4 script fix, are unowned

W4's open questions each end in a recommendation **addressed to W2**, and W2 is finished. These are
not questions for an owner: they are small, decided, and need a work item. W9 adds a fifth of the
same shape aimed at W4's rebuild script.

| Question | Recommendation, by its author | Status | Item |
|---|---|---|---|
| **OQ-W4-1** — `affected_levels` drops and recreates *every* level when one level's membership changed | **(b)** compare each level's rendered definition and include only levels whose text changed — "about ten lines, and the generator already renders both sides" | Unowned. Correct under D-15 and wasteful in exactly the way A-W2-2 describes: an intact minute level would have built the new 5-minute level in 59 s instead of forcing a 401 s raw rescan, and at ten years that difference is hours | **N-13** |
| **OQ-W4-2** / **OQ-W2-6** — `stationname`/`stationtype` should leave the raw table and cannot | **(a)** a new dimension `role = "metadata"`: present in the protobuf and the Rust type, absent from the raw DDL and the aggregates | Unowned, and **blocking**: `[[dimension]]` has no deprecation path at all — `snapshot::from_registry` hard-codes `deprecated: false`, and removing an entry without `deprecated = true` is FATAL with no override. So this is not a registry edit; the generator must grow the concept. It is also the prerequisite for C-W7-26 | **N-14** |
| **OQ-W4-3** — should the registry gain a `jsonb` type so `observations.extra` can be a registry entry? | **(a)** leave it until a driver needs it, **then (c)** — `SqlType::Jsonb`, the `metadata` role, and a protobuf `map<string, double>` so the hatch works across a hop | **Not a work item yet, deliberately.** No driver reports into `extra`. But W4's own note is a scheduling constraint: "worth deciding before W7 writes its first driver that receives an unknown field", so the plan puts the decision point in the first-driver work item rather than leaving it floating | gated |
| **OQ-W4-4** — `codegen` writes stale files and never prunes them | **(b)** have `codegen` delete files under `generated/` that it did not write | Unowned. Measured consequence: adding the 5-minute level orphaned `040_observations_1h.sql` and `050_observations_1d.sql`, so `codegen` leaves a tree `--check` rejects — a developer runs `codegen`, commits, and the gate fails on files `codegen` just made stale | **N-15** |
| **OQ-W9-3** — `rebuild-aggregates.sh` phase 1 leaves `observations_1d` short beyond 14 days | **(b)** refresh each level over the **widest** band any level above it serves, **plus (c)** have `--recent-only` warn, naming the levels it left incomplete | Unowned. The daily band is 26× wider than the hourly one, so bottom-up phase 1 has the daily level read a 14-day hourly level and **cache the hole**. A full run fixes it; `--recent-only` does not — and `--recent-only` is what W4 §4 recommends after a metric addition | **N-16** |

**Resolution: four work items and one gated decision**, as listed. All five are "small, decided, and
need a work item rather than a decision", which is what the W6 prompt's §0 asked for. **N-14 is the
one with a dependency worth noting**: it unblocks both OQ-W4-2's column drop and C-W7-26's eventual
provenance column, so the plan sequences it with the registry work rather than deferring it to
whenever someone wants to drop a column.

### 2.1 The rest of the "do not" union

Everything else in the union is owned. Recorded here so the audit is complete rather than
illustrative.

| Deferral | Appears in | Owner | Status |
|---|---|---|---|
| Design the metric registry format | W0, W2a, W3, W5, W7 | W2 | landed |
| Enumerate the legacy API surface | W0, W2 | W1 | landed |
| Put trait definitions in `DESIGN.md` | W0 | W3 | landed (`mw-component`) |
| Design the crate and workspace layout | W0 (R-4), W1, W2, W8 | W3 §1 | landed as a decision; the crates are unbuilt by design |
| Design the CI workflow | W2 | W8 | landed |
| Design the pipeline internals | W2a, W4, W7 | W3 §2 | landed |
| Design the management UI | W2a, W3 | W5 | landed |
| Design the target schema | W9 | W4 | landed |
| Design the migration of production data | W4 | W9 | landed |
| Decide sum-and-count vs `stats_agg` | W2 | W4 D-2 | landed |
| Decide 1-minute level membership | W2 | W4 D-3 | landed |
| Decide aggregate compression / retention | W2 | W4 D-4, D-5 | landed |
| Design the wire schema's *field* layout | W7 | W2 | landed — except presence (**G-3**) |
| Redesign the component model | W3, W4, W7 | W2a | landed; three extensions reconciled (C-W3-2) |
| Redesign the public website's appearance | W3, W5 | frozen by D-38 | landed |
| Expand into dashboards and charting | W5 | out of scope | not built, deliberately (W5 D-51) |
| Preserve the word "controller" | W2a | D-23 | retired |
| Preserve storage-engine / weather-station framing | W7 | W7 D-1, D-2 | retired |
| Add a hard coverage threshold | W8 | W8 D-15 | a ratchet, not a threshold |
| Replicate the Go pipeline's structure | W8 | W8 | outputs replicated, structure not |
| Touch the production database | W0, W2 | — | never touched; see §5's owner queries |
| Modify either reference repo | W0, W1 | — | `remoteweather` unmodified. **One consequence is unowned: N-2** |
| Decide the history question unilaterally | W9 | owner | presented as OQ-W9-1 → **O-7** |
| Fabricate raw rows from aggregate means | W9 | — | not done; W9 D-12 imports identities only |
| Decommission or modify the source database | W9 | — | read-only apart from the named archive step (W9 D-2, D-3) |
| Amend a `DESIGN.md` decision in place | W2, W4 | protocol | honoured by every document; applied here |
| Write implementation code | all | — | honoured. This document writes none; its edits are manifests, documentation and one prompt-set correction |

---

## 3. Deviation ledger

**What this is.** Every place a design knowingly changes a byte a shipped client could observe,
with who decided it, the reasoning, and whether it is additive or behavioural. This is the artefact
that answers *"did we break the iOS app, and if so where and on purpose."*

**Why it exists.** Two documents reference "W6's deviation ledger" by name before it existed — W3's
Open questions ("listed again in the assumptions for W6's ledger") and W4 assumption 39 ("there is
**no entry** for W6's deviation ledger") — and W9's OQ-W9-4 asks whether one of its findings belongs
in it. The W6 prompt never asked for one; three documents assumed it.

**The baseline.** D-37 freezes the legacy contract and makes new endpoints additive only. The
executable definition is `contract/legacy-v1.json` plus the 70 fixtures in `contract/fixtures/`
(W1 D-1, and A-2 asks D-37 to say so). The two clients that must keep working are
`RemoteWeather.app` @ `77967ab` and the existing weather website.

**Classification.** *Additive* — a shipped client cannot observe the change except by looking for
something it does not read. *Behavioural* — a byte a client does read, or could read, changes.

| | Deviation | Decided by | Class | Can either shipped client observe it? |
|---|---|---|---|---|
| **DEV-1** | `ETag` emitted on `/latest`, `/snow` and the static assets; `Last-Modified` on assets | W3 D-88 / D-35 / D-77, at the owner's request | additive | **No.** Settled by fact, not judgement: the conformance runner asserts a per-fixture *allowlist* of header names (`contract/conformance/src/lib.rs:320`) and never enumerates the response's headers — which is also why `Date` and `Server` have never been a problem. Neither client sends `If-None-Match`. `Cache-Control` is unchanged and `max-age` is deliberately **not** added. |
| **DEV-2** | `Host` matching becomes case-insensitive | W3 D-95 / D-67, at the owner's request | behavioural — **closes a defect** | **Only by depending on the bug.** The Go map lookup is case-sensitive, so a request for `EXAMPLE.COM` falls through and silently serves *someone else's* website. No fixture covers mixed case. Preserving the current behaviour would preserve a data-disclosure bug, not a contract. |
| **DEV-3** | Directory listings on `/js/`, `/css/`, `/images/`, `/fonts/` return `404` instead of an HTML index | W1 OQ-W1-4's own recommendation, taken by W3 D-96 / D-78 | behavioural — **removes a response** | **No shipped client requests them.** W1 raised it, recommended not reproducing them, and asked for it to be recorded here rather than discovered as a diff. It is a small information disclosure that nothing benefits from. This is the one place where "reproduce exactly" costs something and buys nothing. |
| **DEV-4** | `/span`'s `period_rain` and `windd` differ from the Go server in the 6th significant figure | **D-14**, surfaced by W9 OQ-W9-4 | behavioural — **improves accuracy** | **Not meaningfully.** The Go view accumulates in `float4`; D-14 widened the accumulator because `sum(real)` is not associative. Measured at 2,160 readings per daily bucket: `period_rain` differs by up to 3.13 × 10⁻⁶ relative and `winddir` by 1.73 × 10⁻⁷ — and **both round to a different float32 in 100% of buckets**. At production's 2-second cadence the *Go* side's error is roughly 20× larger. iOS decodes both into `Double`; the website charts them. Nobody can see 10⁻⁶ on a rain gauge. |
| **DEV-5** | Three additive `X-MW-*` response headers (`X-MW-Snapshot-Age`, `X-MW-Degraded`) during a database outage | W3 D-36 | additive | **No.** Same allowlist property as DEV-1. The frozen *body* is never used to signal degradation — that was the explicit alternative W3 rejected. |
| **DEV-7** | `/span`'s `solarjoules` becomes the bucket **total** instead of the bucket mean, and is NULL for pre-cutover history | **N-9** (implementation-plan.md M1-6) | behavioural — **corrects a meaningless statistic**, and **needs the owner's acknowledgement** | **Yes, and by orders of magnitude.** `solarjoules` carries the Campbell logger's `slr_mj`: energy per unit area accumulated since its previous output. The Go views stored `avg(solarjoules)`, which is the mean increment per reading and moves when the logger's output interval does; the registry now stores `total`, which is the bucket's insolation. At production's 2-second cadence that multiplies the value `/span` returns by roughly the bucket's reading count — **~30× at the 6 h band, ~150× at 48 h, ~1,800× at 14 d, ~43,200× at 366 d**. Two further consequences. (1) `/latest` serves the last raw reading, so after this change `/latest` and `/span` report different physical quantities under the same key and the same declared unit; the frozen contract has one name for them and `rainincremental`/`period_rain` is the only precedent, where the Go surface gave the bucket total a *second* name. (2) No Go view computed `sum(solarjoules)`, so `solarjoules_total` is NULL for the ~294 pre-cutover rollup days — and `solarjoules` is `"swift_optional": false` in `contract/legacy-v1.json`, the same property that makes `windd` a contract hazard, so the read path must coalesce it rather than emit null. **The conformance suite cannot catch any of this**: it compares shape, type and numeric formatting, not values (`contract/conformance/src/lib.rs`). |
| **DEV-10** | `/stationinfo` emits `about_text` and `weather_device` **unconditionally** rather than omitting them when unset | **O-5**, option (b), concurred by the owner; applied by implementation-plan.md **M4-2** | behavioural — **closes a live user-visible failure** | **Yes, and that is the point.** `StationInfo.aboutText` and `StationInfo.weatherDevice` are NON-OPTIONAL in the iOS client, so Go omitting either makes `JSONDecoder` throw `keyNotFound` and fail the decode of the **whole** object — the app loses the station list, the snow device, the air-quality device and the radar token, not just the missing field, and any portal website is permanently in that state. Emitting a key a non-optional property expects can only help; the website guards both reads. Two of the three `/stationinfo` fixtures carry a `deviation` block holding this response beside the Go server's, so `ci/conformance/go-target.sh` keeps asserting what shipped and `cargo xtask conformance` asserts the fix — one fixture, both halves, both gates still biting. |
| **DEV-11** | A metric whose stored value is `NaN` or `±Inf` serialises as the contract's `0` rather than failing the response | **M4-2**, as a consequence of `mw_api_legacy::zf` | behavioural — **replaces a truncated body with a zero** | **Only in a case neither client can currently reach.** Go's `encoding/json` refuses a non-finite float outright, so the Go server's answer for a `NaN` column is a *failed encode after the `200` header has been written* — a truncated body. `serde_json` would write `null`, which fails the iOS decode of the whole object because every number on this surface is a non-optional Swift property. Neither is a response a frozen route may give (**D-48**), and a value that is not a number is not a measurement (**D-26**'s own standard), so it is filled like an absent one. `mw_pipeline::enrich` already refuses to *store* a non-finite derived value, so the only way to reach this is a driver writing one directly. |
| **DEV-12** | `/span`'s window is **aligned onto the level's bucket grid** and inclusive at its lower bound, so a response carries exactly one bucket more than the Go server's | **D-55**, applied by implementation-plan.md **M4-2** | behavioural — **one extra element, on every response** | **No, and the extra element is older data rather than different data.** Go's predicate is `bucket > NOW() - span` with an unaligned bound (`controller.go:890`); this floors `now - span` onto the level's grid and uses `>=`, so `floor(now-span) <= now-span` always and exactly one bucket Go excludes is included. The alignment is what makes **D-54**'s cache key stable -- an unaligned `since` moves with every request, so every request would miss -- and a chart's left edge gaining one point of real history is not a decode or a layout change. Recorded here rather than left in a source comment because §3's own definition is "every place a design knowingly changes a byte a shipped client could observe", and this changes one on **100%** of `/span` responses. The alternative, dropping the oldest bucket when `since < now - span`, is one line and is available if the owner would rather have exact parity than a cacheable key. |
| **DEV-13** | `/forecast` with no `station` parameter chooses the **most recently updated** row for the span, tie-broken by `station_id`, where Go chooses the lowest surrogate `id` | **M4-2**, forced by the `forecasts` table's shape | behavioural — **which station's forecast an unqualified request gets** | **Only on a node with more than one forecast row for the same span, and only when they disagree.** Go's `First()` orders by the surrogate `id` the Aeris writer inserted in, which is insertion order; the new `forecasts` table's primary key is `(station_id, span_hours)` and it has no surrogate, so insertion order is not recoverable and *some* deterministic order has to be chosen. "Newest" is the better of the two available answers for a cache: a stale row cannot win. The website is the only client that issues an unqualified `/forecast`, and it is coupled rather than frozen (**D-69**), so a change here moves with it. Worth stating plainly: `ci/conformance/rust-seed/04-caches.sql` backdates `csi-minimal`'s row by an hour so that `forecast-240-ten-daily-periods` deterministically selects `davis-full` under this ordering -- the Go rig gives the two rows the same timestamp, so the fixture passes because the seed was shaped to the ordering and not because the two orderings agree. |
| **DEV-6** | Wildcard certificates match exactly one label; on an SNI miss the chain is exact → wildcard → designated default → refuse with `unrecognized_name` | W3 D-69, D-70 | behavioural — **closes two defects** | **Only by depending on the bugs.** The Go `strings.HasSuffix` check matches any depth *and* matches `evilexample.com` for `*.example.com`. On a miss, Go returns an arbitrary certificate by iterating a map — non-deterministic between runs, producing a browser interstitial that blames the user. |
| **DEV-8** | A WeatherLink Live station's readings are stored in the units the registry declares (°F, inHg, inches), its indoor temperature and humidity are stored at all, its rainfall is converted from tip counts, its ten-minute wind average is stored, and its sensor records are matched by record type as well as by transmitter | **D-26** and `metrics/registry.toml`'s declared units, applied by implementation-plan.md **M3-1**; five defects in the Go driver | behavioural — **closes five defects**, and **needs the owner's acknowledgement** | **Yes, if the owner has ever run a WeatherLink Live.** (1) `internal/weatherstations/weatherlinklive/data_transformer.go` converts every temperature `(F - 32) * 5 / 9` and every pressure `* 33.8639`, writing Celsius into `outtemp` and hPa into `barometer`/`baromrelin`/`baromabsin` — columns `metrics/registry.toml` declares as `degF` and `inHg`, which the Go **Davis** driver on the same schema fills in °F and inHg (`convBigVal10` of the LOOP packet) and which `/latest` emits with no conversion. The two Go drivers therefore disagree about what the column means, and the WeatherLink Live one disagrees with the frozen contract: 62.7 °F is stored as 17.06 and rendered as 17.06 °F. (2) It copies `rainfall_daily` and `rain_rate_last` — which are **tip counts** — into `dayrain` and `rainrate`, declared `in` and `in_per_hour`. The error is the reciprocal of the collector size, so it is **100×** for a 0.01 in collector, **127×** for 0.2 mm, **254×** for 0.1 mm and **1000×** for 0.001 in; `rain_size`, the only value that can convert a count into a depth, is decoded into the struct and never used to convert anything (`discovery.go:217` only lists its name). (3) Its `th_indoor` arm looks for `DataStructureBarometer` (record type 3) while `temp_in` and `hum_in` are in record type 4, so `intemp` and `inhumidity` are never populated from one of these units at all. (4) `findCondition` returns the first record whose transmitter matches regardless of record type, so `th:2` on a deployment with a leaf/soil station on transmitter 2 reads `temp` out of a record that has none and the outdoor temperature silently stops. (5) `windspeed10` has a column, the unit reports `wind_speed_avg_last_10_min` on both transports, and the Go driver does not map it — so that column is empty for this station type and populated for Davis. The Rust codec stores the device's own units, converts rainfall through `rain_size`, leaves rainfall `NULL` when `rain_size` is absent rather than storing a count (**D-26**), reads the indoor record, prefers the record type the role implies before falling back to the transmitter, and fills `windspeed10`. **Marked as needing acknowledgement** for DEV-7's reason: these are corrections and not decisions, and whether a stored series changes meaning is the owner's call. **The conformance suite cannot catch any of it** — it compares shape, type and formatting, not values — and nothing in this repository can tell whether any WeatherLink Live data exists in the production database. |
| **DEV-9** | The enrich stage computes `windchill`, `heatindex`, `aqi_pm25_aqin`, `aqi_pm10_aqin`, `potentialsolarwatts` and `snowdepth` **at ingest and stores them**, for every station rather than for the one driver that computed each; `windchill` and `heatindex` are `NULL` rather than the air temperature where their formula does not apply; and the clear-sky model reads the **station's** longitude instead of the **server's** time zone | **R1** (W3 §2.5), applied by implementation-plan.md **M3-3** | behavioural — **changes what five stored series mean**, and **needs the owner's acknowledgement** | **On the wire: one key changes, and it is `potentialsolarwatts`. In the database: five columns change meaning.** The distinction is the whole of this entry and the first draft of it got it wrong, so it is stated before the parts: the Go **read path** already computed three of these per request, so the JSON a client sees for them does not change — what changes is the *column*, which is what `/span`'s aggregate levels and every future query read. Taken in four parts. **(1) `windchill`/`heatindex` — the column changes, the JSON cannot.** The Go drivers store the *air temperature* where the formula does not apply (`utils.go:CalculateWindChill` returns `tempF` above 50 °F or below 3 mph); this stores `NULL`. The frozen JSON is unaffected **because the frozen handler never reads the column**: `transform.go:18` recomputes both from `OutTemp` plus wind speed or humidity on every request, which is exactly what `metrics/registry.toml`'s `legacy_read_time_computed` flag records and what `docs/metric-registry.md` §6.1 means by "the column and the JSON key are not guaranteed to be the same number". **So M4-2 must recompute them too, and this entry is the reason that is now a constraint rather than an open question** — reading the column and zero-filling it would produce `mean(windchill)` where Go produces `windchill(mean(outtemp), mean(windspeed))`, and those differ by more than rounding: for a bucket of two readings at (40 °F, 10 mph) and (60 °F, 10 mph), Go emits 46.04 and the column-mean path emits 33.64. Recorded as a constraint on M4-2 in the plan. The column itself improves either way: a stored `windchill == outtemp` is indistinguishable from a real wind chill and makes the column's `min` and `mean` the temperature's for every summer bucket. **(2) `potentialsolarwatts` — the one key that changes on the wire.** Only the Go **Campbell** driver computed it, so the column is NULL for every other station type and the frozen DTO zero-fills it; it is now computed for any station whose configuration has **coordinates** and whose reading carries **`outtemp` and `outhumidity`** (the model needs all three, so a station with coordinates and no humidity sensor still stores NULL). Where the Go daemon's own time zone is the station's — the production configuration, and the only one in which the Go behaviour is correct — the two agree to **1.1 × 10⁻¹² W/m²** over five zones × 53 days × 24 hours, measured against the Go function. They differ in two cases, both Go defects: a daemon whose zone is not the station's (`pkg/solar/asce.go` reads `time.Local`, and D-24 already says every calendar quantity is the station's), and any station in the eastern hemisphere, where Go's meridian table maps `+7` to 105° exactly as it maps `-7` and the offset doubles instead of cancelling. **(3) `snowdepth` — column only.** Computed per request in Go from a per-website cache (`restserver/controller.go:903`), so the **column** and every aggregate bucket are NULL today while the **response** already carries a number for any snow site with a configured base distance. The arithmetic is unchanged (`base_distance - snowdistance`, with Go's own `snowdistance > 0` guard) and it now reaches storage, which is what gets it into the aggregate levels. One consequence Go does not have: the Go snow-gauge driver discards a distance greater than the base at the device (`snowgauge/station.go:172`), so Go never stored a negative depth and this will — a mis-set base distance now contaminates `min`/`mean`/`max` buckets rather than leaving a gap. The enrich stage is the wrong layer to filter at (it is handed an already-admitted reading), so the discard is recorded as a constraint on **M6-1**'s snow-gauge driver in the plan. **(4) The two AQI keys — column only, and one naming oddity worth knowing.** The Go handler reads the column only when it is greater than zero and **computes otherwise** on both `/latest` and `/span` (`transform.go:209`, called per bucket at `:80`), so a client was never charting a flat zero here; the column is now populated from the same EPA table in `pkg/aqi/aqi.go`, so the handler's stored branch returns the number its computed branch would have. The oddity: `aqi_pm25_aqin` is derived from **`pm25`** (the outdoor sensor) and not from `pm25_in_aqin`, because that is what `transform.go:216` does. Faithful, and newly *stored* — so a station with only an outdoor particulate sensor and no AQIN device now has a permanently populated `aqi_pm25_aqin` column where Go left it NULL and carried the number only in the response. Every formula, constant, guard condition and `f32` rounding is asserted bit-for-bit against values produced by **running** the Go code at `c3ec9f7`. |
| **DEV-14** | The nine configuration values leave `/js/weather-app.js` and arrive as JSON: inline as `<script id="mw-config" type="application/json">`, plus an additive `GET /config.json` | **D-38**, as extended by A-3 and A-W3-3; applied by implementation-plan.md **M4-3** | behavioural — **changes the body of one coupled response and adds a block to another** | **Yes, and that is the point.** `text/template` has no JavaScript context awareness, so a station name containing an apostrophe broke the page and one containing `';alert(1);//` **executed**; the values now pass through a JSON encoder and never through a code generator. The affected response is `coupled` (**D-68**): server and website ship as one binary, so both halves moved in one commit. `contract/fixtures/asset-weather-app-js` therefore asserts three substrings that identify the module on **both** targets rather than four that identify the generated literal on one — the deviation mechanism cannot carry an assertion, only a recorded response (`contract/conformance/src/lib.rs:432`). What the suite stops checking is checked by `crates/mw-web`'s tests: the inline block's key set and order, its byte agreement with `/config.json`, and the byte diff against `html-weather-station`'s recorded body. `/latest` and the other four frozen endpoints are untouched. |
| **DEV-15** | A static asset carries no `Accept-Ranges`, so a range request gets the whole file | implementation-plan.md **M4-3**, as a consequence of serving the embedded tree from a handler rather than from `http.ServeContent` | behavioural — **removes a capability** | **No shipped client uses it.** Go's `ServeContent` advertises `Accept-Ranges: bytes` and honours `Range` on every asset; `mw_web::assets` writes the whole body. The tree is CSS, JavaScript, fonts and three icons, the largest of which is 125 kB, and no fixture asserts the header. Recorded rather than left to be discovered, because it is a byte a client *could* read. Re-adding it is a `Range` parse and a slice, and the reason not to is that a partial-content path with no consumer is a code path nothing exercises. |
| **DEV-16** | The five remaining drivers store **NULL** where the Go drivers store `0.0`, and four of them read a value Go reads wrongly or not at all | implementation-plan.md **M6-1**, under **D-3** and **D-26** | behavioural — **changes what the columns of five station types mean**, and **needs the owner's acknowledgement** | **On the wire: nothing. In the database: a great many columns.** The distinction is the whole of this entry and it is the same one DEV-9 draws. Every absent float on the frozen surface is zero-filled one layer later by `mw_api_legacy::zf` (§1.7), so `/latest` and `/span` are byte-identical; what changes is the **column**, which is what every aggregate level and every future query read. Taken in seven parts. **(a) A sensor that is not connected is NULL.** Each of the Go drivers writes zero for an absent reading, by a different mechanism: Davis's eleven `conv*` helpers return `0` for the console's dashed value (`davis/station.go:688-771`), Campbell's and AirGradient's response structs are non-pointer `float32` so a key the device omits stays at its zero value, and Ambient's hundred-and-thirty `parseFloat` calls leave their target untouched on absence. The symptom is the same in all four: a Vantage with no leaf/soil station stores a tidy row of zeros, so `min(soiltemp1)` over a month is 0 °F rather than NULL, and an Ambient console with no indoor sensor stores `intemp = 0.0` — which is 0 °F — on every upload. **(b) Davis: the specification's dashed value, not only the one Go tests for.** The dashed value of a 16-bit temperature is `32767`; `convBigVal10` tests `-32768`. So a Vantage whose outdoor transmitter is out of range stores **3276.7 °F** under Go, and that enters `max(outtemp)` and every aggregate above it. Both values are refused here. **(c) Davis: a LOOP2 record is refused rather than decoded as a LOOP record.** The two share the header and the length and have different layouts; the Go driver's protection is that it never sends `LPS`, which is a property of a different file. **(d) Campbell: `wind_d` is read as a number.** The Go struct types it `uint16`, so one fractional degree fails `json.Unmarshal`, which returns an error from `ParseCampbellScientificPackets`, which closes the connection — the station is down until the datalogger's next whole-numbered output. **(e) AirGradient: the uncompensated field is read when the compensated one is absent.** Go reads only `atmpCompensated`, so an older firmware that reports `atmp` alone makes it store `celsiusToFahrenheit(0.0)` = **32 °F**, every three seconds, for a monitor that is working. This is the one part of this entry that is a **new behaviour** rather than a NULL: the raw field is the same sensor less one firmware-side correction, which is a better answer than both 32 °F and nothing. **(f) The snow gauge reports its distance when no base distance is configured.** `BaseSnowDistance` is a non-nullable `float64` in Go, so an unconfigured gauge has a base of zero and `reading.Distance > 0` discards **every** reading — a gauge that reports nothing until somebody notices. The column here is nullable with a `CHECK (> 0)`, so "not configured" is expressible: the distance is stored and `mw_pipeline::enrich` derives no depth from it, because it has no base either. **(g) Davis: wind direction `0` is absent, not due north.** The Vantage serial protocol gives the field the range **1 to 360** and reserves `0` for "no wind direction data", which is a second way of saying nothing in the one field that has two — `0x7FFF` being the first. `convWindDir` stores it as a direction, so a vane that is not reporting reads as 0°, which is due north, and enters `min(winddir)` and every wind rose; 360 is north too, so the two are indistinguishable downstream. Refused here. The cost is measurable rather than hypothetical: the vendored `cmd/davis-emulator` draws `uint16(rand.Float64() * 360)`, so about one packet in three hundred and sixty now has an absent direction where Go stored `0`. **Every part is asserted against the Go emulators at the pinned commit** — `tests/emulators/manifest.toml` — rather than reasoned about, except (g), whose assertion is a unit test over a constructed record because the emulator produces the value only by chance. |
| **DEV-17** | `sunrise` and `sunset` are **evaluated per request** rather than read from the stored `sun_times` table. The arithmetic does not change; three cases do | **W2a D-36**, on the strength of the check its OQ-B required: implementation-plan.md **M5-3** (N-10). Recorded by **M5-4**; applied when `migrations/config/` and `mw_api_legacy::sun` were written | behavioural — **changes two frozen-contract string keys in three enumerated cases**, and **needs the owner's acknowledgement** | **Not in the ordinary case, and the ordinary case is asserted rather than argued.** `crates/mw-api-legacy/tests/sun_times.rs`'s `the_served_strings_are_the_stored_go_tables` replays the six `/latest` fixtures that carry the two keys and requires the per-request evaluation to reproduce the string the Go daemon served from its table — 6 of 6, byte for byte. Its reach is stated there and not widened here: six captures are **four** distinct (station, instant) pairs at **one** day-of-year key, not the 366-key table. `sun::sunrise_sunset` ports `pkg/solar.CalculateSunriseSunset`, which is the function `controller.go:118` handed to `PopulateSunTimes` to fill the table in the first place, so the two sides are the same formula and a difference can only come from the table being stale — or from never having been written. **(a) The "no position" predicate differs, and it cuts both ways.** Go's is `device.Latitude == 0 && device.Longitude == 0` (`controller.go:108`); ours is *both coordinates present* (`StationMeta::coordinates` is `latitude.zip(longitude)`, `crates/mw-component/src/ingress.rs:524`). Two instances, and the config schema permits both because `latitude` and `longitude` are independently nullable with no both-or-neither constraint. At **exactly `0, 0`** Go skips the station before `PopulateSunTimes`, `GetSunTimes` meets `sql.ErrNoRows` and returns `(-1, -1)`, and `handlers.go:370`'s `sunrise >= 0 && sunset >= 0` guard leaves both strings empty for `types.go:136`'s `omitempty` to drop — while here the formula has an input and answers with a 6-hour hour angle, so the station **gains** both keys. This is the rig's `nolocation` station, pinned by `the_station_with_no_location_is_a_divergence_from_go` — which also records that the divergence is **unobservable on the frozen surface today**, because that station has no readings and its `/latest` therefore 404s. It is a served value that changes only once the station reports. With **one** coordinate NULL it runs the other way: `provider_sqlite.go:504` leaves `device.Latitude` at `0.0` when the column is NULL, so `lat = NULL, lon = -105.5` fails Go's predicate and gets a table populated at latitude zero, while `zip` yields `None` here and both keys are **dropped**. This is the one case where we omit a key Go serves, and the key Go serves is computed from a latitude the station does not have. With **both** NULL the keys are absent either way — here because there is nothing to evaluate, in Go because two NULLs scan to `0.0` and meet the same predicate as a genuine `0, 0`. **(b) A coordinate edit now reaches the served value.** `controller.go:111` populates only when `HasSunTimes` is false, which is `COUNT(*) = 0` (`provider_sqlite.go:2296`), and `DeleteSunTimes` has **no caller** in the Go tree — the interface method, the SQLite implementation and the `CachedConfigProvider` pass-through are its only three references. So a station moved after its first start serves its old location's sunrise forever. **(c) The values stop being frozen at the year the table was populated.** The equation of time is evaluated at `time.Date(time.Now().Year(), ...)` (`sunrise.go:48`) and the row is then never rewritten, while the day-of-year key carries no year. Measured over the rig's seven stations, both limbs, all 366 keys: a table populated in 2026 and **read in 2027** disagrees with the evaluation in **272 of 5124** values, and **546 of 5124** read in 2028, by one minute each. Two qualifiers travel with those figures. The denominator is 7 × 366 × 2 and therefore includes `nolocation`, the station case (a) establishes Go never tabulates at all: 732 of the 5124 describe a table that does not exist, so each count is an **upper bound** on values Go could have served. And **neither count is asserted** — `sun::sunrise_sunset` takes no year argument, so a test cannot vary the reference year; they were measured by re-running the same expression with that year parameterised, exactly as M5-3's module documentation states. What is asserted is the code-level fact they follow from. Of the three, (b) and (c) are the stored table being stale; in (a) the row was never written at all, which is a difference of predicate rather than of freshness. **What this deviation does not change, and it is the larger number:** the ported formula puts the sun's centre on the *geometric* horizon and carries no refraction or semidiameter term (`sunrise.go:25` is `cosH = -tan(φ)tan(δ)`, zenith exactly 90°), so the served value sits **2–7 min late at sunrise and 1–8 min early at sunset** against a textbook evaluation at `h0 = −0.8333°` — mean absolute difference **4.52 min** over 102 270 comparisons, measured by M5-3 and asserted as a characterisation by `every_configured_station_diverges_from_a_fresh_meeus_evaluation`. That is D-37 working as specified: the frozen contract preserves the Go arithmetic, bug included. Deleting the table neither causes nor cures it, and correcting it would move every shipped client's displayed time by about four and a half minutes — a **separate** decision, and not one M5-4 takes. |


**DEV-4 answers W9's OQ-W9-4 in the affirmative**, which is what W9 asked W6 to decide. It belongs
here rather than only in `docs/data-migration.md` for the reason W9 gave for rejecting its own option
(c): during the side-by-side validation period in the implementation plan, an unrecorded value diff
on two frozen-contract keys looks exactly like a migration defect. Recorded as a consequence of a
merged decision (D-14), not as a deviation W9 introduced — W9's framing was correct.

**DEV-7 is in this ledger for DEV-4's reason, several orders of magnitude louder.** DEV-4 records a
6th-significant-figure change on `/span` precisely because "an unrecorded value diff on two
frozen-contract keys looks exactly like a migration defect" during side-by-side validation. DEV-7 is
the same endpoint and the same class of change at 30× to 43,200×, so it cannot be left to be
discovered as a diff.

It is also the one entry here whose *premise* a work item got wrong rather than decided. M1-6's
brief stated that "`solarjoules` is not on the frozen surface"; it is — `contract/legacy-v1.json`
lists it on `/latest` and `/span/{span}` in the Go response type, the Swift response type and the
client crosswalk, and `contract/fixtures/span-1h-one-minute-buckets.json` holds 37 non-zero values
for it. The physics behind N-9's conclusion is unaffected: an average of per-reading energy
increments is not a quantity anyone wants, and `avg(solarjoules)` was carried forward from the Go
views without the question ever being asked (C-W2-26). But "not on the frozen surface" was the
reason the item was sized as a one-line registry edit needing no record, and that reason does not
hold. **Marked as needing the owner's acknowledgement**, like DEV-2, DEV-3 and DEV-6: the aggregation
is an agent's technical judgement, and whether a chart's numbers may change by three orders of
magnitude is not.

**DEV-8 is a *driver* deviation rather than a read-path one, which is why it reads differently from
the seven above it.** Nothing about the frozen contract changes: the keys, the types and the formatting
are identical, and a client sees the same JSON shape it always did. What changes is the **number**
under `otemp`, `bar`, `dayrain` and `rainrate` for a station of one type — and in the direction of the
unit the registry, the contract and the other driver all already agree on. Three consequences worth
stating rather than discovering:

1. **Historic rows are not migrated.** M7-4's configuration migration and W9's data migration copy
   what is there; a `outtemp` column holding Celsius for some past range is not something either can
   detect, because 17.06 is a plausible Fahrenheit reading for a cold morning. If the owner has
   WeatherLink Live history, a chart spanning the cutover will step.
2. **It is invisible to every gate.** The `conformance` suite compares shape and formatting;
   `db-tests` has no WeatherLink Live in it; the emulator level asserts the Rust decoder against a
   Rust-adjacent oracle. The only check that could have caught the Go behaviour is the one this entry
   *is*: reading the registry's declared unit next to the driver's arithmetic.
3. **The rainfall case is not symmetrical with the others.** A temperature in the wrong unit is
   recoverable arithmetic. A rain total in counts is only recoverable if `rain_size` is known for the
   period, and nothing stored it — which is also why the Rust codec stores `NULL` rather than a count
   when the device omits it.

**One further behavioural change belongs with DEV-8 and is not a stored value: the sensor-mapping
parse refuses configurations the Go daemon starts with.** `config.stations.wll_sensor_mapping` is
validated at plan time, and a mapping that does not parse is a station that does not plan — which,
because `plan()` is a pure function the boot sequence aborts on, is a daemon that does not start. The
Go parser (`mapping_parser.go`) accepts all of the following and the transformer then silently has no
arm for them: an unknown role (`wnid:1`), a role with no transmitter where one is required (`wind`),
a transmitter on a role that has none (`baro:1`), a transmitter outside 1–8 (`th:0`, `th:99` —
`validation.go`'s range check is never called by `station.go`), a leaf/soil role with no port
(`soil_temp:2`), a `leaf_wet` port of 3 or 4 on a station with two leaf channels, a mis-cased option
(`thsw:1:apptemp`), trailing junk (`soil_temp:2:1:junk`), and the same sensor twice (`wind:1,
wind:1`). Each is now a plan error naming the station and the column.

**DEV-9 is the enrich stage, and the thing worth reading twice is which column it does *not* fill.**
W3 §2.5's table and implementation-plan.md M3-3's own text both read "solar potential/joules", which
looks like one derived pair. It is not. `metrics/registry.toml`'s entry for `solar_joules` settles it
in the opposite direction and names the file the question was addressed to: *"`pkg/solar` was the
file this question was addressed to and it is not the answer: it holds the clear-sky irradiance
models (ASCE, Bras, Ineichen-Perez) and the sunrise arithmetic, all in W/m², and never mentions
solarjoules. Nothing computes this value — the instrument reports it."* It is the Campbell
datalogger's `slr_mj`, carried through untouched. The registry is the source of truth for what a
metric *is* (**D-20**), so there is nothing to derive, and `mw_pipeline::enrich` deliberately leaves
it alone — recorded here as well as in that module, because the next reader of §2.5's table will
have the same question and the registry comment is three thousand lines away from it.

Two further things DEV-9 does not change, for the same reason: `sunrise`/`sunset` and the four lunar
values stay **where they are served** (W2a Decision 36 — they are functions of `(date, lat, lon)` and
not of a reading at all, so R1 does not reach them), and `storm_start` stays a station-reported
column, which is W3 §2.5's own consequence 1.

**One thing DEV-9 adds that Go does not have, because Go never stored these: a guard against values
that are not numbers.** Every formula here reaches its inputs through `Option<f32>`, and `NaN` passes
every guard condition in all of them — each comparison against a NaN is `false`, so a NaN
temperature satisfies "is it too warm for wind chill" *and* "is the sun below the horizon", and a NaN
particulate concentration falls past the whole EPA breakpoint table onto the "beyond the top band"
arm and scores **500, Hazardous**. Go has the identical hole and it costs nothing there, because Go
computes on the read path and the NaN lands in one response nobody keeps. Stored, one NaN in a
`float4` column poisons every `mean`, `min` and `max` bucket above it, permanently and
untraceably. So the stage returns `None` for an input or a result that is not finite, which is the
answer **W3 D-26** gives for anything that is not a measurement. This is a *departure* from the Go
arithmetic and the only one that is not a reproduction, which is why it is here rather than in a
source comment.

**DEV-9 is marked as needing the owner's acknowledgement for DEV-8's reason, stated there: "these are
corrections and not decisions, and whether a stored series changes meaning is the owner's call."**
Five columns change meaning. No gate can see any of it — the conformance suite compares shape, type
and formatting rather than values, and `db-tests` has no WeatherLink Live, no snow gauge and no
particulate sensor in it.

Validating at plan time rather than never is the improvement, and the cost is specific: **an
operator's existing mapping string can stop the daemon rather than degrading it.** That is the right
trade only because the configuration migration is a separate step — implementation-plan.md **M7-4**
owns it, and normalising these strings is its job, so it is recorded here as a constraint on that
item rather than left to be discovered at cutover. The alternative considered and not taken was to
demote the four classes that are pure no-ops in Go (`MissingTxid`, `UnexpectedTxid`, `MissingPort`,
`Duplicate`) to a counted warning that drops the entry; it was rejected because a station silently
reporting less than it should is the defect class this programme exists to remove, and because the
failure here is loud, at startup, and names the column to fix.

**What is *not* in this ledger, and why each is more interesting than it looks.**

- **The `/stationinfo` iOS decode hazard was the one entry that belonged here, and it is now above
  it** (W1 D-11, OQ-W1-1). A website with no about text or no bound device returned a `/stationinfo`
  body iOS could not decode **at all**. W1 recommended fixing it (option (b), two characters) as an
  explicit amendment, because it changes bytes and needs the owner's signature rather than an
  agent's judgement; the owner gave it, and implementation-plan.md M4-2 applied it as **DEV-10**.
  There is no longer an item in this programme where the frozen contract knowingly preserves a live
  user-visible failure.
- **The four unit and duplicate-field quirks are preserved, not fixed** (W1 D-12): `snowdepth` in
  millimetres on `/latest` and inches elsewhere; `ohum` and `outhumidity` carrying one value;
  `dayrain`/`rainday` likewise; `extrafloat1` meaning smoothed snow depth on `/span` for snow
  stations and PM1.0 on an AirGradient device. Not deviations — faithful reproduction. They are the
  v2 API's deprecation list, which is why **N-1** carries them.
- **Two latent Go defects are reproduced-or-fixed by accident of the new schema, not by decision.**
  W1-BUG-1 (`/span` is a 500 for any station with no non-NULL `winddir` in the window) is *fixed* by
  W4 D-8's zero-returning finaliser — which was chosen to preserve the contract, and closes the bug
  as a side effect. W1-BUG-2 (`/api/remote-stations` is a 500 for a row with any NULL text column)
  has no new-side counterpart because W7 retires the table (A-W7-2). Neither is a deviation; both are
  recorded so that "the 500 went away" is traceable to a decision rather than a mystery.
- **`format=msgpack` is kept** (W1 OQ-W1-5), because D-37 says frozen and removing a working query
  parameter is the exact class of change the freeze exists to prevent. A v2 deprecation candidate,
  not a deviation. **O-6**.
- **`/alerts` and `/api/remote-stations` are reproduced despite having no shipped consumer**
  (W1 D-13). "Frozen" is about the surface, not observed traffic, and a client outside these three
  repositories cannot be ruled out from here. **O-6** asks the owner for one line of access-log
  analysis.

**The honest summary for the owner: no shipped client breaks.** Of ten deviations, two are purely
additive and invisible to both clients by a verified property of the conformance runner; the
behavioural ones each close a defect rather than removing a response a client reads.

**O-5 is closed, so the sentence this paragraph used to end on is gone.** The one place a
user-visible failure *did* ship was the `/stationinfo` hazard the contract faithfully reproduced;
the owner answered option (b) and implementation-plan.md M4-2 applied it as **DEV-10**. Nothing in
this ledger now records a deviation that leaves a shipped client broken.

---

## 4. Work items raised by this document

Twenty — sixteen from the audit, and **four added 2026-10-31** from the owner's answers (§6, §7).
Each is a named item with an owner-facing one-liner; each appears in
`docs/implementation-plan.md` with dependencies, done-criteria and a test strategy. **None is a
redesign** — every one closes a gap §2, §6 or §7 found.

| | Item | Why it exists | Size |
|---|---|---|---|
| **N-1** | **The v2 API surface** | D-39 puts it out of design scope; W1, W3 and the master issue's pre-flight 4 all defer to "a later work item" that did not exist. Now it does. Starting inventory below. | design item |
| **N-2** | Update the Go provisioner for the new schema | D-40 keeps the provisioner in Go and says it "needs updating". It lives in the read-only reference repo and no work item owns the change. | small |
| **N-3** | Run the conformance suite in CI against the Go daemon | **G-1.** W1's "done when" required it; it has never run in CI, and W1 D-10's negative control has never run at all. | medium |
| **N-4** | The SQLite configuration schema and its migration mechanism | **G-2.** Nine-plus tables assumed by four documents, with no DDL and no migration path, against D-8. Resolves OQ-W5-4. | medium |
| **N-5** | Migrate the owner's existing configuration database | **G-2.** Includes W7's `remote_stations` retirement, dropping four plaintext secret columns, and `NodeId` allocation. | medium |
| **N-6** | `legacy_unit` / `legacy_scale` in the registry | OQ-W2-4: the `mm`→inch conversion with its 0.1-inch threshold is generated nowhere, so somebody hand-writes exactly the mapping lines W2 exists to retire. Deliberately *after* W1's behaviour is pinned. | small |
| **N-7** | A `[[unit]]` table validated by the generator | **C-W2-19.** 25 unit strings, nothing validating them; `degf` for `degF` reaches the UI as a rendered label. | small |
| **N-8** | Make the gate runner distinguish exit 1 from exit 2 | **C-W2-20.** W2 D-18 separated drift from an invariant violation so they could be reported differently; `gates::run` collapses both. | small |
| **N-9** | Settle `solarjoules` against `remoteweather:pkg/solar` | **C-W2-26.** OQ-W2-3 was addressed to W7, which never read it. One file read; one registry line if it is a total. | small |
| **N-10** | A fresh-evaluation assertion for `sunrise`/`sunset` | **C-W2a-27.** W2a D-36 deletes the `sun_times` table on a check that does not exist. Precondition on the deletion. | small |
| **N-11** | Surface W7's fleet observability in W5's pages | **C-W7-29.** Enrolment tokens and log, `spool_time_remaining_seconds`, the ceiling advisory, peer-reported depth labelled as such. | small |
| **N-12** | Emit proto3 `optional` on every metric field | **G-3.** Decided by three documents, implemented by none; without it a NULL becomes `0.0` across a hop *and* across a restart. | small |
| **N-13** | `affected_levels` compares rendered level definitions | **G-5** / OQ-W4-1. Ten lines; turns "add a metric to one level" from four level rebuilds into one. | small |
| **N-14** | Dimension `role = "metadata"` | **G-5** / OQ-W4-2 / OQ-W2-6. `[[dimension]]` has no deprecation path, so `stationname`/`stationtype` cannot leave the raw table at all. Prerequisite for C-W7-26. | small |
| **N-15** | `codegen` prunes files it did not write | **G-5** / OQ-W4-4. Today `codegen` leaves a tree `--check` rejects. | small |
| **N-16** | `rebuild-aggregates.sh` phase 1 uses the widest band above | **G-5** / OQ-W9-3. `--recent-only` currently caches a hole in `observations_1d` beyond 14 days. | small |
| **N-17** | **Run TimescaleDB in Kubernetes** | **§7.** No design document owns where the database runs. Single-replica StatefulSet on a `local` PV, which is also the migration's target — so the platform move and W9's migration are one operation. | medium |
| **N-18** | Classify each fixture frozen / coupled / retired | **§6.4.** Only five endpoints are frozen; the rest are coupled, not unconstrained. The suite keeps its breadth and the runner reports the two classes differently. | small |
| **N-19** | **Weekly off-site `pg_dump` to R2, and a CI restore rehearsal** | **§7.5.** D-6 names the mechanism and nothing owns running it; the owner has made it the condition of accepting a single replica. Raw + the legacy archive + `config.db`; **not** the aggregates (D-15). Per-year dumps, so the weekly job re-uploads ~4–5 GB rather than 40. | small |
| **N-20** | **Serial over TCP for the USB-attached Campbell station** | **§7.6.** The station is on USB serial and the database is moving into Kubernetes. Already supported on both sides — Go dials TCP when `SerialDevice` is empty, and W2a §4a.5 maps Campbell as `Serial \| TcpOut` — so this is `ser2net` plus a config change, and it does **not** block the database move. | small |
| **N-21** | **Suspend the aggregate retention jobs for the duration of a rebuild, and state the transient storage** | **M1-6 review of N-16.** Phase 1 now refreshes every level over the widest band above it, so it materialises ~8.8 GB into a 7-day level and ~1.8 GB into a 14-day one and runs for minutes. A retention job firing between the minute level's refresh and the 5-minute level's read of it hands the upper level a truncated source and caches the hole N-16 closed. **Not new** — phase 2 has always had the same exposure over unbounded history — but N-16 extends it to phase 1 and so to `--recent-only`. `alter_job(..., scheduled => false)` with a trap to restore it is the fix, and it has its own silent failure mode (retention left off after a hard kill), so it needs a database to land safely rather than a careful reading. | small |
| **N-22** | **Make W4's aggregate policy block re-assertable instead of hand-copied** | **M1-6 review of N-9.** `DROP MATERIALIZED VIEW ... CASCADE` takes the four composite indexes, three retention policies and four refresh policies with the level, and the generator does not know they exist — refresh policy, compression and retention are deliberately not generated (OQ-4). So every registry change that rewrites a level's column set needs a hand-written follow-up migration; M1-6 wrote the first one. `migrate` already re-asserts the finalisers into every migration for exactly this reason (`CREATE OR REPLACE FUNCTION mw_mean`), and the same treatment applied to the policy block removes the recurring step, the two-transaction window in which a deployed database has aggregates with no policies, and the hand-copying. Needs the policy parameters to have an owner in the registry or beside it. | medium |
| **N-23** | **`solarjoules` is declared `J_per_m2` and the instrument reports MJ/m²** | **M1-6 review of N-9.** The value is the Campbell logger's `slr_mj`, so the declared unit is out by 10⁶ — and N-7 has just made `label` the thing a UI renders, so the error now has a defined path to the screen. A one-line, non-breaking registry edit (`[[unit]] name = "MJ_per_m2"`, point `solar_joules` at it; no identifier changes, no migration). Deliberately **not** taken inside M1-6: DEV-7 already asks the owner to acknowledge one change to what this key means, and bundling a second decision about the same key into the same unreviewed change compounds them. The rendered unit is also W5's surface, not W2's. | small |
| **N-24** | **`migrate` emits `ALTER COLUMN ... TYPE` before the `DROP MATERIALIZED VIEW` block** | **M1-6 review.** PostgreSQL refuses to alter a column's type while a view depends on it, so `migrate --allow-breaking` on a SQL type change emits a migration that fails at the `ALTER` before reaching the drops that would have made it legal. The documented breaking path in `docs/metric-registry.md` §2.2 has therefore never been executable. Pre-existing and unrelated to M1-6's items; found while reviewing the block N-13 rewrote. Reorder to drops → `ALTER` → recreates, and add a test that asserts the order. | small |
| **N-25** | **A cumulative-widening phase 1, so the rebuild is correct *and* has a fast first pass** | **M1-6 review of N-16.** N-16 made phase 1 correct by widening every level to the widest band above it, which cost D-23's sub-second first phase. Refreshing all four levels bottom-up over each successive band in turn — 6 h, then 48 h, then 14 d, then 366 d — would satisfy both: no level is ever refreshed past the range its source has been materialised over, the first pass restores the sub-second property for the 6 h band, and each later pass adds one more correct band with a natural interruption point. **Not taken in M1-6** because its correctness rests on a daily bucket materialised at a pass frontier being recomputed by the next pass, i.e. on invalidation propagating from the hourly level upward; `docs/data-migration.md` §5.5 observed that happening, but if it does not hold in some case the first pass caches exactly the wrong daily bucket N-16 exists to prevent. That is a property to verify against a database, not to reason about. | small |

### N-1 — the v2 API, with its starting inventory

Recorded as a **named future work item** rather than a dangling reference, which is what D-39, W0
assumption 26, W1 assumption 14 and W3 assumption 60 each ask for. It is **not** scheduled in the
implementation plan: every milestone there leaves the Go version in production and the Rust version
independently verifiable, and a new API surface is the first thing that is neither. It starts after
cutover.

Its inventory is already assembled, from four documents:

*Routes the rewrite adds that are deliberately outside the frozen surface* (W3 assumption 60):
`/metrics` (Prometheus text), `/config.json` (the website config payload), `/api/v1/logs`, the
`X-MW-*` headers, and `POST /v1/ingest` (W7 D-30).

*A time-anchored read endpoint*, which has two independent named callers: W1 §2.1 and W3 both defer
it, and W9 assumption 24 notes it is **the only thing that would ever read
`observations_1d_legacy`** — the ~294 days of rollup archive. `observations_1d_with_legacy` is
already generated and ready (W9 D-6), costing a measured 37% more than the plain level. Without this
endpoint the archive is carried and unreadable, which is the recorded trade.

*A deprecation list*, which is the useful half:

| Candidate | Source | Note |
|---|---|---|
| `format=msgpack` | W1 OQ-W1-5 | implemented on four endpoints, skipped on `/span`, loses number types on `/forecast`, no shipped client sends it |
| `/alerts`, `/api/remote-stations` | W1 D-13, OQ-W1-3 | no working in-repo client; **O-6** asks the owner for access-log evidence first |
| `ohum`/`outhumidity`, `dayrain`/`rainday` | W1 D-12 | one metric under two keys, twice |
| `snowdepth`'s unit split | W1 D-12, Q-4 | millimetres on `/latest`, inches elsewhere |
| `extrafloat1`'s overloading | W1 D-12 | smoothed snow depth *or* PM1.0 depending on the device |
| `/stationinfo`'s `omitempty` on `about_text` / `weather_device` | W1 OQ-W1-1 | if **O-5** chooses option (c), a client release makes both optional and the hazard closes |
| the four read-time-computed keys | W2 D-29 | `windch`, `heatidx`, `aqi_pm25_aqin`, `aqi_pm10_aqin` — now stored at ingest (W3 D-23), so v2 can serve the column directly |

---

## 5. Open questions for the owner

> ### Owner answers — 2026-10-31
>
> **chris answered eight of the twelve** in one pass. Recorded here rather than only in the issue
> thread, because this file is what a later work item reads.
>
> | | Question | Answer |
> |---|---|---|
> | **O-5** | The `/stationinfo` iOS decode hazard | **Option (b)** — drop `omitempty` from `about_text`, emit `0` for an unset `weather_device`. Concurred. Higher stakes than this document recorded: `/stationinfo` is one of the **five** endpoints iOS actually calls. **Applied** by implementation-plan.md M4-2 as **DEV-10**, with the Rust target's response recorded on the two affected fixtures beside the Go server's |
> | **O-6** | Which legacy surface is dead | **Answered structurally; the access-log query is no longer needed.** Only the endpoints iOS calls are frozen — see §6. `/alerts` and `/api/remote-stations` have no iOS *or* website consumer, and no shipped client sends `format=msgpack` |
> | **O-7** | The history question | **Option B** — import the archive. Concurred |
> | **O-8** | RF beacon floor | **600 s.** Concurred. N-5's config migration raises a sub-600 s KISS device with a one-line report rather than failing the parse |
> | **O-9** | Is a Pi Zero in use? | **Retain the broadest hardware support available.** All four Linux targets stay. Consequence, stated plainly: with no ARM1176 hardware to test on, the qemu run under `-cpu arm1176` **is** the validation — which is what W8 D-25 already decided ("the ARMv6 verdict is the qemu run, not the build and not the ELF attribute"), so this needs no new hardware |
> | **O-10** | Four admin-surface calls | **All four accepted** as recommended |
> | **O-11** | Which `chonky` gaps to upstream | **All twelve are fair game.** The four-now/two-later split below was about *sequencing* — proving `AppShell` and `CopyField`'s shape in use before fixing their API — not about permission |
> | **O-12** | Process finding | Noted |
>
> **Still open: O-1, O-2, O-3, O-4** — the four production queries, and see the correction under
> Group A: **pitcairn is reachable from an agent runtime after all**, so three of the four are a
> credential handoff rather than an owner chore.



**Twelve from this pass, plus **O-13** raised by implementation-plan.md M3-1 and **O-14** by M6-1,
consolidated and
deduplicated from sixty-two open questions across ten documents.** Fifty
are closed: forty-one were answered by a later document (W4 alone closed OQ-3, OQ-4, OQ-5, OQ-W1-2,
OQ-W2-1 and OQ-W2-6; W3 closed its own ten plus OQ-D and W8's OQ-1 and OQ-2), four were resolved in
§1 and §2 of this document (OQ-W4-6, OQ-W5-4, OQ-C, OQ-W9-4), and five became work items in §4
because they are decided and merely unowned (OQ-W2-3, -4, -5, OQ-W4-1, -4). The four that were open
when the prompt set was drafted — registry format, `weather-app.js.tmpl`, UI technology, dead Go
tools — are all settled and are **not** reopened here.

**The remaining twelve fit in a dozen, which is the finding.** Ten independent agents writing 16,000
lines of design left twelve things for a human, four of which are database queries. That is the
system working.

### Group A — ANSWERED 2026-10-31. All four, measured against production.

> **The owner fixed `pg_hba.conf` and the queries ran.** Every figure below is measured, not
> extrapolated. **Two of the four premises these questions rested on turned out to be wrong**, and one
> of those is a data loss that had already happened. See §8.

| | Question | Measured answer |
|---|---|---|
| **O-1** | Production's PostgreSQL / TimescaleDB version | **PostgreSQL 16.6, TimescaleDB 2.17.2.** Not PostgreSQL 14 — the `docker-compose` guess was wrong in the harmless direction. 2.17.2 is **above 2.9**, so the four-level hierarchy is constructible and W4 §5.2's contingency does not fire. The new instance is PG 17 + 2.31 (D-2), reached by W9's logical export/import, so there is no in-place upgrade |
| **O-2** | Is raw retention firing? | **Yes — and it was 4 h 55 m from the next run.** Job **1004**, `drop_after = 365 days`, daily. **Now paused** (`scheduled = f`). There were **five** retention policies, not one: 1004 on raw plus 1005–1008 on the four aggregates (1 month / 6 months / 2 years / 10 years). Only 1004 was touched |
| **O-3** | The compression hold rate | **59.6%** over 322,346 sampled pairs — **well below** the ~87% the planning documents assumed and below the 76.87% W4 measured in its own rig. Production also has **zero compressed chunks**: compression was never enabled there, so its 19 GB is uncompressed |
| **O-4** | The ~72-column gap | **There is no gap.** Production's `weather` has **215 columns** — exactly the Go source definition the registry was bootstrapped from. The "~143 columns" in `docs/database-schema.md` §0 was an assumption, never a measurement. 215 + `station_id` = the registry's 216 allocated identifiers, which is a perfect match, so W9 D-8's run-time `expected ∩ source` intersection will match everything |

**Consequences for figures already in these documents:**

- **W4 D-7's 5.3× compression is optimistic for this data.** It was measured at a 76.87% hold rate;
  production is 59.6%, and W0's spike established that the ratio is driven almost entirely by that
  rate (1.89× to 13.74× on the same schema). Expect the lower part of that range. D-10 already says
  no decision depends on the ratio — that still holds — but **every capacity figure derived from
  5.3× should be treated as a floor on size rather than an estimate**.
- **§7.3's “~36 GB for a decade” is revised upward to roughly 50–60 GB.** Same reasoning. It changes
  nothing about the hardware recommendation: it is still trivial against any NVMe.
- **The new system will still be smaller than production for the same data**, because production has
  no compression at all.

### Group A — the original four questions, kept for their reasoning

These four have been outstanding for the whole programme, and the first one is losing data every day
it stays open.

> **Corrected 2026-10-31 — the blocker is credentials, not reachability.** The programme has recorded
> since W0 OQ-1 that "production is not reachable from an agent runtime", and that premise is wrong at
> the network layer. `pitcairn` (10.50.0.35) is on the same /24 as the Kubernetes cluster
> (10.50.0.51–56) and **port 5432 accepts a TCP connection from an agent runtime** — verified by
> connecting to the port and doing nothing else. No authentication was attempted, and none should be
> without being asked for.
>
> So **O-1, O-3 and O-4 are one read-only credential away from being agent work** rather than three
> separate owner chores. **O-2 is not, and should not be:** it writes to production and then disables
> a policy, which belongs on the owner's hand.
>
> W0's conclusion was reasonable when written — the spike's `Do not` list forbade touching production
> at all, so nobody tested the premise. It is recorded as a correction rather than quietly fixed,
> because three open questions sat on it for the whole programme.

**O-1. What PostgreSQL and TimescaleDB version does production run?** *(W0 OQ-1; W9 OQ-W9-2's
version half)*

*Why it matters.* Three spike answers are version-gated, and W4 §5.2 lists the decisions that move.
The only version reference anywhere in the Go repository is a `docker-compose` example naming a
PostgreSQL 14 image, which is documentation rather than evidence. **Below TimescaleDB 2.9 the
four-level hierarchy cannot be built at all**, which makes a major-version upgrade part of W9 rather
than an option.

*Options.* None — it is a fact. *Recommendation:* run the two queries in `docs/spike-findings.md` Q1.
One minute of work that can change the shape of a work item.

**O-2. Is the production 365-day raw retention policy paused, and was the archive taken first?**
*(master-issue pre-flight 1; W9 OQ-W9-2's policy half; W9 assumption 25)*

*Why it matters.* **This is the only item on this list that gets worse with time.** Every day the
policy fires, one more day of raw readings becomes rollup-only history, permanently — the ~294-day
problem, still growing. It is still unconfirmed.

*W9 corrected the ordering, and the correction matters:* **archive first, then pause.** Pausing is
not what protects the aggregate buckets. W9 D-1 measured that `drop_chunks` on the raw hypertable
logs an invalidation over the dropped range, so the *next ordinary scheduled refresh* of any level
over that range deletes the buckets whose raw has gone — a manual refresh removed 21 of 29 daily
buckets and one policy run removed 41 of 83, the destroyed range matching the dropped-chunk range to
the day. So the rollup history is at risk from a *refresh*, not only from retention, and the archive
copy is correct under every answer to this question. `docs/rewrite/README.md` pre-flight item 1 is
corrected in this PR.

*Recommendation:* take `migrate-from-go.sh archive` (~3 MB, ~3 seconds, W9 §2) **now**, before
reading any query output; then `SELECT remove_retention_policy('weather');` or a verified `pg_dump`.

**O-3. What is production's real compression hold rate?** *(W0 OQ-2)*

*Why it matters.* Measured 1.89× to 13.74× on the same schema, with the spread **fully explained by
the consecutive-identical rate**. A 10–12× estimate has already propagated into two planning
documents. No decision depends on it (D-10); the capacity numbers do.

*Recommendation:* run the hold-rate query from `docs/spike-findings.md` Q3 against a week of
production readings — that alone brackets the answer. Note that W9 D-21 deliberately quotes **no**
figure from its lab, whose hold rate was 3.1% against production's expected ~87%.

**O-4. Does anything depend on the ~72-column gap between production's ~143 columns and the Go
source's 215-column definition?** *(W0 OQ-6; W2 assumption 24; W9 assumption)*

*Why it matters.* Columns exist in the source and not in the database, so any code path writing one
is failing or silently not writing — and the registry was bootstrapped from the *source*, which is
the wider of the two disagreeing candidates.

*Already mitigated, which lowers the urgency without closing the question.* W9 D-8 computes the
import's column list at run time as `w9.expected_column INTERSECT w9.source_column`, proved by a
second lab run at 143 columns, so **the migration is correct at either width**. W9 D-10 additionally
stops the migration on a source column with no registry entry. What is still owed is the *report*:
which of the 212 metrics have never had a column behind them.

*Recommendation:* one `information_schema.columns` query against production, diffed against
`metrics/registry.toml`. Nothing is blocked on it; it is the difference between a deliberate superset
(W2 D-23's position) and an unexamined one.

### Group B — eight decisions.

**O-5. Fix the `/stationinfo` iOS decode hazard, or keep reproducing it?** *(W1 OQ-W1-1)*

*Why it matters.* **This is the only place in the programme where the freeze knowingly preserves a
live user-visible failure.** A website with no about text or no bound device returns a `/stationinfo`
body the iOS app cannot decode **at all** — it loses the station list, the snow device, the
air-quality device and the radar token, not just the missing field. Any portal website is permanently
in this state.

*Options.* **(a)** Reproduce it exactly; iOS keeps breaking on those websites. **(b)** Drop
`omitempty` from `about_text` and make `weather_device` emit `0` when unset — strictly additive from
iOS's point of view, since a present key can only help a non-optional property, and the website
guards both reads so it is unaffected. **(c)** Ship an iOS release making both properties optional,
after which (a) is harmless.

*Recommendation:* **(b)**, as an explicit amendment with the owner's signature and a new fixture. It
is a two-character schema change, it cannot break the tolerant client, and it fixes a real failure
without waiting on App Store review. This is W1's own recommendation; it needs a signature because
it is a deliberate deviation from "reproduce what shipped", not an agent's call.

> **Answered (b) and applied**, by implementation-plan.md M4-2 as **DEV-10** in §3's ledger.
>
> "A new fixture" was read as *a fixture that records the new response*, and the record was put on
> the two existing ones rather than beside them. The reason is that the **same 70 fixtures are
> replayed against both targets**: `ci/conformance/go-target.sh` points the runner at the Go daemon,
> which still omits both keys, so a fixture edited in place would turn that gate red for asserting
> what actually ships. Each affected fixture therefore carries a `deviation` block with an id, and
> the runner takes `--deviation DEV-10` — which `cargo xtask conformance` passes and `go-target.sh`
> does not. An id no fixture declares is a refusal, so the flag cannot silently enable nothing.
>
> Adding a 71st fixture was the other reading and was not taken: the deviation changes the response
> for cases the suite already covers, so a new file would record a request that is already there,
> and the runner's `attempted == 70` assertion — which is what stops the classification being a
> silent filter — would have had to move with it.

**O-6. Which of the legacy surface is actually dead?** *(W1 OQ-W1-3 and OQ-W1-5)*

*Why it matters.* `/alerts` and `/api/remote-stations` have no working in-repo client —
`/api/remote-stations`' only reference is a management-UI fetch against a server with no such route —
and `format=msgpack` is implemented on four endpoints, skipped on `/span`, loses number types on
`/forecast`, and no shipped client sends it. All three are being carried forward forever on the
possibility of a consumer outside these three repositories.

*Options.* **(a)** Carry all three indefinitely (what ships today, under D-37). **(b)** Settle it with
evidence and move the genuinely dead ones onto the v2 deprecation list.

*Recommendation:* **(b)**, and the evidence is **one line of access-log analysis on production** — a
single query answers all three at once, which is why they are one question here rather than two.
Until then the default must be to keep them: the cost of being wrong is a broken third-party client.
If they are dead, **N-1** already carries them as the v2 deprecation list's first entries.

**O-7. The history question: delete ~294 days of rollup, or carry it?** *(W9 OQ-W9-1 — W9's own
"the one question only the owner can answer")*

*Why it matters.* Option A permanently deletes the only record of ~294 days of daily and hourly
rollup, and no endpoint notices. Option B keeps it for **~2.5 MB plus ~58 MB** at production scale,
with no read-path change, and nothing can read it until a time-anchored endpoint exists (**N-1**).

*Recommendation:* **Option B**, and — separately and immediately — **take the §2 archive copy
regardless**, because that decouples this decision from any deadline. The cost is nearly nothing and
the loss is irreversible. W9 D-7 sized it; `w9.legacy_boundary` and `w9.assert_legacy_disjoint()`
already keep the two sides non-overlapping so a future union cannot double-count a bucket.

**O-8. Is the RF beacon floor 600 s or 300 s?** *(W2a OQ-A)*

*Why it matters.* **This is the one number in the design that changes existing behaviour.** W2a D-60
makes the KISS floor a *type-level* constraint at 600 s — `RadioInterval`'s only constructor rejects
anything lower — so an existing KISS user configured at today's 300 s default would get a config
**parse error** on upgrade rather than a warning.

*Options.* **(a)** 600 s, with N-5's config migration raising a sub-600 s KISS device to 600 s and
reporting it in one line, rather than failing the parse on first boot. **(b)** 300 s, which is one
constant; the type-level mechanism is unchanged either way.

*Recommendation:* **(a).** Ten minutes is ordinary practice for an RF weather beacon on a shared
band, it matches the 10-minute gust window the CWOP encoding already uses, and the asymmetry
argument — clamp-and-log for APRS-IS and HTTP, which have a feedback channel; a hard floor for a
radio, which does not — only works if the floor is actually conservative.

**O-9. Is a Raspberry Pi Zero or Pi 1 actually in use?** *(W8 OQ-6)*

*Why it matters.* ARMv6 is the only target that needed a special answer — a glibc cross build
produces an ARMv7-tagged binary that dies with SIGILL on an ARM1176, and only the static musl build
runs. It works, so this is not a technical blocker. The answer decides whether the real-hardware
validation in `docs/release.md` A.3 is needed **before the first release, or at all**.

*Recommendation:* **keep the target** — it works and it costs one matrix entry — but answer this
before the first release, because it is the difference between a validated artefact and an untested
published one. The same answer tells W8 whether `ci/spike/armv6-cross-spike.sh`'s qemu run is
sufficient evidence.

**O-10. Confirm or override W5's four admin-surface recommendations.** *(W5 OQ-W5-1, -2, -3, -5)*

Four small calls on one surface, each with a recommendation and none of them load-bearing on anything
else. Grouped because they are one review, not four.

| | Question | W5's recommendation |
|---|---|---|
| OQ-W5-1 | Does the management API bind loopback until the instance is claimed? | **(a) yes**, widening after. The setup code already closes the takeover hazard; loopback closes it twice, and the escape hatches (`--setup-listen`, an SSH tunnel) are one command each and printed in the banner |
| OQ-W5-2 | Check new passwords against a breached-password list? | **(b)** ship a ~10k-entry list (~80 KB in the binary) as a non-blocking **warning**. Works offline; HaveIBeenPwned's API is out — an outbound call from a tool that must work offline, leaking a password prefix |
| OQ-W5-3 | Any API token at all in v1? | **(a) none.** Nothing needs one; the iOS app uses the unauthenticated public API. §3.7 fixes the shape in advance so adding one later cannot regress into "the cookie is the token" |
| OQ-W5-5 | How much configuration history? | **(a) one step** now (`/api/v1/config/revert`, within the session — what the §2.4 failure banner needs), **(b) as a named follow-up.** The generation counter already exists, so it is additive |

*Recommendation:* accept all four. The only one worth a second look is OQ-W5-1, because it is the one
that can inconvenience a headless remote install.

**O-11. Which `chonky` gaps should be upstreamed?** *(W5 OQ-W5-6)*

*Why it matters.* It is the owner's toolkit, and §6.3 found twelve gaps while mapping every screen
onto it. Keeping all twelve local means twelve local components in a project whose point was to use
the toolkit; upstreaming all twelve is work on another repository.

*Recommendation:* **upstream G2 (`Dot` tones), G4 (consistent `error` on form controls, or a `Field`
wrapper), G9 (document the Inconsolata requirement) and G10 (fifteen more icons).** G4 and G2 are the
two every consumer hits; G9 costs a README paragraph and prevents a silent font degradation; G10 is a
three-step change `icons.ts` already documents. `AppShell` (G1) and `CopyField` (G6) are good
candidates once this project has proved their shape in use — keep them local for now.

**O-12. Process, not product: a merged document cannot answer a request.** *(raised by this pass —
C-W3-2)*

*Why it matters.* Six of the 49 failing assumptions are a downstream document asking an upstream one
for a small extension — `ProvisionDetail::Routes`, `ComponentControl`'s log level and
`applied_generation`, `Ingress::accept`, an ordered `path`, `NotFrom(peer)`. Every request was phrased
carefully ("taken, not asked"; "a request, not a unilateral change") and every one arrived at a
document that was already merged. The "stay in your lane, surface don't resolve" instruction is
correct for *producing* documents — it is why the other 280 assumptions hold — and it has **no return
path**. All six are resolved in §1 on the merits, but they were resolved by a reconciliation pass
weeks later rather than by the person who knew the model.

*Options.* **(a)** Nothing; W6 catches them, as it did. **(b)** For a future programme of this shape,
a document's author is nominated as its owner for the programme's duration, and a downstream request
against a merged document is a comment on that document's issue rather than an assumption in a
closing section. **(c)** A lightweight amendment protocol *between* work items, mirroring the one
`DESIGN.md` already has.

*Recommendation:* **(b)**, and only if there is a next programme of this size. (a) is defensible — the
mechanism did catch all six, and none was expensive. The reason to prefer (b) is that W6 resolved them
by reading two documents, and the author of the model would have resolved them by knowing it.

**O-13. The first station driver's emulator is not independently written, and D-40 says it should
be.** *(raised by implementation-plan.md M3-1)*

*Why it matters.* **D-40** keeps the four Go emulators in Go deliberately — "a driver tested against a
simulator written in the same language from the same understanding tests that understanding against
itself" — and **A-W8-4** adds that they are consumed at a pinned commit of the reference repository.
The reference repository at the pinned commit has **no WeatherLink Live emulator**: its four are
`cmd/davis-emulator`, `cmd/campbell-emulator`, `cmd/weather-station-simulator` (Davis on TCP 22222,
Campbell on TCP 7000) and `cmd/live-data-simulator` (Campbell packets from live data). It is read-only
to this programme, so one cannot be added there.

M3-1 is the item whose done-criteria arm the `driver-emulators` gate (**D-64**), and WeatherLink Live
is the only driver that exists, so the choice was between a gate that runs nothing — **G-1**, the
defect this whole mechanism exists against — and an emulator written in this repository. M3-1 wrote
one, in Go, at `tests/emulators/wll`, and recorded the trade in `tests/emulators/manifest.toml`:

- **kept** — a separate process, a different language, real sockets, the real bind, and a driver
  exercised for more than one batch. Four of D-40's five properties.
- **lost** — independent authorship, which is the one the rationale is actually about. It was written
  from the same reading of the published API as the driver, so it cannot catch a misunderstanding the
  two share.

*Options.* **(a)** Accept it, and let each driver's emulator live here as it arrives. **(b)** A
`cmd/weatherlink-live-emulator` in the reference tree, written against the vendor documentation by
somebody who is not writing the driver, with the pin bumped to the commit that adds it; the manifest
entry's `origin` then becomes `"reference"` and the local copy is deleted. **(c)** Narrow D-40 by
amendment to the drivers the reference tree actually covers, and say explicitly that a driver with no
upstream emulator gets a local one — which is what the code does today, unamended.

*Recommendation:* **(b)** for WeatherLink Live, because it is the owner's own station type and the one
whose stored values DEV-8 changes, and **(c)** as the standing rule so that the next driver does not
re-raise this. (a) is the status quo and is the weakest: it leaves D-40's decision table row saying
something the tree does not do. Note that the gate is honest either way — it runs five real tests
against a real second process, binds a declared test set by name, and prints the authorship caveat on
every run — so this is a question about the strength of an oracle and not about whether anything is
checked.

**O-14. Does the registry gain a `jsonb` type, so that `observations.extra` can be a registry
entry?** *(OQ-W4-3, raised at the moment W4 asked for by implementation-plan.md M6-1)*

*Why it matters, and why now.* W4 left this open with an explicit trigger: it is *"worth deciding
before W7 writes its first driver that receives an unknown field"*. **M6-1 is that moment.** Two of
the six drivers it adds receive fields nobody can enumerate in advance:

- **Campbell Scientific.** The datalogger is programmed in CRBasic by whoever installed it, and what
  it emits is whatever that program writes. `mw_codec::CampbellAscii` maps **ten** keys — the ten the
  Go struct declares — and an eleventh is **silently dropped**. A soil-moisture probe, a second wind
  sensor, a thermocouple string: each is one line of CRBasic away, and today each is a measurement
  the daemon receives, parses and discards without counting it.
- **Ambient Weather.** A hundred and thirty-one parameters are mapped, which is the vendor's
  "customized" vocabulary as of the pinned commit. The vendor adds parameters; a console with a
  sensor newer than this build sends keys that go nowhere.

`serde` is **not** configured to reject an unknown field in either codec, and that is deliberate: a
`deny_unknown_fields` there would make one added CRBasic column stop a whole station. So the current
behaviour is the quiet one — the reading is admitted with the keys this build knows, and the rest
vanish.

*Options.* **(a)** Leave it until a driver needs it, which is W4's own recommendation and is the
status quo. **(b)** Add it now: `SqlType::Jsonb`, a `metadata` role, and a protobuf
`map<string, double>` so the hatch survives a hop and a restart. **(c)** The middle course W4
recorded as "(a)-then-(c)": leave the registry alone, and **count** what is dropped, so that an
operator can see a station sending something this build does not store.

*Recommendation:* **(c) now and (b) when the count moves.** The argument for not deciding (b) today is
W4's and still holds — a `jsonb` column with no consumer is a schema commitment bought on
speculation, and `observations.extra` already exists as a hand-authored column that nothing writes.
The argument against pure (a) is what M6-1 found: the drop is currently **invisible**, and this
programme's recurring defect is a station that reports less than it sends with nothing saying so.
(c) is one counter per driver — `mw_station_frames_total` already carries an `outcome` label and a
`dropped_fields` series beside it would be cardinality-safe — and it turns the question from a
judgement into a measurement the owner can take on their own Campbell.

**This is raised and not decided**, which is what M6-1's brief asked for. Nothing in the code
anticipates an answer: `mw_codec::CampbellAscii`'s module documentation states the behaviour and
points here, and a test pins it so that whichever way this goes, the change is visible.

---

## 6. The frozen surface, re-scoped to the iOS client

**Owner decision, 2026-10-31.** *"The only endpoints that matter are the ones used by the iOS app.
Everything else can be changed to whatever you want to do with it. You have freedom to restructure
the API, but we need to retain the legacy endpoints so people's iOS don't break."*

This narrows D-37 considerably, and it replaces the thing D-37 was reaching for with a sharper
version of it.

### 6.1 What iOS actually calls — five endpoints

From `contract/legacy-v1.json`'s `clients` block, which W1 built from `RemoteWeather.app` @
`77967ab`:

| | iOS | website |
|---|---|---|
| `/latest` | ✓ | ✓ |
| `/span/{hours}h` | ✓ | ✓ |
| `/snow` | ✓ | ✓ |
| `/stationinfo` | ✓ | ✓ |
| `/forecast/{hours}` | ✓ | ✓ |
| `/almanac`, `/api/stations` | — | ✓ |
| `/`, `/portal`, `/privacy`, `/support` | — | ✓ |
| `/js/`, `/css/`, `/images/`, `/fonts/` | — | ✓ |
| `/alerts`, `/api/remote-stations` | — | — |

**`iOS \ website` is empty.** Every endpoint iOS calls is also called by the website, so the five are
a strict subset — which means re-scoping the freeze to iOS costs the website nothing it was not
already going to get.

### 6.2 The better reason for a freeze: independent deployment, not legacy

D-37 froze "the legacy external contract", which lumps two unlike things together. The owner's rule
separates them, and the distinction is **who deploys the client**:

- **iOS is deployed independently of the server.** A shipped build is on people's phones and cannot
  be changed in step with a server release — App Store review is in the loop. A response change is a
  **contract break**, and the blast radius is every phone until users update.
- **The website's JavaScript ships *inside the server binary*** — `rust-embed`, W3 D-76. Server and
  client are one artefact and one release. A response change there is not a contract break, it is a
  **refactor that must touch both halves in the same PR**.

So: **five endpoints are frozen because their client is independently deployed. The rest is coupled,
not frozen.** Recorded as **D-69**. This is what D-37 was approximating, and it is falsifiable in a
way "legacy" is not — the test is "can the client be updated atomically with the server?", which has
an answer for any future client too.

### 6.3 What this drops

| Drop | Evidence | Saving |
|---|---|---|
| **`/alerts`** | No working in-repo client; not in either client's endpoint list. W1 D-13 kept it only because D-37 said *frozen* | A handler, a store capability, a fixture set |
| **`/api/remote-stations`** | Same. Its only reference is a management-UI fetch against a server with no such route — and W7 A-W7-2 retires the table behind it anyway. **This also disposes of W1-BUG-2** (a 500 for any row with a NULL text column) by deleting its subject | A handler, and the last consumer of a table already being retired |
| **`format=msgpack`** | `legacy-v1.json`: *"No shipped client sends it."* Implemented on `/latest`, `/snow`, `/forecast`, silently ignored on `/span`, and it loses number types on `/forecast` by decoding the `jsonb` into `interface{}` first | One encoder path and its content negotiation out of `mw-api-legacy`, which is the crate whose entire job is carrying quirks |

**O-6 is closed without the access-log query.** The query was only needed to decide whether an
unknown third-party client might exist. Under the owner's rule the question is no longer "does
anything call this" but "does **iOS** call this", and `legacy-v1.json` already answers that
definitively for all sixteen endpoints.

### 6.4 What does *not* change, and why that is deliberate

**The 70 fixtures and the conformance suite stay at full breadth.** It would be easy to read this
re-scoping as licence to shrink the suite to five endpoints. That would be a mistake, and the reason
is the distinction in §6.2: *coupled* is not *unconstrained*. The website's endpoints still have to
keep working — they are merely allowed to change in lockstep. The fixtures are the cheapest available
regression test for "did I change the website's data without noticing", they already exist, and they
already pass. **Deleting passing coverage to match a narrower contract is paying for the same work
twice.**

So the fixtures are **re-classified, not removed**:

| Class | Endpoints | What a diff means |
|---|---|---|
| **frozen** | the five in §6.1 | A **contract break.** CI must fail; a change needs the owner's signature and a new fixture |
| **coupled** | `/almanac`, `/api/stations`, the HTML pages, the four asset trees | A **lockstep change.** CI fails until the fixture is updated in the same PR as the server and the JavaScript — which is the point: it forces both halves to move together |
| **retired** | `/alerts`, `/api/remote-stations`, `format=msgpack` | Fixtures deleted with the handler |

Carried as work item **N-18**: add a `class` field to each fixture, and make the runner report the
two classes separately so a frozen-class failure is visibly different from a coupled-class one.
`contract/legacy-v1.json`'s `frozen` key becomes the five rather than the sixteen.

> **Done, by implementation-plan.md M4-2.** Every fixture carries `class` — 42 frozen, 19 coupled,
> 9 retired — declared on its `spec(...)` line in `contract/tools/capture.py` so a re-capture
> preserves it. The runner tags every verdict line with the class and prints a per-class summary;
> the **policy** lives in `xtask/src/conformance.rs`, which is the split that matters: the runner
> still replays all 70 and asserts that 70 were attempted, so a classification cannot arrive as a
> narrower replay.
>
> Three departures from the table above, all deliberate. The `retired` fixtures are **kept rather
> than deleted**: the `attempted == 70` assertion is what stops the classification being a silent
> filter, and deleting nine would mean changing the number that assertion checks. And
> `contract/legacy-v1.json`'s `frozen` key is **not** edited: that file is generated by
> `tools/genschema.py` from `tools/endpoints.py` and the Go and Swift sources, so editing it by hand
> is what `contract/README.md` forbids — the classification lives on the fixtures, which is where
> the runner reads it.
>
> The third is the one the table above reads against. The `coupled` row says a diff means *"CI fails
> until the fixture is updated in the same PR as the server and the JavaScript"*, and **thirteen
> coupled fixtures are excused from that at M4-2** — the six HTML pages and seven embedded assets,
> whose subject is `implementation-plan.md` M4-3: the `askama` port, the `GoHtmlEscaper` and the mime
> table. They are not a lockstep change that someone forgot to make; they are a change nobody has
> made yet, and a gate that is red for a whole milestone because of work it does not cover is G-1 by
> the route M2-5 already walked once.
>
> The excuse is bounded in three ways rather than taken on trust. The list lives in
> `xtask/src/conformance.rs` next to the policy, not on the fixtures, so widening it is a reviewable
> line in the gate. It is checked in **both** directions: an excused failure nobody declared is exit
> 2, and a declared fixture that has started passing is exit 2, which is what makes **M4-3's
> done-criteria include emptying it**. And the `frozen` roster is declared in the same file and
> asserted against every reported fixture, so a fixture cannot be moved out of the gating class to
> buy an excuse the list would not have granted.

### 6.5 Two consequences worth stating

**O-5 gets more important, not less.** `/stationinfo` is one of the five. The hazard — a website with
no about text or no bound device returns a body iOS cannot decode **at all**, losing the station list,
the snow device, the air-quality device and the radar token — is now squarely inside the frozen set
rather than at its edge. The owner has concurred with option (b), so it ships as a deliberate,
signed deviation: **DEV-10**, carried on the two existing `/stationinfo` fixtures rather than on a
new one. Each of those fixtures holds both responses, the Go server's under `response` and the Rust
target's under `deviation.response`, so `ci/conformance/go-target.sh` keeps asserting what shipped
while the Rust gate asserts what replaces it. `xtask/src/conformance.rs` names which fixtures may
declare DEV-10 and asserts the runner applied it to exactly those — a deviation is a licence to
compare against author-written bytes, so where it applies is the whole of its safety.

**N-1 (the v2 API) gets both larger and cheaper.** "You have freedom to restructure the API" plus
*"we can redesign future iOS versions as well, later on"* means v2 is no longer constrained to being
additive around a frozen sixteen-endpoint surface — it is a clean design over five frozen endpoints
and a website that moves with it. The fifth ordering constraint still keeps it after cutover: a new
API surface is the first thing that cannot be verified against the Go version. But its scope is now
worth more than when D-39 deferred it.

---

## 7. Where the database runs

**Owner concern, 2026-10-31.** *"Currently it's on an Arch Linux server (pitcairn, 10.50.0.35) that
is quite out of date and it's very hard to migrate. I want to get it moved into K8S. I think a good
way would be to spin up a new physical server with local disk and RAM and have you run it as a
statefulset there, but open to ideas."*

No design document covers where the database runs — W4 designed the schema, W9 designed the
migration, and D-2 pins the versions, but the platform was never anybody's. It is a real gap and it
is raised as **N-17**.

**Settled 2026-10-31**, in two rounds: this document first recommended running on the existing heavy
node and measuring the contention, and the owner rejected that from operating experience (§7.3). The
shape is now **a dedicated Talos worker, single-replica StatefulSet, local NVMe** (§7.4), with weekly
off-site backups as the stated condition (§7.5). The USB-serial station turned out to be a separate
problem with a separate answer (§7.6).

### 7.1 A fresh `modernweather` database was already the plan

Answering the first question directly: **yes, and it is already in the tree.**
`ci/db/migrate-from-go/migrate-from-go.sh`'s own usage header reads

```
SOURCE=postgresql://.../weather_v2_0_0  TARGET=postgresql://.../modernweather
```

The source is read-only for the entire migration apart from one explicitly named archive step
(W9 D-2, D-3), and the target is a new, empty database that the schema migrations create from
scratch. **The weather half was designed this way.** The *configuration* half was not, and that is
**G-2** in this document — the largest genuine absence this pass found, now **N-4** (the schema) and
**N-5** (the migration).

### 7.2 The move and the migration are one operation, not two

The most useful thing to say about sequencing: **do not upgrade pitcairn and then move it.** Stand the
new TimescaleDB up in Kubernetes, point W9's migration at pitcairn as source and the new instance as
target, reconcile, cut over. pitcairn is never upgraded, never modified, and stays as the rollback.

This matters because it **dissolves most of O-1**. O-1 asks production's PostgreSQL version because
W4 §5.2 and W9 OQ-W9-2 both note that below TimescaleDB 2.9 the migration becomes a major-version
upgrade as well as a schema change. With a fresh target there **is** no in-place upgrade: W9's
transform is a logical export and import (`COPY ... TO STDOUT` → `COPY ... FROM`), which crosses
versions by construction. O-1 narrows from "does this change the shape of a work item" to "what flags
does `pg_dump` need and does the archive extraction work", which is a detail rather than a fork.

It also means *"pitcairn is very hard to migrate"* stops being a problem to solve. Nothing is migrated
in place. The box is read from once and then switched off.

### 7.3 `big-bulky-1` was considered and rejected

**Superseded 2026-10-31 by the owner, on operating experience:** *"Big Bulky is not the answer. It's
too busy with maps."*

This section previously recommended running on `big-bulky-1` and measuring whether contention with
the graywolf batch jobs mattered. That recommendation is withdrawn, and the reasoning for withdrawing
it is worth recording rather than just deleting: **the measurement was proposed because this document
had no operating history, and the owner does.** §7.4's original caveat named the risk correctly —
bursty radar-contour and planet-overview tile builds sharing a disk with a latency-sensitive database
— and the person running that node already knew the answer. Measuring to rediscover it would have
been ceremony.

The findings that survive, because they are about the workload rather than the node:

| | Finding | Still true |
|---|---|---|
| **The database is small** | W4 measured 5.3× raw compression at a 76.87% hold rate, and production's `weather` is **19 GB/year** uncompressed at ~143 columns. All four aggregate levels together are **337 MB** under W4 D-5's retention | **Yes.** A decade is roughly **36 GB** compressed. Capacity is not a reason to buy anything |
| **Storage must not be NFS** | W4 D-26 phase-locks the 1-minute refresh 3 s past each boundary with 3.06–3.22 s measured lag against a **10-second** deadline; A-W4-3 decomposed a durable refresh as **~357 s of 758 s being WAL fsync** | **Yes, and it is the one hard constraint.** This workload is fsync-latency-bound. `multica-postgres` on `synology-nfs-csi` is the contrast — right for a 20 GiB config database, wrong for this one |
| **The local-PV pattern is proven here** | `graywolf-publish-big-bulky-1` is a real `local` volume with `nodeAffinity` at `/var/mnt/…`, `Retain`, `WaitForFirstConsumer`, via a `no-provisioner` storage class | **Yes** — the pattern transfers to any node; only the node name changes |
| **Talos removes the class of problem** | pitcairn is hard to migrate *because* it is a hand-maintained Arch box carrying years of accumulated local state. A Talos node has no shell and no accumulated state | **Yes, and this is the strongest argument for the move** |

### 7.4 Recommendation — a dedicated Talos worker

**Owner decision, 2026-10-31:** a dedicated machine, in the cluster, hardware already available. *"I'm
fine with a single standalone node as a Timescale server, so long as we figure out how to get backups
working."*

A single-replica StatefulSet on a `local` PersistentVolume, on **its own Talos worker**, tainted so
nothing else lands there.

**What the workload actually needs**, so the hardware can be matched rather than guessed:

| | Need | Why |
|---|---|---|
| **Disk** | **NVMe or SSD. Not spinning.** Capacity is almost irrelevant — 256 GB is ~7× a decade of history | The workload is **fsync-latency-bound**, not throughput-bound: ~47% of a durable aggregate refresh is WAL fsync (A-W4-3), and the 1-minute refresh has 6.8 s of slack against its deadline (W4 D-26). Latency is what consumes that slack |
| **RAM** | 16–32 GB is generous | The hot working set is the uncompressed recent chunks plus `shared_buffers`. The whole compressed database is ~36 GB for a decade |
| **CPU** | 4–8 cores; single-thread speed matters more than count | The heaviest operation is an aggregate refresh, and it is **one session** — W4 measured 401 s at 627 expressions over 35.9 M rows with `synchronous_commit = off`. More cores do not shorten it |
| **Network** | 1 GbE is ample | Ingest is 4 stations at 2 s. The only bulk transfer is the one-time migration |

**Not** a second replica. A single replica on a `local` volume **cannot reschedule** — if the node is
down the pod is `Pending`, because that is what `local` volumes mean. The owner has accepted that
explicitly, and §7.5 is the condition that makes it safe. CloudNativePG was considered and declined:
TimescaleDB needs a custom image, and CNPG's HA story wants ≥2 replicas on replicated storage, which
fights the non-negotiable in §7.3.

**Image: `timescale/timescaledb-ha:pg17`** — not the plain image, which lacks `timescaledb_toolkit`
that D-2 requires. It is also what CI already uses (W8 D-17), so CI and production share an image
family.

**On Talos specifically:** adding the node is a machine config, the data disk becomes a user volume
mounted under `/var/mnt/`, and the `local` PV is then pinned to it with `nodeAffinity` — the same
shape as `graywolf-publish-big-bulky-1`. No shell, no udev rules, no accumulated state. This is the
part that answers *"pitcairn is very hard to migrate"* structurally rather than by relocating it.

### 7.5 Backups — the condition that makes a single replica safe

D-6 says backup and restore is `pg_dump` of the raw hypertable plus an aggregate refresh, and that no
bespoke tool is built. That is the right mechanism and **nothing in the programme owned running it.**
On pitcairn that was a latent gap. On a single-replica local-PV StatefulSet the local volume *is* the
single point of failure, so it becomes load-bearing — and the owner has made it the condition of
accepting a single replica.

**Owner constraint, 2026-10-31:** weekly, stored off-site, Cloudflare R2 a candidate *"depending on
cost"*. Raised as **N-19**.

#### What to back up, which is less than the database

D-15 makes the aggregate levels **disposable derived data**, so they are not backed up at all — they
are rebuilt. What is irreplaceable is smaller than it looks, and one item is easy to miss:

| | Back up? | Size | Why |
|---|---|---|---|
| `observations` (raw) | **Yes** | ~3.6 GB/year compressed (19 GB/year raw at 5.3×) | Derived from nothing. D-4 retains it indefinitely |
| `observations_1d_legacy`, `observations_1h_legacy` | **Yes — and this is the one that gets forgotten** | ~2.5 MB + ~58 MB | The **only** copy of the ~294 days of rollup with no raw behind it. Not derived, not regenerable, and deliberately outside the aggregate hierarchy (W9 D-4) |
| `config.db` (SQLite) | **Yes** | a few hundred kB | Station calibration, certificates, website configuration. Tiny, irreplaceable, and the thing a human cannot re-derive — see D-66's reasoning for why it is not re-typable |
| `stations`, `station_metrics`, `almanac_cache`, `snow_totals_cache` | Yes, incidentally | small | Projections and caches. Cheap enough that excluding them is not worth the thought |
| `observations_1m` / `_5m` / `_1h` / `_1d` | **No** | would be the largest thing in the dump | Disposable (D-15), retention-bounded (W4 D-5), and rebuilt by `ci/db/rebuild-aggregates.sh` |

#### Why weekly full dumps are cheap here

**Raw is append-only and immutable per time range.** No reading is ever updated, so a historical year
cannot change. That makes the obvious schedule wasteful and a per-range one nearly free:

- Dump **per year** rather than as one file. Old years are written once and never rewritten.
- The weekly job then re-dumps only the **current** year — roughly 4–5 GB, not 40.
- Restores become granular: a single year can be pulled without the other nine.

#### Cost, since it was asked conditionally

Estimated from the measured 19 GB/year and 5.3×, with `pg_dump -Fc`'s zlib on data with a 76.87% hold
rate landing in a similar range — **to be replaced with the real number from the first dump**, since
this is the one figure here that is extrapolated rather than measured:

| | |
|---|---|
| A decade at rest | ~40–50 GB |
| R2 storage | ~$0.015/GB-month → **$0.60–0.75/month**, less the 10 GB-month free tier |
| Weekly upload | ~4–5 GB (current year only). Class A operations stay inside the free tier |
| **Egress** | **Free, and this is the reason to pick R2 specifically** |

**Under a dollar a month, and the zero-egress property is the one that matters.** The same 40 GB on
S3 would cost roughly $3.60 *per restore* in egress — and a backup you hesitate to restore from is
not a backup. That is also why §7.5's deliverable is a **rehearsed** restore rather than a dump.

> **Measured 2026-11-12 by M0-2, and the extrapolation above holds.** `docs/backup-and-restore.md`
> §4 has the full table. The dump's COPY-text width is **871.9 B/row** and does not move with row
> count or with hold rate; the *stored* size is almost entirely a function of the hold rate —
> **80.18 B/row at a 76.88% hold rate, 112.59 at 65.31%, 139.43 at 53.84%, 228.13 at 6.30%**.
> Production's rate is 59.6% (W6 §5 Group A), which interpolates to ~126 B/row, so **~3.4 GB/year
> stored and ~34 GB for a decade** against the 40–50 GB above — conservative by about a quarter, in
> the safe direction. **No decision moves.** What changes is that the only remaining inference is
> one named variable rather than the whole chain, and the archive's manifest records
> `rows`/`bytes`/`stored_bytes` per year: the first weekly dump against *production's own* data
> replaces the interpolation with no further measurement by anyone.

#### Two measured findings the restore path must carry

Both are counter-intuitive and already paid for:

- **A-W9-2 — a continuous aggregate *accepts* an `INSERT` and discards it on the next refresh.**
  Pre-seeded aggregate rows are not a restore mechanism. The restore path is: load raw, then rebuild.
  Measured — the insert succeeds, duplicates succeed, the rows read back correctly, and then a refresh
  over the range leaves zero rows. The failure is entirely later than the mistake.
- **W4 D-23's phase-1-first ordering.** A full rebuild at ten years is an hour-scale operation
  (A-W4-3). `ci/db/rebuild-aggregates.sh` refreshes the `/span` bands first, bottom-up —
  **0.87 s against 467 s**, the difference between "the charts are right" and "everything is
  materialised". A restore runbook that blocks on a full refresh is an outage that did not need to be
  one.

**Both are now mechanisms rather than notes.** M0-2's `db-restore-rehearsal` gate re-measures A-W9-2
on every run — it pre-seeds `observations_1m`, refreshes, and fails if the row *survives*, because
the exclusion of the aggregate levels from the archive is argued entirely from that behaviour — and
it rebuilds with `--recent-only` only, asserting the four `/span` bands against raw before phase 2 is
even considered. A third finding joined them from M0-1: a new retention policy's first job runs
**immediately**, so a restore into a freshly migrated database is inside that window by construction
and the background jobs have to be suspended for the duration. `docs/backup-and-restore.md` §3 is
the runbook and the `db-restore-rehearsal` gate is what keeps it true.

### 7.6 The USB-serial station, which is a separate problem from the database

**Owner constraint, 2026-10-31:** *"my home weather station (CSI driver) uses USB serial, so we need a
way to connect that to the node we'll be using (or a way to forward over the network from another
Linux box)."*

**This is two problems, not one, and separating them is most of the answer.** The USB serial device is
read by a **station driver** — a component of the daemon (W2a §4a). It has nothing to do with where
**TimescaleDB** runs. D-44 makes this explicit: a node's position in a topology is the set of
components its configuration produces, not a mode it is in. So the database node does not need the
USB device, and the machine holding the USB device does not need to be a database node.

Three ways to close it, in recommendation order.

**(a) Serial over TCP — works today, on the shipping Go version, with no code change.**

A small Linux box physically attached to the station runs `ser2net` (or `socat`) exposing the USB
serial port as a TCP socket; the Campbell driver connects over TCP instead of serial.

This needs no new capability on either side, **verified in both trees**:

- The **Go** implementation already does it. `internal/weatherstations/campbell/station.go:254` takes
  the serial path only `if s.config.SerialDevice != ""`; otherwise `:303` builds
  `net.JoinHostPort(s.config.Hostname, s.config.Port)` and `:320` dials TCP. The configuration table
  already carries `hostname`, `port` and `baud` (`pkg/config/provider_sqlite.go:105-108`).
- The **Rust** design already specifies it. W2a §4a.5 maps Campbell's transport as
  **`Serial | TcpOut`**, the same cell as Davis.

So this is a configuration change on the current system, available before the rewrite exists, and it
survives the rewrite untouched. *Honest costs:* one more box in the path, and serial-over-TCP gives up
hardware flow control and DTR manipulation — irrelevant for Campbell's command/response ASCII
protocol, but worth knowing before assuming it generalises to every serial device.

**(b) A satellite node — the better long-term answer, and what W7 was designed for.**

Run `modernweather` *itself* on the box attached to the station, configured as a **satellite**: station
components plus an uplink, no local database. It spools locally and forwards to the collector in
Kubernetes (W7 §8.2 is exactly this configuration).

This is strictly better than (a) on two axes: the driver talks to the real device rather than to a
serial-over-TCP bridge, and **a network outage stops losing readings** — W7's spool holds ~100 days at
the measured rate against a 2 GiB ceiling, and the uplink resumes with a cumulative ack (W7 D-22).
It also gives the ARMv6/ARMv7 targets a purpose: this is the use case that justifies keeping them, and
it connects **O-9**'s "retain the broadest hardware support" to something concrete.

Not available immediately — it needs M6-4. But the Go version already has `grpcreceiver` and remote
stations, so a **Go satellite against a Rust collector** is a supported path from day one: W7 D-42
designs the compat shim for exactly that, with three stated limitations (no resume or cross-reconnect
dedup, no relay, no field presence).

**(c) USB passthrough into the pod — possible, and not recommended.**

A privileged pod with a `hostPath` of `/dev/serial/by-id/…` (never `/dev/ttyUSB0`, whose numbering is
not stable across a replug), plus `nodeAffinity` pinning it to the node the cable is plugged into.

Privileged pods are routine in this cluster — twelve namespaces enforce `privileged` — but **no pod
currently mounts any `/dev` hostPath and there is no device plugin**, so unlike the local-PV pattern
there is nothing here to copy. The deeper objection is not the mechanics: it is that **it pins the
daemon's scheduling to a physical cable**, and then wants that node to also be the database node,
which recreates in Kubernetes exactly the single-box coupling that makes pitcairn hard to move.

**Recommendation: (a) now, (b) when M6-4 lands, (c) not at all.** Raised as **N-20**. The important
consequence is that **the USB-serial question does not block the database move** — it is answerable
with a $35 box and a config change, independently and in either order.


---

## 8. The rollup-only history was already gone

**Measured 2026-10-31, against production.** This is the most consequential thing this programme
found, and it is a finding rather than a recommendation: **the daily and hourly rollup that
`docs/data-migration.md` §3 was written to protect does not exist outside the raw window.** It was
destroyed before anyone looked — and because no endpoint can request a span older than one year
(§8.1b), there was no symptom.

### 8.1 The evidence

Four queries, each independently sufficient:

| | Result |
|---|---|
| Raw window | `2025-10-29` → `2026-10-30`, exactly **366 days** — the 365-day policy trimming daily |
| `weather_1d` extent | `2025-10-29` → `2026-10-30`. **Same start as raw** |
| Day buckets older than the raw window | **0** |
| `_materialized_hypertable_5` queried **directly**, bypassing the real-time view union | `min(bucket) = 2025-10-29` |

And the forensic one — **the chunks are still there and they are empty:**

```
chunk               range                  live rows
_hyper_5_35_chunk   2025-01-01..2025-03-12         0
_hyper_5_36_chunk   2025-03-12..2025-05-21         0
_hyper_5_37_chunk   2025-05-21..2025-07-30         0
_hyper_5_44_chunk   2025-07-30..2025-10-08         0
_hyper_5_58_chunk   2025-10-08..2025-12-17       124   <- only the part from 2025-10-29
```

**301 days of empty chunks**, from `2025-01-01` to `2025-10-29` — the rows are gone and the
containers were left behind. **See §8.1b: "301 days of empty chunks" is not the same as "301 days of
readings deleted", and the stronger reading is not supportable.** The deletion itself is evidenced
directly; its extent in days of real data is not recoverable from this database.

This is the signature of A-W9-1's mechanism, and it distinguishes it from ordinary retention:
`drop_chunks` on **raw** *removes* chunks — the raw hypertable's oldest chunk is simply `2025-10-29`,
with no empty predecessors. A continuous-aggregate refresh over an invalidated range *deletes rows
inside retained chunks*. The empty chunks are only explicable as refresh, not retention.

### 8.1b Why the live site looks completely fine — and the correction this forces

**The owner challenged this finding**, reasonably: *"I'm able to view historical weather on
suncrestweather.com with no issues."* Both observations are true, and reconciling them is what
establishes the finding's real scope.

**The site cannot ask for more than a year.** Measured against the live server:

| Request | Result |
|---|---|
| `/span/8760h` (365 d) | **HTTP 200**, 364 points, oldest `2025-11-01` |
| `/span/8784h` (366 d) | **HTTP 400** — `time span exceeds maximum allowed duration of 1 year` |
| `/span/17520h` (2 y) | **HTTP 400**, same |

So every byte the website can request is inside the intact window, and the server **refuses** to ask
for anything older. W9 D-6 predicted exactly this — *"`/span` takes no time anchor and rejects a span
over one year … so the furthest band it can ask for always has real data behind it"* — which is
precisely **why the loss was invisible**: no endpoint exposes the range that is gone.

**Two corrections to §8.1, in the direction of less certainty.**

*First, the size of the loss is not 301 days of data.* 301 days is the span of **chunk ranges** that
are now empty, which is not the same as 301 days of readings. `pg_stat_all_tables` is more precise
and partly disagrees with the stronger reading:

```
chunk               range                  live  ins   del   autovacuumed
_hyper_5_35_chunk   2025-01-01..2025-03-12    0     0     0   no
_hyper_5_36_chunk   2025-03-12..2025-05-21    0     0     0   no
_hyper_5_37_chunk   2025-05-21..2025-07-30    0     0     0   no
_hyper_5_44_chunk   2025-07-30..2025-10-08    0    14   121   yes
_hyper_5_58_chunk   2025-10-08..2025-12-17  124     8    50   yes
```

**`n_tup_del` of 121 and 50 is direct evidence of deletion** on the two chunks nearest the boundary.
The three older chunks show no activity at all — consistent with having been emptied before the last
statistics reset, but **not proof of it**, because these counters do not survive a restart or
`pg_stat_reset()`. The honest statement is therefore: *rows were demonstrably deleted from the
boundary chunks, and the older chunks exist with zero rows.*

*Second, that data did exist is established independently* — by `almanac_cache`, which survived
because it is a precomputed table rather than a continuous aggregate. It holds 34 records dated from
**2025-01-22**, including `low_barometer = 24.899` on 2025-01-24, under station names
**`DavisVantagePro2`** and **`backyard-snow`** that no longer appear in the current data at all. So
there were readings back to at least January 2025, under earlier station names — which is also a live
instance of the station-rename bucket split W2 D-5 fixes.

**Revised scope.** Daily and hourly rollup covering roughly 2025-01 to 2025-10-29 is gone from the
aggregates; the exact number of days that held readings cannot be established from the surviving
database. The all-time records survive in `almanac_cache`. **Nothing a client can request is
affected**, which is why there was no symptom.

### 8.2 Why it happened, and why it was certain to

Three settings in combination, all of them individually reasonable:

| | |
|---|---|
| Job **1004** | retention on raw, `drop_after = 365 days`, **daily** |
| Job **1003** | refresh `weather_1d` with **`start_offset = 10 years`** |
| Job **1002** | refresh `weather_1h` with **`start_offset = 2 years`** |

Every day: retention drops a day of raw at the 365-day boundary, and then a refresh policy whose
window reaches back *ten years* re-materialises over that range, finds no raw, and deletes the
buckets. The invalidation log confirms the mechanism was live — every level carried an unbounded-low
invalidation entry, and all four showed invalidations reaching below the raw window.

**So the loss was not an event. It was a daily process**, and it had already consumed everything
outside the raw window before this programme started. `docs/data-migration.md`'s framing — that each
day "converts one more day of raw readings into rollup-only history" — describes the first half of a
mechanism whose second half ran the same night. **A day of raw did not become rollup-only. It became
nothing.**

### 8.3 What this changes

- **O-7 is moot.** The history question (W9 OQ-W9-1: delete ~294 days of rollup, or carry it for
  ~60 MB) has no subject. There is nothing to carry. **And it never had a consumer either** — §8.1b
  shows `/span` refuses any span over one year, so even had the rollup survived, no endpoint could
  have served it without the time-anchored v2 endpoint (**N-1**) that does not exist yet. The owner's answer — Option B, import the
  archive — stands as the right call on the facts as they were known, and the archive it refers to is
  empty of anything unique.
- **The archive copy was still taken** (`weather_1d_archive`, 967 rows; `weather_1h_archive`, 20,041
  rows; 2025-10-29 → 2026-10-30, 42 MB total) **and is now redundant rather than irreplaceable** —
  every row in it is reconstructible from raw, which is the opposite of the premise. It is kept as a
  point-in-time snapshot taken before the policy change, and it can be dropped whenever the owner
  likes.
- **W9's legacy-archive apparatus has no input.** `observations_1d_legacy`, `observations_1h_legacy`,
  `w9.legacy_boundary`, `w9.assert_legacy_disjoint()` and `observations_1d_with_legacy` were all built
  for data that is not there. **Recommendation: keep them, do not rip them out.** They are built,
  tested and harmless, and they become useful the moment an older `pg_dump` of pitcairn surfaces from
  anywhere — which is now the only route by which that history could return. Recorded as **D-71**.
- **§7.5's backup argument is strengthened, not weakened.** The legacy archive was listed there as the
  irreplaceable item that gets forgotten. It is now empty — but the reasoning was right, and the thing
  it was protecting against is precisely what happened here: a derived-data mechanism quietly eating
  the only copy of something.

### 8.4 Why nobody caught it, which is the part worth keeping

W0's spike measured the destructive refresh behaviour **on TimescaleDB 2.31** and W9 reproduced it in
a lab. Both were right about the mechanism. What neither could do was look at production — W0 OQ-1
recorded it as unreachable, and that premise went unchallenged for the whole programme until
`docs/reconciliation.md` §5 found the network was fine and only `pg_hba` was in the way.

**The measurement was available the entire time and the question was framed as unanswerable.** W9
even wrote the query that proves it — P-4b, *"the ~294-day number, measured rather than remembered"* —
and shipped it unrun. Every document that cites ~294 days cites an inference.

The lesson is narrow and worth stating: **"production is unreachable" was a conclusion drawn from a
`Do not` instruction, not from a test.** W0 was told not to touch production, correctly, and recorded
the consequence as a property of the environment. Nobody retested it, because it was written down.

---

## Decisions

1. **This pass reconciles; it does not redesign.** Every resolution in §1 is either a clerical fix, a correction forced by measurement, or an acceptance of a recommendation its own author made. Where a resolution was a genuine design choice it went to §5.
2. **329 assumptions were checked individually against the other document's Decisions section**, and all 329 are recorded in §1 as holding or not — 208 holding as written, 64 with a narrowing or correction, 8 forward, 49 failing across 27 distinct defects.
3. **None of the 27 defects is a disagreement about what the system should do.** The dominant causes are stage-parallel blindness (9) and a request aimed at a document already merged (3).
4. **The deviation ledger is a required deliverable of this work item**, because three documents reference it by name. It carries eight entries; two are purely additive, five close defects, one removes a response no shipped client requests, and none removes a response a shipped client reads.
5. **The conformance gate's `arms_when` and `cmd` describe an interface nobody built** (G-1), so a job named "frozen contract conformance" has reported green on every PR while W1's suite — whose own done-criteria required it to pass — has never run in CI against either binary. Repointed in this PR; the Go-target run is raised as N-3.
6. **A gate's `arms_when` must name a path that some work item's done-criteria require it to create.** G-1 is what happens otherwise. Recorded as D-64.
7. **The configuration store and its migration are unowned** (G-2) and are the largest genuine absence found: nine-plus SQLite tables assumed by four documents, with no DDL and no migration mechanism, against D-8. Raised as N-4 and N-5.
8. **"Re-enter the configuration by hand" is declined**, with one deliberate exception: the four plaintext third-party credentials are not migrated, because migrating a secret that crossed the wire in cleartext carries the exposure forward.
9. **Proto3 field presence was decided by three documents and implemented by none** (G-3). `generated/proto/reading.proto` contains `optional` zero times, which silently converts a NULL into `0.0` across a hop *and* across a restart. Raised as N-12 and sequenced inside the registry work item.
10. **`/span`'s 60-second cache and W4's trailing-bucket requirement are incompatible as written** (C-W3/W4-1 — the only contradiction no author had half-seen). Resolved by caching the closed-bucket prefix and appending the measured 0.44–6.89 ms tail per request, which preserves both decisions exactly.
11. **Sixteen work items are raised** (§4), twelve of them small, every one closing a gap §2 found rather than inventing scope.
12. **Twelve questions go to the owner** (§5), four of which are production queries no agent can run. Sixty-two open questions across ten documents reduced to twelve, which is the output contract earning its cost rather than a sign that too much was left open.
13. **The amendments are applied in `DESIGN.md`, not here**, via the protocol: appended to `Amendments`, with every superseded decision marked in place and pointing at its amendment.
14. **`remoteweather:docs/rewrite/` is stale in 6 of 12 files, not "across the board"**, and the two that matter are W2's and W4's prompts, which are still 58-line and 42-line **outlines** there against 255 and 248 lines here. See *Proposed amendments* for the recommendation.

## Open questions

All twelve are in §5 above, which is this document's point rather than an appendix: **O-1** to
**O-4** are production queries, **O-5** to **O-11** are decisions, **O-13** and **O-14** are decisions
raised after this pass by implementation items, and **O-12** is a process finding
about the programme's own method. They are not restated here.

## Assumptions about other work items

This document is the last design work item, so these are assumptions about **implementation** work
items and about the owner, not about documents yet to be written.

1. **The implementation plan's ordering constraints are binding, not advisory.** In particular: the frozen-contract conformance suite must be green **before** any legacy endpoint is reimplemented, not after. N-3 is what makes that possible at all, which is why it sits in milestone 1.
2. **N-12 (proto3 `optional`) is done inside the registry work item**, before anything reads `generated/proto/`. Retrofitting presence after a collector exists is a fleet-wide flag day for a change that is free today.
3. **N-14 (dimension `role = "metadata"`) lands with the registry work**, because it is the prerequisite for both OQ-W4-2's column drop and C-W7-26's eventual provenance column, and because a `[[dimension]]` with no deprecation path is a one-way door.
4. **The seventy-one decisions in `DESIGN.md`'s ledger are the implementation's specification**, and the ten design documents are its rationale. An implementation agent that reads only the ledger should never be wrong; it will sometimes not know *why*.
5. **D-47 through D-67 (this document's additions) are resolutions, not new design.** D-68 and D-69 are the owner's, taken 2026-10-31. Each one's reasoning is the reasoning its originating document gave; where this document chose between two options it says which argument it chose on.
6. **The owner answers O-2 before the next raw chunk is dropped.** Nothing in the plan is sequenced against it, because it is not a dependency — it is a loss that is already occurring.
7. **`remoteweather` stays readable at `c3ec9f7`** for the emulators (D-40, W8 D-23), the Go view inventory (W9 D-11) and N-9's reading of `pkg/solar`.
8. **The implementation plan assumes the Go version stays in production through every milestone.** Every milestone's done-criteria are written so that is true; a milestone that cannot be verified without cutting over has been split.

## Proposed amendments to `DESIGN.md`

**The accumulated amendments from all ten documents are applied in `docs/DESIGN.md` by this work
item**, which is what §3 of this prompt instructs and what W9's own amendment section asks for ("W6
consumes these four sections from every document and is the right place for the amendments to be
applied together"). Fourteen amendments were proposed across the programme — W1's A-1 to A-4, W2's
A-W2-1 and A-W2-2, W2a's D-30 split, W3's A-W3-1 to A-W3-4, W4's A-W4-1 to A-W4-3, W5's A-W5-1 to
A-W5-3, W7's A-W7-1 to A-W7-4, W8's A-1 to A-4 and W9's A-W9-1 to A-W9-3. All are applied there under
the protocol; none is edited in place. See `docs/DESIGN.md`'s `## Amendments` section.

This document proposes **one** amendment of its own, which is not to a decision. Twenty-one
more were appended later by implementation items and are marked as such.

**A-M4-2-1 — a `Soft` capability handle is read when it is used, not once at build.**
*Proposed by implementation-plan.md **M4-2**, 2026-11-09. Not applied: the protocol above reserves
that for a work item that owns the ledger.*

*What was found.* `BuildCx::soft` borrows the capability slot once and returns what is in it, and
`mw_supervisor`'s reconcile does not rebuild a `Running` component when a soft capability later
arrives — the `(State::Running, true)` arm refreshes the `degraded` label and returns. So for a
component whose hard needs are satisfied in the first reconcile pass, a soft handle is `None` for
the life of the process. M4-1's code review found `snapshot:latest` in exactly that state against
`WindowedView`, and its fix was to take the handle from the registrar instead — which works because
`RollupState` is a root resource `main` owns.

**`api:legacy` has no such fix available**, and that is why this is an amendment rather than a
repetition. Its three read stores — `ReadingStore`, `ForecastStore`, `SnowCache` — are published by
`db:postgres`, whose pool does not exist until its own task has connected and asserted **D-62**
(`mw_component::store`'s module documentation has why that cannot be a root resource). The handle
genuinely arrives after the build, and a once-only read makes `/span`, `/almanac` and `/forecast`
permanently empty on a healthy process.

*What replaces it.* `mw_component::Deferred<H>`, and `BuildCx::deferred::<H>()`, which is restricted
to a **`Soft`** need. `BuildCx::additive` refuses an exclusive capability on the grounds that a
component able to watch its database being revoked would decide for itself what that means — W2a
Decision 21's "no error branch in business logic". That reasoning is about **`Hard`** needs, where
absence means `Waiting` and there is nothing to decide. A `Soft` need is the declaration that the
component *does* degrade in a defined way, and `docs/component-model.md` §3.2's table says so for
this exact component and capability: *"`api:legacy` `/forecast` | failing query | `Running {
degraded: [ForecastStore] }`, defined stale/empty response"*. Asking for a `Deferred` against a
`Hard` need is `BuildError::UndeclaredNeed`, the same answer `BuildCx::soft` gives for the inverse
mistake.

It resolves a shape the design left between Decision 21 and §3.2's table rather than contradicting a
decision, which is why it is here and not an amendment to one.

**A-M3-3-1 — D-56's `admit` half is not expressible, and lands on the writer instead.**
*Proposed by implementation-plan.md **M3-3**, 2026-11-05. Not applied: the protocol above reserves
that for a work item that owns the ledger.*

*What was found.* **D-56** reads: *"`Ingress::admit` upserts `stations` on identity resolution;
`writer:timescale` maintains `stations.last_seen` and `station_metrics`."* The first clause cannot be
implemented as written, and not for want of trying — two independent mechanisms forbid it:

- **W3 D-87** allows `admit` no I/O but the spool append, and that is a property of the signatures
  rather than a rule the code follows. `StationRegistry::resolve` is a synchronous `fn` returning a
  `Result`, `SpoolIngress` holds a spool and a registry handle and nothing else, and there is no
  store handle anywhere on that path. An `admit` that upserted a PostgreSQL row would also be an
  `admit` that can block on a database, which is the whole of what D-87 exists against — and
  measured, in the Go collector, `LoadConfig()` on every reading is the shape it was written about.
- **`ci/layers.toml`** forbids `mw-pipeline` both `sqlx` and `mw-store`, so "upsert a PostgreSQL row"
  is not an expression that crate can contain. `cargo xtask gate layout` rejects the dependency that
  would make it one, which means the first clause is not merely inadvisable there; it does not
  compile.

*What replaces it.* **Both** projections are maintained by `writer:timescale`, in the same
transaction as the rows they describe. The projection is a copy of `config.stations`, so the writer
needs the station registry — and it gets the handle **through the registrar**, the way
`mw_station`'s ingress does, rather than as a declared `Need`. That is not a shortcut: W2a §4.1b
gives `StationRegistry` to a `config:sqlite` component and no work item in
`docs/implementation-plan.md` creates one, so a hard need on it would be "a requirement nobody
provides" and `plan()` would refuse the whole configuration. `mw_store::registry`'s module
documentation carries the same note for the same reason, and the need becomes declarable in the item
that builds `config:sqlite`.

*Why that is better rather than merely possible.* `station_metrics` has a foreign key to `stations`,
so the two writes have to be ordered; with both in one transaction there is no window in which
`observations` holds rows for a station `stations` does not have, and no ordering constraint between
two components. The cost is one thing and it is small: the projection is refreshed when a station
**reports** rather than when it is **configured**, so a station whose coordinates are edited while it
is not reporting catches up on its next reading. `/span`'s join is the only reader and it reads rows
that have readings.

*Why the original was wrong rather than merely inconvenient.* D-56 was a clerical resolution of
C-W4-20 — W4 identified the gap precisely and only lacked the authority to place it — and it placed
the upsert on `admit` because `admit` is where the station is resolved. That is true and it is not
sufficient: having the *data* is not the same as being allowed to do the *write*, and D-87 was
decided in the same ledger.

**A-M3-4-1 — the reading store's two halves are published as one handle.**
*Proposed by implementation-plan.md **M3-4**, 2026-11-06. Not applied: the protocol above
reserves that for a work item that owns the ledger.*

*What was found.* **D-30** keeps the read and the write halves of the reading store as two
traits, "named separately so nobody makes the website a sink", and `mw_component::store`'s
module documentation (written by M3-3) left the next item a choice it could not make for
itself: `db:postgres` publishes exactly one handle per capability, and until M3-4 that
handle was the write half alone. Two shapes were available —

1. one published handle offering both halves, each still its own trait; or
2. the write half travelling outside the capability bus, which is what `Ingress` does
   today.

*What was chosen, and why the alternative was not available.* **Shape 1.** The `Ingress`
precedent works because `main` *constructs* the spool, so it has something to hand to the
configuration before `plan()` runs. The pool does not exist until `db:postgres`'s own task
has connected and asserted **D-62** against it, so there is nothing for a registrar to hand
anybody. Making shape 2 work would mean moving the pool into `main` as a fourth root
resource — which is the `ValidateTimescaleDBConfig`-in-a-constructor shape **D-27** rejects,
and which W2a §4.1 split `db:postgres` from `writer:timescale` precisely to avoid.

*What that means in the types.* `mw_component::Store` is a trait with **no methods of its
own**, whose only content is `ReadingSink + ReadingStore`. The two halves stay two traits
with two query sets in two files, and a consumer **upcasts the handle to the half it
declared**: `writer:timescale` keeps an `Arc<dyn ReadingSink>` and `window:rollup` keeps an
`Arc<dyn ReadingStore>`, so after `build` neither can reach the other's methods even though
both resolved `Capability::ReadingStore`. That is W2a §4.1b's `ReadingStore H W` against
`ReadingStore H`, enforced rather than declared. Neither half implements `CapabilityHandle`
on its own any more, so publishing two handle *types* under one exclusive capability — the
shape that would fail at build time with `HandleTypeMismatch` — does not compile.

*Why this is an amendment at all.* It contradicts nothing. **D-30** is about the traits and
they are untouched; what is settled is a question the design deliberately left open, and
recording the answer is what stops M4-1 and M4-2 reopening it.

**A-M3-4-2 — `Window` is an enum, and `rain:totals` is realised as two components that
already exist.**
*Proposed by implementation-plan.md **M3-4**, 2026-11-06. Two departures from prose, neither
from a decision.*

*The first: `Window`.* `docs/component-model.md` §4b.2 sketches `pub struct Window(Duration)`
with `Window::DAY` among its constants. **A station's local day is not a duration.** On a
spring-forward date it is 23 hours and on a fall-back date it is 25, so a `DAY` expressed as
86,400 seconds is wrong twice a year in exactly the way `date_trunc('day', NOW())` is wrong
every day — which is the defect this item exists to close. §4b.3 says as much in prose two
paragraphs later: *"`Window::DAY` is the one window that needs a time zone, and it takes it
from the station"*, and a type that cannot hold a zone cannot express that. So `Window` is an
enum: `Rolling(Duration)`, `PerHour(Duration)`, `Day`, `Month`, `Year`, `Storm`. No
`DESIGN.md` decision fixes its shape, so there is nothing to amend — but the sketch is what a
reader will have in mind. `Agg` is untouched and is exactly **D-50**'s five.

`Window::PerHour` deserves its own line, because the obvious alternative would have been a
sixth `Agg` and **D-50** forbids that. The frozen `/latest` body's `rainrate` is
`sum(rainincremental) over 10 minutes x 6` (`rainfall_calc.go:149`) and **not** the
`rainrate` column the station reported — `handlers.go:344` overwrites the column's value with
the computed one, so the contract promises the extrapolation. That is not a sixth way of
combining bucket state; it is the same `Sum` over the same buckets, reported per hour. What
changes is the window's units, so it is the window that says so.

*The second: where the rainfall accumulation lives.* W3 §2.5's consequence 2 writes
`rain:totals` as a pair — "a `Calendar { daily 00:00, TzSource::Station }` component for the
boundary plus `Every { 60 s }` for the accumulation since it, so the boundary is correct by
construction". The pair is built, and the accumulating half is **`window:rollup`** rather
than a third component. The reason is the capability inventory: W2a §2.1 has no capability
for the rainfall windows, `snapshot:latest` declares `WindowedView` S and nothing else that
could carry them, and `Window::DAY` is one of the windows `WindowedView` resolves — so the
accumulation has to be inside `WindowedView`'s provider or it is not readable at all. The
`Calendar` half is `rain:rollover:<station>`, which declares **no capabilities**: the
boundary is a function of a zone and the clock, and a rollover that needed `ReadingStore`
would sit in `Waiting` through a database outage and publish an hour-late boundary when the
server came back — a wrong `dayrain` rather than a stale one. The two share an
`Arc<RollupState>` handed to them through the registrar, the way `Ingress` is.

*What is deliberately not in this item.* **The snow totals.** M3-4's prose lists R2's whole
set, "the snow totals" included, and those are four components over
`Capability::SnowCache` (W3 §2a.7, `docs/component-model.md` §4.3) which `db:postgres` does
not publish — `mw_store::pg`'s own spec test on `main` asserts that, naming M5 and M6-3 as
the items that bring it. `snow:*` additionally needs the PELT changepoint detection and the
RBF smoothing, which is a body of arithmetic with its own done-criteria. M6-3 builds them.

**A-M4-1-1 — `snapshot:warm` requires `ReadingStore` HARD, because a soft need seeds
nothing.**
*Proposed by implementation-plan.md **M4-1**, 2026-11-09. Not applied: the protocol above
reserves that for a work item that owns the ledger. The decision stands as written until it
is.*

> **Which D-37.** This amends **W3 Decision 37** in
> `docs/service-architecture.md`'s `# Decisions` list, **not** the `DESIGN.md` ledger row of
> the same number — that one is *"the legacy external contract is frozen and new endpoints
> are additive only"*, and `snapshot:warm` appears nowhere in `DESIGN.md`. The collision is
> systematic across this range: ledger D-30, D-31, D-33, D-35 and D-36 are all about
> unrelated subjects, and `implementation-plan.md`'s M4-1 paragraph disambiguates the first
> one ("W3 D-31") and then stops. A-M3-4-1 has the same latent defect with its "D-30".
> Stated here because this entry is the record of a deliberate spec deviation, and an agent
> told to apply it to the ledger would otherwise find no row to amend.
>
> One consequence worth naming: **C-W2a-28** above attributes "starts successfully with
> `ReadingStore` unavailable" to D-37 and therefore to `snapshot:warm`. The property **D-48**
> actually rests on belongs to `snapshot:latest`, which provides `LatestSnapshot`, declares
> no hard requirement at all and publishes in `build`. Without that correction a reader
> concludes that making warm's need hard breaks D-48. It does not, and
> `crates/modernweather/tests/snapshot_degraded.rs` asserts both halves.

*What was found.* **W3 Decision 37** reads: *"`snapshot:warm` is an `OnStartup` component that seeds the
snapshot table from the store so a restart does not 404 a station that was inside the
10-minute window; with `ReadingStore` unavailable it finishes empty rather than refusing to
start."* W3 §2a.6 spells the requirement `ReadingStore` **soft**.

**Soft does not work, and the reason is the supervisor rather than the decision.** W2a
Decision 17 is that *only hard edges form the ordering graph*, and `mw_supervisor`
implements exactly that: a soft need is read in `degraded()` and contributes to
`State::Running { degraded }`, and nothing anywhere delays a start for one. An `OnStartup`
component with soft `ReadingStore` therefore fires the instant it is spawned — which on
**every** node, healthy or not, is before `db:postgres` has connected, asserted **D-62** and
published. `mw_store::pg` publishes after a successful connect and not before, by design
(**D-27**: a server that is slow or unreachable at startup is logged and started past). So
the seed would run against `None` and populate nothing, always, and the one thing the
component exists to do would never happen.

Waiting for the handle *after* `build` is not available either, and deliberately:
`BuildCx::additive` refuses a non-additive capability precisely so that a component cannot
watch an exclusive one appear and decide for itself what its absence means (W2a Decision 21).

*What is proposed.* D-37 gains: *the requirement is `ReadingStore` **hard**. With the
database down the component sits in `Waiting { ReadingStore }` rather than reaching
`Finished`, and the observable is unchanged — an empty snapshot table, and `/latest` 404ing
until a reading arrives.*

*Why this is safe, which is the part that matters.* **Nothing requires `snapshot:warm`**: it
provides no capability, so its `Waiting` cascades nowhere. In particular it does not reach
**D-48** — the invariant that a `503` from an unmounted route is unobservable on a frozen
path — because that invariant rests on `snapshot:latest`, which provides `LatestSnapshot`,
declares **no hard requirement at all**, and publishes its handle in `build`.
`crates/modernweather/tests/snapshot_degraded.rs` asserts both halves against the real
supervisor with `db:postgres` planned and never publishing.

What the change buys beyond correctness at startup: when the server comes back, the seed
actually runs, where a soft need would have finished empty an hour earlier and never looked
again.

**A-M4-1-2 — `snapshot:latest` resolves `StationRegistry` and `WindowedView` through the
registrar, and the declarations stay.**
*Proposed by implementation-plan.md **M4-1**, 2026-11-09. Two departures from
`docs/component-model.md` §4's table, neither from a `DESIGN.md` decision.*

*What was found.* That table gives `snapshot:latest` `StationRegistry` **H**, `WindowedView`
**S** and `SnowCache` **S**. The implementation declares `WindowedView` **S** and nothing
else, and takes both handles from the registrar — which is the shape `mw_station`'s stations
already have for `ReadingStream` (`mw_station::config`'s module documentation has the general
argument). The two have *different* reasons and only the second was discovered by building
it:

1. **`StationRegistry` has no provider.** W2a §4.1b gives it to a `config:sqlite` component
   and no work item in the plan creates one, so `main` owns the snapshot as a root resource —
   which `mw_store::pg` already records for `db:postgres`. Declaring it `Hard` would be a
   hard need nobody provides, and `plan()` refuses that configuration under **D-44**.
2. **A soft `WindowedView` resolved off the bus is `None` for the life of every healthy
   process.** `BuildCx::soft` resolves **once**, in `build`, and `mw_supervisor`'s
   `reconcile` never rebuilds a `Running` component when a soft capability later arrives:
   the `(State::Running, true)` arm refreshes the `degraded` label and returns.
   `snapshot:latest` has no hard need, so it is built in the **first** reconcile pass, while
   `window:rollup` cannot build until `db:postgres` has published and does not publish
   `WindowedView` until its first completed tick. Eight of the nine window-derived values
   would therefore be absent from `/latest` forever and every snapshot would report
   `Staleness::Degraded`, pinning `mw_snapshot_degraded` at 1 on every key — which makes
   **D-36**'s only operator-facing signal a constant, and is the inverse of W3 §2a.6's first
   table row. Found by code review, and
   `a_window_provider_that_publishes_after_the_build_is_still_read` is the regression test.

*What is proposed.* The `component-model.md` §4 row gains: *a capability whose provider is a
root resource `main` owns, or whose handle must outlive one `build`, is **declared** on the
component and **resolved** through the registrar. The declaration is what `plan()` and
`mw_component_state` read; the registrar is what the handle's lifetime requires.*

*`SnowCache` S is dropped rather than deviated from.* Nothing provides it until M6-3, and a
`Staleness` that could name a capability nothing can revoke would be dishonest. It comes back
with `snow:*`, and nothing currently forces that — recorded here so that M6-3 is told.

**A-M4-1-3 — the `/latest` `ETag`'s `payload_rev` folds in the window resolution's
instant.**
*Proposed by implementation-plan.md **M4-1**, 2026-11-09. An extension of W3 Decision 35 and
§2a.5, not a contradiction.*

*What was found.* §2a.5 defines `payload_rev` as `hash(serialiser_version,
config_generation)` and calls the tag **strong** on the grounds that *"the bytes for a given
value are byte-identical by construction"*. They are not. The payload is also a function of
the window-derived values — wind gust, rain rate, the calendar and multi-day rain totals, the
storm total — and those are re-resolved on every fold, independently of `observed_at`. Three
reachable ways two snapshots share a station and an `observed_at` and differ in their bytes:
a station re-reporting an instant it already sent (which is what the writer's
`ON CONFLICT (station_id, time) DO UPDATE` exists for), `snapshot:warm`'s seed followed by
the same reading arriving through `OnReading`, and a `Degraded`→`Fresh` transition on an
unchanged last reading. A client that sent `If-None-Match` would get a `304` and keep the
older rain totals until the next *distinct* reading.

*What is proposed.* §2a.5 gains: *`payload_rev` is `hash(serialiser_version,
config_generation, window_resolution_instant)`.* The tag's four-part shape is unchanged;
what gains an input is the field §2a.5 already describes as a hash of everything that is not
the reading, which is exactly what the window resolution is.

Latent today, because `Cache-Control` is frozen at `no-store` and no shipped client sends
`If-None-Match` — and fixed now rather than later because M4-2 is what starts emitting the
header. Found by code review.

**A-W6-1 — retire `remoteweather:docs/rewrite/`, and reference the Go repository by URL only.**

*What was found.* The master issue describes the prompt set as version controlled in **both** repos at
`docs/rewrite/`. Measured file by file, the copies have diverged:

| Identical | Differs |
|---|---|
| W0, W2a, W5, W6, W7, W8 | `README.md` (pre-flight item 2's strikethrough), W1, W3, W9 (each had a `§0` appended mid-flight), **W2 (58 lines vs 255)**, **W4 (42 lines vs 248)** |

So the claim that it is "stale across the board" is an overstatement — half the files are identical —
but **the two that are stale are the two that matter**: W2's and W4's prompts were outlines when the
programme started and were written mid-flight from merged output, and the `remoteweather` copies are
still the outlines. Anyone who picked up the reference-repo copy of W4 would be running an agent on
42 lines of a 248-line prompt. That is precisely the failure the master issue's pre-flight item 3
exists to prevent, preserved in the other repository.

*What is proposed.* **W0's R-1 already argues the right answer and this extends it one step.** R-1
says: reference the Go repository by documented URL, no submodule, no required sibling checkout,
because the only access anyone needs is read access. The converse holds too — the *new* repository
should not depend on the old one carrying a copy of its own planning documents.

Proposed as a new decision rather than an edit:

> **D-72. `modernweather:docs/rewrite/` is the single canonical copy of the programme's prompt set.
> `remoteweather` is read-only reference for the Go implementation's *behaviour* and carries no
> planning documents.** The copy under `remoteweather:docs/rewrite/` is deleted, and anything that
> referenced it references the `modernweather` path.

*Why this is the owner's call and not applied here.* `remoteweather` is read-only reference for every
other purpose in this programme, and deleting a directory from it is a write to a repository every
work item was told not to modify. **This document does not make that write.** It is recorded with a
recommendation, and the recommendation is to delete rather than to re-sync: a second copy of a
document set has to be maintained forever, and the evidence that nobody will is in the table above.

*Why this is more than tidiness.* A stale prompt in a repository the master issue names as canonical
is an active hazard, not a documentation nit. It does not look stale — W4's 42-line outline is a
well-formed document that simply stops. The failure mode is an agent run on half instructions, which
is the one failure this programme's pre-flight list explicitly set out to avoid.


**A-M4-3-1 — `html/template`'s escaping is contextual, and §7.1's table is missing two entries.**
*Proposed by implementation-plan.md **M4-3**, 2026-11-10. Not applied: the protocol above reserves
that for a work item that owns the ledger.*

*What was found.* W3 §7.1 specifies one escaper, `GoHtmlEscaper`, as *`&` → `&amp;`, `<` → `&lt;`,
`>` → `&gt;`, `"` → `&#34;`, `'` → `&#39;`, NUL → `&#0;`*, and makes the byte diff against W1's
recorded bodies the acceptance test. Two things about that do not hold, and the byte diff is what
surfaced both — which is exactly what §7.1 said it was for.

**The table.** Go's `htmlReplacementTable` (`go1.26/src/html/template/html.go:54`) also carries
**`+` → `&#43;`**, and spells NUL as **`U+FFFD`** rather than `&#0;` — the HTML5 parse-error
replacement, which the table's own comment quotes. An escaper following the prose would differ from
the recording on any interpolated value containing `+`.

**The contexts.** `html/template` escapes per context, and the page enters a JavaScript one: the
`radar-config` block is `<script id="radar-config" type="application/json">`, and its three values go
through `jsValEscaper` and `jsStrEscaper`. `jsValEscaper` **pads a number with spaces** when its
first or last rune is a JavaScript identifier character (`js.go:219`), so the recorded body reads
`{"lat":  39.9 , "lon":  -105.5 , …}` — two spaces, because the template has one and the escaper adds
one. No HTML escaper produces that. §7.1's single-escaper account is incomplete rather than wrong:
one table is right for the five HTML interpolations, and the golden capture confirms that
`htmlEscaper`, `attrEscaper` and `rcdataEscaper` agree for a plain string.

*What replaces it.* §7.1 gains: *`GoHtmlEscaper` reproduces Go's `htmlReplacementTable` as Go has it,
including `+` → `&#43;` and NUL → `U+FFFD`. The `<script type="application/json">` block is a
JavaScript context and needs `jsStrEscaper`'s table and `jsValEscaper`'s padding, which are a second
escaper rather than a filter; both are asserted against output produced by running Go's
`html/template`.*

*Why this is more than inconvenient.* The decision §7.1 makes — a custom escaper so that the
acceptance test can be a byte diff — is right, and the transcription of the table is what the test
would have been run against. Following the prose would have produced an escaper that passes its own
unit tests and fails the recording, which is the one failure mode the decision exists to prevent.
`crates/mw-web/src/escape.rs` and `src/gojs.rs` carry the measurement; `tests/golden/go-escapes.tsv`
is the capture.

*And one consequence that cost a second finding.* A custom escaper also changes what an escaping
**bypass** means, which M4-3's code review found the hard way. `askama::filters::HtmlSafe` -- the
obvious way to let the one value that must not be escaped through -- implements its bypass only for
askama's own `Html` escaper (`askama-0.16.1/src/filters/escape.rs:229`), so with `GoHtmlEscaper`
registered it silently does nothing and the page shipped `1e&#43;21` for a coordinate in exponent
form. `askama::filters`' `Safe` wrapper is generic over the escaper and is what works. So §7.1's
amendment gains one more clause: *a custom escaper means `HtmlSafe` is inert, and the only bypass
that composes with one is `Safe`.* `ci/layers.toml`'s `no-template-escape-opt-out` rule now forbids
every askama bypass in the workspace and exempts exactly the one file that needs one, with that
file's own test asserting the exemption stays two numbers.

**A-M4-3-2 — `html/template` elides comments, so the recorded bodies are not the template's bytes.**
*Proposed by implementation-plan.md **M4-3**, 2026-11-10. Not applied: as above.*

*What was found.* §7.1's byte diff compares the port's output against W1's recorded bodies, and the
recordings are **short of every comment in the templates**: **42** in
`weather-station.html.tmpl` — 29 HTML comments and 13 JavaScript line comments — and 9 HTML plus 1
CSS comment in `portal.html.tmpl`. `html/template` removes them as part of its escaping analysis,
and the rule is **not uniform**: `escapeText` replaces an HTML or JavaScript *line* comment with
nothing, a CSS *block* comment with a single space, and a JavaScript *block* comment with a space or
a newline. So the Go server never shipped any of them. `askama` does not remove any, and neither
does a static file.

*What replaces it.* §7.1 gains: *the port reproduces `html/template`'s comment elision.* Two
mechanisms, because the two files are different kinds of thing: the template keeps its comments as
askama `{# … #}` comments, which carry the text and occupy no output bytes; and `portal.html`, which
**D-75** makes a static asset, is the Go server's *rendered* output rather than its template source.

*Why this is more than inconvenient.* The alternatives were all worse than recording this. Comparing
with comments normalised out of both sides would make the "byte diff" a diff modulo a transformation
nobody can audit. Reproducing the elision at render time would mean a contextual HTML/CSS/JavaScript
scanner in `mw-web` — `//` inside a `<script>` is a comment and inside `<a href="https://…">` is
not — to *delete* developer comments from a page. And shipping the comments would be a byte change on
100% of page responses, in the same class as the directory listings **DEV-3** removes. The chosen
answer keeps the source documented, the delivered bytes identical, and the test a real byte diff.

**A-M4-3-3 — the asset tree is not per-website, and `web:site:*`'s hard need is unread.**
*Proposed by implementation-plan.md **M4-3**, 2026-11-10. Not applied: as above. **Two proposals, and
the first is already implemented while the second is not** — see below.*

*What was found.* `docs/component-model.md` §2.6 names one website component, `web:site:<id>`,
requiring `LatestSnapshot` **Hard**. Implementing it surfaces two separate problems.

**The asset tree has no website.** The Go handlers for `/privacy` and `/support` read the file
straight out of the asset filesystem and never look at the website (`handlers.go:819`, `:832`), which
W3 §6.3 records as "`/privacy` and `/support` bypass the website check entirely"; the file server
behind `router.PathPrefix("/")` has no website either. Attaching any of that to a per-website table
means three copies of a 2.6 MB tree for three websites, or an arbitrary website owning everyone's
assets, or `/css/weather.css` disappearing when website 1 stops. **M4-3 adds `web:static`** — the
asset tree, `/privacy` and `/support`, no capability needs, planned on every node including one with
no website at all. The proposal is that `docs/component-model.md` §2.6's provider table gain a row
for it; this document does not edit that file, for the reason the protocol above gives.

**The hard `LatestSnapshot` need is declared and nothing reads it.** §2.6's stated reason for the
capability is `/latest`'s zero-query invariant, which is `api:legacy`'s; for `web:site:*` it is
unexplained. W3 §7.3 says in as many words that "`/` and `/portal` depend only on
`(website, config_generation, binary_version)`", and every live value on the page is fetched by the
browser from `api:legacy`'s endpoints. Declaring it has one real consequence: `plan()` refuses a
configuration whose hard need nobody provides, so a node with websites and **no station** would be a
daemon that will not start.

*What replaces it.* §2.6 gains the `web:static` row, and the proposal for the second half is: *drop
`LatestSnapshot` from `web:site:*`'s need set until something on the page reads it.*

*What M4-3 did instead, which is the protocol.* The need is **declared as §2.6 says**, and
`mw_web::register` guards it the way `mw_api_legacy::register` guards the identical problem — it asks
whether a provider will exist and plans no website component when none will, logging which websites
are therefore not served. So a station-less node boots, serves `/metrics`, `/privacy`, `/support` and
the asset tree, and answers `404 page not found` on `/`. That is strictly better than refusing to
start and strictly worse than serving the page, and which of those two the owner wants is the
decision this amendment asks for.

**A-M4-3-4 — DEV-6's refusal is `access_denied`, because rustls chooses the alert.**
*Proposed by implementation-plan.md **M4-3**, 2026-11-10. A correction to §3's ledger rather than to a
decision.*

*What was found.* §3's **DEV-6** entry says that on an SNI miss "the chain is exact → wildcard →
designated default → refuse with `unrecognized_name`". The chain is implemented exactly; the alert is
not `unrecognized_name`. A `rustls::server::ResolvesServerCert` that returns `None` makes rustls send
a fatal **`AccessDenied`** (`rustls-0.23.46/src/server/hs.rs:451`), and the trait offers no hook to
choose the code. Choosing it would mean driving every handshake through `LazyConfigAcceptor` and
building a `ServerConfig` per connection, to change one byte of an alert nothing branches on.

*What replaces it.* DEV-6's wording: *refuse the handshake*. The alert code is the TLS library's.

*Why this is recorded rather than silently different.* §3's own definition is "every place a design
knowingly changes a byte a shipped client could observe", and an alert code is such a byte. What
DEV-6 is **for** is unaffected: the Go version iterates a map and hands out an arbitrary certificate,
which is non-deterministic between runs and produces a browser interstitial that blames the user. A
refusal is diagnosable either way.

**A-M6-1-1 — D-32's crate table has no row for a serial line rate, and this build cannot set one.**
*Proposed by implementation-plan.md **M6-1**, 2026-11-11. **Accepted and applied 2026-11-14** by
GRA-788, which is the work item that owns this ledger entry — the owner asked for it directly, on
the delegation of technical calls of 2026-11-12. From 2026-11-11 to 2026-11-14 the implementation
carried on under the existing decision, which is what the protocol asks for:
`crates/mw-station/src/stream.rs` recorded the gap at its one call site and warned once per
connect, naming the station and the configured rate.*

*What was found.* **D-32** names the default crate for each idiom the daemon needs and **A-W3-1**
added seven rows for idioms it had not named, on the grounds that *"it should be complete for the
daemon, or it is a list that answers some questions and silently delegates others"*. Opening a
**serial port** is such an idiom, and no row names it. M6-1 is the item that needs one: W2a §4a.5
gives both Davis and Campbell `Serial | TcpOut`, and implementation-plan.md M6-1 requires a `socat`
pty pair for exactly that transport.

What a serial port needs beyond a file descriptor is **`tcsetattr`** — the line rate, eight bits, no
parity, raw mode — and reaching it needs either `libc` or a serial-port crate. Neither is in D-32's
table, and `[workspace.lints.rust]` sets `unsafe_code = "warn"`, which `-D warnings` makes an error,
so hand-rolling the `ioctl` is not available either.

*What this build does instead.* **(Superseded 2026-11-14 — see "What was built", below. The three
bullets that follow describe the build up to that date and are kept because the protocol does not
edit a record in place.)** `crates/mw-station/src/stream.rs` opens the character device with
`O_NONBLOCK | O_NOCTTY` and polls it through `tokio::io::unix::AsyncFd`, which needs no new
dependency. **`transport_baud` is therefore recorded and not applied**: the line discipline is
whatever the device already has. Three consequences, each stated at that module rather than left to
be discovered:

- a **`socat` pty pair works completely**, because a pty has no line rate — which is the whole of W3
  **D-83**'s test level, and `tests/emulators/manifest.toml`'s `davis` and `campbell` entries
  exercise it against the pinned emulators;
- a **`ser2net` bridge works completely**, because the bridge owns the line — which is **N-20**'s
  production path, the owner's Campbell being on USB serial with the daemon moving into Kubernetes;
- a **directly attached console works if and only if the line is already configured**, by a `udev`
  rule, by `stty`, or because the kernel default matches. The symptom when it is not is *not*
  silence: the bytes arrive framed wrong, so every Davis packet fails its CRC-16 and every Campbell
  line fails to parse, and `mw_station_frames_total{outcome="undecodable"}` and
  `mw_station_resync_bytes_total` both count every one. `open` also logs the configured rate and the
  fact that it was not applied, once per connect, naming the station.

*What replaces it.* D-32's table gains a row:

| Concern | Crate | Why this one |
|---|---|---|
| Serial port configuration | `tokio-serial` | the `tokio` idiom for a `tcsetattr`-configured port; wraps `mio-serial`/`serialport`, so one crate covers the open, the line rate and the async read |

*Why this is more than inconvenient.* It is the one transport in W2a §4a.5's table that this build
cannot fully serve, and the failure mode is a station whose readings are all refused — which is the
shape of defect this programme keeps finding. It is also a *one-line* fix once the row exists: the
`AsyncFd` path becomes a `tokio_serial::SerialStream` and the `ByteStream` impl is unchanged.

*What was built, 2026-11-14.* `tokio-serial` 5.5 is in `[workspace.dependencies]` with
`default-features = false` — its `libudev` feature links the C library of that name, for the port
enumeration nothing here calls, and every Linux target in `ci/release.toml` is a static musl build
that cannot link it. `ci/layers.toml`'s **`serial-line-rate-only-where-the-line-is-opened`** rule
confines the crate to `mw-station`, because `mw_station::stream::open` is the one place in the
workspace that turns a `Transport` into a `ByteStream` and `mw-output`'s KISS-serial TNC reaches it
from there. `stream.rs`'s `serial` now opens through `tokio_serial`, which is `tcgetattr`,
`cfmakeraw`, 8N1, no flow control, the rate, `tcsetattr`; the "recorded and not applied" `warn!` is
deleted, and what replaced it reads the rate back out of the line with `tcgetattr` and logs that,
because "`tcsetattr` returned" and "the line is at 115200" are not the same claim.

*Seven corrections to this entry's own claims, now that they have been measured rather than read.*

- **The fix is not one line.** The *open* is, near enough; the `ByteStream` impl is **not**
  unchanged. `SerialStream` is `AsyncRead + AsyncWrite`, so `read`/`write_all` go through
  `AsyncReadExt`/`AsyncWriteExt` rather than `AsyncFd::async_io` over `std::io::Read`.
- **It deleted the only `unsafe` block in the workspace**, which this entry did not predict and is
  the better half of the trade. `tokio_serial::SerialStream` is itself an `AsyncFd` over an owned
  `mio_serial::SerialStream`, so the safety argument that lived at `stream.rs`'s registration call
  site is now the dependency's. `unsafe_code` is still `warn` and no `unsafe` was added.
- **`libc` is now in the graph transitively**, through `mio`, `nix` and `serialport`. That was never
  the property anyone wanted — `unsafe_code = "warn"` was — but this entry's framing ("needs either
  `libc` or a serial-port crate") reads as though the two were alternatives, and they are not: the
  crate brings it. What it does not bring is an `unsafe` block in this workspace.
- **One platform regressed, and it is `serialport`'s own recorded note.** On macOS the rate is set
  with the `IOSSIOSPEED` ioctl, which refuses a pseudo terminal with `ENOTTY`
  (`serialport-4.10.1/src/posix/termios.rs:93`), so the pty tests are `cfg`'d off there, where
  before they ran. macOS is supported so that the crate builds on a developer's machine and is not
  in `ci/release.toml`; a real `/dev/cu.*` is unaffected. Linux uses `BOTHER` with `TCSETS2` and
  takes arbitrary rates.
- **The open is now EXCLUSIVE, which this entry never mentioned and is the largest behavioural
  change in the diff.** `serialport`'s builder defaults to `exclusive: true`, so the open takes
  `TIOCEXCL` **and** an exclusive `flock` (`posix/tty.rs:134-137`) where `OpenOptions::open` took
  neither. It is kept — written out as `.exclusive(true)` rather than inherited — because one
  serial line has one reader and two readers *split* the stream rather than each getting it, which
  is the failure `tests/serial_bridge.rs` exists to prove `ser2net` does not have. The cost is
  that **two components naming one `transport_dev` no longer both start**, and nothing refuses that
  at plan time: neither `mw_station::plan` nor `mw_output::plan` compares device paths, so a station
  and an APRS `kiss-serial` sink may both name `/dev/ttyUSB0` and whichever is built second gets
  `EBUSY` and `Backoff`s. **Build order decides which**, so the failure is not deterministic. That
  is worse than it should be and better than the silent split it replaced; refusing it at plan time
  is a separate work item and is raised as one.
- **Applying the rate is an upgrade hazard in the inverse direction, and this entry argued only one
  way.** `transport_baud` has been *mandatory* for a serial station since
  `migrations/config/20261102230000_config_schema_initial.sql`'s `CHECK`, and nothing ever compared
  it against reality — so every existing row holds an unvalidated value. A deployment whose `udev`
  rule or `stty` set the line **correctly** while the column was stale worked before and fails
  after, with exactly the CRC symptom this change removes. The readback cannot catch it: the kernel
  accepts a wrong rate as readily as a right one, so the mismatch arm never fires. Worse on the APRS
  side, where the value may be absent — `mw_output::plan` turns a NULL `aprs_kiss_serial_baud` into
  9600 and now puts that on the radio, so a TNC at 19200 on a `udev`-configured line was beaconing
  correctly and transmits noise after the upgrade. **This belongs in the release note**: on upgrade,
  confirm both columns against whatever was configuring the line.
- **Two smaller facts about what the crate brought.** The `unsafe` block's own argument was that
  `AsyncFd::{new, with_interest, try_new, try_with_interest}` are deprecated in `tokio` 1.54 as
  "unsound due to lack of I/O safety"; `tokio-serial` 5.5 calls `AsyncFd::new`, so the deprecation
  was **relocated to the dependency rather than resolved** — low risk, since `tokio` will not remove
  a deprecated 1.x API, and still clearly worth the trade. And the dependency graph gained six
  crates and **duplicated two majors**: `nix` 0.26.4 (via `serialport`) alongside `nix` 0.31.3 (via
  `mio-serial`), and `bitflags` 1.3.2 alongside 2.13.2. No gate objects — `ci/gates.toml` has no
  duplicate-version check — and `Cargo.toml`'s "one version of each crate across the workspace" is
  about `[workspace.dependencies]` rather than the transitive graph, but the claim is now less true
  than it was and saying so costs nothing.

*The test, which is the part worth arguing about.* GRA-788's criterion 3 asked for a test that would
have caught the old behaviour, and said in as many words that a `socat` pty cannot be it. **That is
half right, and the half that is wrong is the useful half.** A pty has no line *rate* — no bits are
clocked at any speed — but it is a real tty with real `termios`, and Linux stores the rate set on it
and reports it back. Measured, not assumed: a fresh `socat` pty is at **38400**, and `stty -F <link>
115200` then reads back 115200. So
`stream::tests::the_configured_line_rate_is_set_on_the_tty` opens a pty at 115200 — the owner's
Campbell's rate — and asserts `tcgetattr` reports 115200, then drops the port and reopens the **same
device** at 9600 as a negative control. **Against the previous build the first assertion answers
38400**, because that build never called `tcsetattr`. That is a test that fails on the defect, which
is what criterion 3 is for and what **G-1** is about.

What it does not prove is timing, and nothing in this repository does: that the kernel accepted and
recorded 115200 for this device is not evidence that a UART shifted bits at 115200. The only
apparatus that could assert that is a Campbell datalogger on a real serial cable, and this change is
**unproven against hardware** — stated here rather than left for someone to discover, because the
owner's own station is served over the `ser2net` bridge and will not exercise it.

**A-M6-1-2 — D-87 and §4a.6 are both satisfiable for an Ambient secret, by verifying it once per
snapshot rather than once per reading.**
*Proposed by implementation-plan.md **M6-1**, 2026-11-11. An addition to D-87's reasoning rather
than a contradiction of it; **applied in the code**, because `mw_store::registry`'s own comment
assigned the reconciliation to this item.*

*What was found.* Three decisions meet on one line of the admit path and the previous build could not
satisfy all three. **W2a §4a.6** requires Ambient's `?PASSWORD=` to be compared against the
registry's copy; the stored copy is an Argon2id PHC string, as `admin_credential.password_phc` is, so
that a stolen `config.db` does not hand over a reusable secret; **D-33** requires the comparison to
be constant-time; and **D-87** requires `admit` to do no I/O and nothing slow, which is why
`StationRegistry::resolve` is a synchronous `fn` rather than a future. An Argon2 verification per
reading is exactly what cannot go there, and `mw_store::registry`'s comment said so and returned
`Reject::BadSecret` unconditionally — a correct answer that made the Ambient driver unusable.

*What replaces it.* `ConfigRegistry::verify`:

- the **first** upload that presents a secret pays for one Argon2 verification — once per station,
  per published snapshot;
- every **later** upload is a `subtle` constant-time comparison against the secret that verified. An
  Ambient console sends the same string every minute for years, so the cache hits essentially always;
- a **failure is never cached**, and while a station has no verified secret a verification is
  attempted at most once per second. Without that bound, a flood of wrong passwords is a flood of
  memory-hard hashes — one upload's worth of CPU per packet, which is a denial of service against a
  node's own admission path. Refusals inside the interval are `bad_secret` and are counted like any
  other, so the condition is visible;
- **one rejected secret is remembered**, so repeating it is free. Without that, the interval's single
  attempt went to whoever arrived first *in that interval*, and a sender repeating one wrong password
  at rate *R* made the console's correct upload succeed with probability about `1/(1+R)` per
  attempt — an outage lasting as long as the flood, where the bullet above describes a one-second
  wait. With it, a repeated wrong password costs one verification in total and the console's own
  upload finds the next interval's attempt unspent;
- a cache entry **records the hash it was verified against**, and a read whose entry does not match
  the hash in hand is a miss. `publish` also clears the map, which releases a plaintext secret
  promptly, but the clear is not what makes a rotation safe: a reader preempted between loading the
  snapshot and taking the cache lock can repopulate the cache from the superseded snapshot whichever
  order `publish`'s two statements are in;
- the Argon2 verification runs **outside** the cache lock. Holding it across the hash serialised
  every station's constant-time compare behind one station's memory-hard one, on the worker thread
  the HTTP handler is running on — which is the thing D-87 is about.

*What it costs, stated.* Four things, and the first was previously understated.

A verified secret is held in memory in the clear for the life of the snapshot. It is the same
material the console transmits in a query string on every upload, and it is a `SecretString` so it
redacts itself in every `Debug` and log field. It is **not** zeroized on drop, and three earlier
copies of it exist before that one — the raw query string, the decoded pair vector and the frame — so
the redaction is real at the `SecretString` boundary and not before it. Nothing writes it to
`config.db`; "it never reaches a disk" was the earlier wording here and is a stronger claim than a
plaintext secret held for a process lifetime supports, because swap is enabled by default on
Raspberry Pi OS and core dumps exist.

**There is a station-existence oracle in the latency.** An unknown `?ID=` is two map lookups; a
known one with a wrong password reaches Argon2 once per station per interval, which is 19 MiB and
tens of milliseconds on this hardware. So a sender who times the two replies can tell "no such
station" from "wrong password", and probing twice separates "no such station" from "known station,
this interval's attempt already spent" — notwithstanding that both answer `401` with the same body.
Removing it would mean verifying a dummy hash for every unknown claim, which hands an
unauthenticated sender a memory-hard computation per request; that is a worse trade, so the oracle
is accepted and recorded rather than closed. The comments on
`mw_component::UploadStatus::Unauthorised`, `mw_http::status` and `mw_grpc::grpcrx` previously
claimed the two were indistinguishable and now say this.

**A flood of distinct wrong secrets still contends for the interval's attempt.** The remembered
rejection above covers a repeated one, which is the shape a misconfigured console and a naive
flood both take; a sender varying the password each time still takes the budget it wins. The bound
that remains is one Argon2 per station per second, which is the denial-of-service limit this
mechanism exists to hold, and the cost is that a console whose secret has just been corrected may
wait longer than one second while such a flood is running.

**Ambient's `?PASSWORD=` is echoed into a `Location` header** when `https_port` is configured, by
`mw_http`'s plaintext redirector copying `path_and_query()` verbatim — so an intermediary's log sees
it. That is the existing redirector's behaviour rather than this mechanism's, and an Ambient console
will not follow a `301` anyway, so the practical answer for that configuration is that the mount is
unreachable on the plaintext port. Recorded here because this is the entry a reader checks for what
the credential touches.

*Why this is more than inconvenient.* Without it the Ambient driver cannot admit a reading at all,
and §4a.6's entire argument is that the endpoint's authentication is the thing the Go version does
not have. The parameters differ from the administrator password's deliberately —
`mw_store::registry::hash_ingest_secret` has the three reasons — and the PHC string carries its own,
so a hash written by any other tool still verifies.

**A-M6-2-1 — D-32's crate table has no HTTP client, and the uploaders need a trust store.**
*Proposed by implementation-plan.md **M6-2**, 2026-11-11. Not applied: the protocol above
reserves that for a work item that owns the ledger.*

*What was found.* **D-32** names `axum`/`tonic` for HTTP and, as **A-4** already recorded,
lists no HTTP *client*. `crates/mw-output/src/lib.rs` has carried the consequence as a note
since the crate was created: *"The outbound HTTP client is not named by any design document;
it arrives with the work item that writes `Uploader`."* This is that item, and the gap is
real: Weather Underground and PWS Weather are `https` endpoints and a credential crosses the
public internet on that path.

*What was chosen.* **The request is written out**, not taken from a client crate —
`mw_output::https` is one `GET` with no body, a response-head parse, a `Retry-After` parse
and a status classification, all pure functions over byte strings with unit tests. That is
the same answer `mw_station::http` reached and for the same reason, which is recorded there:
`hyper`'s client splits a connection into a sender and a connection future something else
must drive, and on this path that means the `tokio::spawn` `clippy.toml` forbids.

*What that leaves, and the one addition.* A root certificate store, which is the one part of
a TLS client that cannot be written out. `rustls` + `ring` are already pinned for the
listener (**A-W8-3**) and `tokio-rustls` with them; the addition is **`webpki-roots`**, a
pure-data crate with no build step and no transitive dependency beyond
`rustls-pki-types`. `ci/layers.toml`'s `webpki-roots-only-for-the-uploaders` rule confines it
to `mw-output`, because nothing else in this daemon verifies a public endpoint: the SNI
resolver serves certificates this node owns and `sqlx` verifies PostgreSQL against a CA an
operator configured.

*Why the compiled-in store rather than the platform's.* `rustls-native-certs` and
`rustls-platform-verifier` would both answer the same question and are **not** equivalent
here. The daemon ships as a static musl binary to four targets (`docs/release.md`) and into a
container whose `/etc/ssl/certs` is the base image's, so a platform store makes the trust
decision a property of the artefact: it succeeds on one target and fails on another, and the
failure arrives as a handshake error once per upload period. A compiled-in anchor set behaves
identically everywhere, which for two fixed public endpoints is the property worth having.
The cost is stated: a root rotation is a release rather than a package update.

*Why `reqwest` was not chosen.* It would answer the whole problem in one line. Measured
rather than asserted: `cargo add reqwest --no-default-features --features
rustls-no-provider` in a scratch crate resolves **132 packages** and pulls
`rustls-platform-verifier`, `rustls-native-certs` and `hyper-rustls` — a second HTTP client
stack beside `axum`'s server one, and a platform trust decision, against a release matrix
whose ARMv6 member was measured once by `ci/spike/probe` (the evidence behind
`docs/release.md` Appendix A). The figure is for that one feature set and would differ for
another; it is recorded with the command that produced it so the next reader can re-measure
rather than re-derive the trade.

**A-M6-2-2 — an output reads a current value as the newest one-minute mean, because the
capability it is given cannot answer anything finer.**
*Proposed by implementation-plan.md **M6-2**, 2026-11-11. A departure from prose, not from a
decision.*

*What was found.* **D-50** removes `Agg::Last` and says an output needing a metric's current
value reads `LatestSnapshot`. The three uploaders cannot. `docs/component-model.md` §4b's
component table gives `aprs:*`, `pws:*` and `wu:*` exactly `WindowedView` and
`StationRegistry` — no `LatestSnapshot` — and that is not an omission: `LatestSnapshot` is
keyed per `(website, station)` (**D-31**), and an uploader is configured per *station*
through a `station_service` pairing. A station may appear on several websites or none, so
there is no key an uploader could form, and inventing a rule for which website's snapshot a
CWOP packet is cut from would be a second answer to a question **D-31** settled.

*What replaces it.* The six "current" rows across the three tables declare
`Window::Rolling(60 s)` with the metric's own strategy — the newest **complete one-minute
mean**, which is the finest thing `WindowedView` resolves, because `window:rollup` holds
1-minute aggregate buckets and a one-minute window is one bucket.

*Why that is better rather than merely possible.* It is also what the Go uploaders send
today, accidentally: their "latest reading" is a `weather_1m` row selected by
`WHERE bucket > NOW() - INTERVAL '2 minutes' LIMIT 1` with no `ORDER BY`
(`internal/database/client.go:121`), which is a 1-minute average of an arbitrary bucket up to
two minutes stale. The difference is that this one is the newest complete minute,
deliberately, and says so in a declaration a reviewer can check against the service's
documentation. **D-50 is untouched**: nothing here declares `Agg::Last`, and nothing claims
`LatestSnapshot` is the wrong home for an instantaneous value — the surfaces that have a
website key still read it.

**A-M6-2-3 — D-57's layer ORDER holds; its `ServiceBuilder` expression does not compile.**
*Proposed by implementation-plan.md **M6-2**, 2026-11-11. A departure from a mechanism, not
from the decision it implements.*

*What was found.* **D-57** and `docs/component-model.md` §4b.5 both write the stack
literally:

```rust
ServiceBuilder::new()
    .layer(RetryLayer::new(ClassPolicy::new(budget)))  // outermost
    .layer(RateLimitLayer::new(1, period))
    .layer(TimeoutLayer::new(attempt_timeout))         // innermost
    .service(http)
```

Two of those three layers cannot be composed in that order against `tower` 0.5:

- **`tower::retry::Retry<P, S>` requires `S: Clone`** and `tower::limit::RateLimit<S>` is not
  `Clone` — it owns a `Pin<Box<Sleep>>`. So `Retry` cannot wrap `RateLimit`, which is the one
  relationship the decision is about.
- **`tower::timeout::Timeout` erases the error to `BoxError`**, and §4.2's entire mechanism is
  that the retry decision is read from `Classify` on a **typed** error. A layer that turned
  `UploadError::RateLimited { retry_after }` into a `Box<dyn Error>` would make the policy
  above it unable to honour the header, which is W2a Decision 59's whole path.

`tower`'s own answer to the first is `buffer::Buffer`, which makes a service `Clone` by moving
it behind a channel into a **spawned task** — and `clippy.toml` forbids `tokio::task::spawn`
with "spawn through `mw_supervisor::task`, so the task table owns the handle and a leak is
unexpressible", a mechanism a component's own tick cannot reach.

*What replaces it.* The order, kept, with two substitutions and no change to the behaviour:

| D-57 writes | `mw_output::uploader` has |
|---|---|
| `RetryLayer` outermost | an attempt loop in the component body, driven by `ClassPolicy` |
| `RateLimitLayer` | `tower::limit::RateLimitLayer`, unchanged |
| `TimeoutLayer` innermost | `AttemptTimeout`, this crate's own, over `UploadError` |

The loop calls `ServiceExt::ready()` before **every** attempt, and
`RateLimit::poll_ready` is where a permit is spent — so a retry *waits for the limiter*
rather than bypassing it, which is the property D-57 states in words: *"each attempt takes
its own permit."* `uploader::tests::each_retry_takes_its_own_permit_from_the_limiter`
asserts it as elapsed time between attempts under a paused clock, which is the form the
reversed stack would fail in.

*The door a reader will try next, so they do not have to.* A **`Clone`-able** limiter —
shared state behind an `Arc<Mutex<…>>` rather than `tower`'s owned `Pin<Box<Sleep>>` — would
compose with `tower::retry::Retry` and *would* have the property: `Retry::call` binds
`self.service.call(request)` on the line before it clones itself, so the clone the retry path
drives inherits the post-call limiter state. The composed form is therefore available at the
cost of a second rate limiter to audit against `tower`'s, for a saving of about thirty lines
in one component — which is why it was not built, and is the thing to build if a second tower
stack in this process ever needs it.

*Why this is more than inconvenient.* The decision's content is the ordering and the reason
for it — *"reversed, the first failure spends one permit and then hammers the service as fast
as the retry policy allows, which is worst exactly when the remote service is already
struggling"* — and an implementation that followed the expression instead of the reasoning
would have had to choose between inverting the order and spawning a task. `ClassPolicy` is
still generic over `E: Classify` and knows nothing about uploads, so the first stack that
*can* use `tower::retry` has one line to write.

*One half of §4b.5 is not implemented and is not taken.* Adaptive pushback has a
supervisor-side half: *"the limiter accepts an external 'not before T' signal, and the
supervisor skips any occurrence falling before T, counting it as a skip with a distinct
reason."* The limiter's half is here — `Uploader` holds the `Retry-After` across occurrences
and skips one that falls inside it, counted as `mw_output_uploads_total{outcome="held"}` —
but the **supervisor** has no "not before" input and no skip reason beyond `Outcome::Skipped`.
Adding one is a change to `mw-supervisor`, which is M2-3's crate, so it is reported here
rather than taken. The observable difference is where the skip is counted, not whether the
service is obeyed.

**A-M6-2-4 — `aprs:*` does not declare `StationRegistry`, because nothing provides it.**
*Proposed by implementation-plan.md **M6-2**, 2026-11-11. A departure from prose, not from a
decision.*

*What was found.* `docs/component-model.md` §4.2's component table gives `aprs:<device>` the
needs `WindowedView` H **and `StationRegistry` H**; `pws:<device>` and `wu:<device>` get
`WindowedView` H alone, and `docs/service-architecture.md` §9.3's one-line summary gives all
three both. So one of the three components is specified with a need the other two are not.

Declaring it refuses every configuration this program can have. W2a §4.1b gives
`StationRegistry` to a `config:sqlite` component, no work item in
`docs/implementation-plan.md` creates one, and a hard need nobody provides makes `plan()`
refuse the whole configuration (**D-44**). That is not a new discovery: **A-M3-3-1** reached
it for `writer:timescale`, which wants the same handle for a related reason and takes it
*through the registrar* instead.

*What replaces it.* The uploaders declare `WindowedView` **only**, and what they want from a
station — its `station_key`, its coordinates and its `aprs_callsign` — is read from
`config.stations` by `mw_output::plan` at plan time, joined in by
`mw_store::config_db::uploaders`. The APRS encoder therefore holds a validated
`mw_aprs::Position` and `Callsign` from the moment it is built, rather than resolving a
station at run time.

*Why that is better rather than merely possible.* A beacon's position is **configuration**,
not a property of a reading: W4 assumption 24 says as much for the same columns, and the Go
controller's readiness predicate — `device.Latitude != 0 && device.Longitude != 0` — is what
reading it per send looks like. Resolved once, at plan time, a station with no coordinates is
a refusal that names the row (`OutputPlanError::NoPosition`) instead of a worker that is
never started with one `Debugf` nobody reads.

*The need becomes declarable with the item that builds `config:sqlite`*, which is the same
sentence A-M3-3-1 ends on, and at that point there is a second question to settle: what the
uploaders would actually use the handle for, given the registrar already has the rows.

**A-M6-2-5 — `window:rollup` maintains windows only for the stations this node collects
for, and an uploader cannot share any other.**
*Raised by implementation-plan.md **M6-2**, 2026-11-11. Not resolved here — it needs a
decision in M3-4's component or M6-4's, and this records the conflict rather than taking
it.*

*What was found.* `window:rollup`'s station set is the stations this node **collects** for:
`RollupState` is built from `RollupConfig::rollup_stations`, which keeps a station only if it
is enabled and has a `transport_kind`. `WindowedView::resolve` answers
`WindowError::UnknownStation` for anything else, which classifies `Fatal` — so an uploader
paired with such a station fails **every occurrence, forever**, with the reason visible only
in that component's `last_error`.

That is reachable, and the configuration schema **endorses** the case. The `uploader`
table's D-45 / A-W7-2 comment reads: *"A collector that uploads on behalf of a satellite
selects the station and uses its own account."* A station a satellite forwards readings for
has no `transport_kind` on the collector — it is inventory there rather than a component —
so the endorsed configuration is exactly the one the rollup's station set excludes.

*What this item did.* Refused the pairing at plan time, naming the uploader, the station and
the reason (`OutputPlanError::StationNotCollected`), and skipped-with-a-warning the one
sub-case that is a reversible operator action (a station whose `enabled` is 0). A refusal is
not the right long-term answer for a configuration the schema endorses; it is the right
answer to "this component would fail forever", which is the failure mode this programme keeps
finding.

*What would resolve it.* Either `window:rollup` maintains windows for every station whose
readings reach this node's database rather than only those it collects from — the readings
are there, they arrive through `Ingress::accept` — or the schema's comment is narrowed to say
that a collector uploads only for stations it collects. The first is M3-4's component and the
second is a documentation change with W7's consequences; neither belongs to this item, which
is why the refusal names what it cannot do.

**A-M6-3-1 — §6.3 outranks M6-3's done-criterion, so `/alerts` is not built.**
*Decided 2026-11-12 on the owner's delegation of these calls ("make your best calls here and keep
driving the project forward"). Applied: `docs/implementation-plan.md`'s M6-3 criterion is edited in
the same commit, because a criterion nobody can satisfy is worse than no criterion.*

*What was found.* Two texts on `main` disagreed about whether `/alerts` exists.
`docs/implementation-plan.md` said M6-3 is done when "`/forecast` **and `/alerts`** are
conformance-green" — written in `72e6b36`, **2026-10-30**. §6.3 of this document drops `/alerts`
outright ("no working in-repo client; not in either client's endpoint list; W1 D-13 kept it only
because D-37 said *frozen*") and §6.4 puts its four fixtures in the `retired` roster, where they fail
by design — written in `f4d7e7a6`, **2026-10-31**.

*Why §6.3 wins.* Three independent reasons, none of which is a preference:

1. **It is later**, by one day, and it is the owner-informed one — the same commit closed O-6 "under
   the owner's rule" that the question is not "does anything call this" but "does **iOS** call this".
2. **The code already implements it.** The conformance runner's rosters, the capture tool's
   `spec(...)` lines and the gate's green-with-9-failures contract all encode the retirement. The
   plan's criterion is the only artefact that does not.
3. **It cannot be satisfied as written.** `/alerts` has no handler and no store capability. Making it
   conformance-green means building an endpoint that §6.3 deleted for having no client, which no
   amount of reading resolves in the criterion's favour.

*What this means for M6-3.* Build the four Aeris components — two forecast spans, alerts, and the
expired-alert `gc` sweep — and the three snow caches. The alert **components** are in scope: they
fetch and store alerts, which is what makes "alerts failing, forecasts fine" expressible (W2a D-33).
The alert **endpoint** is not. Do not move fixtures between rosters to make a gate agree with a
document; that inversion is G-1.

**A-M5-4-1 — the sunrise/sunset formula stays bug-compatible, and the deviation is named.**
*Decided 2026-11-12 on the same delegation. Nothing is applied: this records a decision not to
change code.*

*What was found.* M5-3 measured the served `sunrise`/`sunset` strings against a textbook Meeus
evaluation over 102 270 comparisons — seven stations, both limbs, every day 2026–2045 — and found
**zero** exact agreements: sunrise 2–7 min late, sunset 1–8 min early, mean absolute difference
**4.52 min**. The cause is in the ported Go formula, not the port: `pkg/solar.CalculateSunriseSunset`
puts the sun's **centre** on the **geometric** horizon and carries no term for refraction (34′) or
solar semidiameter (16′), where a textbook evaluation uses `h0 = −0.8333°`. The rewrite reproduces
the Go values byte for byte, which is D-37 working exactly as specified.

*The decision.* **Do not add the term.** Two reasons:

1. **D-37 freezes this surface and the iOS client deploys independently of the server.** A silent
   4½-minute shift in a displayed time, delivered by a server update the client knows nothing about,
   is precisely the class of change the freeze exists to prevent.
2. **It is not a correctness emergency.** Four and a half minutes of sunrise is below the resolution
   anyone reads these values at, and it has been wrong for the life of the Go daemon.

*What would change it.* After cutover, as its own work item: add the `h0 = −0.8333°` term, record it
as a named deviation with its own fixture, and ship it deliberately rather than folding it into an
unrelated change. `the_served_strings_are_the_stored_go_tables` and
`every_configured_station_diverges_from_a_fresh_meeus_evaluation` are the two tests that would move,
and they are already written — M5-3 built the characterisation before anyone asked for the fix, which
is why this is a decision and not an investigation.

**A-M6-3-2 — `mw-output`'s responsibility widens to cover the Aeris fetchers.**
*Decided and applied 2026-11-12 by implementation-plan.md **M6-3**, under the same delegation as
A-M6-3-1.*

*What was found.* **No document assigns the Aeris components a crate.** W3 §1.2's eighteen crates
are enumerated and closed, and the Aeris components appear in neither `mw-output`'s row ("rate-limited
outputs") nor `mw-pipeline`'s (which names `snapshot:latest`, `window:rollup`, `rain:totals`,
`snow:*` and `live:stream`, and not Aeris). `docs/component-model.md` §4.3 specifies the four
components in full and names no crate; §1.2 names eighteen crates and omits these four. That is the
**G-2** class again — several documents assumed a home into existence and none of them created one.

*What the choice turns on, and what it does not.* The deciding fact is **where the outbound HTTP
client already is**, not the `webpki-roots` rule — and the first draft of this amendment had that
backwards, which is worth recording because the inverted version is circular. `ci/layers.toml`'s
`webpki-roots-only-for-the-uploaders` rule is a one-element `only` list in the same file this item
edits, added by M6-2; permitting a second crate is a one-word change to it. A rule this item could
rewrite is not a constraint on this item. What the rule *is* is a statement of intent — "a crate
CHOOSING to verify a public endpoint" should be a reviewable line — and the question it leaves is
which crate should be making that choice.

`mw_output::https` is the answer because it is the hand-written client: a connect with a TLS
handshake, a request builder, and all three of RFC 9112 §6's framings, written out rather than taken
from a crate for reasons `crate::https`' own module documentation gives at length. This item
generalised it (`get`, `parse_response`) so that an upload and a fetch share one of each. The four
alternatives:

1. **`mw-pipeline`** is where §1.2 puts `snow:*`. It is `mw-station`'s and `mw-output`'s **peer** in
   the `service` layer — an earlier draft of this amendment said "below `mw-station`", which is wrong:
   `ci/layers.toml` gives `mw-pipeline`, `mw-station` and `mw-store` all `layer = "service"`, and the
   gate's rule is a *strictly lower* layer. So it may name neither, and would need its own copy of
   `connect` and its own trust store — a second answer to "which TLS provider does this process use".
2. **A nineteenth crate** (`mw-aeris`, surface layer) costs a `[[crate]]` roster entry and, because a
   surface crate may not name `mw-output`, a second hand-written HTTP client. Two HTTP clients in one
   binary is the accretion W3 Decision 1 exists to prevent, arriving as the fix for a layering
   question.
3. **Hoisting `crate::https` into a lower-layer crate** that both `mw-output` and a new `mw-aeris`
   depend on, moving the `only` list with it. This is the alternative that actually dissolves (2)'s
   objection, and it is the one worth revisiting if a third caller appears: a `service`-layer
   `mw-http-client` beside `mw-station`, or a module inside `mw-station` itself, whose `http` module
   is the sibling that refuses TLS today. It is not taken now because it is a refactor of shipped code
   in service of one new caller, and the cheapest version of it — a module in `mw-station` — would put
   a public trust store in the crate that talks to hardware on the LAN, which is the one placement the
   `webpki-roots` rule's stated reason argues against.
4. **The binary** is where a consumer and a provider meet (§1.3 rule 1), not where protocol code
   lives. `main` has no business parsing a vendor's JSON.

*The decision.* `mw-output`'s declared responsibility becomes **"Third-party HTTPS integrations: the
rate-limited outputs, and the Aeris fetchers"**, in `ci/layers.toml`, in that crate's manifest
`description` and in its module documentation. Its **`forbidden` list does not change** — `sqlx` and
`mw-store` — and it covers the new half for exactly the reason it covers the old one: the fetchers
write through capability handles and name no SQL, which is W2a Decision 50 applied to a component
that happens to read rather than write.

*What the gate does and does not check, stated because the widening sounds like a loosening.*
`xtask/src/layout.rs` reads `responsibility` **only as prose inside a violation message**;
enforcement is `layer`, `forbidden`, `forbidden_source` and the `dep_rule`s, none of which this item
relaxes. So the string is documentation and the boundary is unchanged — which also means the
"narrow test" below has **no checker**, and calling it checkable would be exactly the
degradation-into-code-review-preference that G-4 names. It is a rule for a reader, not a gate.

*What this is not.* It is not a licence to put the next unhomed component here. The test it passed is
narrow and is a question for whoever proposes the next one: *does this component need the outbound
HTTP client this crate hand-wrote, and nothing a lower layer could give it?* A component that needs a
database, a socket to bind or a reading stream fails it, and a third caller for the client is the
signal that alternative 3 above has become the right answer.

**A-M6-3-3 — the snow decomposition, where §4.3 and §2a.7 disagree.**
*Decided and applied 2026-11-12 by implementation-plan.md **M6-3**, under the same delegation.*

*What was found.* Two documents decompose the snow cache and they do not produce the same components.

| | `docs/component-model.md` §4.3 | `docs/service-architecture.md` §2a.7 |
|---|---|---|
| current depth | `snow:depth`, `Every { 30 s }` | `snapshot:snow`, `OnReading { Stations(snow), Latest, 100 ms }` |
| midnight / 24 h / 72 h, smoothing | `snow:events` + `snow:smoothing`, `Every { 15 min }` | same |
| storm total, snowfall rate | *(no row)* | `snow:storm`, `Every { 5 min }` |
| season total | `snow:season`, `Calendar { daily 00:10, Station }` | `snow:season`, `Every { 15 min }` **plus** that `Calendar` |

Both tables have exactly four `snow:*` rows, which is the count A-M3-4-1's note uses ("those are four
components over `Capability::SnowCache`"), and their union is five. **Decision 33's own prose says
three** — "the snow cache becomes three" — and that is a third disagreement, inside the document §4.3
belongs to. It is read as counting the Go `snowcache` controller's three *tickers*, which is what the
sentence is about ("because they have independent periods, failures and rate budgets"): the 30-second
depth refresh, the 15-minute event pass and the 15-minute smoothing pass. `snow:season` is the fourth
and comes from somewhere else entirely — `calculate_total_season_snowfall()`, which the Go handler
calls per request on a cache miss and which scans months — so four components implement a decision
that counted three tickers, and §4.3's table is the one that enumerates them. **D-33** forbids the one row that
is internally impossible: `snow:season` cannot have two triggers, because "one component, one
trigger" is the decision the whole Aeris decomposition rests on.

*The resolutions, both applied.*

A note on the decision labels used below, because this range of them collides. **D-33** here and
throughout this amendment is `docs/component-model.md`'s Decision 33, "one component, one trigger";
`DESIGN.md`'s own D-33 is the secrets decision, which this item also cites, correctly, where it wraps
a credential in `SecretString`. **D-24** is `docs/service-architecture.md`'s Decision 24, "every
calendar quantity is the station's"; `DESIGN.md` D-24 is "a component is a cancellable task". A-M4-1-1
records the collision across these lists as systematic in this range, and these two amendments follow
the convention of naming the document when the label is ambiguous.

1. **`snowstorm` and `snowfallrate` belong to `snow:depth`, not to a fifth component.** §2a.7 calls
   them "the fast-moving members of the expensive set" and gives them five minutes; §4.3 gives
   `snow:depth` thirty seconds and the fast tier. They are **cheaper than the column `snow:depth`
   already computes** — the storm total is a dual-threshold walk over 24 hourly buckets and the rate
   is a subtraction between two 1-minute buckets, against `snowtoday`'s walk over up to 288 — so a
   separate component would be strictly staler for no saving. §4.3's four rows are preserved, and
   §2a.7's reasoning about *why* those two values are not in the slow tier is honoured: they are in
   the fastest tier there is.
2. **`season` has two writers, which is §2a.7's own request expressed within D-33.** The `Calendar`
   component is `snow:season` exactly as §4.3 writes it, and §2a.7's 15-minute re-derivation is
   performed by **`snow:smoothing`**, which has just rewritten the series the season total is a
   function of and holds the handle already. Without it a storm shows `snowtoday = 12"` beside a
   `snowseason` from yesterday, on a frozen response, for up to a day. `mw_component::SnowPatch` is
   what makes two writers of one column safe rather than a race: a patch writes the columns its
   component owns and coalesces every other against the stored value, inside the `DO UPDATE`.

*`snapshot:snow` is not built, and that is the third resolution.* §2a.7's cheap tier is an
`OnReading` component feeding a snapshot; §4.3's is a column of `snow_totals_cache`. The **frozen
handler decides it**: `mw_api_legacy::snow` reads all seven numbers from `SnowCache` and nothing else,
because the snapshot "holds nothing for a station outside the ten-minute window — which is exactly
the gauge this endpoint has to answer for — and its `snowdepth` is in millimetres while this
endpoint's is in inches". M4-2 settled that when it wrote the handler; §4.3's column is therefore the
operative design and `snapshot:snow` has no consumer.

**A-M6-3-4 — four measured deviations from the Go snow and Aeris arithmetic.**
*Applied 2026-11-12 by implementation-plan.md **M6-3**. None of the four is reachable on a frozen
response; each is recorded because it changes a stored number or a detected event.*

1. **The storm total uses the station's own base distance.**
   `calculate_storm_snowfall` carries `base_dist := 1798.0; -- TODO: Pass this as parameter from
   application`, so under Go **every** station's storm total is computed against the owner's own gauge
   height. For any other gauge that is a depth series offset by the difference between the two, and —
   because the bare-ground filter is `snowdistance < base - 2` — a reading that is bare ground for one
   gauge is 299 mm of snow for the other. The station's own `snow_base_distance_mm` is used instead.
   Asserted by `the_storm_total_is_computed_against_the_stations_own_calibration`.
2. **`snowtoday` and `snowseason` are measured in the station's zone.**
   `get_new_snow_midnight` hard-codes `AT TIME ZONE 'America/Denver'` in the SQL and
   `getSeasonStart` builds 1 October in the **daemon's** `time.Local`. Both are **D-24** ("every
   calendar quantity is the station's") and the same defect W3 §2.5's consequence 2 records for the
   rainfall rollovers. A gauge with no usable `timezone` is now a `SnowPlanError::MissingTimeZone`
   naming the station rather than a silent fallback. The season's date is **1 October**, which is what
   `ComputerTypeSmoothed` — the controller's default, and therefore the number the Go daemon actually
   serves — uses; the SQL functions say 1 September (migration 019) and 1 July (migration 011), and
   both have had no caller since migration 022 dropped them.
3. **An empty window leaves a total alone instead of writing zero.**
   Go's computers `return 0, nil` for a window with no readings in it, so a gauge that stops reporting
   at noon reports no snow today by one o'clock. Here a window with no usable reading contributes **no
   column**, and the stored value persists — which is also what reproduces W1 Decision 16 ("an offline
   gauge returns its last known depth indefinitely", fixture `snow-device-stale`) without a recency
   filter anywhere. A gauge that has never reported gets **no row at all**, keeping
   `snow-device-never-reported`'s "no row" distinguishable from "a row of zeros".
4. **The three longer event windows are detected at the hour level, not the 5-minute level.**
   `EVENT_WINDOWS` reads the 5-minute level for the 24-hour window alone; 72, 168 and 744 all read
   the hour level.
   PELT over an RBF kernel is `O(n²)` in both time and memory in the bucket count, and Go reads
   `weather_5m` for every window up to 168 hours — which is 2 304 buckets, a **42 MiB** kernel matrix
   and a **21 MiB** distance vector, inside a 10-second budget, on a release target `ci/spike/probe`
   measured at ARM1176. The faithful version does not complete there, and a detector that times out is
   a chart with no bands. The 5-minute series is median-filtered with a **61-bucket** kernel first —
   five hours — so the signal being segmented is already hourly; at the levels chosen the largest
   window is 768 buckets (a 4.7 MiB prefix sum). **No frozen response reads `snow_events`**, which is
   what makes this item's call to make. Two further changes to the same code are *exact*
   reformulations rather than deviations, and are asserted as such: the segment cost is evaluated from
   a 2-D prefix sum (`the_prefix_sum_cost_is_the_submatrix_sum`) because the kernel's diagonal is
   exactly 1, and the dynamic program carries two arrays rather than a map of partitions
   (`the_dynamic_program_agrees_with_an_exhaustive_search`).

5. **A `success: true` response whose one group carries no periods is refused, where Go stored `[]`.**
   Two upstream shapes look alike and only one is a Go panic: `"response": []` is
   `response.AerisForecastData[0]` on an empty slice, which takes the daemon down, and refusing it is
   fidelity. `"response": [{"periods": []}]` does not panic there — the icon loop iterates nothing,
   `json.Marshal` of an empty slice yields `[]`, and the caller stores it, so Go's `/forecast` serves
   `{"lastUpdated": <now>, "data": []}`. Refusing it leaves the previous forecast and its older
   `lastUpdated` in place, which is the better behaviour on a cache and is a **byte difference on a
   frozen body** either way. A maintenance window is how it is reached.
6. **`serving_from_inches` answers zero for a non-finite input, where `mmToInchesWithThreshold`
   answers the non-finite value.** `math.Round(NaN)` is `NaN`, which `encoding/json` refuses to
   marshal — so the Go path's answer to this input is a `500` on a frozen endpoint rather than a
   number. Unreachable from a stored reading (the bare-ground filter drops every non-finite distance)
   and recorded because the function is `pub` and the guard is not faithfulness.

*Three smaller ones, recorded for completeness.* The `alerts` table's primary key is
`(station_id, alert_id)` where Go's unique index is `alert_id` alone — so a county warning covering
two configured stations is no longer a row whose `station_id` is rewritten every fifteen minutes by
whichever component fetched last. And `alerts.data` stores the upstream object **as fetched** rather
than Go's ten-struct subset, because nothing reads the column and dropping `includes.counties`,
`place.name` and the polygon is the irreversible direction. Both are database-only: no response on any
surface reads this table (§6.3, A-M6-3-1). And a **repeated `id` within one alerts response** is
de-duplicated last-wins before the write, which reproduces the outcome of Go's per-alert
`First()`-then-`Create`/`Save` loop: the rows reach PostgreSQL as one
`INSERT ... FROM UNNEST(...) ON CONFLICT ... DO UPDATE`, and a source-row collision on the arbiter is
`21000: ON CONFLICT DO UPDATE command cannot affect row a second time`, which no retry recovers — so
without it every alert in that fetch would be lost for as long as the warning was active.

*One predicate moved **towards** Go rather than away from it*, which is worth noting beside the
deviations: a paired station at exactly `(0, 0)` is refused, because Go's enablement test is
`device.Latitude != 0 && device.Longitude != 0` and the schema permits the value. Treating it as a
location would serve a Gulf of Guinea forecast as that station's on a frozen endpoint, indefinitely.
DEV-17 records the same predicate mismatch for the sun times and notes it is unobservable there; here
it is observable, so Go's predicate is reproduced — with a refusal naming the station in place of
Go's silent skip.

---

**A-M7-1-1 — the save preview takes a change, not a partial configuration document.**
*Proposed by implementation-plan.md **M7-1**, 2026-11-13. Not applied: the protocol above
reserves that for a work item that owns the ledger.*

*What was found.* W5 §2.4 writes the preview's body as a partial configuration document —
`POST /api/v1/config/preview { "stations": [ ... ] }` — and W5 §8.7 requires the **deletion**
consequence list ("this will leave your website with no weather to show") to come from the
same call. Those two cannot both hold. A document that omits a station is indistinguishable
from a document that was not asked about one, so "delete Backyard" has no representation in
the shape §2.4 shows, and §8.7's dialog would have to be driven by a second endpoint whose
answer could differ from what the delete then did.

*What was chosen.* The preview takes the **same value the mutating route builds from its own
body** — a discriminated `Change` union with `put_station`, `delete_station`, `put_website`
and the rest. `ConfigAdmin::apply(change, Commit)` is then one function: it opens a
transaction, applies the change's statements, reads the configuration back **out of that
transaction**, runs `plan()` and diffs against the live plan. A preview rolls back; a save
commits. Nothing else differs.

*Why that is the stronger shape.* M7-1's done-criterion is "a save's preview names exactly
the components that then restart". Under a partial-document preview that is an agreement two
code paths have to keep — one that interprets a document and one that interprets a form
submission — and the first divergence is silent, because a preview that under-reports looks
exactly like a change that did less. Under one path it is not an agreement at all. The cost
is that a client assembling a multi-resource edit previews it as several changes rather than
one document, which no screen in §7.1 does.

---

**A-M7-1-2 — the management API's TypeScript client is emitted by `cargo xtask`, not by
`openapi-typescript`.**
*Proposed by implementation-plan.md **M7-1**, 2026-11-13. Not applied: see above.*

*What was found.* W5 §4.2 names `openapi-typescript` as the emitter for
`generated/ts/mgmt-api.d.ts` and `openapi-fetch` as the client wrapper. The gate that checks
the result, `mgmt-schema-drift` (**D-57**), is also a developer command — W8's rule is that a
gate you cannot reproduce locally gets worked around instead of fixed — and `cargo xtask
mgmt-schema --check` would then need `npm` on every machine and in a CI job whose only other
requirement is a Rust toolchain.

*What was chosen.* The `.d.ts` is emitted from the OpenAPI document by `xtask/src/mgmt.rs`,
in `openapi-typescript`'s own shape, so `openapi-fetch` consumes it unchanged and W5 §4.3's
mechanism 2 (`tsc --noEmit` against the committed `.d.ts`) works exactly as specified. What
§4.2 actually requires of the file is that it is **types only** — no runtime, no class per
endpoint, so the client costs about 2 KB against §6.5's budget — and a `.d.ts` is types only
whoever writes it. The emitter fails loudly on a schema shape it does not know rather than
falling back to `unknown`, because a silent `unknown` is the one outcome that would make
mechanism 2 useless.

*What this does not change.* The schema itself is still derived from the handlers by
`utoipa`, which is §4.1's requirement and the reason the gate is separate from
`codegen-drift`. M7-3 may replace this emitter with `openapi-typescript` once the UI build
exists and `npm` is in that job anyway; the committed file is the contract either way.

---

**A-M7-1-3 — forecasts and alerts get their own resource rather than a `services/` entry.**
*Proposed by implementation-plan.md **M7-1**, 2026-11-13. Not applied: see above.*

*What was found.* W5 §1.4 maps `GET /api/config/controllers/{type}` onto
`GET /api/v1/services/{service}` for `pwsweather`, `weatherunderground`, `aprs`,
`aerisweather` and `snowcache`. The first three are `uploader` rows; `aerisweather` is an
`aeris_account` row, which the schema deliberately separates (its migration's own argument:
Aeris is four fetching components rather than an egress); and `snowcache` has no
configuration at all under the new schema — the snow components plan from the station's
`snow_base_distance_mm`.

*What was chosen.* `/api/v1/services/{service}` covers the three upload accounts, and
forecasts and alerts are `/api/v1/forecasts`. An upload account's interval, endpoint and APRS
symbols have no field in common with a client-id pair and a station list, so one union would
be §4.4's "object with named fields" turned into an object where every field is optional —
which is the shape §4.4 exists to avoid. There is no `/services/snowcache`, because there is
nothing to configure. The **page** is one page under "Share my data" either way, which is
what §1.4's mapping is actually about.

**A-M7-2-1 — `D-32`'s crate table has no general-purpose hash, and two of W5 §3's secrets
need a fast one.**
*Proposed by implementation-plan.md **M7-2**, 2026-11-13. Not applied: see above.*

*What was found.* D-32 names `argon2` for password hashing, `subtle` for constant-time
comparison and `secrecy` for secret wrappers, and no digest. W5 §3.3 and §3.4 both need
**SHA-256**: the session secret is stored as its SHA-256 and so is the setup code, and
`migrations/config/20261102230000_config_schema_initial.sql` has already fixed that answer in
two `CHECK (length(token_hash) = 32)` constraints.

*What was chosen.* `sha2 = "0.10"` is added to `[workspace.dependencies]`, nameable by
`mw-api-mgmt` only. Argon2id is the wrong primitive for both of these twice over. It is
**unnecessary**, because the input is 32 bytes of CSPRNG output and there is nothing to
brute-force; and it is **actively wrong**, because `AuthStore::resolve` runs on every
management request, so m=64 MiB/t=3 would make one page load allocate 64 MiB three times over
on the 512 MB target W3 §5.7 sizes against.

`sha2` rather than a second candidate for the reason that makes this amendment cheap: it is
**already in this dependency graph at this version** — `sqlx-core` depends on 0.10 — so the
lock file and the shipped binary gain nothing at all, and nothing new enters the audit
surface. The two `CHECK`s above also rule out Blake2 and Blake3 at their default digest
lengths, so the schema had already made the choice.

Two smaller feature changes travel with it and are not separate amendments, because neither
adds a crate. `secrecy` gains `features = ["serde"]`, without which W5 §3.2's "a
`SecretString` from the moment it is deserialised" cannot be a `#[derive(Deserialize)]`; and
`argon2` gains `features = ["std"]`, without which `OsRng` — the CSPRNG every secret in §3
comes out of — is not in scope. The second resolved anyway through another crate's feature
unification, which is exactly the kind of accident that breaks on an unrelated dependency
bump, so it is asked for explicitly.

**A-M7-2-2 — a 10,000-entry list of the most common passwords is almost entirely
unreachable behind a twelve-character minimum.**
*Raised and resolved by implementation-plan.md **M7-2**, 2026-11-13.*

*What was found.* **O-10** accepts W5 OQ-W5-2's recommendation (b): "ship a ~10k-entry list
(~80 KB in the binary) as a non-blocking warning". The list that recommendation was written
against is `SecLists/Passwords/Common-Credentials/10k-most-common.txt`. §3.5 — the *same
section* — also sets a **minimum length of 12**, enforced before the warning can be reached.

Of the 10,001 entries in that file, **ten** are twelve characters or longer. The other 9,991
describe passwords the length rule has already refused, so shipping it as written would have
put 72 KB in a binary that ships to a Raspberry Pi in order to warn about ten passwords.
Nothing in the design is wrong; the two paragraphs were written to be read separately.

*What was chosen.* The same decision, over a list that can actually be reached: the
**10,000 most common passwords that are at least twelve characters long**, taken in frequency
order from `xato-net-10-million-passwords-1000000.txt`, lowercased and deduplicated.
`crates/mw-api-mgmt/data/README.md` carries the provenance and the exact derivation, and
`auth::tests` asserts both invariants the lookup depends on — sorted bytewise, and every entry
within the length policy — so a replacement list that breaks either fails the build rather
than silently matching nothing.

**Two measured costs, stated rather than estimated.** The file is **137 KB**, not the ~80 KB
OQ-W5-2 estimated, because the entries that can be reached are the long ones. And the lookup
is a bisection over the embedded bytes with **no index built and nothing retained**, because
the shapes that would have been easier — a `HashSet<&str>`, or a `Vec<&str>` in a `OnceLock` —
are ~500 KB and ~160 KB of heap held for the life of the process, which is a worse trade than
the file itself on the machine §3.2's parameter choice was sized for.

**A-M7-2-3 — W5 §3.4's setup code is 32 random bytes in its sentence and 160 bits in its
worked example.**
*Raised and resolved by implementation-plan.md **M7-2**, 2026-11-13.*

*What was found.* §3.4 step 1 says "**32 random bytes**, base32-encoded without ambiguous
characters". Step 2's worked example shows `K7QF-2MXB-9HTD-4RWP-6JSA-1NVC-8YEK-3LZU` — eight
groups of four, which is 32 *characters* and 160 bits, not 32 bytes. Thirty-two bytes is 52
base32 characters. The two cannot both be right.

*What was chosen.* **The sentence**, because two places state a byte count and they agree with
each other: §3.4's own step 1, and `admin_setup_token`'s column comment in the initial
migration ("SHA-256 of 32 random bytes, base32-encoded without ambiguous characters for
retyping"). The example is illustrative and its characters are not in any base32 alphabet
anyway — it contains `L` and `U`, which Crockford's excludes. So the code is 52 characters in
thirteen groups of four rather than eight, and the banner is correspondingly wider.

The alphabet is **Crockford's** (`0123456789ABCDEFGHJKMNPQRSTVWXYZ`), which is the published
answer to "without ambiguous characters", and the typed code is canonicalised before it is
hashed: hyphens dropped, lower case folded up, `I` and `l` folded to `1` and `O` to `0`. `U`
is **not** folded to `V` — it is excluded from the alphabet rather than confused with
anything, so a `U` is a typo the operator should see fail rather than a character the server
guesses at.

**A-M7-2-4 — §3.4's automation path is not built, and the reason is that the deployment it
exists for reads the console.**
*Proposed by implementation-plan.md **M7-2**, 2026-11-13. Not applied: see above.*

*What was found.* §3.4 offers a third first-run path beside the console code and the CLI:
`MW_INITIAL_ADMIN_USERNAME` / `MW_INITIAL_ADMIN_PASSWORD`, or the preferred
`MW_INITIAL_ADMIN_PASSWORD_FILE`, consumed once at first start. It is marked **optional**, and
the design states its own trade-off plainly: an environment variable ends up in shell history,
in the compose file, in `docker inspect` and in `/proc/<pid>/environ`.

*What was chosen.* **Not built.** Two reasons, and the second is the one that matters.

It is explicitly optional, so not building it is inside the design rather than a deviation
from it — this entry exists so that it is a decision somebody can reverse rather than an
omission somebody has to notice.

And the deployment it exists for already has the console path. The code goes to **stderr**
(W5 **D-27**), and in a container stderr *is* the aggregated log: `kubectl logs` and
`docker logs` show the banner. The trust boundary that makes the console path sound — "only
someone with console or service-log access has the code" — is exactly the boundary a
`kubectl exec`/`kubectl logs` grant already crosses, so a provisioning tool that can set an
environment variable can also read the code it would have replaced. What the environment
variable would add is not reachability but **unattended** first run, and nothing in this
programme needs one: `modernweather reset-setup-token` covers the case where the banner has
scrolled away, and M7-5's cutover is one host with an operator in front of it.

If it is wanted later, the shape is fixed by §3.4 and the work is small: read the `_FILE`
variant preferentially, consume it in the same transaction `claim` already uses, and log the
trade-off on the startup line.

**A-M7-2-5 — `created_ip` and the claim's audited source IP need the serving path to supply
a peer address, which `mw-http` was not doing.**
*Raised and resolved by implementation-plan.md **M7-2**, 2026-11-13.*

*What was found.* W5 §3.3 puts `created_ip` on every administrator session and §3.4 point 5
requires the successful claim to be logged with its source IP. `mw_http::socket::serve_one`
served `axum::serve(listener, router)`, which installs no connect information — so no handler
could learn the peer's address, and both columns would have been permanently `NULL`.

*What was chosen.* `serve_one` serves
`router.into_make_service_with_connect_info::<SocketAddr>()`, on **every** socket rather than
only the management one: the alternative is two serving paths that differ in what a handler
may ask for, and the cost is one `ConnectInfo<SocketAddr>` in each request's extension map.

One implementation detail is worth recording because it looks like a workaround and is not.
`axum` implements `Connected<IncomingStream<_, TcpListener>>` for `SocketAddr` only for its
own `TcpListener`, and this module's TLS listener is its own type; the orphan rule forbids
adding that impl in `mw-http`, because both the trait and `SocketAddr` are foreign. The hook
`axum` provides for exactly this case is its blanket impl over `TapIo<L, F>`, which holds for
any listener whose `Addr` is already the connect info — so the listener is tapped with a
no-op closure. That is the supported path, not a trick, and it costs one indirect call per
accepted connection.

**A-M7-2-6 — the CORS allowlist is read at startup, and no route edits it.**
*Raised and resolved by implementation-plan.md **M7-2**, 2026-11-13.*

*What was found.* §3.5 replaces `Access-Control-Allow-Origin: *` with an exact-match
allowlist, default empty, and says "a configurable allowlist exists under
`System → Advanced`". `management_cors_origin` is in the schema and has been since M2-2.
**No route in M7-1's surface writes it**, and `mw_api_mgmt::Change` has no variant for it, so
there is nothing for this item to wire a form to.

*What was chosen.* The allowlist is read once, by the boot sequence, and handed to the gate.
The consequence is stated rather than hidden: **an origin added to the table takes effect at
the next start.** That is already the whole of an operator's path — there is no endpoint, so
it is "edit the row and restart" either way — and it follows from the management router being
a root resource that every re-plan puts back rather than rebuilds (`crate::admin::plan_from`).
Adding the endpoint is additive under §4.4 and belongs with the `System → Advanced` page,
which is **M7-3**'s.

The default is the absence of data rather than a sentinel, which is what makes "empty means
same-origin only" true of a fresh install without anything having to seed it. The value that
reaches `Access-Control-Allow-Origin` is **the entry from the list**, never the presented
`Origin` header: a reflected origin is the defect §3.1 lists with more steps.

**Two smaller calls taken by M7-2 and recorded here rather than as their own amendments**,
because neither contradicts a design document.

**`modernweather reset-password` requires `--config <path>`.** §3.6 writes the command
without it. It is required for the reason the daemon's own `--config` is required and that
`crates/modernweather/src/args.rs` already states as a test: a program that invents a path
creates a configuration database somewhere nobody looked, and then reports that it has no
administrator.

**Terminal echo is turned off with `stty`, not with a terminal crate.** D-32's table is the
list of crates this program may use and no terminal crate is in it, and the alternative is an
`ioctl` — which means `libc` and `unsafe` in a workspace whose lints warn on both. `stty` is
in `coreutils` and is present on every machine where a person is sitting at a console, which
is the only situation that code path runs in. **If it is missing, the password is echoed and
the operator is told so**, rather than the command refusing: §3.6's whole purpose is that the
locked-out case has an answer, and a recovery command that will not run on a minimal system
does not have one.

### O-10's four calls, as implemented

Recorded together because **O-10** was one review rather than four, and because the
implementation of each is where a reader will look for it.

| | Question | Answered | Where it is |
|---|---|---|---|
| OQ-W5-1 | loopback until claimed? | **(a) yes**, widening after | `crate::config::Config::management_addr`, with `--setup-listen` as §3.4's headless opt-out. The widening is a **re-plan**: `claimed` is an input to the bind address, so a successful claim produces a plan whose `http:listener` fingerprint differs and the socket moves without a restart |
| OQ-W5-2 | a breached-password list? | **(b)** ship one, warn, never block | `mw_api_mgmt::auth::is_breached`, over the list A-M7-2-2 describes. The warning lands in §8.5's `warnings` array on the claim and the password change, and in a console line on `reset-password` — both surfaces, so the rule cannot be evaded by using the other one |
| OQ-W5-3 | any API token in v1? | **(a) none** | Nothing. There is no token table, no `Authorization` parsing and no bearer path; §3.7 fixes the shape in advance so adding one later cannot regress into "the cookie is the token" |
| OQ-W5-5 | how much configuration history? | **(a) one step** | `POST /api/v1/config/revert`, which **M7-1 already shipped**, scoped to this process rather than to a session because a session-scoped one could not be written before sessions existed. M7-2 changes nothing here; the entry is kept so the answer is not looked for twice |
