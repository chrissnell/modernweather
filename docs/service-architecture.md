# ModernWeather — service architecture

**Work item:** W3. **Produces:** this document. **Depends on:** W0 (`docs/DESIGN.md`), **W2a**
(`docs/component-model.md`), and the merged outputs of W1 (`docs/legacy-api-contract.md`,
`contract/`), W2 (`docs/metric-registry.md`, `generated/`) and W8 (`docs/release.md`, the CI gate).
**Consumed by:** W4, W5, W7, W6.

This document owns the ***how***: crate and module boundaries, trait signatures at every pipeline
boundary, buffer sizes with their reasoning, per-sink policies, the supervisor's mechanics, the error
taxonomy, logging and observability, the single HTTP listener, how the frozen website is served, the
testing strategy, and the efficiency boundaries.

It does **not** own what a component *is* (W2a), what a metric *is* (W2), the schema (W4), the
operator UI (W5), the collector wire protocol (W7), or the frozen surface itself (W1). Where those
documents decide something, this one implements it; where they are silent on mechanism, this one is
the mechanism. It contains trait and type **sketches** and no implementation code.

---

## Read this before §1 — five things that are not where you expect them

### 1. No `DESIGN.md` decision is contradicted. Four are *extended*, and they are flagged, not applied.

Per the amendment protocol, nothing below silently overrides a shared decision. The four proposed
amendments — all additive — are written out in full in **Proposed amendments to `DESIGN.md`** at the
end:

| | Decision | What this document proposes |
|---|---|---|
| A-W3-1 | **D-32** (crate list) | six idioms this document needs are not in the table: TLS, templating, asset embedding, proto parsing, observability, and systemd readiness. Additions, not substitutions. |
| A-W3-2 | **D-29** (sources → fan-in → transform → fan-out → sinks) | name the fan-out point: it is a **durable spool**, not a channel. Folds in W2a's proposed D-30a. |
| A-W3-3 | **D-38** (static JS + a JSON config endpoint) | record the delivery mechanism (an inline `<script type="application/json">` block, with `/config.json` additive) and the nine values, completing W1's A-3. |
| A-W3-4 | **D-31** (transport is orthogonal to role) | "one listener" should read **one router and one route table**; machine-to-machine gRPC peers may still need a second socket. |

This document also **carries forward W2a's proposed D-30 amendment** (rate-limited outputs are
scheduled producers, not stream sinks) rather than re-litigating it. §2 and §4 are written as though
D-30a/D-30b are already in force, which is what W2a asked for.

### 2. Two measured numbers in the W3 prompt and in `docs/rewrite/README.md` are wrong, in the direction that matters

Both were re-counted against `remoteweather` at the commit W1 captured.

**`/latest` costs 12 TimescaleDB round trips, not 7.** The prompt lists seven calls and misses that
three of them are multi-query functions:

| Call | Round trips | Source |
|---|---|---|
| `fetchLatestReading` | 1 | `restserver/handlers.go:263` |
| `CalculateDailyRainfall` | **2** | `controllers/rainfall_calc.go:24,38` |
| `CalculateMonthlyRainfall` | **2** | `rainfall_calc.go:64,77` |
| `CalculateYearlyRainfall` | **3** | `rainfall_calc.go:104,118,128` |
| `get_rainfall_with_recent()` | 1 (+1 on error) | `handlers.go:308` |
| `calculate_storm_rainfall()` | 1 | `handlers.go:335` |
| `CalculateRainRate` | **1 — not in the prompt's list at all** | `rainfall_calc.go:153` |
| `calculate_wind_gust()` | 1 | `handlers.go:354` |
| `GetSunTimes` | 1, against SQLite | `handlers.go:367` |

So the steady-state cost is **12 TimescaleDB round trips plus 1 SQLite query per request**, and at
the measured 17,740 requests that is roughly **213,000 queries**, not 124,000. The conclusion is
unchanged and strengthened; only the figure moves. §2a is designed against 12.

**`/snow` costs 4 round trips on a cache hit and 8 on a miss, not 7.** The handler reads
`snow_totals_cache` first (`handlers.go:494`) and falls back to the four PL/pgSQL functions only on a
miss, then always runs `calculate_storm_snowfall()` and `calculate_current_snowfall_rate()` live.
The shape the prompt describes — *"reads the snow cache and then computes six more values live
anyway"* — is right; the count is for the miss path. Either way `/snow` is the more expensive of the
two hot paths per request, and §2a treats it as a first-class snapshot rather than a special case.

### 3. One premise in §7 of the prompt does not hold

`AboutStationHTML` is passed to both templates and **consumed by neither** — `weather-station.html.tmpl`
has no `{{ .AboutStationHTML }}` action, and `js/weather-app.js.tmpl` receives it and ignores it (W1
§9 records the same thing from the other direction). It reaches clients only as `about_text` on
`/stationinfo`, where Go's JSON encoder escapes it. So "injected into pages as raw unescaped HTML" is
not the current behaviour, and §7 specifies the trust boundary for what *is* happening plus a rule
that keeps it that way.

### 4. Every open question has been decided

The first draft of this document raised ten questions for the owner. The owner asked for the calls to
be made rather than referred, so they are all decided — and two of them turned out to rest on a fact
rather than a judgement and were settled by reading the code (the conformance runner's header
handling, and whether `grpc_port` is a real configuration column; it is). The **Open questions**
section now records each call with its one-line reason and **what would reverse it**, because a
decision taken on somebody else's behalf without a reversal trigger is one nobody can revisit safely.
Decisions 88–97 carry the same ten calls in the numbered list.

One of the ten is work for another work item rather than a choice here: **Decision 89**, proto3 field
presence, needs a line per field in W2's generator — without it a collector stores `0.0` for a metric
a satellite never reported.

### 5. The vocabulary here is internal

`Ingress`, `Spool`, `Cursor`, `RouteTable`, `SniResolver`, `WindowedView`, `Envelope` are names chosen
for Rust source. **W5 §0 owns the user-facing translation table and an internal term on screen is a
bug** (D-36). What this document owes W5 in exchange is in §5: every log record and every metric is
structured, so a plain sentence can be rendered from it without parsing a string.

---

## 1. Crate and module layout

### 1.0 What is already fixed, and what is left

W8 committed the workspace root with `members = ["xtask", "crates/*"]`, `[workspace.package]`,
`[workspace.lints]`, `release`/`dist` profiles, `rust-toolchain.toml` at 1.96.0, and an empty
`crates/.gitkeep`. The PR gate runs `--workspace --locked`. **Nothing in this section changes the
root manifest except to populate `[workspace.dependencies]`**, which W8 deliberately left empty for
this work item. The glob member pattern means a new crate needs no root edit at all.

W2 committed `generated/` with a do-not-edit header on every file and `cargo xtask codegen --check`
gating drift. §1.2 answers the only question left open there: how hand-written modules consume it
without hand-written code leaking back in.

### 1.1 A cargo workspace of many small crates, not one crate of modules

**Decision: a workspace, eighteen crates.** The deciding argument is not compile time or
taste. It is that this document is required to say, for each unit, **what it is forbidden to depend
on** — and a module boundary inside one crate cannot express that. Any module in a single crate can
write `use crate::store;`; a crate that does not list `mw-store` in its `[dependencies]` *cannot*,
and `cargo build` says so. The anti-goal in §1.5 is a type that accreted an HTTP server, an HTTPS
server, a gRPC server, SNI, middleware, database queries and config reload. Every one of those
accretions is a line somebody could add to a `Cargo.toml` and a reviewer would see.

Three secondary consequences, all of which are load-bearing elsewhere in this document:

- **Pure crates can be linked alone.** W2a assumption 7 requires that a golden test for the APRS
  encoder link it without a database or a transport. That is a property of `mw-aprs` having no such
  dependency, which only a crate can have.
- **The dependency graph is a testable artefact.** §1.4's layer check is a few lines of `xtask`
  reading each manifest; the equivalent over modules would be a parser for `use` statements.
- **Cross-compilation for four architectures gets parallelism for free**, which matters because
  `platform.yml` builds all four.

The cost is eighteen `Cargo.toml` files and the discipline of `[workspace.dependencies]`. Both are
one-time.

### 1.2 The crates

`H`/`S` mark whether other crates depend on this one *hard* or not at all. "Forbidden" is the part
that matters; it is enforced by §1.4.

| Crate | Single responsibility | Forbidden to depend on |
|---|---|---|
| `mw-types` | Domain types. Adopts `generated/rust/{metrics,reading,aggregates}.rs`; adds `StationKey`, `StationId`, `WebsiteId`, `OriginId`, `Seq`, `Window`, `Agg`, `Envelope`, `Provenance`. | everything. No `tokio`, no `sqlx`, no `axum`, no `serde` attribute naming a legacy key. |
| `mw-proto` | The **wire** types. `prost` + `pbjson` over `generated/proto/reading.proto`, plus exactly one `TryFrom<&wire> for Reading` and one `From<&Reading> for wire`. | `sqlx`, `axum`, `mw-store`, `mw-api-legacy`. |
| `mw-codec` | **Pure** frame → `Vec<PartialReading>` decoders: Davis LOOP, Campbell ASCII, WLL conditions, Ambient query string, AirGradient JSON, snow-gauge proto. | `tokio`, `std::net`, `std::fs`, any clock. A codec that can read the time is not golden-testable. |
| `mw-aprs` | **Pure** APRS/CWOP protocol: information-field encode *and* decode, base-91 position/telemetry, course/speed/altitude compression, passcode. The twelve functions W2a §4b.4 moves out of the controller. | `tokio`, `sqlx`, any transport. |
| `mw-component` | W2a's model as code: `ComponentSpec`, `Trigger`, `Capability`, `Need`/`Provision`, `Tick`/`OnReading`/`Serve`, `Cx`, lifecycle states, `ComponentError`, **and every capability handle trait**. | every concrete component crate. It must not know that a station or a database exists. |
| `mw-supervisor` | `plan()`, the hard-edge digraph, the single timer heap, the task table, capability watches, the `Spool` and the fan-out. | every concrete component crate (see §1.3 for how inversion works). |
| `mw-obs` | `tracing` setup, the reload handle, the ring layer, the dedupe layer, the access-log layer, the metrics recorder. | everything but `mw-types`. |
| `mw-store` | `sqlx` against PostgreSQL and SQLite. Owns the pools and implements `ReadingStore`, `ForecastStore`, `AlertStore`, `SnowCache`, `ConfigStore`, `ConfigAdmin`, `StationRegistry`. Reads `generated/rust/aggregates.rs` for column names. | `axum`, `tonic`, `mw-api-legacy`, `mw-web`. The storage layer must not be able to name an endpoint. |
| `mw-pipeline` | The `OnReading` path: `Ingress`, the enrich stage, `Sink`, `SpooledSink`, `snapshot:latest`, `window:rollup`, `rain:totals`, `snow:*`, `live:stream`. | `mw-store` (it holds *handles*, not implementations), `axum`, `tonic`. |
| `mw-station` | `Transport`, `Session`, `StationIdentity`, identity resolution, and the station components. | `mw-store`, `mw-api-legacy`, `mw-web`. |
| `mw-http` | **The one listener.** `axum` + `tonic`, `RouteTable` mounting, host routing, the SNI resolver, ALPN, compression, CORS. | `mw-store`, `mw-pipeline`, `mw-api-legacy`, `mw-web`. It mounts route tables; it does not know what is in them. |
| `mw-api-legacy` | **The compatibility adapter.** The 133-key DTO, the duplicate keys, the unit split, the zero-fill, the four error-body conventions, msgpack. | `sqlx`, `mw-store`. It is handed `LatestSnapshot` and the read stores as capability handles. |
| `mw-web` | The public website: `askama` templates, embedded assets, the inline config block, `/config.json`. | `mw-store`, `mw-proto`, `mw-api-legacy`. |
| `mw-grpc` | `WeatherV1` and `WeatherAppsV1` as `tonic` services over the shared router. | `mw-store`. |
| `mw-output` | Rate-limited outputs: `OutputEncoder` impls for APRS/CWOP, PWS Weather, Weather Underground, and the one shared `Uploader<E>` body. | **`sqlx` and `mw-store`** — W2a Decision 50, made structural. |
| `mw-collector` | W7's uplink and gRPC receiver. | `mw-api-legacy`, `mw-web`. |
| `mw-api-mgmt` | W5's server half: 46 routes over `ConfigAdmin` + `ComponentControl`. Stubbed by this work item, designed by W5. | `mw-store` directly (it uses `ConfigAdmin`), `mw-pipeline`. |
| `crates/modernweather` | **The binary.** Parses arguments, opens SQLite, installs the subscriber, calls `plan()`, runs the supervisor. The only crate that may depend on all the others, and the only one that may depend on `anyhow`. | — |

W8's `ci/release.toml` already names `crates/modernweather/Cargo.toml` and a binary called
`modernweather`; that is satisfied exactly, and `[package.metadata.deb]` / `[package.metadata.generate-rpm]`
go into that manifest verbatim from `docs/release.md` §6.

### 1.3 Three boundary rules that do the actual work

**Rule 1 — capability handles are traits in `mw-component`; implementations live in the providing
crate.** This is what lets `mw-pipeline` consume `ReadingStore` without depending on `mw-store`, and
it is the reason there is no dependency cycle anywhere in the table above. A consumer names
`dyn ReadingStore`; a provider implements it. The binary is the only place the two meet.

**Rule 2 — the supervisor does not know its components.** `mw-supervisor` exposes
`plan(&Config, &Registrars) -> Result<Vec<ComponentSpec>, PlanError>`, where each component crate
contributes one `fn register(cfg: &Config, out: &mut Vec<ComponentSpec>)`. The binary assembles the
list. So adding a component crate never edits the supervisor, and the supervisor cannot grow a match
arm per component kind — which is the shape `managers/controller.go` took, down to the
`if con.Type == "management" { continue }` special case.

**Rule 3 — a component is handed only what it declares.** W2a §2.3 resolves handles once, in
`build`. The structural consequence this document depends on repeatedly: `api:legacy` requires
`LatestSnapshot` and is **not** handed a `ReadingStore`, so the `/latest` handler cannot query — not
"does not", *cannot* (W2a Decision 35). The same rule is what makes W2a Decision 50 real for
`mw-output`, and it is reinforced at the crate level because `mw-output` does not depend on `sqlx`
at all.

### 1.4 Enforcing "forbidden to depend on"

Three mechanisms, all of which run in the existing gate without a workflow change:

1. **Cargo itself.** A missing `[dependencies]` entry is a compile error. This covers most of the
   table for free.
2. **`cargo xtask gate layout`**, declared in `ci/layers.toml` and added to `ci/gates.toml` as a
   gate armed by `crates/mw-types/Cargo.toml` existing. It asserts:
   - each crate's declared layer only depends on lower layers (`types` < `proto|codec|aprs|obs` <
     `component` < `supervisor|store|pipeline|http|station` < `api-legacy|web|grpc|output|collector|api-mgmt`
     < `bin`);
   - the forbidden list above, by crate name, read from the same file;
   - **no crate other than `mw-types` mentions the path `generated/`**;
   - **no crate other than `mw-http` names `axum::serve`, `tonic::transport::Server` or
     `TcpListener::bind`** — the direct structural answer to seven listener-owning sites;
   - **no crate other than `crates/modernweather` depends on `anyhow`** (§4).
3. **`clippy.toml` `disallowed-methods`** for `tokio::spawn`, `tokio::task::spawn`,
   `std::thread::spawn` and `tokio::time::interval`, allowed only inside `mw-supervisor::task` and
   `mw-supervisor::clock`. This is the lint half of §3's leak invariant.

The gate's `gates` job already prints the deferred set into every run's summary, so `layout` appears
there until `crates/mw-types` lands and then runs for good.

### 1.5 How these boundaries prevent another `restserver.Controller`

The anti-goal is one Go type owning nine responsibilities. Mapped onto this layout:

| What the Go `Controller` owns | Where it lives here | What stops it coming back |
|---|---|---|
| an HTTP server | `mw-http` | only `mw-http` may bind a socket (§1.4) |
| an HTTPS server | `mw-http`, same router | TLS is a resolver, not a second server (§6) |
| a gRPC server | `mw-grpc`, mounted on the same router | gRPC is a codec (D-31); `mw-grpc` cannot bind |
| the gRPC service implementation | `mw-grpc` | it cannot depend on `mw-store`; it holds handles |
| SNI certificate management | `mw-http::tls` | reads paths from `ConfigStore`; no other crate may |
| middleware | `mw-http`, as `tower` layers | per-route-table flags, not a global mutable field |
| database queries | `mw-store` | `mw-http` may not depend on it |
| config reload | the supervisor's plan diff (W2a §2.5) | nothing calls "reload"; `ConfigAdmin` writes, the supervisor re-plans |
| the lifecycle of everything else | the supervisor | a component is a `Tick`/`OnReading`/`Serve` impl with no constructor side effects |

The general statement: **accretion requires a type that can reach the thing being accreted.** A
component is handed handles for exactly its declared needs and lives in a crate that cannot name the
others, so the reach does not exist. The failure mode that remains is somebody adding a dependency
to a `Cargo.toml`; that is one reviewable line and `cargo xtask gate layout` fails it.

### 1.6 How generated code is consumed, and how it stays out of hand-written modules

`generated/` is unchanged. Exactly one file in the workspace references it:

```rust
// crates/mw-types/src/generated/mod.rs — the entire seam, three lines.
#[path = "../../../../generated/rust/metrics.rs"]    pub mod metrics;
#[path = "../../../../generated/rust/reading.rs"]    pub mod reading;
#[path = "../../../../generated/rust/aggregates.rs"] pub mod aggregates;
```

`#[path]` rather than `include!`, deliberately: `generated/rust/reading.rs` opens with the inner
attribute `#![allow(clippy::struct_excessive_bools)]`, and a macro expansion cannot contribute an
inner attribute to the enclosing module, so `mod reading { include!(...) }` fails to compile.
`#[path]` loads the file *as* the module body and inner attributes work. The relative depth is from
the containing file's directory, so `../../../../` is the repository root.

`reading.rs` says `use super::metrics::MetricKey;`, which resolves because the three are siblings
under `generated`. W2's assumption 14 asks W3 to choose the timestamp type: **`chrono::DateTime<Utc>`
stands**, which is what the generator already emits. `sqlx` supports it, `chrono-tz` is needed anyway
for W2a §1.6's `Calendar` time zones, and introducing `time::OffsetDateTime` would mean two date
libraries in one binary for no gain.

**Four rules keep hand-written code out of generated code and vice versa:**

1. The `generated` module is **private to `mw-types`**. `mw-types` re-exports `Reading`, `MetricKey`,
   `METRICS`, `MetricMeta`, `Tier`, `Level`, `AggregateColumn` and `Strategy` from its root. No other
   crate can name `mw_types::generated::...`, so a future "just add a field to the generated struct"
   has nowhere to land.
2. `cargo xtask gate layout` fails any crate but `mw-types` that mentions `generated/`.
3. **Nothing hand-written is ever placed in `generated/`.** W2 already gates drift; the layout gate
   adds the converse (no file in `generated/` without a do-not-edit header).
4. Anything that would be a hand-written *addition* to a generated type is an `impl` block in
   `mw-types` on the generated type, in `crates/mw-types/src/ext.rs`, which is where a reviewer looks
   for "what did we add to `Reading`". Today that is: `Reading::enriched_with`, `Reading::is_empty`,
   and the `Display` for `MetricKey`.

`mw-proto` is the one exception to "generated output is committed": its `build.rs` runs `prost-build`
into `OUT_DIR` at compile time rather than into `generated/`. Two reasons. First, committing it would
mean two generators writing one directory and a second drift check that carries no information its
input's check does not already carry — `generated/proto/reading.proto` is itself drift-gated, so the
prost output is a pure function of a gated input. Second, the proto is parsed with **`protox`**, a
pure-Rust parser, so no `protoc` binary is needed at build time; that is load-bearing for the four
cross-compiled targets and for building on a Pi.

### 1.7 Where the legacy-compat adapter lives, and what its boundary prevents

`mw-api-legacy`. It owns, and is the only place that owns:

- `LegacyWeatherReading` (133 JSON keys), `SnowReading`, `StationInfoResponse`, `AlmanacData`,
  `StationData`, `RemoteStationResponse` — with their exact `serde` renames, including the
  inconsistent ones (`otemp`, `bar`, `winds`, `period_rain`, the four camelCase `moon*` keys);
- **the duplications**: `ohum`/`outhumidity` and `dayrain`/`rainday` emitted from one value (Q-1);
- **the unit split**: `snowdepth` in millimetres on `/latest` and inches on `/snow` and `/span`
  (Q-4), with `mm_to_inches_with_threshold`'s 0.1-inch noise floor;
- **the zero-fill**: exactly one function, `fn zf(v: Option<f32>) -> f32 { v.unwrap_or(0.0) }`, used
  by the DTO constructors and nowhere else;
- the `omitempty` semantics on the eighteen keys that have them, and the preserved hazard on
  `about_text` / `weather_device` (W1 Decision 11);
- the four error-body conventions and their exact bytes;
- `format=msgpack`, including `/span` ignoring it.

**What the boundary prevents, and how.** Legacy naming cannot leak into the core because `mw-types`
contains no `serde` attribute at all — `Reading` derives `Debug, Clone, PartialEq, Default` and is
not serialisable to JSON. A hand-written `#[serde(rename = "otemp")]` in `mw-types` would have to
first add `serde` to its manifest, which the layout gate rejects. Zero-filling cannot leak into
storage because the domain type is `Option<f32>` end to end and the only `unwrap_or(0.0)` in the
workspace is `zf`, asserted by a grep in the layout gate. That matters beyond tidiness: **the writer
stores `NULL` for a metric the station did not report** (D-3's density premise, D-9's nullable
`ADD COLUMN`), and the frozen contract's zeros are produced one layer later, at the DTO. The two
facts are now in two crates and cannot be confused.

The direction is one-way and checked: `mw-api-legacy` depends on `mw-types`; `mw-types` has no
reverse dependency and no knowledge that a legacy surface exists.

### 1.8 Where `contract/conformance/` belongs — decided: it stays its own workspace

W1 made it a standalone crate with its own empty `[workspace]` so it could run against the Go binary
before any Rust daemon existed, and left the layout to W3. **It stays separate.** Three reasons, in
order of weight:

1. It is blocking test infrastructure using `ureq`, not `tokio` (W1 Decisions 7–8; D-32's scope
   amendment A-4). Joining the root workspace would put a second HTTP client and a second TLS
   configuration into `cargo build --workspace --all-targets --locked` on every PR *and* into the
   four-architecture cross matrix, for a crate that never ships.
2. `[workspace.lints]` at the root is written for the daemon. The runner would inherit
   `missing_debug_implementations` and the raised clippy groups for no benefit.
3. Its value depends on it being able to target the Go daemon, which is a different binary with a
   different lifetime. A crate whose purpose is to be version-independent should not share a lockfile
   with the thing it validates.

What changes: nothing structural. `cargo xtask conformance --target-binary <path>` — the command
`ci/gates.toml` already declares — shells into `contract/conformance` with `--locked`, so the nested
`Cargo.lock` is committed and reviewed like any other. The root `cargo build --all-targets` not
seeing it is correct, and the gate builds it separately, which it already does.

Two small follow-ups this implies, recorded so they are not lost: W8's `conformance` gate arms on
`tests/conformance/manifest.toml` while W1 shipped fixtures under `contract/`. One of the two paths
must move; **recommend changing the `arms_when` path in `ci/gates.toml` to
`contract/fixtures/`**, because moving W1's artefacts would break the provenance paths recorded in
`contract/legacy-v1.json`.

### 1.9 Module layout inside a crate

One convention, stated once so it is not re-decided sixteen times:

```
crates/<crate>/
  src/lib.rs        re-exports; no logic
  src/error.rs      the crate's `thiserror` enum and its `Classify` impl (§4)
  src/plan.rs       `fn register(cfg, out)` — the pure half: specs, manifests, floors
  src/<area>/...    the impure half
  tests/            integration tests; golden fixtures beside them
```

`plan.rs` existing in every component crate is what makes `plan()` pure and the CI check over the
shipped example configurations possible (W2a Decision 2). A crate with no `plan.rs` contributes no
components, which is true of `mw-types`, `mw-proto`, `mw-codec`, `mw-aprs` and `mw-obs` — and that is
a useful thing to be able to see at a glance.

---

## 2. The reading pipeline — the `OnReading` path

**Scope.** This section implements the `ReadingStream` capability and the `OnReading` trigger. It is
one trigger among six. Of roughly thirty components in the design, five are reading-driven:
`writer:timescale`, `uplink:collector:<peer>`, `snapshot:latest`, `snapshot:snow` and `live:stream`.
Everything else is periodic or a long-running service. Reading this section as the shape of the
system is the mistake W2a §4.5 warns about.

### 2.1 The shape, end to end

```
 station components                                                    the fan-out point
 (Serve or Every)                                                              │
      │ admit(batch, meta)                                                     │
      ▼                                                                        ▼
 ┌──────────────────────────────────────────────────────────────┐   ┌────────────────────┐
 │ Ingress            resolve identity → authenticate (W2a 4a.6)│   │ Spool              │
 │ (mw-pipeline)      → enrich (pure, §2.4) → assign seq        │──▶│ append + group     │
 │                    → build Envelope                          │   │ commit, segmented  │
 └──────────────────────────────────────────────────────────────┘   └─────────┬──────────┘
                                                                              │
           ┌──────────────────────────┬───────────────────────────────────────┤
           ▼                          ▼                                       ▼
    durable cursors            watch::Sender<Arc<Envelope>>        broadcast::Sender (1024)
    writer:timescale           snapshot:latest                     metrics tap, future Lossy
    uplink:collector:<peer>    snapshot:snow (tier 1)
                               live:stream
```

The spool **is** the fan-out point. That is the one structural commitment in this section and it
falls directly out of W2a Decisions 61 and 62: if a source may never be blocked by a sink, and a
bounded in-memory queue means loss when full, and an unbounded one means an OOM when the database
stalls, then durability has to live on disk at the point where fan-out happens. `Latest` and `Lossy`
subscribers tap the same append — they are notified from the spool's append path, not from a
separate channel the source also has to write.

### 2.2 `Envelope` and provenance

```rust
// mw-types
pub struct Envelope {
    pub station:     StationKey,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub provenance:  Provenance,
    pub reading:     Arc<Reading>,
}

/// Origin, path and sequence. Constructed only by `Ingress::admit` and `Envelope::forward`.
pub struct Provenance {
    origin: OriginId,       // the node that first admitted this reading. Never rewritten.
    path:   PathSet,        // every OriginId this envelope has traversed, including `origin`
    seq:    Seq,            // monotonic per (origin, station); gaps are visible, reuse is not
}

impl Provenance {
    pub fn origin(&self) -> OriginId { self.origin }
    pub fn seq(&self) -> Seq { self.seq }
    pub fn has_visited(&self, node: OriginId) -> bool { self.path.contains(node) }
    // There is no `set_origin`, no `set_path`, no `Default`, and no public constructor.
}

pub struct LoopDetected { pub node: OriginId, pub path: PathSet }

impl Envelope {
    /// The ONLY way to produce an envelope for forwarding to a peer.
    pub fn forward(mut self, hop: OriginId) -> Result<Self, LoopDetected> {
        if self.provenance.path.contains(hop) {
            return Err(LoopDetected { node: hop, path: self.provenance.path.clone() });
        }
        self.provenance.path.insert(hop);
        Ok(self)
    }
}
```

**Loop prevention is structural, not a filter.** `Provenance`'s fields are private to `mw-types` and
there is no constructor outside `Ingress::admit`. The only way to obtain an envelope addressed at a
peer is `forward(peer)`, which refuses when the peer is already on the path. So an A→B→C→A cycle is
not *detected*; it is **unrepresentable** — the type that would have to exist cannot be built. Two
independent guards then both have to fail for a loop to occur, and the second one is a compile-time
property.

Contrast with what is there now. The Go collector calls `LoadConfig()` **on every single reading**
and compares the reading's `StationName` against the names of local `grpcreceiver` devices
(`internal/storage/grpcstream`). Because `convertToInternalReading` preserves the *origin's* name,
the guard fires only on a naming coincidence; name two nodes' devices differently and a mutual uplink
is not stopped. Multi-hop relay is not expressible at all, because there is nowhere to record a hop.
Here a hop is a set insert, a relay is a chain of `forward` calls, and the config is not consulted.

`PathSet` is a 64-bit bitset over locally-known `OriginId`s with an overflow list — a fleet of more
than 64 nodes is not a case this system has, and the overflow path exists so that it is not a
correctness cliff. W7 owns the wire encoding of `Provenance` and the `OriginId` allocation; this
document owns only that it is carried, immutable except through `forward`, and that
`ReadingFilter::NotFrom(origin)` is implemented as `!prov.has_visited(origin)`.

### 2.3 `Source` and `Sink` — exact definitions

There is no `Source` *trait* in the sense of something the supervisor polls, and saying so is part of
the design: a station driver is a `Serve` or `Every` component (W2a §4.1b) that **pushes**. What it
holds is an ingress handle, resolved once in `build` from a `Need { ReadingStream, Hard, Write }`:

```rust
// mw-component — the capability handle. mw-pipeline implements it.
#[async_trait]
pub trait Ingress: Send + Sync {
    /// Admit decoded readings. Resolves identity, authenticates, enriches, assigns a
    /// sequence number, and durably accepts the batch before returning.
    ///
    /// Cancellation: cancel-safe at any await point. A cancelled `admit` either appended
    /// the whole batch or none of it — the spool's group commit is the atom.
    ///
    /// Backpressure: the ONLY thing this can wait on is the local spool's fsync. It never
    /// waits on a sink, a database, a peer or a subscriber.
    async fn admit(
        &self,
        batch: &[PartialReading],
        meta:  &FrameMeta,
    ) -> Result<Admitted, AdmitError>;
}

pub struct Admitted { pub accepted: u32, pub rejected: u32, pub seq_high: Seq }

#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    /// Identity resolution refused the frame (W2a §4a.6). Counted by reason, not fatal.
    #[error("rejected: {0:?}")] Rejected(Reject),
    /// The spool is at its configured ceiling. The batch is lost; see §2.7.
    #[error("spool full ({depth_bytes} bytes)")] SpoolFull { depth_bytes: u64 },
    /// The spool cannot be written. Fatal for the process (W2a Decision 62).
    #[error("spool write failed")] Spool(#[from] SpoolError),
}
```

`Sink` is the delivery side. W2a Decision 4 gives the supervisor `OnReading::readings(batch, cx) ->
Result<()>`, which cannot express a partial ack — and a partial ack is exactly what a durable sink
needs so that a cancelled or timed-out delivery replays only what was not stored. So `Sink` is a
narrower trait and a generic adapter implements W2a's trait for it:

```rust
// mw-pipeline
#[async_trait]
pub trait Sink: Send {
    type Error: Classify + Into<ComponentError> + std::error::Error + Send + Sync + 'static;

    /// Deliver a batch in order. Returns the length of the PREFIX that is durably handled.
    /// Returning `Acked(0)` with `Ok` is legal and means "try again later" — it is how a
    /// sink expresses soft backpressure without failing.
    ///
    /// Cancellation: must be cancel-safe. On cancellation mid-batch, whatever prefix was
    /// durably handled is lost from the ack's point of view and simply replays. A sink
    /// must therefore be idempotent over a replayed prefix, which for `writer:timescale`
    /// is `ON CONFLICT (station_id, time) DO UPDATE`.
    async fn deliver(&mut self, batch: &[Envelope], cx: &Cx) -> Result<Acked, Self::Error>;
}

pub struct Acked(pub usize);

/// Implements W2a's `OnReading` for any `Sink`, owning the cursor and the ack.
pub struct SpooledSink<S: Sink> { sink: S, cursor: Cursor }

#[async_trait]
impl<S: Sink> OnReading for SpooledSink<S> {
    async fn readings(&mut self, batch: ReadingBatch, cx: &Cx) -> Result<(), ComponentError> {
        let Acked(n) = self.sink.deliver(batch.as_slice(), cx).await
            .map_err(|e| { /* classify, count, log with the chain */ e.into() })?;
        self.cursor.ack(n).await?;          // durable; the only place an offset advances
        if n < batch.len() { cx.metrics().partial_ack(n, batch.len()); }
        Ok(())
    }
}
```

`Latest` and `Lossy` components implement W2a's `OnReading` directly — there is no ack to express, so
wrapping them would be ceremony. `snapshot:latest`, `snapshot:snow` and `live:stream` are in that
group.

**How failure propagates.** `deliver` returning `Err` becomes a `ComponentError`, which the
supervisor turns into `Failed { error, at, attempt }` and then `Backoff`. For an `OnReading`
component there is no period to be the backoff, so §3.4 specifies the schedule: exponential with full
jitter, base 1 s, cap 60 s, and the cursor does not advance, so the same batch replays. A durable
sink that is failing is therefore visible as three things at once — component state `Failed`,
`spool_lag_envelopes` growing, and `last_error` with its full chain — and loses nothing.

### 2.4 Stages, buffer sizes and the reasoning

| Stage | Mechanism | Size | Why that size |
|---|---|---|---|
| ingress queue | **none** | — | A queue in front of the spool only relocates the loss decision. Measured load is ~4 stations at 0.5 Hz; there is nothing to smooth. |
| spool group commit | `Mutex` + batched `fsync` | window **50 ms**, max **64** appends | At 2 readings/s a 50 ms window almost never coalesces, so the cost is 50 ms of added latency on a path whose fastest consumer is a 2-second snapshot. The cap exists for a replay burst from a reconnecting satellite (W7), where coalescing 64 appends into one fsync is the difference between 64 and 1 disk syncs. |
| spool segment | append-only file | **8 MiB** | A segment is deleted when every registered cursor has acked past it, so the segment size is the granularity of reclamation. At a worst-case ~1 KiB encoded envelope and 2/s, 8 MiB is ~68 minutes of history per segment — small enough that a brief outage reclaims promptly, large enough that rotation is rare. |
| spool ceiling | config, default **2 GiB** | — | At 2 envelopes/s that is **~12 days** at a worst-case ~1 KiB encoding and **~100 days** at the measured 30% column population (~120 B). Chosen so the ceiling is never the thing that fires first: `spool_depth_bytes` is a gauge an operator watches rise for days beforehand. §2.7 states what happens if it is reached anyway. |
| durable cursor read-ahead | batch from the spool | **256 envelopes** | PostgreSQL's wire protocol caps a statement at **65,535 bind parameters**. At the registry's 216 allocated columns the hard ceiling is 303 rows per multi-row `INSERT`; 256 is the power of two below it, leaves headroom for the dimension columns, and makes the batch a single round trip. |
| `writer:timescale` deadline | W2a trigger field | **5 s** | One round trip for 256 rows against a local TimescaleDB is single-digit milliseconds; 5 s is three orders of magnitude of slack, so a deadline miss means the server is hung rather than slow. |
| `uplink:collector` deadline | W2a trigger field | **10 s** | A satellite's link is the variable; 10 s accommodates a TCP retransmit without making a hung peer invisible. |
| `Latest` publish | `tokio::sync::watch` | capacity **1**, by construction | Coalescing is the semantics, not an optimisation: the snapshot and the SSE stream want current conditions. A `watch` never blocks the sender and never lags. |
| `Lossy` publish | `tokio::sync::broadcast` | **1024** | ~8.5 minutes of slack at the measured rate. A subscriber that falls further behind is broken, and `lagged_total` says which one. `broadcast` rounds to a power of two regardless. |
| enrich stage | inline, synchronous | — | Pure arithmetic over one reading (§2.5). Measured in the Go implementation as microseconds; a stage boundary would cost more than the work. |

**The policy for a slow consumer, per sink kind.** It differs because the cost of loss differs, and
that is the whole of the justification:

| Policy | Mechanism | On a slow consumer | Why this is right for this kind |
|---|---|---|---|
| `Durable` | spool cursor | falls behind **on disk**; `spool_lag_envelopes{cursor}` rises; nothing upstream notices | A reading lost from the database is history that cannot be recovered. D-4 makes raw permanent, which only means something if raw is complete. |
| `Latest` | `watch` | intermediate values are **dropped by design**; only the newest is kept | The `/latest` payload is replaced every ~2 s. An intermediate value that nobody read is not a loss; serialising it would be the waste. |
| `Lossy` | `broadcast(1024)` | oldest dropped, `lagged_total{component}` incremented | A sampled metric costs a pixel. Making it durable would mean a cursor and a segment for data whose value expires in seconds. |

No policy can block a source, because none of the three is on `admit`'s path. `admit` returns once
the append is committed; the cursors, the watch and the broadcast are all read *from* the spool by
their own tasks.

**The Go reference failure, precisely.** `internal/managers/storage.go:212`:

```go
for _, e := range s.Engines {
    e.C <- r        // blocking, unguarded, one per engine, no select, no ctx
}
```

Four distinct defects, all closed structurally rather than fixed:

| Defect | Measured | Closed by |
|---|---|---|
| a blocking send per engine back-pressures the serial read loops | distributor channel is 20-deep (`storage.go:37`), the TimescaleDB engine 10-deep (`timescaledb.go:35`), and **grpcstream's is unbuffered** (`grpcstream/grpc.go:44`) | nothing writes to a sink channel on the admit path at all |
| no `select`, no context check, so a blocked send hangs shutdown forever | — | cancellation is observed at the cursor's await point; a stalled sink is aborted after its grace deadline (§3.6) |
| `s.Engines` is mutated without a mutex while the distributor iterates it | `AddEngine`/`RemoveEngine` append to and splice the slice | there is no shared collection: `Spool::open_cursor(id)` returns a `Cursor` owned by exactly one task |
| `RemoveEngine` **closes a channel the distributor may be sending on** (`storage.go:138`) | — | removal is `Drop` of the `Cursor` plus a durable `cursor_dropped` record; nobody closes anything anyone else holds |

### 2.5 The transform/enrich stage, and the rule that decides where a derived value lives

The Go version has no such stage, which is why wind chill is computed in a forwarder binary with a
`TODO` admitting it is missing elsewhere, AQI in a package, rainfall accumulation in HTTP handlers,
rainfall windows in PL/pgSQL, and snow in both a Go package *and* PL/pgSQL. The inconsistency is not
an accident of history; there was no rule. Here is the rule, in two clauses:

> **R1 — storage.** A derived value is **computed once at ingest and stored as a column** if and only
> if it is a pure function of a **single reading** plus static station metadata. Those are computed
> in the enrich stage, before the spool, so storage, the snapshot and every peer see the same
> enriched reading and no consumer can disagree about it.
>
> **R2 — serving.** A derived value that is a function of a **window of readings** is never computed
> per request. It is maintained by a component and read from memory: `window:rollup` for windows up
> to an hour, `rain:totals` for calendar windows, `snow:*` for the expensive snow tiers. SQL computes
> a window only for `/span`, which is the one genuinely query-shaped endpoint, and for the aggregate
> hierarchy itself.

One question decides every case: **does this value need more than one reading?** That is what makes
the choice non-arbitrary rather than per-case taste, and §2a supplies the corollary that pins the
serving half — *anything on the `/latest` payload is computed at ingest or maintained in the
snapshot; nothing on the `/latest` payload is computed per request.*

Applied to the table the prompt supplies, plus the three values it omits that are in the same
situation:

| Derived value | Function of | Decision | Owner |
|---|---|---|---|
| **wind chill** | one reading (temp, wind speed) | **ingest, stored** `windchill` | `mw-pipeline::enrich` |
| **heat index** | one reading (temp, humidity) | **ingest, stored** `heatindex` | `mw-pipeline::enrich` |
| **AQI** (`aqi_pm25`, `aqi_pm10`, and the `_aqin` variants) | one reading | **ingest, stored** | `mw-pipeline::enrich` |
| **solar potential watts / joules** | one reading + (time, lat, lon) | **ingest, stored** `potentialsolarwatts`, `solarjoules` | `mw-pipeline::enrich` |
| **snow depth** | one reading + the station's base distance | **ingest, stored** `snowdepth` (millimetres) | `mw-pipeline::enrich` |
| **sunrise / sunset** | (date, lat, lon) — pure, microseconds | **neither.** Evaluated where it is served (W2a Decision 36) | `snapshot:latest` |
| **lunar phase, illumination, age, crescent angle** | (instant, lat, lon) — pure | **neither.** Evaluated where it is served | `snapshot:latest` |
| **wind gust** (max over 10 min) | window | **read side, maintained** | `window:rollup` → snapshot |
| **rain rate** (10-minute extrapolation) | window | **read side, maintained** | `window:rollup` → snapshot |
| **rainfall 24 / 48 / 72 h** | window | **read side, maintained** | `rain:totals` → snapshot |
| **rainfall day / month / year** | window, **station-local calendar** | **read side, maintained** | `rain:totals` → snapshot |
| **storm rainfall total** | window with carried state | **read side, maintained** | `rain:totals` → snapshot |
| **snow: new snow midnight / 24 h / 72 h** | window, expensive (PELT, RBF, median filter) | **read side, maintained, slow tier** | `snow:events` → `SnowCache` |
| **snow: season total** | window spanning months | **read side, two rates** (§2a) | `snow:season` → `SnowCache` |
| **snow: storm total, snowfall rate** | window | **read side, maintained** | `snow:storm` → `SnowCache` |
| **circular wind-direction average** | window | **aggregate columns** `_sin_sum`/`_cos_sum`/`_dir_count` (W2 generates them) finalised by `mw_circular_mean()`; sub-hour windows by `window:rollup` | W4 / `window:rollup` |
| **`windcard`** (the cardinal string) | one value, presentation only | **neither.** Derived in the DTO from `winddir` | `mw-api-legacy` |

Four consequences worth stating because each closes something specific:

1. **`storm_start` stays a station-reported column** where the hardware supplies it (Davis does); the
   *computed* storm window is `rain:totals`'s, and the two are different facts that today share a
   name.
2. **The day/month/year rollover happens in the station's time zone.** Today
   `rainfall_calc.go:30` writes `date_trunc('day', NOW())`, which truncates in the database server's
   zone, so the device's declared `timezone` field is decorative for every rollover that claims to
   use it (W2a §1.6 measured the same thing). `rain:totals` is a `Calendar { daily 00:00,
   TzSource::Station }` component for the boundary plus `Every { 60 s }` for the accumulation since
   it, so the boundary is correct by construction.
3. **Enrich runs before the spool, so a forwarded envelope is already enriched.** A satellite and its
   collector therefore agree on wind chill rather than each computing it from a different formula
   version. The collector does **not** re-enrich: `enrich` writes a metric only where the value is
   `None`, which makes it idempotent and makes multi-hop safe. One rule, stated once.
4. **The writer stores `NULL` for a metric the station did not report.** That is D-3's premise and
   D-9's mechanism, and it is the condition W1-BUG-1 is latent against: with the current
   `circular_avg` finaliser, a window whose `winddir` is entirely NULL makes `/span` return 500. This
   document's writer is exactly the writer that triggers it, so W4 must take OQ-W1-2 option (c) — a
   NULL-safe finaliser that still emits `0`. Recorded as an assumption so it is greppable rather than
   discovered.

### 2.6 `Ingress::admit` in full

The order is load-bearing; it is the order that makes an unauthenticated reading unstorable.

```
admit(batch, meta):
  1. resolve(meta, identity, registry) -> StationKey | Reject        W2a §4a.6, the single gate
       Reject::{UnknownStation,BadSecret,Disabled,UnauthorizedPeer}
       -> rejected_total{component,reason} += 1; return Err(Rejected)
  2. for each PartialReading:
       merge into a Reading (registry-typed, Option-valued)
       enrich(&mut reading, &station_meta)                            R1, pure, idempotent
  3. seq = seq_table.next(origin, station)                            monotonic, persisted with the spool
  4. envelope = Envelope { station, observed_at, received_at: now, provenance, reading }
  5. spool.append(&envelopes).await?                                  group commit; the atom
  6. watch.send_replace(newest); broadcast.send(each)                 non-blocking, after the commit
  7. readings_ingested_total{station,origin} += n; return Admitted
```

Step 1 before step 4 is the whole of W2a's structural authentication claim: **there is no `Envelope`
without a `StationKey`, and the only producer of a `StationKey` is `resolve`.** The Go gRPC receiver
does the opposite — `station.go:174` pushes into the distributor and `:194` consults the registry
afterwards, debug-logging an unknown sender — so an unregistered peer's readings are stored and then
noticed.

Step 6 after step 5 means a subscriber never observes an envelope that is not durable. That matters
for `live:stream`: a browser cannot be shown a reading the database will not have.

### 2.7 The degenerate cases, decided rather than left implicit

**The spool cannot be written** (permissions, a full filesystem, a failing disk). W2a Decision 62
says this is fatal rather than a silent drop, and that stands: `SpoolError` propagates out of
`admit`, the binary logs it at `ERROR` with the chain and exits non-zero, systemd restarts, and the
restart loop is visible. A daemon that cannot durably accept readings is not degraded, it is wrong.

**The spool reaches its ceiling** — the case W2a does not cover, because it is W3's. There are only
two possible answers and both lose something:

- *Stall ingest.* But a stalled `admit` back-pressures the Davis serial read loop, and an undrained
  Davis console **drops packets anyway**; so the data is lost regardless, and additionally the
  process can no longer shut down cleanly. This is the Go behaviour, arrived at by accident.
- *Drop at the edge, loudly.* The batch is refused with `AdmitError::SpoolFull`, counted as
  `admit_rejected_total{reason="spool_full"}`, logged once per minute by the dedupe layer (§5), and
  surfaced as a component `Failed` state on every station.

**Decision: drop at the edge.** Loss is certain either way at that point; the only choice is whether
shutdown still works and whether the operator is told. `spool_depth_bytes` is a gauge that rises for
days before this can happen, so the real protection is upstream of the policy.

**A cursor is permanently behind** — a peer that never acknowledges. Segments cannot be reclaimed, so
the ceiling approaches. The cursor carries a configured `max_lag` (default **7 days**); past it, the
cursor is marked `abandoned`, its component moves to `Failed { CursorAbandoned }`, and the segments
are reclaimed. Reasoning: seven days is longer than any plausible link outage and shorter than the
ceiling's worst case (~12 days, §2.4), so the operator gets a named failure attached to the cursor
that caused it rather than a ceiling event attributed to whichever station happened to admit last.

---

## 2a. The read path and the hot-path snapshot

### 2a.1 Three shapes, named separately so nobody merges them

```
write:  Sources ──admit──▶ Spool ──cursor──▶ Sink            TimescaleSink, uplink, (future MQTT)
read:   REST / gRPC ─────────────────────▶ ReadingStore      /span, /almanac, aggregates
live:   SSE / gRPC stream ───────────────▶ a tap on the fan-out   live:stream, GetLiveWeather
publish: OnReading ──▶ LatestSnapshot ───▶ REST / gRPC       /latest, /snow, /stationinfo, /forecast
```

`TimescaleSink` (write) and `ReadingStore` (read) are two traits over **one `sqlx::PgPool`**, both
implemented in `mw-store` by the `db:postgres` component. They are separate because their concerns
do not overlap: delivery policy, batching and idempotence on one side; aggregate-level selection,
pagination and TTL on the other. `GetLiveWeather` is neither — it is a `watch` subscriber, which is
why `live:stream` is an `OnReading { All, Latest, 50 ms }` component and not a route that polls.

Stated so it survives: **the website is not a sink.** Nothing that serves bytes is on the write path.

The three signatures, so the separation is in the types rather than in the prose:

```rust
// mw-component — the read side. Implemented by db:postgres; required Read by api:legacy,
// web:site:*, window:rollup and the snow components. NEVER handed to api:legacy's /latest path.
#[async_trait]
pub trait ReadingStore: Send + Sync {
    /// The /span family. `level` is selected from `span` by the frozen boundary table (W1 §2.3)
    /// and the columns come from `generated/rust/aggregates.rs`, not from a hand-written list.
    async fn span(&self, q: SpanQuery) -> Result<Vec<AggregateRow>, StoreError>;
    /// The newest raw reading, used only by `snapshot:warm` (§2a.6) — not by a request path.
    async fn latest(&self, station: StationId) -> Result<Option<Reading>, StoreError>;
    /// One rolled-up window. `window:rollup` is the only caller; it feeds `WindowedView`.
    async fn window(&self, q: WindowQuery) -> Result<WindowRow, StoreError>;
    async fn almanac(&self, station: StationId) -> Result<Option<Almanac>, StoreError>;
}

pub struct SpanQuery { pub station: StationId, pub span: Duration, pub snow_join: bool }
pub struct WindowQuery { pub station: StationId, pub want: Vec<(MetricKey, Window, Agg)>, pub as_of: DateTime<Utc> }

// mw-component — what an OutputEncoder resolves against (W2a §4b.2). Provided by window:rollup
// from an in-memory ring of recent 1-minute buckets; `aprs:*`, `pws:*` and `wu:*` require THIS
// and are never handed a ReadingStore, so they cannot run a query (W2a Decision 50).
pub trait WindowedView: Send + Sync {
    fn resolve(&self, station: StationId, want: &[(MetricKey, Window, Agg)])
        -> Result<Resolved, WindowError>;
}
pub struct Resolved { values: SmallVec<[(MetricKey, Window, Agg, Option<f64>); 16]> }

// mw-store — the write side. One `impl Sink` (§2.3) over the same PgPool.
pub struct TimescaleSink { pool: PgPool, stmt: PreparedInsert }
// deliver(): one multi-row INSERT ... ON CONFLICT (station_id, time) DO UPDATE per batch.
```

`Resolved::values` is private and read through accessors keyed by the exact `(metric, window, agg)`
triple the encoder declared, so an encoder cannot read a window it did not declare — which is what
makes the declaration table reviewable against the protocol specification rather than merely
documentation.

### 2a.2 `LatestSnapshot`

```rust
// mw-component — the capability handle.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotKey { pub website: WebsiteId, pub station: StationId }

pub struct Snapshot {
    pub key:         SnapshotKey,
    pub etag:        ETag,                  // strong; see 2a.5
    pub observed_at: DateTime<Utc>,         // the newest reading folded in
    pub rendered_at: DateTime<Utc>,
    pub staleness:   Staleness,
    /// Pre-serialised legacy `/latest` body, including the trailing newline Q-12 requires.
    pub json:        Bytes,
    /// Lazily produced on first request; no shipped client sends `format=msgpack`.
    pub msgpack:     OnceCell<Bytes>,
    /// Lazily produced; the gRPC services share it.
    pub proto:       OnceCell<Arc<mw_proto::WeatherReading>>,
}

pub enum Staleness {
    Fresh,
    /// Window-derived fields are being served from their last good value.
    Degraded { missing: SmallVec<[Capability; 4]>, since: DateTime<Utc> },
}

pub trait LatestSnapshot: Send + Sync {
    fn get(&self, key: SnapshotKey) -> Option<Arc<Snapshot>>;
    fn subscribe(&self, key: SnapshotKey) -> watch::Receiver<Option<Arc<Snapshot>>>;
    /// For `/stationinfo` and the portal, which need the set rather than one entry.
    fn table(&self) -> Arc<SnapshotTable>;
}
```

Published behind `arc_swap::ArcSwap<SnapshotTable>` where `SnapshotTable = HashMap<SnapshotKey,
Arc<Snapshot>>`, plus one `watch` per key for SSE. `ArcSwap` rather than `RwLock` because a reader
must never wait on the writer — that asymmetry is the whole point, and it is also the specific thing
D-28 exists to fix.

**The handler's entire job**, and the one piece of logic beyond cloning an `Arc`:

```
GET /latest
  1. website = host_table.load().resolve(host)              ArcSwap read, no lock
  2. station = query.station_id | query.station | website.primary_device
       unknown -> 404 "station not found\n"                 resolved against the config table
  3. snap = snapshot.get((website, station))                ArcSwap read
       None -> 404 "no recent weather data available for this station\n"
  4. if now - snap.observed_at > 10 min -> 404, same body   W1's recency rule, preserved
  5. if If-None-Match == snap.etag -> 304, no body
  6. write snap.json                                        Bytes clone = a refcount bump
```

Zero queries, and `api:legacy` is not handed a `ReadingStore` handle, so zero is enforceable rather
than aspirational (W2a Decision 35).

Step 4 is required by W1 Decision 15: `/latest` is the only endpoint where staleness becomes a 404,
because `fetchLatestReading` filters `time >= NOW() - INTERVAL '10 minutes'`. Serving a stale
snapshot instead would turn a documented 404 into a 200 and change what the iOS client does.

### 2a.3 Pre-serialisation — with the prompt's arithmetic corrected

The prompt argues for pre-serialisation on the grounds that the 133-field payload is *"not free work
to repeat 17,000 times"*. **At the measured volume the ratio runs the other way.** One station at a
2-second cadence produces 43,200 readings a day; the whole system served 17,740 `/latest` requests.
Serialising per reading is therefore roughly **ten times more** serialisation work than serialising
per request, not less — and with four stations and six websites it is more again.

The honest numbers: a flat 133-field struct through `serde_json` is single-digit microseconds, so
172,800 serialisations a day is well under a second of CPU. **Neither choice matters for
throughput**, and anyone optimising it is in §10's territory.

**Pre-serialisation is still right, for three reasons that are not throughput:**

1. **Tail latency.** Serialisation leaves the request path entirely, so p99 for 93% of traffic is a
   hash lookup, a comparison and a write.
2. **The ETag needs a stable identity for the bytes.** A payload that is produced per request has no
   identity to name; one produced per reading does, for free.
3. **It is the same mechanism that removes the 12 queries.** The queries and the serialisation are
   both per-request work on a payload that is identical for all callers; removing one and not the
   other would leave the design half-applied.

So: **JSON is serialised eagerly, once per reading-version, for every key. msgpack and protobuf are
produced lazily on first request via `OnceCell`** — no shipped client sends `format=msgpack` (W1
OQ-W1-5), so eagerly producing it would be the one genuinely wasteful choice available.

### 2a.4 Cardinality, keying and memory — stated, not left as a worry

**Keyed by `(website, station)`, not by station.** The payload genuinely depends on the website:
`getSnowBaseDistance(website)` feeds `snowdepth`; the primary-device selection is per website; and
`/latest?station=` may name any configured device while `/snow` always reports
`website.SnowDeviceName`. A station-only key would produce the wrong `snowdepth` for a second website
with a different base distance.

| | Count |
|---|---|
| websites in W1's capture rig | 6 |
| devices | 5 |
| worst-case keys (cross product) | 30 |
| realistic keys (devices actually bound to a website) | ~8 |

Per key: JSON ~2 KB, msgpack ~1.5 KB (lazy), proto ~1 KB (lazy), the enriched `Reading` ~1 KB. Call
it **5 KB**. Thirty keys is **150 KB**. On the smallest release target — a Pi Zero with 512 MB — that
is 0.03% of RAM.

**Memory is a non-issue and here is the ceiling at which it would stop being one:** 10,000 keys is
~50 MB, and the configuration cannot express 10,000 websites or devices without a management UI that
creates them in bulk. No eviction policy, no TTL, no `LruCache`. Entries are created and destroyed by
the config watch's plan diff, which is also what keeps a deleted website from leaking a snapshot.

### 2a.5 Conditional GET

```
ETag: "l1-<station_id>-<observed_at_unix_ms>-<payload_rev>"
```

`payload_rev` is `hash(serialiser_version, config_generation)`, so a website rename or a changed base
distance invalidates even when the reading has not moved. Strong, not weak: the bytes for a given
value are byte-identical by construction, which is the definition of a strong validator.

- No `If-None-Match` → **normal 200 with the full body.** Additive only; the frozen contract is
  untouched.
- Exact match → **304**, with `ETag`, `Cache-Control` and `Vary`, and no body.
- `If-None-Match: *` → treated as a match if a snapshot exists.

**The `Cache-Control` subtlety, which has to be said out loud.** The frozen contract sets
`Cache-Control: no-cache, no-store, must-revalidate` on `/latest`, and W1 freezes it. A client that
honours `no-store` does not cache, so it never sends `If-None-Match`, so the 304 never fires for the
iOS app as it ships today. That is fine: the ETag is a capability the client opts into, and a future
iOS release that sends `If-None-Match` gets 304s with no server change and no contract change. What
must **not** happen is "fixing" `Cache-Control`, which would be a bytes-visible change to a frozen
header.

**This cannot fail conformance, and that is checked rather than assumed.**
`contract/conformance/src/lib.rs:320` iterates `assert.headers` — a per-fixture **allowlist of header
names** — and for each one asserts presence and a value match. It never enumerates the response's
headers, so it cannot notice an extra one. `contract/fixtures/latest-default-station.json` lists
exactly four (`Access-Control-Allow-Origin`, `Cache-Control`, `Content-Type`, `Vary`). An additive
`ETag` is therefore invisible to the suite by construction, which is also why `Date` and `Server`
have never been a problem.

### 2a.6 Graceful degradation

With `/latest` served from memory, a database outage no longer touches 93% of traffic. What it does
touch is the window-derived fields, whose providers move to `Waiting`:

| Field group | Provider | Behaviour during an outage |
|---|---|---|
| the reading itself, wind chill, heat index, AQI, snow depth, sun, moon | `snapshot:latest` folding arrivals in memory | **unaffected** |
| wind gust, rain rate | `window:rollup` → `Waiting { ReadingStore }` | last good value retained |
| rain day/month/year, 24/48/72 h, storm | `rain:totals` → `Waiting { ReadingStore }` | last good value retained |
| snow totals | `snow:*` → `Waiting { SnowCache }` | last good value retained |

The snapshot keeps the last good value for each window-derived field and records `Staleness::Degraded
{ missing, since }`.

**Where the staleness indicator goes.** Not into the frozen body: `/latest` has no field for it and
inventing one is a contract change. So:

- additive response headers `X-MW-Snapshot-Age: <seconds>` and `X-MW-Degraded: window,snow`;
- `mw_snapshot_age_seconds{website,station}` and `mw_component_state` (§5);
- the **management** UI, which renders `Staleness` and the `Waiting { on }` edge as a sentence (W5
  §2);
- the public website shows nothing, because changing it is out of scope (D-38).

That is the honest answer to "serve the last snapshot with a staleness indicator": the indicator is
real and it reaches the operator, not the frozen clients.

**Cold start.** A freshly started process has an empty snapshot table, so `/latest` would 404 until
the first reading arrives — up to 3 s for an AirGradient, 30 s for a WeatherLink Live poller, and
*forever* for a station that is offline but was within the 10-minute window when the process
restarted. The Go server would have served that station from the database. So:

> **`snapshot:warm` is an `OnStartup` component** requiring `ReadingStore` **soft**, which seeds the
> snapshot table from the newest reading per `(website, station)` inside the 10-minute window, plus
> the window-derived values from `window:rollup` and `SnowCache` where available. If `ReadingStore`
> is unavailable it finishes with an empty table and `/latest` 404s until a reading arrives — which
> is strictly better than refusing to start, and is the `Waiting`-not-refusing principle (D-27)
> applied to a precompute.

W2a assumption 26 asks W1 to pin what `/latest` returns when the snapshot is empty. The answer this
design produces is **404 with `no recent weather data available for this station\n`** — the same
bytes as a stale station — and W1's fixture `latest-no-recent-data` already covers those bytes.

### 2a.7 `/snow` is a second hot path, with two tiers

`SnowCache` is a separate capability from `LatestSnapshot` because its contents refresh at two
different rates, and conflating them would mean recomputing a season total per reading.

| Tier | Values | Trigger | Component |
|---|---|---|---|
| cheap, fast-changing | current depth, snow distance | `OnReading { Stations(snow), Latest, 100 ms }` | `snapshot:snow` |
| expensive, slow-changing | new snow midnight / 24 h / 72 h, smoothing | `Every { 15 min, deadline 60 s, AtPhase, Spread }` | `snow:events`, `snow:smoothing` |
| expensive, storm-rate | storm total, snowfall rate | `Every { 5 min, deadline 30 s }` | `snow:storm` |
| expensive, month-scanning | season total | `Every { 15 min }` re-derived from cached events **plus** `Calendar { daily 00:10, TzSource::Station }` full rescan | `snow:season` |

**Why those intervals.** 15 minutes matches what the Go snow cache controller already does for events
and smoothing, and snow accumulates on an hour timescale, so 15 minutes of staleness on a 24-hour
total is invisible. Storm total and snowfall rate are the fast-moving members of the expensive set
and are the two the current handler computes *live on every request*, so they get 5 minutes. The
season total is the one that scans months (`calculate_total_season_snowfall()`); splitting it into a
cheap 15-minute re-derivation from the cached event list and one daily full rescan means the month
scan happens once a day instead of 17,000 times.

The `/snow` handler then does what `/latest` does: resolve the website, read the two tiers, write
pre-serialised bytes. Note two frozen behaviours it must keep (W1): `/snow` has **no recency filter**,
so an offline gauge returns its last known depth indefinitely (Decision 16); and `stationname` is
always `website.SnowDeviceName` even when `?station=` named something else, which redirects only the
depth lookup (Q-8).

**What the UI shows for snapshot staleness:** the management UI renders `computed_at` and an age per
tier, sourced from `X-MW-Snow-Computed-At` and `mw_snapshot_age_seconds`. The public website shows
nothing new, for the same reason as §2a.6.

The Go tree already reached half of this conclusion: a `snowcache` controller exists *because* these
are expensive, the handler reads it, and then computes six more values live anyway, with
`handlers.go:1418` carrying `// TODO: Move snow calculations to almanac_cache refresh job`.

### 2a.8 The rest of the measured mix

| Endpoint | Share | Mechanism |
|---|---|---|
| `/latest` | ~93% | `LatestSnapshot`, §2a.2 |
| `/snow` | 2.6% (biased low) | `SnowCache` + `snapshot:snow`, §2a.7 |
| `/forecast/{span}` | 2.4% combined | `ForecastStore` → a per-`(device, span)` snapshot refreshed hourly. **The cached value is the stored `jsonb` text, byte for byte.** |
| `/` and `/portal` | 1.1% | rendered-page cache per `(website, config generation, version)`, invalidated by the config watch (§7) |
| `/stationinfo` | 0.7% | per-website snapshot from `ConfigStore`, invalidated by the config watch |
| `/almanac` | — | `almanac_cache` → per-station snapshot; already `max-age=3600` |
| `/alerts` | — | `AlertStore` → per-station snapshot, 15-minute refresh; already `max-age=300` |
| `/span/{span}` | <1% | **the one query-shaped endpoint.** 60-second TTL cache keyed by `(station, span, snow-join)`, lossless because the 1-minute aggregate only advances once a minute |
| `/api/stations`, `/api/remote-stations` | — | `ConfigStore` / `StationRegistry`, config-generation keyed |

`/forecast` deserves its own paragraph because it is the one place where re-serialising would break
the contract. The body is `{"lastUpdated": <Go time.Time.String()>, "data": <stored jsonb>}` and
Postgres's `jsonb` writer puts a space after `:` and `,` while Go's `json.Marshal` does not (W1
§6.2). Reproducing jsonb's key normalisation would be reproducing an accident of the storage engine.
So: **the component caches the `jsonb` column's text exactly as the server returned it and splices
it between a prefix and a suffix.** Nothing parses it, nothing re-emits it, and Q-12's missing
trailing newline is a property of not appending one.

The `?period=N` variant is the exception — Go decodes to a map and re-marshals, which sorts keys
alphabetically and emits compactly (visible in W1's own example:
`{"avgTempF":…,"compactWeather":…,"dateTimeISO":…}`). The Rust side reproduces that with
`serde_json` over a `BTreeMap<String, Value>` in compact mode, which produces exactly that ordering
and spacing. `format=msgpack` on `/forecast` additionally turns every number into a float64 because
Go decodes into `interface{}` first (Q-14); reproduced by decoding the cached text to
`serde_json::Value`, coercing all numbers to `f64`, then encoding with `rmp-serde`.

### 2a.9 What not to do

"Optimise the hot path" invites gold-plating, and the absolute rate here is 19,000 requests — call it
0.2 requests per second. This section is about tail latency and about not generating 213,000
redundant queries. It is **not** about throughput headroom.

- **No Redis, no memcached.** The data is tiny and single-process. An `Arc` *is* the cache, and
  adding a network hop to reach a 2 KB value that lives in the same address space is strictly worse
  on every axis including operability.
- **No CDN.** The payload changes every two seconds, so a TTL buys nothing, and it adds an
  operational dependency and a cache-invalidation story to a system that currently has neither.
- **No hard optimisation of `/span/24h`.** A 60-second TTL is lossless. Do not add a second tier, do
  not denormalise, do not precompute spans nobody requested.
- **No hand-tuned JSON encoder** once serialisation is off the per-request path. `serde_json` is
  fine; `simd-json` is not a dependency this earns.
- **No request-level caching layer.** The snapshot *is* the cache; a second one in front of it would
  have its own invalidation bug.

---

## 3. Supervisor implementation

W2a specifies the lifecycle *states*, capability resolution and startup ordering. This section
specifies the mechanics.

### 3.1 What exists before any component does

`main` is the only place that touches a resource outside a component, and it touches exactly three:

```
main:
  1. parse args (--config <path>)                     W8's systemd unit and Dockerfile assume this spelling
  2. install the tracing subscriber (§5), capture the reload handle
  3. open the SQLite pool and run its migrations      ← the root resource
  4. cfg = Config::load(&pool)
  5. specs = plan(&cfg, registrars)                   pure; every structural error is caught here
  6. supervisor = Supervisor::new(pool.schedule_store(), log_control, cfg_watch)
  7. supervisor.run(specs).await                      never returns until shutdown completes
```

Step 3 before step 5 is forced and worth justifying, because it answers W2a's **OQ-D** (where does
the supervisor's own durable state live).

`plan()` is a pure function *of configuration*, and configuration lives in SQLite — so something has
to read SQLite before any component exists. The supervisor also needs `Misfire::RunLatest` to survive
a restart, which means persisting the latest `Calendar` occurrence per component. Routing either need
through `ConfigStore` would mean the supervisor depending on a component it supervises, which is the
`AppReloader` cycle in a new coat.

> **Decision: the SQLite pool is a root resource owned by `main`.** It hands out two narrow,
> non-overlapping handles — a `ScheduleStore` over one table (`component_schedule`) to the
> supervisor, and the full handle to `config:sqlite`, which provides `ConfigStore`, `ConfigAdmin` and
> `StationRegistry` as capabilities. The supervisor never sees `ConfigStore` and `config:sqlite`
> never sees `component_schedule`.

This is W2a OQ-D's recommendation, adopted, with the one addition that makes it coherent: the pool is
not owned by `config:sqlite` either.

### 3.2 Task spawning, and the invariant that makes a leak unexpressible

```rust
// mw-supervisor::task — the only module in the workspace allowed to call tokio::spawn.
struct Task { token: CancellationToken, handle: JoinHandle<()>, started: Instant }

pub struct TaskTable { tasks: HashMap<ComponentId, Task>, exits: mpsc::Sender<(ComponentId, Outcome)> }

impl TaskTable {
    /// Spawns and retains. Returns NOTHING — there is no handle for a caller to drop.
    fn spawn<F>(&mut self, id: ComponentId, f: impl FnOnce(CancellationToken) -> F)
        where F: Future<Output = Result<(), ComponentError>> + Send + 'static { ... }

    /// The ONLY way a task leaves the table. There is no `fn remove`.
    async fn stop(&mut self, id: &ComponentId, grace: Duration) -> StopOutcome { ... }
}
```

> **The invariant: a `JoinHandle` is never owned by anything but `TaskTable`, and `TaskTable` has no
> removal path that does not await.**

Two halves, and both are needed:

- **`spawn` returns `()`.** There is no handle to forget, no `let _ =`, no detached future. A
  component body cannot hold its own task.
- **There is no `remove`.** The only way out is `stop`, whose signature is `async` — so "delete the
  map entry and log that it will stop on the next restart" (`RemoveController`, measured) is not a
  thing a caller can write, because the type offers nothing that does it.

Reinforced by `clippy.toml`:

```toml
disallowed-methods = [
  { path = "tokio::spawn",            reason = "spawn through mw_supervisor::TaskTable" },
  { path = "tokio::task::spawn",      reason = "…" },
  { path = "std::thread::spawn",      reason = "…" },
  { path = "tokio::time::interval",   reason = "the supervisor owns every clock (D-25)" },
]
```
with `#[allow]` only inside `mw-supervisor::{task, clock}`. The gate already runs
`cargo clippy --workspace --all-targets -D warnings`, so this costs no CI change.

What this deletes, measured: the APRS controller's **private** `sync.WaitGroup`, which makes its
goroutines invisible to application shutdown while it receives no context at all; and the three sites
that call `wg.Add(1)` from *inside* a function already launched with `go`, so `wg.Wait()` can return
before they register. Neither is expressible here: there is one table, registration happens in
`spawn` before the future is handed to the runtime, and the cancellation token is constructed by the
table rather than by the component.

**How the supervisor learns a task ended.** Not by polling handles. Every spawned future is wrapped
so that on completion it sends `(id, Outcome)` on an `mpsc`, and the supervisor's single `select!`
loop has four arms:

```rust
loop {
    select! {
        Some((id, outcome)) = exits.recv()     => self.on_exit(id, outcome),
        _                   = timer.next()     => self.on_due(),            // §3.3
        Ok(_)               = cfg_watch.changed() => self.replan().await,   // W2a §2.5
        Some(cmd)           = control.recv()   => self.on_control(cmd),     // ComponentControl
        _                   = shutdown.cancelled() => break,
    }
}
```

One loop, one owner of state, no locks on component state at all — which is what makes "a component
never sets its own state" (W2a Decision 28) a property of the architecture rather than a convention.

### 3.3 One clock for the whole process

**One `tokio::time::Sleep`, re-armed from a binary heap**, not N `Interval`s:

```rust
struct Due { at: Instant, id: ComponentId, occ: OccurrenceKey }
struct Clock { heap: BinaryHeap<Reverse<Due>>, sleep: Pin<Box<Sleep>>, wall_ref: (SystemTime, Instant) }
```

- **`Every`** — the next instant is `phase_origin + n·period` where `phase_origin` is derived from
  `Phase::Spread`'s stable hash of the component id (W2a §1.5), computed in `Instant` terms so it is
  immune to wall-clock steps.
- **`Calendar`** — the next instant is computed in the component's IANA zone with `chrono-tz`, then
  converted to an `Instant` via the current wall/monotonic reference. W2a §1.6's DST rules are
  implemented here, not in a component: a nonexistent local time fires at the first instant after the
  gap; an ambiguous one fires on the first pass only, deduplicated by `OccurrenceKey =
  (component_id, local_date, slot)`.
- **`OnStartup`** — pushed at `Instant::now()` the moment the component's hard needs are first
  satisfied; terminal state `Finished`.
- **`OnDemand`** — `ComponentControl::run_now(id)` pushes an immediate entry, deduplicated by
  `OccurrenceKey`, so a double-click in the UI runs once.
- **`Service`** — never in the heap. A `Service` has no schedule to own, which is why W2a Decision 7
  lets it own time *inside* its loop (a UDP read deadline, a reconnect backoff, a keep-alive). That
  is explicitly not a D-25 violation and this document relies on it.

**Wall-clock jumps**, which the Go version does not handle at all. Each tick compares the elapsed
`SystemTime` against the elapsed `Instant` from `wall_ref`; a divergence over 1 s means the wall clock
was stepped (NTP correction after a Pi boots with no RTC, which is the common case on the smallest
target). The `Calendar` half of the heap is then recomputed from scratch and `clock_step_total` is
incremented. `Every` entries are untouched, because they are monotonic by construction.

**What this buys over 21 hand-rolled clocks**, beyond the deduplication: `next_run` is a heap peek,
`last_run` and `last_error` are table fields, and a configuration change rebuilds the heap in one
place. W2a §1.3 counted the 21, plus an unused generic `RunPeriodicTask` helper with zero callers —
somebody had already reached this conclusion and had nowhere to put it.

### 3.4 Firing, deadlines, and how `Err` maps onto the state machine

```
on_due(id, occ):
  if state != Running         -> skipped_total += 1 ; reschedule ; return
  if in_flight.contains(id)   -> skipped_total += 1 ; reschedule ; return     // Skip is the only policy
  child = component_token.child_token()                                       // per-tick, not per-component
  res   = timeout(trigger.deadline, body.run(child.clone(), cx)).await
  reschedule(id)                                                              // BEFORE interpreting res
  match res {
    Ok(Ok(()))  => { ticks_total{outcome="ok"} += 1; last_run = now; last_error = None }
    Ok(Err(e))  => { failed_total += 1; last_error = Some(e); transition(Failed{e} -> Backoff{next_occurrence}) }
    Err(_elapsed) => { child.cancel(); deadline_exceeded_total += 1;
                       transition(Failed{DeadlineExceeded} -> Backoff{next_occurrence}) }
  }
```

Three details that are decisions rather than code:

**The per-tick token is a child of the component's token.** Cancelling a tick on its deadline must
not kill the component; cancelling the component must kill the tick. A child token gives both from
one line, and it is why a `Serve` is never affected by a deadline (it has none).

**Rescheduling happens before the outcome is interpreted**, so a panic or an unexpected branch
cannot silently stop a component from ever running again. The Go `GetLoopPackets` defect W1 found —
the Davis driver loops only on *error* and `return`s on success, so polling stops after the first
successful batch of 20 — is this class of bug, and it survived because nothing else owned the
schedule.

**`Err` → `Failed` → `Backoff`, and for scheduled triggers `Backoff { until }` is the next
occurrence.** W2a Decision 29 says the period *is* the backoff; expressing that as
`Backoff { until: next_occurrence }` keeps W2a's state machine literal without inventing a second
clock. A 60-second uploader that is failing retries every 60 seconds — what the operator configured
and what the remote service's rate limit expects — while `failed_total`, `attempt` and `last_error`
make the condition visible.

Exponential backoff with full jitter (base 1 s, cap 60 s, reset after an uptime exceeding the cap)
applies to exactly three cases, all of which have no period of their own: a **build** failure, a
**`Serve`** exit, and an **`OnReading`** delivery failure. §2.3's durable sink is in the third group,
and because the cursor does not advance, the backoff is a retry of the same batch.

No permanent give-up (W2a Decision 30). The cap is 60 s and it keeps trying; `restart_total`,
`attempt`, `last_error` and `Backoff { until }` are all state, so a component restarting every minute
for a day is *visible*, which is the thing that was missing — not the retrying.

### 3.5 Readiness, for `Type=notify`

W8's systemd unit is `Type=notify` and its assumption 3 says that if W3 does not implement readiness
the unit must revert to `Type=simple`. **It is implemented.** Mechanism: the `sd-notify` crate, which
writes a datagram to `$NOTIFY_SOCKET` and links nothing — important, because every release target is
a statically linked musl build and `libsystemd` is not available in one.

```
READY=1      once the first full reconciliation pass completes — every spec has reached
             Running, Waiting, Finished or Backoff at least once
STATUS=…     "12 running, 2 waiting (db:postgres), 0 failed", refreshed on every transition
RELOADING=1  at the start of a plan diff, READY=1 again at the end
STOPPING=1   on shutdown
WATCHDOG=1   from the select loop if WatchdogSec is set, at half the configured interval
```

Readiness is deliberately **not** "every component is `Running`". An uploader waiting on a dead
database would then block readiness forever, and systemd would kill a daemon that is working exactly
as D-27 specifies. "The supervisor has converged" is the correct signal, and `STATUS=` carries the
nuance.

### 3.6 Stopping

W2a Decision 32: cancellation plus `Drop`, reverse topological order, grace deadline, then abort.

```
shutdown:
  STOPPING=1
  for rank in topo_order.rev():                      a provider outlives its consumers
      join_all(rank.map(|id| table.stop(id, grace)))  siblings concurrently
  assert!(table.is_empty())                           debug-only; the type makes it true

stop(id, grace):
  token.cancel()
  match timeout(grace, handle).await {
      Ok(_)  => Stopped,
      Err(_) => { handle.abort(); stop_timeout_total += 1;
                  warn!(component = %id, grace_s = grace.as_secs(), "aborted after grace"); Aborted }
  }
```

Default grace **10 s**, per-component overridable. The two that need more are `writer:timescale`
(finish an in-flight 256-row insert; 5 s deadline plus margin) and `uplink:collector` (flush the ack;
10 s deadline plus margin) — both get **15 s**. A `Service` holding a serial port needs none of it:
cancellation is observed at its next await and `Drop` releases the port.

Reverse order matters for one specific reason: a consumer must never observe a *revoked* capability
during shutdown, because that would move it to `Waiting` while it is trying to stop and produce a
spurious transition in the last thing the operator sees in the log.

### 3.7 Reconfiguration is a plan diff

W2a §2.5, implemented: on a config-watch change, re-run `plan()`, diff by `ComponentId` and spec
equality, then stop removed components (awaiting each), start added ones, and restart changed ones.
Unchanged components are not touched, so saving a website configuration does not disturb the APRS
schedule.

Because `plan()` is pure, an invalid configuration is rejected **before** anything is stopped: the
diff is computed against a successfully planned new configuration or not at all. That is the opposite
of the Go behaviour, where `ReloadWebsiteConfig` replaces the website list, the device list, the
device-name map, the devices-by-website map and the TLS configuration wholesale as plain unsynchronised
fields while in-flight requests read them — and a concurrent map read and write in Go is an
unrecoverable fatal error, so saving a website while serving traffic can kill the process (D-28).

The replacements for those five fields are all `ArcSwap` (§6.2, §6.3, §2a.2), so a reader sees one
generation or the next and never a torn state.

---

## 4. Error taxonomy

### 4.1 One enum per module, one classification, one retry policy

Per D-34 and D-32: `thiserror` per module, `anyhow` only at the binary boundary — and the layout gate
(§1.4) makes "only at the binary boundary" a fact rather than a convention, because
`crates/modernweather` is the only crate permitted an `anyhow` dependency.

The named enums:

| Crate | Enum | Notable variants |
|---|---|---|
| `mw-component` | `PlanError` | `UnknownCapability`, `DuplicateExclusive`, `Unprovided`, `Cycle{path}`, `DeadlineExceedsPeriod`, `IntervalBelowFloor`, `ImpossibleIdentity`, `RouteConflict` |
| `mw-component` | `ComponentError` | `Build`, `Tick`, `Unavailable(Capability)`, `DeadlineExceeded`, `Cancelled` |
| `mw-codec` | `CodecError` | `ShortFrame`, `BadChecksum{expected,got}`, `UnknownRecordType(u8)`, `OutOfRange{metric,value}` |
| `mw-station` | `SessionError`, `TransportError` | `HandshakeTimeout`, `UnexpectedResponse`, `PortUnavailable`, `PeerClosed` |
| `mw-pipeline` | `AdmitError`, `SpoolError`, `DeliverError` | §2.3 |
| `mw-store` | `StoreError`, `ConfigError` | `Pool`, `Query{sql_state}`, `Migrate`, `Unavailable` |
| `mw-http` | `RouteError`, `TlsError` | `Conflict`, `NoCertificate{sni}`, `BadKey`, `BindFailed{addr}` |
| `mw-api-legacy` | `ApiError` | the four frozen error-body conventions as variants, so a handler cannot invent a fifth |
| `mw-output` | `EncodeError`, `UploadError` | `MissingWindow{metric,window}`, `RateLimited{retry_after}`, `Rejected{status}` |
| `mw-obs` | `ObsError` | `FilterParse`, `ReloadPoisoned` |

### 4.2 Classification drives retry, instead of being re-decided per call site

```rust
pub enum Class {
    /// Transient. `after` carries an externally supplied hint (Retry-After, SQLSTATE 40001 …).
    Retryable { after: Option<Duration> },
    /// A configuration or programming error. No retry can help; failing fast is the service.
    Fatal,
    /// An expected absence. Count it and continue; it is not an error at the caller's level.
    Ignorable,
}

pub trait Classify { fn class(&self) -> Class; }
```

The mechanism that makes this more than documentation: **`tower`'s retry policy is implemented once,
generically, over `E: Classify`**, and every tower stack in the process uses it.

```rust
pub struct ClassPolicy { budget: u8 }

impl<Req: Clone, Res, E: Classify> tower::retry::Policy<Req, Res, E> for ClassPolicy {
    fn retry(&mut self, _: &mut Req, result: &mut Result<Res, E>) -> Option<()> {
        match result { Err(e) => match e.class() {
            Class::Retryable { after } if self.spend() => { self.not_before(after); Some(()) }
            _ => None,
        }, Ok(_) => None }
    }
}
```

So a new error type acquires a retry behaviour by implementing one trait — a decision made **once,
beside the error's definition**, where the knowledge of what the variant means lives — rather than
by a call site guessing. `Class::Retryable { after }` is also how `Retry-After` flows end to end
(W2a Decision 59): `UploadError::RateLimited` carries the parsed header, the policy honours it, and
the limiter's "not before T" signal makes the supervisor skip occurrences before T with a distinct
skip reason.

The layer order is W2a §4b.5's, unchanged and restated because it is easy to invert:

```rust
ServiceBuilder::new()
    .layer(RetryLayer::new(ClassPolicy::new(budget)))  // outermost: a retry re-enters the limiter
    .layer(RateLimitLayer::new(1, period))
    .layer(TimeoutLayer::new(attempt_timeout))         // innermost: bounds an ATTEMPT
    .service(http)
```

### 4.3 How `#[from]` and `?` make chain-preserving the default path

Every enum variant that wraps another error uses `#[from]`, so the shortest thing to write is also
the correct thing:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("query failed")]        Query(#[from] sqlx::Error),
    #[error("pool unavailable")]    Pool(#[source] sqlx::Error),
    #[error("migration failed")]    Migrate(#[from] sqlx::migrate::MigrateError),
}

// The whole of the chain-preserving path:
let row = sqlx::query_as!(...).fetch_one(&self.pool).await?;
```

Three conventions make that hold at the edges:

1. **`Display` prints one level only.** `#[error("query failed")]` and not
   `#[error("query failed: {0}")]`. The chain is walked once, at the point of logging, by a
   `DisplayChain` wrapper that follows `source()`. Embedding the source in `Display` produces the
   duplicated nested messages that make Go error strings unreadable, and it tempts the next person
   to format with `{}` and throw the chain away.
2. **Logging records the error as a field, not as a formatted message**:
   `error!(err = %DisplayChain(&e), sink = %id, "delivery failed")`. One place knows how to render a
   chain; the JSON layer emits it as an array of causes, which is what W5 renders as an expandable
   list.
3. **`anyhow` exists only in the binary**, where the chain is printed once with `{:#}` and the
   process exits.

The measured thing this avoids: the Go implementation formats wrapped errors with `%v` in **767 of
its 1,174** error-construction sites, flattening every one of them to a string. There is no mechanism
that would have caught that, because `%v` was the shortest thing to type. Here the shortest thing to
type is `?`.

### 4.4 What is retryable, fatal or ignorable — the actual classifications

| Error | Class | Why |
|---|---|---|
| `sqlx::Error::PoolTimedOut`, `Io`, SQLSTATE `08***`/`40001`/`57P01` | `Retryable` | the server went away or a serialisation conflict; the next attempt may differ |
| SQLSTATE `23505` on the writer's `ON CONFLICT` path | `Ignorable` | a replayed prefix after a cancelled delivery; the row is already there |
| SQLSTATE `42***` (syntax, undefined column) | `Fatal` | a generated query is wrong; retrying hides a drift the codegen gate should have caught |
| `CodecError::BadChecksum` | `Ignorable` | one corrupt serial frame; count it, read the next |
| `CodecError::ShortFrame` repeated | `Ignorable` per frame, but the **session** fails after N | a persistently short frame means the session is desynchronised, which the session can fix by reconnecting |
| `SessionError::HandshakeTimeout`, `PeerClosed` | `Retryable` | reconnect backoff inside the `Service` |
| `TransportError::PortUnavailable` | `Retryable` | the USB adapter may come back; W2a Decision 30 forbids permanent give-up |
| `AdmitError::Rejected` | `Ignorable` at the station, **counted** | somebody is pointing a station at us; it is a signal, not a failure |
| `AdmitError::SpoolFull` | `Retryable { after: 60 s }` | §2.7 |
| `SpoolError` (write failed) | `Fatal` | W2a Decision 62 |
| `UploadError::RateLimited` | `Retryable { after }` | the service told us when |
| `UploadError::Rejected{4xx}` | `Fatal` for the tick | a bad API key is not fixed by trying again; it fails, it is visible, and the period is the retry |
| `TlsError::BadKey`, `RouteError::Conflict` | `Fatal` | both are caught in `plan()` or `build`, before serving |
| `PlanError::*` | `Fatal`, and it never reaches a running process | by construction — `plan()` runs before anything is spawned |

---

## 5. Logging and observability

A first-class deliverable. The requirement this section is measured against is that somebody can
implement it without making another decision.

### 5.1 One path: `tracing`, and nothing else

The Go version has a package-global sugared logger (`internal/log`) **and** an injected
`*zap.SugaredLogger` passed into constructors, used interchangeably inside the same functions. That
is not reproduced. Concretely:

- `tracing` macros only. No `log::` macros in workspace code.
- `tracing_log::LogTracer` is installed once in `main` so that dependency crates using `log` (`sqlx`,
  `hyper`, `rustls`) land in the same pipeline.
- **No package-global logger**, and no logger passed as a constructor argument. A component gets its
  logger as the *ambient* subscriber plus a span: `Cx` carries a pre-entered span with
  `component = %id`, so every event a component emits is attributed without the component doing
  anything.
- `cargo xtask gate layout` rejects any workspace crate declaring a `log` dependency and any use of
  `println!`/`eprintln!` outside `crates/modernweather`'s argument-parsing path.

### 5.2 The subscriber stack

```rust
tracing_subscriber::registry()
    .with(DedupeLayer::new(DEDUPE))      // outermost: implements `event_enabled` → global suppression
    .with(reload_layer)                  // reload::Layer<EnvFilter> — the global level, runtime-adjustable
    .with(stderr_layer)                  // pretty+ANSI on a TTY, JSON otherwise
    .with(ring_layer.with_filter(ring_filter))   // the UI buffers; independently filterable
    .with(access_layer)                  // target == "mw::access" → the access ring
    .with(metrics_layer)                 // counter events → the Prometheus recorder
    .init();
```

The ordering is not cosmetic. `DedupeLayer` is outermost because `tracing_subscriber`'s `Layered`
ANDs `event_enabled` across the stack: a layer returning `false` there suppresses the event for
**every** sink, not just its own. That is the only hook with whole-pipeline reach, and it is why
repetition control (§5.6) is a layer rather than a feature of each sink.

### 5.3 Pretty for humans, structured for machines

Detection, then an override:

| Condition | Format |
|---|---|
| `--log-format` or `MW_LOG_FORMAT` is `pretty`/`json` | as asked |
| `std::io::stderr().is_terminal()` | pretty, ANSI, aligned fields |
| otherwise (journald, Docker, a pipe) | JSON, one object per line, `flatten_event(true)` |
| `NO_COLOR` set | pretty without ANSI |
| `CLICOLOR_FORCE` set | ANSI even when not a TTY |

The JSON form carries `ts` (RFC3339 with `Z`), `level`, `target`, `module`, `span` (the chain of span
names), the span fields (`component`, `station`, `origin`, `seq`, `sink`, `route`…), `message`, and
`err` as an array of causes when present.

**No journald-native layer.** journald captures stderr and parses one JSON object per line perfectly
well, and `libsystemd` is a shared library that a statically linked musl build does not have. The
same decision as §3.5's `sd-notify`, for the same reason.

### 5.4 Spans make per-station filtering trivial

The requirement is that an operator can isolate one device's entire story. That needs `station` to
appear on the reading path, the delivery path and the request path, and it needs the filter to be
able to match on a field *value*, which `EnvFilter` cannot do.

**Where `station` is attached:**

| Path | Span / event | Fields |
|---|---|---|
| ingest | `#[instrument(skip_all, fields(station, origin, seq))]` on `Ingress::admit` | plus `rejected`, `metrics` counts |
| decode | `codec` span inside the station component | `station`, `frame_len`, `codec` |
| delivery | `deliver` span on `Sink::deliver` with `sink`, `batch`; **plus one `TRACE` event per envelope carrying `station`** | a batch spans stations, so the per-envelope event is what the station filter catches |
| periodic | the component's ambient span | `component` (which embeds the station: `aprs:backyard`) |
| HTTP | `http.request` span | `method`, `route` (the *pattern*), `host`, `website`, `station` (after resolution), `status`, `latency_ms` |

**The filter.** The ring layer's filter is a reloadable pair: a `Targets` filter (module + level) and
an optional field predicate implemented as a `FilterFn` closing over `Option<StationId>` that
inspects the current span's recorded fields. "Show me only `backyard`" is applied **server-side**, so
the buffer is not flooded with the other stations' events while the operator is looking — and the UI
additionally filters client-side for what is already buffered. That is W5 §5's client-side filtering
and this document's server-side half, specified so they do not each invent one.

### 5.5 Runtime-adjustable levels, and the concurrency story

`tracing_subscriber::reload::Handle<EnvFilter, Registry>`, captured in `main` and handed to the
supervisor as a root fact.

- `handle.reload(new_filter)` and `handle.modify(|f| …)` take an internal write lock; every event
  takes a read lock. At a few hundred events per second the read lock is unmeasurable, and the write
  is held for the duration of parsing one directive.
- **Poisoning is handled rather than unwrapped.** A panic while the write lock is held would poison
  it and silence logging — the worst possible failure for a logging subsystem. `ReloadPoisoned` is a
  named `ObsError`, the modify closure is panic-free by construction (it only replaces a parsed
  filter), and the parse happens *before* the lock is taken.
- **Scoped raises are the common case and they expire.** The management API's call is
  `set_level(target, level, ttl)`; the supervisor records the expiry in its heap as an `OnDemand`
  occurrence, so "raise `mw_station::davis` to DEBUG for ten minutes" reverts without anybody
  remembering. A raise with no TTL is allowed and is shown in the UI as a standing override, because
  a debug level left on forever is a condition an operator should see stated.
- `RW-Debug: 1` (frozen, §6.6) is implemented as a per-request escalation rather than a shared
  mutable logger: the request span records `debug = true` and the ring filter admits events beneath
  a span carrying it, regardless of level. The current implementation instead mutates the shared
  `*gorm.DB` logger, so one debug request changes logging for every concurrent request.

**Where the handle lives in the capability graph. Decided: `ComponentControl`.** It is operator
control of a running process, the provider is the same root (the supervisor), and the consumer is the
same single component (`api:management`). A thirteenth capability for one consumer is noise in a
graph whose only two jobs are start ordering and `Waiting`.

The alternative worth naming and rejecting: making it a root fact in every `Cx`, like the config
watch. W2a's reason for the config watch being a root fact is that *every* component needs it;
exactly one component needs this, so the same argument runs the other way. Recorded as a widening of
`ComponentControl`'s meaning in the assumptions, so W2a can grep it.

### 5.6 Repetition control

A disconnected serial port on a 1-second reconnect backoff emits 3,600 identical lines an hour. That
is the difference between logs that are useful and logs nobody reads.

```rust
struct DedupeKey { callsite: callsite::Identifier, level: Level, station: Option<StationId>, class: Option<Class> }
const DEDUPE: Policy = Policy { burst: 3, refill: Duration::from_secs(30), keys: 1024, summary_every: Duration::from_secs(60) };
```

- **Keyed on the callsite, not the message.** `event.metadata().callsite()` is stable and free; a
  formatted message containing a varying port name or address would never match itself. `station` is
  in the key so one failing device does not suppress another's identical error.
- **Token bucket: burst 3, refill 1 per 30 s.** The first three occurrences are emitted
  *immediately* — nothing is delayed, which is the property that makes suppression acceptable.
- **Suppression is itself an event.** While a key is suppressing, one summary per 60 s:
  `warn!(suppressed = 118, window_s = 60, callsite = "mw_station::serial:214", "repeated event suppressed")`.
  So the serial-port example produces 3 immediate lines plus 60 summaries an hour instead of 3,600,
  and the operator can still see the rate.
- **Bounded**: 1,024 keys, LRU, `dedupe_evicted_total` counted.
- **Never suppressed**: events whose class is `Fatal`, and anything on `target = "mw::audit"` (config
  mutations, authentication outcomes, certificate reloads). An audit log with holes is not an audit
  log.

### 5.7 Two buffers, or one? — two rings, one stream

The current system keeps the application log (500 entries) and the HTTP access log (1,000 entries) in
separate buffers surfaced at `/logs` and `/http-logs`. Both halves of that are partly right:

- **The separation is justified by eviction asymmetry, and the asymmetry is measured.** `/latest` is
  93% of ~19,000 requests, so the access stream is one to two orders of magnitude the volume of the
  application stream. A single ring would mean an afternoon of normal traffic evicting the one
  `ERROR` the operator is looking for.
- **The *presentation* separation is wrong.** An operator correlating "this request 500'd" with "the
  query failed" has to read two screens with two clocks, and the correlation is the whole job.

**Decision: two rings, one API, one stream.**

```rust
pub struct LogRings { app: Ring<4096>, access: Ring<16384> }
// One subscribe() merges both in timestamp order; every record carries `stream: app | access`.
```

- Sizes: 4,096 application records (≈ 8× today's 500) and 16,384 access records. At ~300 bytes a
  record that is ~1.2 MB and ~5 MB, so **~6 MB resident, bounded and constant** — affordable on the
  512 MB target and stated so nobody has to guess.
- `/logs` and `/http-logs` collapse into one management route with a `stream` filter. W5 renders one
  view with a toggle.
- **On overflow**: the oldest record is dropped, `ring_dropped_total{stream}` is incremented, and the
  merged stream emits a synthetic record `{"kind":"gap","stream":"access","dropped":412}` so the UI
  shows "412 entries dropped" rather than silently skipping. That is the answer to W5 §5's "what the
  operator sees when the buffer overflows".
- Backfill on connect is the ring's current contents, newest-last, followed by live records — one
  code path, because the ring and the live stream are the same structure.

### 5.8 The ring layer stores structure, and ANSI is a rendering

The Go implementation serialises zap output to JSON and then **re-parses it back into structs**
(`json.Unmarshal` inside `LogBuffer::Write`, `internal/log/log.go:48`) to feed the UI. That is not
reproduced: `RingLayer` implements `Layer::on_event` and visits the event's fields directly, so a
record is built from the typed values and never from text.

```rust
pub struct Record {
    pub ts: DateTime<Utc>, pub level: Level, pub target: &'static str,
    pub module: &'static str, pub spans: SmallVec<[SpanName; 4]>,
    pub fields: SmallVec<[(FieldName, Value); 8]>,   // station, component, sink, route, …
    pub message: CompactString,
    pub causes: SmallVec<[CompactString; 4]>,        // the walked error chain
    pub stream: Stream,
}
```

**The owner's requirement — ANSI (or faux-ANSI) colours in the management UI — is satisfied from this
structure, in two renderings that share one formatter:**

1. **The default UI rendering** sends the structured record over SSE and the UI applies CSS colours
   from `chonky`'s theme tokens, using the same palette as the terminal formatter: ERROR red, WARN
   yellow, INFO green, DEBUG blue, TRACE magenta; `target` and `module` dimmed; field names dim
   italic, field values normal; the message in the default foreground. Text stays selectable,
   searchable and copyable, the colours follow light/dark mode, and contrast is a theme concern
   rather than a 16-colour accident.
2. **The terminal rendering** — for an operator who wants the literal thing, to paste into a ticket
   or to read through `mw logs -f` — renders the *same* `Record` through the *same* formatter with
   ANSI forced on, served as `text/plain` from an additive management route
   (`GET /api/logs?format=ansi`). The UI offers it as a "terminal view" and converts the escapes to
   spans client-side.

> **The rule: the ring never stores formatted text. ANSI is a rendering, produced from the record by
> the same formatter that writes stderr — never scraped from stderr and never re-parsed.**

That delivers faux-ANSI in the UI, a real-ANSI escape hatch, and a terminal session that looks like
the UI, while keeping the property that makes the whole thing work: one structured record, three
renderings (stderr pretty, stderr JSON, UI), one palette.

### 5.9 Actionable messages — the standard

Every event at `WARN` or above names four things: **the subject**, **the quantity affected**, **the
disposition**, and **the cause chain**. A message that would read identically for two different
subjects is a bug.

```rust
warn!(station = %key, metric = "out_temp", value = -412.0, limit = -100.0,
      reason = "below_range", disposition = "dropped", "reading discarded");
// pretty:  WARN  mw_pipeline::enrich  backyard  reading discarded
//                metric=out_temp value=-412 limit=-100 reason=below_range disposition=dropped

error!(sink = "writer:timescale", envelopes = 37, disposition = "spooled",
       spool_lag = 214, err = %DisplayChain(&e), "sink delivery failed");
// pretty:  ERROR mw_pipeline::sink  writer:timescale  sink delivery failed
//                envelopes=37 disposition=spooled spool_lag=214
//                caused by: query failed <- connection closed by peer
```

The two the prompt names, specified: **a dropped reading names the station, the metric and the
reason**; **a sink failure names the sink, how many envelopes, and whether they were spooled or
lost**. `disposition` is a closed vocabulary — `stored`, `spooled`, `dropped`, `retried`,
`suppressed`, `rejected` — so the UI can colour it and a query can count it.

### 5.10 Metrics are not logs

**Decision: Prometheus text exposition on an additive `/metrics` route on the one listener**, via the
`metrics` facade and `metrics-exporter-prometheus`, with the same numbers available as JSON on the
management API for the UI to read without a scraper.

Why Prometheus rather than counters on a status endpoint: a status endpoint has to invent a format,
and the operator already runs a homelab where a scraper is one more target. It costs nothing when
nobody scrapes — the recorder is an in-process registry and the route renders on demand. `metrics` is
not in D-32's table, so it is proposed as an **addition** (A-W3-1), not a substitution.

The handful of numbers that matter:

| Metric | Type | Labels | Why it is here |
|---|---|---|---|
| `mw_readings_ingested_total` | counter | `station`, `origin` | the system's reason to exist; a station that stops is a flat line |
| `mw_readings_rejected_total` | counter | `reason` | somebody pointing a station at us, by reason (W2a §4a.6) |
| `mw_spool_depth_bytes`, `mw_spool_lag_envelopes` | gauge | `cursor` | **sink lag and spool depth** — the single best indicator that something downstream is wrong |
| `mw_sink_delivery_seconds` | histogram | `sink` | distinguishes "slow" from "stalled" |
| `mw_readings_dropped_total` | counter | `reason` | **dropped-reading count by reason**: `spool_full`, `lagged`, `out_of_range`, `rejected` |
| `mw_snapshot_age_seconds` | gauge | `website`, `station` | the degradation signal of §2a.6 |
| `mw_aggregate_refresh_seconds` | histogram | `level` | **aggregate refresh duration**, against D-17's measured 306 s → 25.6 s |
| `mw_component_state` | gauge | `component`, `state` | one series per state, so "how long has it been Waiting" is a query |
| `mw_component_ticks_total` | counter | `component`, `outcome` | with `skipped_total`, `deadline_exceeded_total`, `misfired_total`, `failed_total`, `restart_total`, `lagged_total` — exactly W2a Decision 20's list |
| `mw_http_requests_total`, `mw_http_request_seconds` | counter, histogram | `route`, `status` | `route` is the **matched pattern**, never the URI |

**Cardinality is bounded by construction and that is a rule, not an observation**: every label is a
station id, a component id, a route pattern or a closed enum. A label whose values come from a
request — a path, a `Host`, a user agent — is forbidden, and the review rule is that a new label must
be justifiable as "there are N of these and N is in the configuration".

---

## 6. The HTTP layer

Today listener construction is spread across seven files, each with its own TLS setup and shutdown
path: `restserver/controller.go` (HTTP, HTTPS, gRPC, plus two more), `storage/grpcstream/grpc.go`,
`weatherstations/grpcreceiver/station.go`, `controllers/management/controller.go`,
`weatherlinklive/udp_receiver_{unix,windows}.go` and `ambientcustomized/station.go` — the last of
which creates **one `http.Server` per device**.

### 6.1 One router, one route table — and why that is not literally one socket

D-31 and W2a's `HttpRoutes` give one `http:listener` component that mounts every route table. Read
literally as "one socket", that would move the gRPC services from their current dedicated port onto
443 and break every machine-to-machine peer on upgrade.

> **The unit that is singular is the router and the route table, not the file descriptor.**
> `http:listener` binds **N sockets from configuration** — the main port, an optional plaintext port
> that redirects, and the configured gRPC port — and serves **one `Router`** on all of them. One
> place owns listening, one route table, one TLS story, one shutdown path.

**The gRPC port keeps its own socket, and this is not a concession.** `grpc_port` is a real
configuration column with a default of 50051 (`pkg/config/provider_sqlite.go:231`), and the
management API *validates that it differs from both the HTTP and the HTTPS port*
(`handlers_controllers.go:501`, `:506`). So a separate socket is the configured, validated,
shipped behaviour — folding it onto 443 would mean invalidating a config column, breaking every
machine-to-machine peer at upgrade, and making the rewrite's first operational event a flag day
across the fleet. It stays, bound whenever the configuration names a port, and serving the same
`Router` as everything else so there is still exactly one route table.

That is recorded as proposed amendment **A-W3-4** to D-31, because the prompt's own wording is "one
listener, not seven" and the difference matters to W7.

### 6.2 Declaring and mounting a route table

```rust
// mw-http — the type published through the additive HttpRoutes capability.
pub struct RouteTable {
    pub owner:  ComponentId,
    pub scope:  Scope,              // Site(WebsiteId) | Any | Management
    pub routes: Vec<Route>,
    pub grpc:   Vec<GrpcService>,   // tonic services, mounted by their full method prefix
}

pub struct Route {
    pub pattern: &'static str,      // axum syntax: "/span/{span}"
    pub methods: Methods,           // `Methods::ANY` for the frozen surface (Q-17)
    pub flags:   RouteFlags,        // CORS, compression, cache-control, access-log class
    pub handler: MethodRouter,
}

/// The PURE half, available from the spec before anything is built.
pub struct RouteManifest { pub scope: Scope, pub entries: Vec<(Methods, &'static str)> }
```

**Conflict detection happens at plan time, which is before any socket exists.** `Provision` carries
an optional `ProvisionDetail::Routes(RouteManifest)`, so the union of every component's declared
paths is a pure function of configuration and `plan()` returns
`PlanError::RouteConflict { scope, pattern, a, b }` naming both owners. That makes conflict detection
a unit test over fixture configurations and a CI check over the shipped examples — the same property
W2a Decision 2 gives every other structural error.

It is **not** detectable at build time in the Rust sense, and the reason is worth stating so nobody
tries: the set of websites is configuration, not source. A six-website deployment and a one-website
deployment have different route tables from the same binary. The nearest compile-time win available
is that every pattern is a `const` on the owning component and the manifest is derived from the same
constants the handlers are registered with, so a typo exists in one place; a mismatch between the
manifest and the registered handlers is a `BuildError::UndeclaredRoute` and a per-component unit test
asserts the two sets are equal.

Carrying a `RouteManifest` on `Provision` is an extension to W2a's `ComponentSpec` — one enum with
one variant — and it is **taken**, because the two alternatives are both worse in a specific way.
Detecting at mount time means the one component everything else's reachability depends on is the
component that fails, and it fails *after* its siblings have started. Not detecting is the current
behaviour, and it is how two components can silently claim `/latest` with the last registration
winning. Recorded in the assumptions so W2a can grep it.

**Mounting.** `http:listener` subscribes to the additive capability's
`watch<Arc<Vec<RouteTable>>>`, rebuilds a `Router`, and publishes it through
`ArcSwap<Router>`. In-flight requests keep the old `Arc`; new ones take the new. Rebuilding is a
handful of tables and happens only on a plan diff.

**Late binding is load-bearing** (W2a Decision 19): the listener comes up with the routes it has and
never waits for all providers, because a management UI that is unreachable exactly when something is
broken is the worst possible failure. A request to a *declared but not yet mounted* path returns
**503** with a body naming the component and the capability it is waiting on. W1 assumption 28 asks
whether that 503 can be observed on a frozen path; the answer this design produces is **no**:
`api:legacy` requires only `LatestSnapshot` hard, and `snapshot:latest` requires `StationRegistry`
hard, both of which are available as soon as `config:sqlite` is ready — which is before the listener,
since it is a root in the ordering.

### 6.3 Hostname routing

```rust
struct HostTable {
    exact:    HashMap<Box<str>, WebsiteId>,   // lowercase, port-stripped
    wildcard: Vec<(Box<str>, WebsiteId)>,     // suffix after "*.", one label only
    default:  WebsiteId,
    generation: u64,
}
// Published as ArcSwap<HostTable>, rebuilt wholesale on the config watch.
```

A reader does `host_table.load()` — one atomic — and holds an `Arc` for the duration of the request,
so it sees one generation or the next and never a mixture. **This is the direct replacement for the
measured race**: the Go controller holds `WeatherWebsites`, `Devices`, the device-name map, the
devices-by-website map and `tlsConfigs` as plain fields with no mutex and replaces them wholesale
from the management API while in-flight requests read them (`controller.go:708`), and a concurrent
map read and write in Go is an unrecoverable fatal error.

Normalisation, matching the frozen behaviour plus one deliberate deviation:

1. strip the port (`localhost:18080` ≡ `localhost`) — frozen behaviour, reproduced;
2. strip a single trailing dot (`example.com.` ≡ `example.com`) — new, and unobservable today
   because such a request currently falls through to the default;
3. **lowercase** — a deliberate deviation. Hostnames are case-insensitive, the Go lookup is a
   case-sensitive map, and today `EXAMPLE.COM` silently serves the default website's data under
   someone else's hostname. No fixture covers it. Recorded in the Decisions list so W6's ledger
   carries it rather than someone discovering a diff.

**Unmatched `Host` → the default website**, preserving Q-18 exactly: nothing in the response
distinguishes "you asked for the right host" from "you asked for an unknown host and got someone
else's station". The default is selected by reproducing `controller.go:186` precisely — the first
website the config provider returns, **unless some website has an empty hostname, in which case that
one wins**.

**No website configured at all** → `503` with the JSON body `{"error":…,"message":…}` on the API
routes and on `/` and `/portal`, while `/privacy` and `/support` bypass the check entirely (W1 §9).
That asymmetry is in the contract, so it is a `RouteFlags` bit (`skip_website_check`) rather than
something each handler remembers.

### 6.4 Per-website TLS via SNI

```rust
struct CertStore {
    exact:    HashMap<Box<str>, Arc<CertifiedKey>>,
    wildcard: Vec<(Box<str>, Arc<CertifiedKey>)>,
    default:  Option<Arc<CertifiedKey>>,
    generation: u64,
}
struct SniResolver { store: ArcSwap<CertStore> }

impl rustls::server::ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let s = self.store.load();
        let name = hello.server_name()?.to_ascii_lowercase();   // None → fall through to default
        s.exact.get(name.as_str()).cloned()
            .or_else(|| s.wildcard.iter()
                .find(|(suf, _)| matches_one_label(&name, suf)).map(|(_, k)| k.clone()))
            .or_else(|| s.default.clone())
    }
}
```

**Loading.** Certificate and key paths come from `ConfigStore`, exactly as today. They are read when
`http:listener` builds and again on every config generation, parsed with `rustls-pemfile`, and the
private key is wrapped in `secrecy::SecretSlice` and zeroised on drop (D-33). A path that is
configured but unreadable is a **build** failure → `Failed` → `Backoff`, with the path in the error
chain — never a silent substitution of some other site's certificate.

**Keying and matching.** Exact match first, lowercase. Then wildcards, and `*.example.com` matches
**exactly one label**: `a.example.com` yes, `a.b.example.com` no. The current implementation uses
`strings.HasSuffix(hostname, domain)` after stripping `*.`, which matches any depth **and matches
`evilexample.com` for `*.example.com`** — a certificate handed out for a name it does not cover.
Recorded as a found defect; the replacement requires a `.` boundary and a single remaining label.

**Reload without dropping the listener.** The resolver holds `ArcSwap<CertStore>`; a renewal is
`store.store(Arc::new(new))`. The socket is untouched, in-flight handshakes keep the `Arc` they
loaded, and the next `ClientHello` sees the new store. A certificate swap is therefore an atomic
pointer write and not a listener restart — which the Go version cannot do, because it rebuilds
`tlsConfigs` as a plain map (`controller.go:708`, `:734`) while `getCertificate` reads it.

**On an SNI miss** the chain is: exact → wildcard → the **designated default certificate** →
otherwise fail the handshake with `unrecognized_name`. The default is explicit: a configured
`default_tls_website`, else the default website's certificate if it has one, else none. The current
behaviour instead iterates a Go map and returns the first certificate it finds
(`controller.go:826`), which is **non-deterministic between runs** and hands a client a certificate
for a name it did not ask for. Refusing is better: a client-visible `unrecognized_name` is
diagnosable, while a wrong certificate produces a browser interstitial that blames the user.

A client sending **no SNI at all** (an IP-address request, an ancient client) takes the same path and
gets the designated default. ACME and automatic issuance are out of scope: certificate paths come
from the config store, which is the frozen operational model.

### 6.5 One schema, two encodings

`generated/proto/reading.proto` is the single definition. `mw-proto` runs `prost-build` (fed by
`protox`, §1.6) to produce the Rust types and `pbjson-build` to produce `serde` impls with protobuf's
JSON mapping. Together they replace the ~591 hand-maintained mapping lines in `grpcstream/conversion.go`
(276) and `restserver/weatherapps_grpc.go` (315), which is 591 opportunities for the two surfaces to
disagree.

**The wire type stays distinct from the domain type, with one conversion at the boundary:**

```rust
// mw-proto — the only two conversions in the workspace. Units and Option live here.
impl TryFrom<&pb::Reading> for Reading { type Error = WireError; /* … */ }
impl From<&Reading> for pb::Reading     { /* … */ }
```

Every transport shares them: `station:grpcrx`, `uplink:collector`, `grpc:weatherv1` and
`grpc:weatherappsv1` all go through the same `TryFrom`. The legacy REST surface does **not** — it has
its own DTO in `mw-api-legacy` (§1.7), because the frozen JSON keys are not protobuf's JSON mapping
and pretending otherwise is how `ohum`/`outhumidity` would get "cleaned up".

**One defect this exposes, and it is not W3's to fix alone.** The committed proto declares metric
fields as bare proto3 scalars (`float barometer = 4;`), which have **no field presence** — absent and
`0.0` are the same bytes. This document's writer stores `NULL` for an unreported metric (§2.5), so a
reading forwarded to a collector arrives with `0.0` where the satellite held `NULL`, and the
collector stores a zero. That is a silent data corruption across a hop, visible only as a wrong
average months later. **The call: W2's generator emits proto3 `optional` on every metric field.** It is wire-compatible —
same field numbers, same tag bytes, same encoding; explicit presence is bookkeeping in the generated
code, not a format change — so it costs nothing on the wire and is the only option that lets a
satellite and its collector hold the same data. The work is one line per field in a generator that
already emits the file. This is the one decision here that another work item has to execute, so it is
recorded against W2 (the generator), W7 (the uplink must carry presence) and W4 (a collector-written
row can now be NULL) rather than being settled unilaterally in silence.

### 6.6 gRPC on the same socket, and the rest of the transport behaviour

**ALPN.** `rustls` is configured with `alpn_protocols = [b"h2", b"http/1.1"]`, matching the Go
server's `NextProtos`. gRPC is HTTP/2 with `content-type: application/grpc`, so dispatch is a header
test in the router's fallback (`tower::steer::Steer` over the axum router and the tonic service) —
no byte sniffing, no `cmux`, no second TLS context.

**Plaintext gRPC** for a LAN peer with no TLS is served by `hyper-util`'s auto connection builder,
which accepts HTTP/1.1 and HTTP/2-with-prior-knowledge on the same plaintext socket. So a peer that
speaks h2c works on the main port, and §6.1's optional legacy gRPC port exists for peers pinned to
the old address.

**Frozen transport behaviour that is a `RouteFlags` bit rather than a convention**, because every one
of these is a way to silently break a client (W1 §1):

| Behaviour | Implementation |
|---|---|
| every route accepts every method (Q-17) | `Methods::ANY`; `HEAD` returns headers with an empty body |
| routes are exact, not prefixes; `/latest/` 404s into the file server | **no `NormalizePath` layer** — axum must not redirect or strip trailing slashes |
| `Vary: Accept-Encoding` and gzip when asked | `tower_http::CompressionLayer`, **gzip and deflate only** — brotli is deliberately disabled, because advertising `br` would change `Content-Encoding` for clients that accept it, and the fixtures are `identity` |
| `Access-Control-Allow-Origin: *` on JSON endpoints, absent on HTML and assets | a per-route-table flag, not a global layer |
| `/forecast/{span}` and `/alerts` only exist when some device has Aeris enabled | the Aeris components' `RouteManifest` is conditional on configuration, so when no device enables Aeris the paths are never mounted and the catch-all file server answers with `404 page not found` — exactly today's behaviour, and it falls out of plan-time route declaration rather than needing a special case |
| `RW-Debug: 1` | §5.5 — per-request log escalation, no shared state, response unchanged |

---

## 7. Serving the frozen public website

W1 §9 is the specification of what these routes must emit. This section is the mechanism.

### 7.1 Templates: askama, and how the port is verified

**Decision: port the two HTML templates to `askama`** (compile-time checked, derive-based).

- *Not maud*: maud is a Rust DSL, so porting 932 lines of HTML means rewriting it as Rust. The
  requirement is that visual output must not change, and a rewrite is the highest-risk way to meet
  it.
- *Not pre-rendering*: the pages interpolate per-website values (station name, latitude, longitude,
  radar token, Apple app id) and are invalidated by configuration, so pre-rendering is a cache rather
  than a mechanism. §7.3 *does* cache the rendered bytes; that is a different thing.
- *Not a runtime engine (Tera, minijinja)*: the templates carry 60+ actions, and a runtime engine
  moves "this field does not exist" from compile time to request time. That is the defect class being
  removed, not a trade.

The port is mechanical — `{{ .StationName }}` → `{{ station_name }}`, `{{ if .SnowEnabled }}` →
`{% if snow_enabled %}` — and mechanical ports fail on typos. So the acceptance test is not an
eyeball:

> **The website port is verified by byte-diffing the rendered output against W1's recorded HTML
> bodies for the six captured websites.** W1 Decision 14 records the full rendered bodies *as a
> reference for W3* while asserting only at substring level; this document takes the stronger
> assertion.

Byte equality is achievable only with one more decision. Go's `html/template` escapes `'` as `&#39;`
and `"` as `&#34;`; askama's default HTML escaper emits `&#x27;` and `&quot;`. Those differ on every
attribute containing an apostrophe.

> **`mw-web` implements `GoHtmlEscaper`** — `&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;`, `"` →
> `&#34;`, `'` → `&#39;`, NUL → `&#0;` — and registers it as askama's escaper for the `html`
> extension. That is what makes the byte-diff a real test rather than an aspiration.

Two incidental wins: the per-request `htmltemplate.Must(...ParseFS(...))` inside the handlers
(`handlers.go:801`, `:873`, `:947`) disappears, because askama compiles the template into the binary;
and `Must` panicking inside a request handler stops being a possible failure mode.

`portal.html.tmpl` consumes **nothing** — the handler passes `PageTitle` and the template has zero
`{{ }}` actions — so it becomes a static asset with no template at all. Recorded because it looks
like an omission otherwise.

### 7.2 Static assets

**Embedded in the binary** (`rust-embed`), as the Go version already does with `embed.FS`. One static
musl binary per architecture, no asset path to configure, and no partial upgrade where the binary and
the JavaScript disagree about the API. A debug-only `--assets-dir` override exists for development
and is not compiled into release builds.

**The mime table is a hard-coded `const`, not `mime_guess`.** Two of the frozen content types are
quirks that a mime crate will get "right" and therefore wrong:

| Path | Frozen Content-Type |
|---|---|
| `/js/weather-app.js` | `text/javascript` — **no charset** |
| every other `/js/*.js` | `text/javascript; charset=utf-8` |
| `/css/*.css` | `text/css; charset=utf-8` |
| `/site.webmanifest` | **`text/plain; charset=utf-8`** (Go's mime table has no entry for the extension) |
| `/fonts/*` | `font/ttf`, `font/woff2`, … |
| `/images/*`, `/favicon.ico` | sniffed — `image/vnd.microsoft.icon` for `.ico` |

**Cache headers: reproduce the frozen behaviour exactly, add conditional GET, and do not add
`max-age`.** Today only `/fonts/` carries `Cache-Control: public, max-age=31536000, immutable`; every
other asset is served with no caching headers at all, on payloads that change only when the binary
does. The frozen behaviour is kept byte for byte, and `ETag` (the embedded content's hash) plus
`Last-Modified` (the build timestamp) are **added**, so a client sending `If-None-Match` gets a 304
and one that does not gets today's response.

**`max-age` is deliberately not added, and that is a decision rather than caution.** `ETag` already
captures the entire practical benefit: the browser revalidates and gets a 304 with no body, so the
bytes are saved. What `max-age` adds on top is skipping the revalidation request itself — worth
microseconds on a LAN — and what it costs is real: the asset filenames are not content-hashed, so a
long `max-age` means a browser keeps running the *previous release's* `weather-app.js` against the
new server until the TTL expires, which is a support call that looks like a server bug. A short
`max-age` saves nothing `ETag` has not already saved. The right version of this change is
content-hashed filenames plus a year-long `max-age`, and that is a website change, which D-38 puts
out of scope.

**Directory listings** (Q-20): W1's OQ-W1-4 recommends returning 404 instead of reproducing
`http.FileServer`'s HTML index for `/js/`, `/css/`, `/images/` and `/fonts/`. This document follows
that recommendation — **404** — and records it as a deliberate deviation needing the owner's
confirmation, because it is the one place where "reproduce exactly" costs something and buys nothing.

### 7.3 The rendered-page cache

`/` and `/portal` depend only on `(website, config_generation, binary_version)`, so `web:site:<id>`
renders once per generation and serves `Bytes`. Invalidation is the config watch; there is no TTL and
no revalidation.

At 217 requests a day this is **not** a performance need and claiming otherwise would be dishonest.
It is here because it makes the website the same shape as every other read path — a component
maintains a published artefact, a handler writes bytes — so there is one pattern in the codebase
instead of two.

### 7.4 `weather-app.js.tmpl` is replaced — the config payload and how it is delivered

D-38 decides the replacement; this document owns the mechanism. The template interpolates **nine**
values into a JavaScript object literal (`js/weather-app.js.tmpl:9-17`), which W1 §9 independently
confirms is exactly what it consumes:

```js
const config = {
    pullFromDevice: '{{ .PullFromDevice }}',  stationID: {{ .StationID }},
    snowEnabled: {{ .SnowEnabled }},          snowDevice: '{{ .SnowDevice }}',
    airQualityEnabled: {{ .AirQualityEnabled }},  airQualityDevice: '{{ .AirQualityDevice }}',
    airQualityDeviceID: {{ .AirQualityDeviceID }},  stationName: '{{ .StationName }}',
    aerisWeatherEnabled: {{ .AerisWeatherEnabled }},
    debug: new URLSearchParams(window.location.search).get('debug') === '1'
};
```

The payload, with keys that are exactly the JavaScript object's keys so the module's body does not
change at all:

```json
{
  "pullFromDevice": "backyard",
  "stationID": 2,
  "stationName": "Backyard Weather",
  "snowEnabled": true,
  "snowDevice": "snowgauge",
  "airQualityEnabled": true,
  "airQualityDevice": "aqm",
  "airQualityDeviceID": 4,
  "aerisWeatherEnabled": true
}
```

**Delivered two ways, with the inline block as the primary path:**

1. **Inline**, as a `<script id="mw-config" type="application/json">` block in
   `weather-station.html.tmpl`. The page already does exactly this for the radar module
   (`<script id="radar-config" type="application/json">` at line 323), so it is the established
   pattern in this codebase rather than an invention. `weather-app.js` becomes a static asset whose
   first line is `const config = JSON.parse(document.getElementById('mw-config').textContent)`.
2. **`GET /config.json`**, additive, per-website by `Host`, `Cache-Control: no-cache`, for anything
   else that wants it.

The inline block is primary because a `fetch` at module initialisation would introduce an extra round
trip and an asynchronous boundary into a module that is currently synchronous — the kind of change
that produces a flash of unstyled content and an ordering bug, in a page that is supposed to be
unchanged. JSON embedded in a `<script type="application/json">` block escapes `<` as `\u003c`, so a
`</script>` sequence inside a station name cannot terminate the block.

**No JavaScript is ever templated.** That is the whole point of D-38: `text/template` has no
JavaScript context awareness, so a station name containing an apostrophe currently breaks the page
and one containing `';alert(1);//` executes. After this change, configuration values pass through a
JSON encoder and never through a code generator.

**The rendered result is unchanged**, and that is checkable: the HTML differs from W1's recorded
bodies by exactly one added `<script id="mw-config">` block and a changed `src` attribute on the
weather-app script tag, and the DOM after the module runs is identical because `config` holds the
same nine values. The byte-diff of §7.1 is run with that single expected delta.

### 7.5 `AboutStationHTML` and the trust boundary

**The premise in the prompt does not hold, and the correction changes the answer.**
`AboutStationHTML` is passed to `weather-station.html.tmpl` as `htmltemplate.HTML` (which *would*
suppress escaping) but the template contains **no `{{ .AboutStationHTML }}` action** — it is passed
and ignored, which W1 §9 records from the template's side. It is also passed to
`js/weather-app.js.tmpl` and ignored there. So today it reaches no rendered page at all.

Where it does reach a client is `/stationinfo` → `about_text`, as a JSON **string**, escaped by Go's
encoder (`<`, `>` and `&` become `\u003c`, `\u003e` and `\u0026`) and rendered by the
client.

The boundary, stated as a rule so it survives:

> **The administrator is trusted to author HTML. The server never widens that trust by rendering it
> into a page, where it would acquire the page's origin.** `about_text` is carried as a plain
> `String`, serialised as a JSON string with the same escaping Go applies (W1 §6.4 — any Rust encoder
> must escape `<`, `>` and `&` or the bytes differ), and never interpolated into a template. If a
> future page needs to display it, it is sanitised with `ammonia` at that boundary and the
> sanitisation lives in `mw-web`, not in the config store.

So `mw-web` gains a rule with teeth rather than a convention: `cargo xtask gate layout` rejects the
`safe` and `escape("none")` filters anywhere under `crates/mw-web/templates/`, and the custom
`GoHtmlEscaper` of §7.1 is the only escaper registered. A template that wants raw HTML fails the
gate, and the one place that could legitimately want it does not exist.

### 7.6 The module boundary

`mw-web` depends on `mw-component`, `mw-http` (for `RouteTable`) and `mw-types`. It may **not**
depend on `mw-store`, `mw-api-legacy` or `mw-proto`, which is checked by §1.4. Its template context
structs live in `mw-web` and are used nowhere else, so "the website needs one more field" cannot
become "the domain type grows a field".

`web:site:<id>` requires `LatestSnapshot` hard and `ReadingStore`, `ForecastStore`, `AlertStore` and
`SnowCache` **soft** (W2a §4.4), so a failed Aeris component costs `/forecast` and nothing else, and
a database outage does not keep the site from serving.

---

## 8. Testing strategy

### 8.1 What is a unit test, per layer

| Layer | What is tested without I/O | Where |
|---|---|---|
| `mw-types` | `Reading::get` over all 212 metrics, `METRICS` lookups, unit round trips | `crates/mw-types/src/` |
| `mw-codec` | every codec against **captured frames**: Davis LOOP (struct layout, CRC16), Campbell ASCII, WLL conditions, Ambient query strings, AirGradient JSON | `crates/mw-codec/tests/frames/<driver>/*.bin` |
| `mw-aprs` | the twelve pure functions W2a §4b.4 moves out of the controller, against **captured packets**; CWOP information fields byte for byte | `crates/mw-aprs/tests/packets/` |
| `mw-pipeline::enrich` | wind chill, heat index, AQI, solar, snow depth — against the Go formulas **and** against W1's recorded `/latest` values, plus property tests for monotonicity and the NWS formula's cut-offs | `crates/mw-pipeline/src/enrich.rs` |
| `mw-supervisor::plan` | every `PlanError` variant, including route conflicts, cycles, `deadline > period`, sub-floor intervals, impossible identities | `crates/mw-supervisor/tests/plan/` |
| `mw-supervisor::clock` | `Calendar` arithmetic: spring-forward gap, fall-back ambiguity, `Misfire::RunLatest` grace, occurrence-key deduplication, wall-clock step | pure, with a fixed `chrono-tz` database |
| `mw-api-legacy` | the DTO against W1's 70 fixtures — key set, key order, numeric formatting rule, trailing newline | via the conformance runner |
| `mw-http` | host normalisation, wildcard single-label matching, the SNI chain including miss and no-SNI | with generated test certificates |
| `mw-web` | **byte-diff** of rendered pages against W1's recorded HTML bodies | `crates/mw-web/tests/` |

All of these run in the existing `test` job with no services.

**`plan()` over the shipped configurations is a CI check, not just a test.** `cargo xtask gate layout`
also loads every example configuration in `ci/examples/*.db` and asserts `plan()` succeeds, so a
change that makes a shipped example unplannable fails the PR rather than a deployment.

### 8.2 What needs a real TimescaleDB container

The `database` job already runs `timescale/timescaledb-ha:pg17` as a service container with
`DATABASE_URL` set, so this costs no workflow change — it gains a second step.

- **Migrations** — W4's `db-migrate-twice` gate, already declared.
- **`ReadingStore` queries**, and specifically `/span`'s four levels and their exact boundaries
  (≤ 6 h, ≤ 48 h, ≤ 14 d, > 14 d), because a wrong boundary changes the `ts` spacing in every
  client's chart.
- **The finalisers.** `mw_mean(c_sum, c_count)` and `mw_circular_mean(c_sin_sum, c_cos_sum,
  c_dir_count)` compared against a direct scan over the same rows — this is D-12 and D-13 as an
  executable assertion rather than a spike result.
- **W1-BUG-1 as a regression test.** Insert a station whose `winddir` is NULL throughout a window and
  assert `/span` returns **200**, not 500. This document's writer stores NULL (§2.5), so this test is
  the thing that keeps the two decisions compatible.
- **`window:rollup`'s arithmetic.** Roll ten 1-minute buckets into a ten-minute mean and compare
  against a direct scan; assert bit-identity for `sum`/`count` means, and agreement to 1e-9 degrees
  for circular means.
- **Aggregate refresh duration**, recorded as a metric rather than asserted as a threshold, so the
  trend is visible without a flaky test.
- **Batch-insert bounds.** Assert the 256-row batch does not exceed 65,535 bind parameters at the
  current column count, and that the assertion is computed from `generated/rust/metrics.rs` rather
  than hard-coded — so adding metrics fails this test before it fails in production.

**Skipping must be loud.** A test needing the database checks `DATABASE_URL`; absent, it prints a
named skip. `cargo xtask gate db-tests` fails if any database test *skipped* in CI, so "green because
it did not run" is not reachable.

### 8.3 How a driver is tested without the physical device

Three levels, and the decomposition in W2a §4a.2 is what makes the first two possible:

1. **`Codec` alone, against captured frames.** Pure, no harness, no clock. This is the level that
   exists only because `Session` was split out — a codec that had to perform the Davis wake
   handshake could not be tested this way.
2. **`Session` against a recorded transcript.** A transcript is a list of steps over an in-memory
   `ByteStream`:
   ```
   Expect(b"\n")                  # we wake the console
   Send(b"\n\r")                  # it acknowledges
   Expect(b"LOOP 20\n")
   Send(<20 captured LOOP packets>)
   Expect(b"LOOP 20\n")           # the re-issue — see below
   ```
   A mismatch reports the step index and a hex diff. This level is what would have caught the Davis
   defect W1 found while capturing: `GetLoopPackets` loops only on *error* and `return`s on success,
   so after one successful batch of 20 the station never asks for another and readings simply stop.
   The transcript's final `Expect` is the assertion that the re-issue happens.
3. **The whole driver against the Go emulators.** D-40 keeps `davis-emulator`, `campbell-emulator`,
   `weather-station-simulator` and `live-data-simulator` in Go deliberately, as independently written
   oracles. `cargo xtask emulator-tests` (W8's `driver-emulators` gate, armed by
   `tests/emulators/manifest.toml`) checks out `chrissnell/remoteweather` at a **pinned commit**,
   builds the emulator, runs it on a TCP port, points the Rust station component at it, and asserts
   N envelopes arrive with plausible values and the expected metric set.

   **Serial framing gets a pty.** W8 assumption 15 leaves per-driver whether a `socat` pty pair is
   worth it; the answer for **Davis and Campbell is yes**, because the serial path has framing and
   read-timeout behaviour the TCP path does not exercise, and because a timeout bug is exactly the
   class that produced the defect above. The other drivers are TCP, UDP or HTTP natively and need no
   pty.

   Fault injection is deterministic: the harness supplies a seed, and corruption, truncation and
   disconnection rates are seeded from it, so a failure reproduces from the seed printed in the
   output.

The emulators are **fixed external test infrastructure**, not something to reimplement. An emulator
written alongside the driver, by the same person, from the same understanding, tests that
understanding against itself.

### 8.4 The concurrency tests that would have caught the Go bugs

Both jobs already exist in the gate (`race` under ThreadSanitizer, `loom` on stable) and currently
find nothing to run. These are the first things they run.

**1. Config reload racing live readers (D-28).** The measured failure is a concurrent map read and
write that kills the Go process when a website is saved while serving traffic.

```
spawn 64 reader tasks, each: host_table.load().resolve(host) → assert the returned website's
    `generation` matches the table it came from (a sentinel the test checks, not a field the
    production path reads)
spawn 1 writer: 10,000 × host_table.store(Arc::new(next_generation()))
assert: every reader saw a self-consistent table; TSan reports no race; no reader observed a
    website that was never in any generation
```
Run for `HostTable`, `CertStore` and `SnapshotTable`. The assertion is the **absence of a torn
read**, which is the property; absence of a crash is not, because `ArcSwap` would not crash even if
the design were wrong.

**2. Shutdown with a stalled sink (D-29).** The measured failure is an unguarded blocking send that
hangs shutdown forever.

```
let sink = StallingSink;                     // deliver() never returns
drive 1,000 admits through Ingress
assert: no admit took longer than 50 ms + fsync       — a source is never blocked by a sink
signal shutdown
assert: shutdown completes within grace + 500 ms
assert: stop_timeout_total == 1 and the stalled task was aborted
restart the pipeline against the same spool
assert: the cursor replays from the un-acked offset and zero envelopes are lost
```
Run with `tokio::time::pause()`, so it is deterministic and finishes in milliseconds rather than
sleeping through a 10-second grace.

**3. `loom` interleavings** — the two places with lock-free code, both small enough to be exhaustive:
the `ArcSwap` snapshot publish/read, and the spool's append/ack/replay ordering (does a cursor ever
ack past an append it did not observe?).

**4. Capability revoke and restore under load.** Revoke `ReadingStore` while `writer:timescale` is
mid-batch; assert the component moves to `Waiting { on: [(ReadingStore, db:postgres)] }`, that the
spool accumulates, that no envelope is lost, and that delivery resumes automatically when the
capability is republished. This is W2a §3.4's worked example as a test.

**5. Deadline and cancel safety.** A `Tick` that ignores its cancellation token; assert the deadline
fires, the tick is dropped, `deadline_exceeded_total` increments, the **component** survives (the
per-tick token is a child, §3.4), and the next occurrence is not skipped.

**Answering W8's open question (assumption 5).** The gate's shape does not change. Tests 1, 3 and 4
run on a multi-threaded tokio runtime, which is ordinary pthreads and instrumentable by
ThreadSanitizer. Tests 2 and 5 use `tokio::time::pause()`, which requires the current-thread runtime
and is therefore single-threaded by construction — TSan has nothing to find in them, and they run
under plain `cargo test`. No test needs a runtime TSan cannot instrument.

### 8.5 Coverage

W8's ratchet starts at 0.00 and is raised by hand in the PR that earns it. Two notes so the ratchet
is not gamed:

- The pure crates (`mw-types`, `mw-codec`, `mw-aprs`, `mw-pipeline::enrich`, `plan()`) are cheap to
  cover and should be near-total from the PR that lands them. Raise the floor in that PR.
- A floor satisfied by those alone is a floor that says nothing about `mw-http`, `mw-store` or the
  supervisor. The review rule is that a PR landing an impure crate raises the floor by *less* than
  its line count would suggest, and the conformance, emulator and database gates — none of which
  `llvm-cov` counts well — are the real coverage for those.

---

## 9. Extensibility walkthroughs

Each traces a change end to end and names every hand-written file touched. The calibration the prompt
asks for: adding a metric in the Go version touches roughly eight places.

### 9.1 Add a new weather station driver — all three trigger shapes

**(a) Serial / TCP loop — e.g. a Modbus sensor.**

| File | Change |
|---|---|
| `crates/mw-codec/src/modbus.rs` | **new** — `impl Codec`, pure, over the response frame |
| `crates/mw-codec/src/lib.rs` | +1 line, `pub mod modbus;` |
| `crates/mw-station/src/session/modbus.rs` | **new** — `impl Session`: write the register read, read the response |
| `crates/mw-station/src/plan.rs` | +1 match arm: config `type = "modbus"` → `(Transport::TcpOut \| Serial, ModbusSession, ModbusCodec, Identity::Fixed, Trigger::Every{30s})` |
| `crates/mw-codec/tests/frames/modbus/*.bin` | **new** — captured frames |

**Four files.** Not touched: the supervisor, the pipeline, the listener, identity resolution,
authentication, the spool, any SQL, any schema.

**(b) HTTP poller — e.g. a vendor cloud API.** The codec file, its `lib.rs` line, and one arm in
`mw-station/src/plan.rs` declaring `Transport::HttpOut` + `Trigger::Every`. **Three files**, no
session. The cadence is the trigger, not the transport — W2a Decision 40, and the reason the
abstraction does not need an `HttpPoll` variant.

**(c) Inbound listener — e.g. a second vendor webhook.** The codec file, its `lib.rs` line, one arm
in `plan.rs` declaring `Transport::HttpIn { mount }` with `Identity::FromQuery`, and the
`RouteManifest` const beside it. **Four files.** Not touched: any listener, any TLS configuration,
any port. The component provides `HttpRoutes` and is mounted by `http:listener` — which is the whole
difference from today, where a second Ambient station means a second `http.Server` on a second port.

The contrast with the Go tree: a new driver there needs a package under `internal/weatherstations/`,
a case in `createStationFromConfig`, a configuration struct, often its own `http.Server`, and it
shares neither the identity path nor the authentication path with any other driver.

### 9.2 Add a new stream sink — an MQTT publisher

| File | Change |
|---|---|
| `crates/mw-pipeline/src/sink/mqtt.rs` | **new** — `impl Sink` (one `deliver`), ~60 lines |
| `crates/mw-pipeline/src/sink/mod.rs` | +1 line |
| `crates/mw-pipeline/src/plan.rs` | +1 arm: `Trigger::OnReading { All, Lossy, 5s }`, wrapped in `SpooledSink` only if the config asks for `Durable` |
| `crates/mw-store/src/config/types.rs` | +1 config struct for broker, topic, credentials |

**Four files, and zero lines of fan-out code.** The delivery policy is a declaration: choosing
`Durable` gets a spool cursor, replay and the ack protocol for free; choosing `Lossy` gets the
broadcast and `lagged_total`. Nothing in `Ingress`, the spool or the supervisor is aware that a new
sink exists.

### 9.3 Add a new periodic component — another upload service

**This is the case the owner cares about most, and the one the Go version handles worst**: pwsweather
and wunderground are line-for-line identical because there was no shared shape for "a thing that
uploads a windowed summary on a timer".

| File | Change |
|---|---|
| `crates/mw-output/src/encoder/windy.rs` | **new** — `impl OutputEncoder`: a `&[(MetricKey, Window, Agg)]` table and an `encode`, ~40 lines |
| `crates/mw-output/src/encoder/mod.rs` | +1 line |
| `crates/mw-output/src/plan.rs` | +1 arm: `Every { ≥ floor }`, `requires: [WindowedView(Hard), StationRegistry(Hard)]` |
| `crates/mw-store/src/config/types.rs` | +1 config struct |

**Four files, and zero lines of scheduling, retry, rate limiting, backoff or SQL.** The shared body
is `mw_output::uploader::Uploader<E: OutputEncoder>`: it resolves the encoder's declared windows
through `WindowedView`, encodes, and sends through the one `ServiceBuilder` stack of §4.2. So
`pws:<device>` and `wu:<device>` become two ~40-line `OutputEncoder` impls over one component body,
and the difference between them is a declared table of `(metric, window, agg)` that a reader can
check against the service's documentation without reading Rust.

The generation half — where the windowed values come from — is W2a §4b.3's `WindowedView` and W2's
registry `agg` field. Referenced, not redesigned.

### 9.4 Add a new metric

| Case | Hand-written files |
|---|---|
| a metric some station already reports into an existing column | `metrics/registry.toml` — **one** |
| a metric a driver must newly map out of a frame | the registry, plus one line in that driver's codec — **two** |
| a derived metric (R1: a function of one reading) | the registry, the codec line if any, plus one function in `crates/mw-pipeline/src/enrich.rs` — **three** |

Then `cargo xtask migrate` and `cargo xtask codegen` regenerate `migrations/` and all thirteen files
under `generated/`: the DDL, the aggregate SQL, `Reading`, `MetricKey`, `METRICS`, the aggregate
column tables, the protobuf, the OpenAPI schema and the TypeScript. Nothing in `mw-api-legacy`,
`mw-store`, `mw-proto`, `mw-grpc` or `mw-web` is edited, because each reads the generated tables
rather than carrying its own list.

**Against the Go calibration of roughly eight places, this is one to three** — and the one that is
always present is a registry entry, which is a reviewable declaration rather than code.

---

## 10. Efficiency boundaries

This system handles a few readings per second and serves ~19,000 requests dominated by one endpoint.
**Three things matter**: `/latest` and `/snow` tail latency (§2a), on-disk compression ratio (D-4,
D-10 — W4's), and not stalling ingest on a slow sink (§2). Everything below is a place somebody will
be tempted to optimise. Don't.

| Tempting | Don't, because |
|---|---|
| a faster JSON encoder (`simd-json`, a hand-rolled writer) | once serialisation is off the request path it is well under a second of CPU per day (§2a.3). This is a dependency with no measurable benefit. |
| optimising `/span/24h` | under 1% of traffic; a 60-second TTL is lossless. Adding a second cache tier adds an invalidation bug to an endpoint nobody waits on. |
| a custom spool encoding | `postcard` over the envelope at ~2 appends/s. A bit-packer saves bytes on a file whose ceiling is 2 GiB and whose typical depth is zero. |
| `COPY` instead of multi-row `INSERT` | the round trip dominates at 2 rows/s, and `COPY` loses `ON CONFLICT`, which §2.3's idempotent replay needs. |
| batch sizes above 256 | the bind-parameter ceiling is 303 at the current column count. Larger batches do not exist to be tuned. |
| `mimalloc` or another allocator | W8's OQ-2 deliberately did not measure it under emulation. Adopt one when a real workload shows contention, not before — it is a C build dependency for an unobserved problem. |
| pooling `Reading` allocations | ~1 KB of `Option<f32>` a few times a second. A pool is a lifetime bug waiting for a reason to exist. |
| avoiding `Arc` clones, `Bytes` slicing games, `smallvec` everywhere | a refcount bump per request at 0.2 requests/s. |
| a lock-free log ring | a `Mutex<VecDeque>` at a few hundred events/s is not contended. The ring's interesting property is its *bounds* (§5.7), not its throughput. |
| tuning the `sqlx` pool | the default of 10 against 2 writes/s and a handful of `/span` reads. The only pool setting that matters is the acquire timeout, because it decides how fast `db:postgres` notices the server is gone. |
| `-C target-cpu=native` | four cross-compiled targets, one of which is an ARM1176. There is no native. |
| caching `/stationinfo`, `/almanac` or `/alerts` harder | they are already config- or component-published; they are in §2a.8 for uniformity, not speed. |
| micro-optimising the enrich stage | pure arithmetic over one reading, microseconds, and it runs ~2 times a second. |

One genuine efficiency *requirement* that is easy to mistake for an optimisation and is not:
**`admit` must not do I/O other than the spool append.** A config lookup, a registry query or a
metric-name formatting call on that path would reintroduce the thing D-28 and the Go collector's
per-reading `LoadConfig()` are both examples of. Identity resolution reads an `ArcSwap` snapshot of
the registry, not the database.

---

# Decisions

1. The layout is a **cargo workspace of eighteen crates**, not one crate of modules, because "forbidden to depend on" is only enforceable as a `Cargo.toml` fact — a module in one crate can always `use crate::store`.
2. The workspace root W8 committed is **unchanged** except to populate `[workspace.dependencies]`; the `crates/*` glob means a new crate needs no root edit.
3. **Capability handle traits live in `mw-component`; implementations live in the providing crate**, which is what lets a consumer require `ReadingStore` without depending on `mw-store` and is why the dependency graph is acyclic.
4. **The supervisor does not know its components**: each component crate contributes `fn register(cfg, out)` and the binary assembles the list, so `mw-supervisor` can never grow a match arm per component kind.
5. Generated code stays exactly where W2 put it and is consumed through **three `#[path]` module declarations in `mw-types` only** — `#[path]` rather than `include!`, because `reading.rs` carries an inner attribute that a macro expansion cannot contribute.
6. The `generated` module is **private to `mw-types`**, which re-exports the types by name, so no other crate can reach generated code; `cargo xtask gate layout` enforces it.
7. **`chrono::DateTime<Utc>` is the timestamp type**, confirming what W2's generator already emits; introducing `time::OffsetDateTime` would mean two date libraries for no gain, and `chrono-tz` is needed anyway for `Calendar`.
8. `mw-proto`'s prost output is generated into `OUT_DIR` at build time rather than committed, because its input is already drift-gated and committing it would mean two generators writing one directory; the proto is parsed with **`protox`**, so no `protoc` binary is needed on any cross target.
9. The legacy-compat adapter is **`mw-api-legacy`**, which exclusively owns the 133-key DTO, the duplicate keys, the `mm`→inch split, `omitempty`, the four error-body conventions and msgpack.
10. Legacy naming cannot leak into the core because **`mw-types` has no `serde` dependency at all**, and zero-filling cannot leak into storage because the only `unwrap_or(0.0)` in the workspace is `mw_api_legacy::zf`.
11. `cargo xtask gate layout` enforces the layer order, the forbidden lists, "only `mw-types` mentions `generated/`", "**only `mw-http` binds a socket**", and "only the binary depends on `anyhow`".
12. **`contract/conformance/` stays its own workspace** — it is blocking `ureq` test infrastructure that must target the Go binary, and joining the root would add a second HTTP client and TLS stack to every PR build and every cross-compile. `cargo xtask conformance` drives it with `--locked`.
13. The **spool is the fan-out point**, not a channel: W2a Decisions 61 and 62 together leave no other place for durability to live.
14. `Provenance` has private fields and **`Envelope::forward(hop)` is the only way to address a peer**, refusing when the hop is already on the path — so a relay cycle is unrepresentable rather than filtered, and no configuration is consulted per reading.
15. `Source` is not a supervisor-facing trait: a station is a `Serve`/`Every` component holding an **`Ingress` handle**, resolved once from `Need { ReadingStream, Hard, Write }`.
16. **`Sink::deliver` returns an acked prefix**, which W2a's `OnReading -> Result<()>` cannot express, and `SpooledSink<S>` adapts it to W2a's trait; `Latest` and `Lossy` components implement `OnReading` directly.
17. **The only thing `admit` can wait on is the local spool's fsync** — never a sink, a database, a peer or a subscriber.
18. Buffer sizes with their reasons: no ingress queue; 50 ms / 64-append group commit; 8 MiB spool segments; a 2 GiB spool ceiling; **256-envelope durable batches**, which is the power of two below PostgreSQL's 65,535 bind-parameter ceiling at 216 columns; a capacity-1 `watch` for `Latest`; a 1024 `broadcast` for `Lossy`.
19. The slow-consumer policy differs per sink kind because the **cost of loss** differs: `Durable` falls behind on disk, `Latest` coalesces by design, `Lossy` drops and counts.
20. There is **no shared collection of sinks to mutate**: `Spool::open_cursor(id)` yields a `Cursor` owned by one task and removal is `Drop`, so nothing iterates a mutating slice and nothing closes a channel another task is sending on.
21. **R1 — a derived value is stored as a column iff it is a pure function of one reading** plus static station metadata; it is computed in the enrich stage, before the spool.
22. **R2 — a derived value that is a function of a window is never computed per request**; it is maintained by a component and read from memory. SQL computes a window only for `/span` and for the aggregate hierarchy.
23. Applied: wind chill, heat index, AQI, solar potential/joules and snow depth are **ingest-time, stored**; sun times and lunar values are **neither stored nor queried** (pure functions evaluated in `snapshot:latest`); wind gust, rain rate, rainfall day/month/year and 24/48/72 h, storm totals and every snow total are **component-maintained**; `windcard` is presentation and lives in the DTO.
24. **Rainfall rollovers happen in the station's time zone**, via a `Calendar { TzSource::Station }` boundary plus `Every { 60 s }` accumulation — today's `date_trunc('day', NOW())` truncates in the database server's zone and makes the device `timezone` field decorative.
25. **Enrich is idempotent** — it writes a metric only where the value is `None` — which is what makes a forwarded, already-enriched envelope safe across a multi-hop relay.
26. **The writer stores `NULL` for an unreported metric**, which is D-3's premise and the condition W1-BUG-1 is latent against; W4 must take OQ-W1-2 option (c).
27. Identity resolution runs **before** the envelope exists, so an unauthenticated reading is unstorable by construction; publication to subscribers happens **after** the spool commit, so nothing is shown that is not durable.
28. **A spool that cannot be written is fatal** (W2a Decision 62); **a spool at its ceiling drops at the edge and alarms**, because stalling ingest loses the Davis packets anyway and additionally hangs shutdown.
29. A cursor more than `max_lag` (default 7 days) behind is **abandoned by name**, so the operator sees a named failure rather than a ceiling event caused by a different component.
30. `TimescaleSink` (write), `ReadingStore` (read) and the fan-out tap (live) are **three traits over one pool**, named separately so nobody makes the website a sink.
31. `LatestSnapshot` is keyed **per `(website, station)`**, because the payload genuinely depends on the website — the snow base distance, the primary-device selection and the station-parameter semantics all do.
32. **Snapshot memory is 150 KB at the worst-case 30 keys** (~5 KB each) and would not matter until ~10,000 keys, which the configuration cannot express. No eviction, no TTL, no LRU.
33. **JSON is pre-serialised eagerly, once per reading-version; msgpack and protobuf are produced lazily via `OnceCell`.** The prompt's arithmetic is backwards — there are ~10× more readings than requests — so pre-serialisation is justified by tail latency and by the ETag needing a stable identity, not by total CPU.
34. The `/latest` handler does **four** things: resolve the website, resolve the station, apply W1's 10-minute recency rule, and write `Bytes`. It performs no queries and is not handed a handle that could.
35. **A strong `ETag` derived from `(station, observed_at_ms, payload_rev)`** enables conditional GET; `payload_rev` folds in the config generation so a website rename invalidates. No `If-None-Match` means a normal 200 — additive only, and `Cache-Control` stays frozen.
36. During a database outage the snapshot keeps the last good window-derived values and records `Staleness::Degraded`; the indicator reaches the operator through **additive headers, metrics and the management UI**, never through the frozen body.
37. **`snapshot:warm` is an `OnStartup` component** that seeds the snapshot table from the store so a restart does not 404 a station that was inside the 10-minute window; with `ReadingStore` unavailable it finishes empty rather than refusing to start.
38. `/snow` gets a **two-tier snapshot**: depth and distance `OnReading`; events and smoothing every 15 min; storm total and snowfall rate every 5 min; the season total re-derived every 15 min from cached events **plus** a daily `Calendar { 00:10, Station }` full rescan — so the month-scanning query runs once a day instead of per request.
39. `/forecast` **caches the stored `jsonb` text byte for byte** and splices it between a prefix and a suffix; nothing parses or re-emits it, which is how Postgres's whitespace and the missing trailing newline are preserved. `?period=N` is reproduced with a `BTreeMap` in compact mode, matching Go's map-key sort.
40. `/span` is the **one query-shaped endpoint** and gets a 60-second TTL keyed by `(station, span, snow-join)`, lossless because the 1-minute aggregate advances once a minute.
41. **The SQLite pool is a root resource owned by `main`**, handing a one-table `ScheduleStore` to the supervisor and the full handle to `config:sqlite` — which answers W2a OQ-D without the supervisor depending on a component it supervises.
42. **A `JoinHandle` is owned only by `TaskTable`, `spawn` returns nothing, and there is no `remove` that is not `async`** — so a leaked task and an un-awaited handle are both unexpressible, reinforced by `clippy.toml` denying `tokio::spawn` outside `mw-supervisor`.
43. The supervisor learns of task exits through an **`mpsc` of `(id, Outcome)`** feeding one `select!` loop, so component state has a single owner and no lock.
44. **One `Sleep` re-armed from a binary heap** replaces 21 hand-rolled clocks; `next_run` is a heap peek, and a wall-clock step (an NTP correction on an RTC-less Pi) rebuilds only the `Calendar` half.
45. A tick runs under a **child cancellation token** so a deadline kills the tick and not the component, and **rescheduling happens before the outcome is interpreted** so no branch can silently stop a component forever.
46. For scheduled triggers `Backoff { until }` **is the next occurrence**, keeping W2a's state machine literal without a second clock; exponential backoff with full jitter applies only to build failures, `Serve` exits and `OnReading` delivery failures.
47. **systemd readiness is implemented** with `sd-notify` (no `libsystemd`, which a static musl build does not have), and `READY=1` means *the supervisor has converged*, not *everything is Running* — otherwise a component correctly `Waiting` on a dead database would block readiness forever.
48. Shutdown walks the topological order in reverse with a 10 s default grace (15 s for the writer and the uplink), then aborts and counts; reverse order exists so a consumer never observes a revoked capability while stopping.
49. **`anyhow` exists only in `crates/modernweather`**, enforced by the layout gate; every other crate has a `thiserror` enum and a `Classify` impl beside it.
50. **One generic `tower` retry policy over `E: Classify`** means a new error type acquires its retry behaviour where the error is defined, not at each call site; `Class::Retryable { after }` is also how `Retry-After` reaches the limiter.
51. **`Display` prints one level only** and the chain is walked once at the logging site by `DisplayChain`, so `?` with `#[from]` is both the shortest and the only chain-preserving path — against 767 of 1,174 Go sites that flatten with `%v`.
52. **`tracing` is the only logging path**: no package-global logger, no injected logger, `LogTracer` for dependencies, and the layout gate rejects a `log` dependency or a stray `println!`.
53. The **`DedupeLayer` is outermost** because `event_enabled` is the only hook with whole-pipeline reach; suppression is therefore global rather than per sink.
54. Format is **pretty+ANSI on a TTY, JSON otherwise**, with `--log-format`/`MW_LOG_FORMAT`, `NO_COLOR` and `CLICOLOR_FORCE`; **no journald-native layer**, because journald reads JSON on stderr and `libsystemd` is unavailable in a static musl build.
55. `station` is carried on the ingest, decode, delivery and HTTP paths, and the ring layer's filter is a reloadable **`Targets` filter plus an optional station predicate**, applied server-side — because `EnvFilter` cannot match on a field value.
56. **Level changes are scoped and expire**: `set_level(target, level, ttl)` schedules its own revert as an `OnDemand` occurrence, and a standing override is shown as such; `RW-Debug: 1` becomes a per-request escalation instead of mutating a shared logger.
57. **Two rings, one API, one stream**: separate rings because the access stream is one to two orders of magnitude the application stream's volume and would evict it; one merged, timestamp-ordered stream with a `stream` field because correlating a 500 with the query that caused it is the operator's actual job. 4,096 + 16,384 records ≈ 6 MB, bounded.
58. On ring overflow the stream emits a synthetic **`{"kind":"gap","dropped":N}`** record, so an operator sees a gap rather than silently missing entries.
59. **The ring stores structured records, never formatted text**; ANSI is a *rendering* produced from the record by the same formatter that writes stderr. The management UI gets faux-ANSI CSS colours by default and a real-ANSI `text/plain` view on request, from one palette — so the UI, a pasted ticket and the terminal all look the same.
60. Repetition control is a **token bucket keyed on the callsite** (burst 3, refill 1/30 s, 1,024 keys LRU) with a 60-second summary event; `Fatal`-class and `mw::audit` events are never suppressed. A disconnected serial port goes from 3,600 lines an hour to 63.
61. Every `WARN`-or-above event names **subject, quantity, disposition and cause chain**, and `disposition` is a closed vocabulary so it can be counted and coloured.
62. **Metrics are Prometheus text on an additive `/metrics` route**, with the same numbers as JSON for the UI; every label is a station, component, route pattern or closed enum, and a request-derived label is forbidden.
63. **The unit that is singular is the router and the route table, not the socket**: `http:listener` may bind several sockets from configuration (443, a redirecting port, an optional legacy gRPC port) and serves one `Router` on all of them.
64. **Route conflicts are detected in `plan()`**, from a `RouteManifest` that is a pure function of configuration, naming both owners — not at build time, which is impossible because the website set is configuration rather than source.
65. The router is published through **`ArcSwap<Router>`**, so a plan diff never blocks an in-flight request, and an unmounted declared path returns **503** naming the component it waits on.
66. Hostname lookup is an **`ArcSwap<HostTable>`** rebuilt wholesale on the config watch — the direct replacement for five unsynchronised Go fields that can kill the process on a website save. Unmatched `Host` still serves the default website, with the empty-hostname rule reproduced exactly.
67. **Host matching becomes case-insensitive** — a deliberate deviation, recorded, because the Go map lookup is case-sensitive and `EXAMPLE.COM` therefore silently serves someone else's website today.
68. The SNI resolver is an **`ArcSwap<CertStore>`**, so a certificate renewal is an atomic pointer write and never a listener restart; keys are `secrecy`-wrapped and zeroised.
69. **Wildcard certificates match exactly one label.** The Go `strings.HasSuffix` check matches any depth *and* matches `evilexample.com` for `*.example.com`; that is a found defect, not a behaviour to carry forward.
70. **On an SNI miss the chain is exact → wildcard → a designated default → refuse with `unrecognized_name`.** The Go version returns an arbitrary certificate by iterating a map, which is non-deterministic between runs and produces a browser interstitial that blames the user.
71. REST-JSON and gRPC-protobuf are generated from one definition (`prost` + `pbjson`), with **exactly two conversions** between the wire type and the domain type, shared by every transport — replacing ~591 hand-maintained mapping lines. The legacy DTO is deliberately *not* one of them.
72. **gRPC shares the socket by ALPN and a `content-type` test**, not by byte sniffing; plaintext h2c is served by `hyper-util`'s auto builder for LAN peers.
73. Frozen transport behaviour is carried as `RouteFlags` rather than convention: every method on every route, **no trailing-slash normalisation**, `Vary: Accept-Encoding` with **gzip and deflate only** (brotli deliberately disabled), CORS only on JSON, and Aeris routes that exist only when a device enables Aeris — which falls out of conditional route declaration.
74. The HTML templates are ported to **askama**, with a **`GoHtmlEscaper`** that reproduces `html/template`'s `&#34;`/`&#39;` exactly — which is what makes the acceptance test a **byte-diff against W1's recorded bodies** rather than an eyeball.
75. `portal.html.tmpl` consumes nothing and becomes a **static asset with no template**.
76. Static assets are **embedded in the binary** with a hard-coded mime table (not `mime_guess`), because two frozen content types — `/js/weather-app.js` without a charset and `site.webmanifest` as `text/plain` — are quirks a mime crate would "correct".
77. Asset cache headers are reproduced exactly and **`ETag`/`Last-Modified` are added, but `max-age` is not**: `ETag` already saves the bytes via a 304, while `max-age` without content-hashed filenames means a browser runs the previous release's `weather-app.js` against the new server until the TTL expires.
78. **Directory listings return 404** (W1's OQ-W1-4 recommendation), recorded as a deliberate deviation.
79. The config payload replacing `weather-app.js.tmpl` is **delivered inline as `<script id="mw-config" type="application/json">`**, following the page's existing `radar-config` precedent, with `GET /config.json` as the additive endpoint — avoiding an extra round trip and an async boundary in a module that is synchronous today.
80. **The nine values are exactly W1 §9's consumed set**, with JSON keys equal to the JavaScript object's keys, so the module's body does not change.
81. **`AboutStationHTML` reaches no rendered page today** — the prompt's premise is wrong — so the rule is that it stays a JSON string on `/stationinfo` and is never interpolated into a template; **the layout gate rejects `safe` and `escape("none")` anywhere under `crates/mw-web/templates/`**, so raw HTML in a template fails CI.
82. A driver is tested at three levels — pure codec against captured frames, **`Session` against a recorded transcript**, and the whole driver against the pinned Go emulators — and the transcript level is the one that would have caught the Davis "stops polling after one batch" defect.
83. **Davis and Campbell get a `socat` pty pair** for the serial path, because its framing and read-timeout behaviour is where that class of defect lives; the other drivers are natively TCP, UDP or HTTP.
84. Database tests **fail CI if they skip**, so "green because it did not run" is unreachable.
85. The two concurrency tests the gate exists for are specified: **config reload racing 64 readers** (asserting the absence of a torn read, not the absence of a crash) and **shutdown with a stalled sink** under `tokio::time::pause()`, asserting ingest is never blocked, shutdown completes, the stall is aborted and counted, and the spool replays losslessly.
86. All four extensibility walkthroughs touch **at most four hand-written files**; adding a metric touches one to three, against the Go calibration of roughly eight.
87. **`admit` must do no I/O but the spool append** — identity resolution reads an `ArcSwap` of the registry, never the database. This is the one efficiency statement in §10 that is a requirement rather than a prohibition.

**The following ten were raised as open questions in the first draft and decided on the owner's
request. Each carries a reversal trigger in the Open questions section.**

88. **`ETag` is emitted on `/latest`, `/snow` and the assets, and it cannot fail conformance** — verified, not assumed: the runner asserts a per-fixture *allowlist* of header names (`contract/conformance/src/lib.rs:320`) and never enumerates the response's headers.
89. **W2's generator emits proto3 `optional` on every metric field**, because it is wire-compatible and is the only option under which a collector stores what the satellite actually measured rather than `0.0` for an unreported metric.
90. **Runtime log-level control is exposed through `ComponentControl`** rather than as a thirteenth capability or as a root fact in every `Cx` — one consumer, same root provider, and W2a's own reason for root facts (every component needs it) runs the other way here.
91. **`Provision` gains `ProvisionDetail::Routes(RouteManifest)`**, because the alternatives are failing the one component everything else's reachability depends on, *after* its siblings have started, or not detecting a duplicate route at all.
92. **The gRPC service keeps its own socket** — verified, not conceded: `grpc_port` is a config column defaulting to 50051 and the management API validates that it differs from the HTTP and HTTPS ports, so a separate socket is the shipped, validated behaviour and folding it onto 443 would be a fleet-wide flag day.
93. **Metrics are Prometheus text on `/metrics`** rather than counters on a status endpoint, because a status endpoint has to invent a format and then somebody writes an exporter for it anyway.
94. **A full spool drops the newest at the edge, counted and alarmed** — stalling `admit` loses the same Davis packets *and* hangs shutdown, and dropping the oldest spooled envelopes would discard exactly what durability exists to protect.
95. **`Host` matching is case-insensitive**, because the current case-sensitive map silently serves one website's data under another's hostname and preserving that preserves a bug rather than a contract.
96. **Directory listings return 404** on the four asset trees, per W1's own recommendation.
97. **Every decision taken on the owner's behalf carries a stated reversal trigger**, because a call made for somebody else without one is a call nobody can revisit safely.

---

# Open questions

**None are left for the owner.** The ten questions this document originally raised have all been
decided; the owner asked for the calls to be made rather than referred, and the two that turned on a
fact rather than a judgement were settled by reading the code instead of by choosing.

The format below is deliberate: each entry records the **call**, the **reason in one line**, and —
the part that matters for a decision taken on someone else's behalf — **what would reverse it**. A
decision with no stated reversal trigger is a decision nobody can revisit safely.

| | Call | Why | What would reverse it |
|---|---|---|---|
| **1** | **`ETag` is emitted on `/latest`, `/snow` and the assets.** | **Settled by fact, not judgement.** The conformance runner asserts a per-fixture *allowlist* of header names (`contract/conformance/src/lib.rs:320`) and never enumerates the response's headers, so an additive header is invisible to it — which is also why `Date` and `Server` have never been a problem. | Nothing. This was a question about what the runner does, and the runner does not care. |
| **2** | **W2's generator emits proto3 `optional` on every metric field.** | Wire-compatible (same field numbers, same tags, same encoding), and the only option under which a satellite and its collector hold the same data rather than the collector silently storing `0.0` for a metric that was never reported. | Evidence that a deployed non-Rust consumer of the proto breaks on explicit presence. None is known; the only consumers are this daemon and its own peers. |
| **3** | **Log-level control is exposed through `ComponentControl`.** | Operator control of a running process, same root provider, one consumer. A thirteenth capability for one consumer is noise in a graph whose only jobs are start ordering and `Waiting`. | W2a deciding that `ComponentControl` is strictly about component lifecycle, in which case it becomes a thirteenth capability — a rename, not a redesign. |
| **4** | **`Provision` gains `ProvisionDetail::Routes(RouteManifest)`.** | It is what makes a duplicate route a `PlanError` naming both owners instead of a silent last-registration-wins. The alternative fails the one component everything else's reachability depends on, and fails it *after* its siblings have started. | W2a declining the extension; detection then moves into `http:listener::build` and the listener becomes the thing that fails when two other components disagree. |
| **5** | **Assets get `ETag` and `Last-Modified`. They do not get `max-age`.** | `ETag` already saves the bytes — the browser revalidates and gets a 304 with no body. `max-age` only saves the revalidation *request*, worth microseconds on a LAN, and costs a browser running the previous release's `weather-app.js` against the new server until the TTL expires. | Content-hashed asset filenames. With those, a year-long `max-age` is correct and safe — but that is a website change, which D-38 puts out of scope. |
| **6** | **The gRPC port keeps its own socket.** | **Settled by fact.** `grpc_port` is a configuration column defaulting to 50051 (`pkg/config/provider_sqlite.go:231`) and the management API *validates that it differs from the HTTP and HTTPS ports* (`handlers_controllers.go:501`, `:506`). A separate socket is the shipped, validated behaviour; folding it onto 443 would invalidate a config column and make the rewrite's first operational event a fleet-wide flag day. | A W9 migration report showing no peer still uses the legacy port, at which point it can be retired as an ordinary deprecation rather than as an upgrade break. |
| **7** | **Prometheus text on an additive `/metrics` route**, with the same numbers as JSON for the UI. | A status endpoint has to invent a format and then somebody writes an exporter for it anyway. The recorder is in-process and costs nothing when nobody scrapes. | Nothing in sight. OpenTelemetry would be the upgrade path if tracing ever needs to leave the process, and `metrics` does not block it. |
| **8** | **A full spool drops at the edge, counted and alarmed.** | Stalling `admit` back-pressures the Davis serial loop, which drops packets anyway — so stalling loses the same data *and* hangs shutdown. Dropping the *oldest* spooled envelopes instead would discard exactly what durability exists to protect, so the newest is the right thing to refuse. | Nothing. The real protection is upstream: `spool_depth_bytes` rises for days first, and at 2 GiB the ceiling is ~12 days of worst-case density. |
| **9** | **`Host` matching is case-insensitive.** | Hostnames are case-insensitive, and today a request for `EXAMPLE.COM` falls through a case-sensitive map and silently serves *someone else's* website. Preserving that is preserving a bug, not a contract. | A fixture showing a client depends on the fall-through. None exists; no fixture covers mixed case. |
| **10** | **Directory listings return 404.** | W1's own recommendation. No client requests `/js/`, `/css/`, `/images/` or `/fonts/` as directories, and an HTML index of the asset tree is a small information disclosure that nothing here benefits from. | A client discovered to enumerate the asset tree. There is none in either shipped client. |

**Three of these change bytes a client could in principle observe** — entries 5 (an added `ETag`), 9
(case-insensitive `Host`) and 10 (404 instead of a directory index) — so they are deviations from
D-37 however small, and they are listed again in the assumptions for W6's ledger. Each is additive
or closes a defect; none removes a response any shipped client reads.

**One of these is work for another work item rather than this one**: entry 2 needs a line per field
in W2's generator, W7 must carry presence on the wire, and W4 should expect a collector-written row
to contain NULL. It is recorded against all three rather than being quietly assumed, because it is
the one finding here that no single document would have produced on its own.

---

# Assumptions about other work items

Every assumption this document makes about a document it did not write. **This is the drift
detector**: if one of these is wrong, it is findable with a grep rather than during implementation.

**About W2a (component and capability model)**

1. `ComponentSpec`, `Trigger`, `Capability`, `Need`/`Provision`, `Strength`, `Access`, `Arity`,
   `Tick`/`OnReading`/`Serve` and the lifecycle states are **implemented as written**; this document
   adds mechanics, not variants — with the two exceptions in assumptions 2 and 3, both decided here
   and both reversible by a rename rather than a redesign.
2. `Provision` gains `ProvisionDetail::Routes(RouteManifest)` so that route conflicts are a
   `PlanError` (Decision 91). **Taken**, not asked: if W2a declines, §6.2's detection moves into
   `http:listener::build` and the listener becomes the component that fails when two others disagree
   — which is why it was taken.
3. `ComponentControl` also carries runtime log-level control (Decision 90). **Taken**, not asked: if
   W2a reads `ComponentControl` as strictly lifecycle, the capability set grows to thirteen and
   nothing else about §5.5 changes.
4. **`Durable` is satisfied by a local durable spool at the fan-out point, not a bounded channel**
   (W2a Decision 62), and W7's spool is that mechanism rather than a collector-only facility. §2 is
   written against that; if the spool were a channel, W2a Decision 61 or 62 is violated.
5. A `Service` component owns time *inside* its loop and that is not a D-25 violation (W2a Decision
   7). §3.3 keeps `Service` out of the timer heap entirely on that basis.
6. The supervisor's durable `Calendar` state lives in the config SQLite database in a
   `component_schedule` table, reached through a narrow handle and **not** through `ConfigStore`
   (W2a OQ-D). §3.1 goes one step further: the pool is owned by `main`, not by `config:sqlite`.
7. The pure APRS encoder lives in a crate with no database or transport dependency, so a golden test
   links it alone (W2a assumption 7). That is `mw-aprs` (§1.2).
8. The Go emulators are fixed external test infrastructure (D-40), which §8.3 depends on for driver
   testing without hardware.
9. `ComponentError` is a typed error preserving its cause chain (D-34); §4.1 defines it and §4.3
   defines how the chain survives.
10. `OnReading` components do **not** declare `Need { ReadingStream }` — their trigger implies the
    subscription and the supervisor delivers. `ReadingStream` is declared only by *emitters*, and
    only with `Access::Write`.
11. `ReadingPolicy::Latest` is a coalescing `watch` and `Lossy` is a counted `broadcast` (W2a
    Decision 63); §2.4 fixes the sizes.

**About W2 (metric registry and codegen)**

12. `generated/rust/{metrics,reading,aggregates}.rs` are adopted unchanged into `mw-types` via
    `#[path]`, and the generator keeps emitting `chrono::DateTime<Utc>` (W2 assumption 14, answered:
    **chrono**).
13. The query layer reads `generated/rust/aggregates.rs` for which columns a level holds rather than
    re-deriving them (W2 assumption 15), and knows that a mean requires `mw_mean(c_sum, c_count)` and
    a direction requires `mw_circular_mean(c_sin_sum, c_cos_sum, c_dir_count)` (assumption 16).
14. **The generator emits proto3 `optional` on metric fields** (Decision 89). Without it,
    the uplink cannot distinguish an unreported metric from zero, and §2.5's NULL-storing writer
    produces zeros on the far side of a hop.
15. The registry's per-metric `agg` is the same field that generates aggregate SQL and that resolves
    an `OutputEncoder`'s `(Metric, Window, Agg)` declaration (W2a Decision 49), so `window:rollup`
    needs no second table.
16. `legacy_rest_keys`, `legacy_read_time_computed` and `legacy_aggregate_rest_keys` are sufficient
    to generate the frozen DTO's key set; where they are not, `mw-api-legacy` hand-writes the
    difference and says so in a comment naming the registry field that would have covered it.
17. W2 restructures nothing when this document's crates land: `xtask` stays a workspace member and
    `.cargo/config.toml`'s `xtask` alias survives (W2 assumption 22, honoured — §1.0).

**About W4 (database schema)**

18. **W4 takes OQ-W1-2 option (c)**: unreported metrics are NULL and the circular-average finaliser
    is NULL-safe while still emitting `0` rather than `null`. §2.5's writer is exactly the writer
    that makes W1-BUG-1 live, and §8.2 carries the regression test.
19. `/span`'s four aggregate levels and their exact span boundaries are preserved, or every client's
    chart resolution changes (W1 assumption 10).
20. `almanac_cache` and `snow_totals_cache` continue to exist as precomputed tables (W1 assumption
    12); §2a.7's `snow:*` components write the latter and `/almanac` reads the former.
21. The 1-minute aggregate's refresh policy makes a bucket readable within `window:rollup`'s
    10-second deadline of the minute boundary (W2a assumption 14). If it lags more,
    `window:rollup`'s period must grow and `Phase::Aligned` needs revisiting.
22. The raw table carries `ON CONFLICT (station_id, time) DO UPDATE` semantics, or an equivalent
    unique constraint, because §2.3's cancel-safe delivery replays a prefix and the writer must be
    idempotent over it.
23. Daily, monthly and yearly boundaries are computed in the **station's** time zone (W2a assumption
    16); `rain:totals` supplies them to the snapshot and must not be contradicted by a database job
    using `date_trunc('day', NOW())`.
24. Aggregate rebuild duration is observable, so §5.10's `mw_aggregate_refresh_seconds` has something
    to record.

**About W1 (frozen contract)**

25. Conformance asserts a per-fixture **allowlist** of header names and never enumerates the
    response's headers — **verified at `contract/conformance/src/lib.rs:320`**, so this is a
    confirmed property rather than an assumption, and the additive `ETag`, `X-MW-Snapshot-Age` and
    `X-MW-Degraded` headers cannot fail the suite. If a future runner adds a full header-set
    comparison, it must exempt additive headers or it will also fail on `Date` and `Server`.
26. The recorded HTML bodies in `contract/fixtures/` are usable as **byte goldens** for §7.1's
    template port, not merely as substring references. If they were captured with any normalisation,
    §7.1's acceptance test weakens to substring level and the `GoHtmlEscaper` decision loses its
    teeth.
27. The JSON configuration endpoint carries exactly the nine values W1 §9 lists as consumed by
    `weather-app.js.tmpl` (W1 assumption 2); §7.4 lists them and their key names.
28. `/latest`'s 10-minute recency window and `/snow`'s absence of one survive the snapshot scheme
    (W1 assumption 3); §2a.2 step 4 and §2a.7 implement both.
29. Multi-hostname routing preserves the **fallback** of Q-18 and not merely the matching (W1
    assumption 5); §6.3 reproduces the empty-hostname default rule verbatim.
30. The 503 on an unmounted route cannot be observed on a frozen path (W1 assumption 28); §6.2 argues
    why, and W1 owns the surface.
31. W1's fixtures pin what `/latest` returns when the snapshot is empty (W2a assumption 26). §2a.6
    produces `404 no recent weather data available for this station\n`, matching the existing
    `latest-no-recent-data` fixture's bytes.
32. The conformance gate's `arms_when` path is corrected to point at `contract/` rather than
    `tests/conformance/` (§1.8), or W1's artefacts move and their recorded provenance paths move with
    them.
33. W1's fixtures pin sunrise and sunset to the minute so W2a Decision 36's deletion of the
    `sun_times` table is verifiable (W2a OQ-B); §2.5 evaluates them in `snapshot:latest` on that
    basis.

**About W5 (management UI)**

34. W5 §0 owns the vocabulary table and no term from this document reaches a screen (D-36).
35. W5 consumes **one** log stream carrying a `stream: app | access` field rather than two endpoints
    (§5.7), and renders the synthetic `gap` record as "N entries dropped".
36. W5 renders log records from **structure**, applying the §5.8 palette as theme colours, and offers
    the `?format=ansi` terminal view as a secondary rendering. The owner's requirement for ANSI (or
    faux-ANSI) colours in the UI is met by that pair; W5 owns the final theming.
37. W5 surfaces the per-component counters W2a Decision 20 lists, plus this document's additions:
    `spool_depth_bytes`, `spool_lag_envelopes`, `ring_dropped_total`, `admit_rejected_total{reason}`,
    `clock_step_total`, `stop_timeout_total`.
38. W5's log-level control uses `set_level(target, level, ttl)` and shows a TTL-less override as
    standing (§5.5).
39. W5 displays `Staleness::Degraded` and `Freshness` as distinct from component state, because an
    available-but-stale snapshot is not a failure (§2a.6, W2a Decision 20).
40. W5's generated OpenAPI client comes from the management API's own schema; this document's
    additive routes (`/metrics`, `/api/logs`, `/config.json`) are part of that surface and are
    **not** part of the frozen legacy contract.

**About W7 (collector)**

41. W7 owns `Provenance`'s wire encoding and `OriginId` allocation; this document fixes only that it
    is carried, that its fields are private, and that `Envelope::forward` is the sole constructor for
    a forwarded envelope (§2.2).
42. `ReadingFilter::NotFrom(origin)` is implemented as `!prov.has_visited(origin)` — a set test, not
    a name comparison, and not a per-reading configuration read.
43. W7's acked, resumable spool **is** the mechanism §2 specifies, shared by `writer:timescale`; the
    segment size, group-commit window, ceiling and `max_lag` in §2.4 and §2.7 are this document's
    defaults over W7's format.
44. The uplink is a `Trigger::OnReading { NotFrom(self), Durable, 10 s }` component wrapped in
    `SpooledSink`, with no bespoke lifecycle (W2a Decision 25).
45. W7 must carry proto3 field presence on the wire (Decision 89); without it a forwarded reading
    cannot say "not reported" and the collector stores `0.0`. This is the one decision here that
    another work item has to execute.
46. **A satellite's endpoint does not change at upgrade** (Decision 92): the configured `grpc_port`
    stays bound, serving the same router.
47. `station:grpcrx` authenticates with mTLS at the TLS layer, before any frame is decoded (W2a
    Decision 46), which means the SNI resolver and the client-certificate verifier are configured
    together in `mw-http` and the peer identity reaches `Ingress::admit` through `FrameMeta`.

**About W8 (CI and release)**

48. **`crates/modernweather` produces a binary named `modernweather`** and carries the
    `[package.metadata.deb]` and `[package.metadata.generate-rpm]` blocks from `docs/release.md` §6
    verbatim (W8 assumption 1).
49. **The config flag is `--config <path>`** (W8 assumption 2, honoured — §3.1).
50. **Readiness notification is implemented, so `Type=notify` stands** (W8 assumption 3, answered —
    §3.5), using `sd-notify` rather than `libsystemd` because every target is static musl.
51. **W3's concurrency tests run under both ThreadSanitizer and `loom`** (W8 assumption 5, answered —
    §8.4): the multi-threaded ones are ordinary pthreads and instrumentable, and the `pause()`-based
    ones are single-threaded by construction, so the gate's shape does not change.
52. **The TLS crypto provider is `ring`** (W8 OQ-1, answered), with `rustls`; the ARMv7-tagged-object
    allowlist in `ci/spike/armv6-cross-spike.sh` already covers it.
53. **No replacement allocator is adopted** (W8 OQ-2, answered: not pre-emptively — §10).
54. `protox` means no `protoc` on any build host, including the four cross targets (§1.6) — if
    `prost-build` is used with a system `protoc` instead, `platform.yml` needs a new install step.
55. Three gates need their `arms_when` or `cmd` updated as this document's artefacts land: `layout`
    (new, armed by `crates/mw-types/Cargo.toml`), `conformance` (path, §1.8), and `db-tests` (new,
    §8.2).
56. `[workspace.dependencies]` is populated by this document's crates and the version pinning there
    is what keeps `codegen --check` and the conformance replay comparing like with like (W8's own
    comment in the root manifest).

**About W6 (reconciliation)**

57. W6 reads the four closing sections and treats the prose as background.
58. W6 carries the three deliberate deviations from D-37 forward as a ledger: **case-insensitive
    `Host` matching** (Decision 95), **404 instead of directory listings** (Decision 96), and
    **additive response headers** (Decision 88). All three were decided here at the owner's request;
    each is additive or closes a defect, none removes a response a shipped client reads, and each
    carries a reversal trigger in the Open questions section.
59. W6 records the two corrected measurements in this document's header (`/latest` is 12 round trips,
    `/snow` is 4 or 8) against `docs/rewrite/README.md`'s "Measured facts" table, which currently
    states 7 for both.
60. W6 records the **v2 API** as a named future work item (D-39); every additive route this document
    introduces (`/metrics`, `/config.json`, `/api/logs`, the `X-MW-*` headers) is deliberately
    outside the frozen surface and is the v2 item's starting inventory.
61. W6 reconciles Decision 89 (proto3 field presence) across W2, W4 and W7 — the one finding here
    that no single document would have produced, and the only decision this document makes that
    another work item has to implement.
62. **W6 should expect no open questions from W3.** All ten were decided at the owner's request; the
    Open questions section records each call with its reversal trigger instead of deferring it.

---

# Proposed amendments to `DESIGN.md`

Four. None is applied here; `DESIGN.md` decisions are never edited in place, and this document is
written to be internally consistent with the decisions as they currently stand.

> **A-W3-1 — 2026-10-28 — D-32's crate table needs six additions.**
>
> *What was found.* D-32 lists the default crate for each named idiom and says a *substitution* must
> be recorded as an amendment. Six idioms this document requires are not named at all, so six agents
> would each pick one: TLS, HTML templating, asset embedding, protobuf parsing, observability, and
> systemd readiness. That is precisely the divergence D-32 exists to prevent, in the one form the
> decision does not cover.
>
> *What replaces it.* Nothing is replaced; the table gains rows:
>
> | Concern | Crate | Why this one |
> |---|---|---|
> | TLS, and the SNI resolver | `rustls` + **`ring`**, `rustls-pemfile` | answers W8's OQ-1; smaller than `aws-lc-rs` and already on the ARMv6 allowlist |
> | HTML templating | `askama` | compile-time checked, so a missing field is a build error rather than a request-time one (§7.1) |
> | Static asset embedding | `rust-embed` | one static binary per target, matching what the Go version already does with `embed.FS` |
> | Protobuf parsing at build time | **`protox`** + `prost-build` | no `protoc` binary on any of four cross targets or on a Pi |
> | Logging and metrics | `tracing`, `tracing-subscriber`, `metrics`, `metrics-exporter-prometheus` | §5; one logging path and a metrics surface that is not logs |
> | systemd readiness | `sd-notify` | `Type=notify` without linking `libsystemd`, which a static musl build cannot |
>
> *Why this is more than inconvenient.* W1 already filed A-4 asking D-32 to state that it governs the
> daemon rather than test infrastructure. The same decision needs the converse: it should be complete
> for the daemon, or it is a list that answers some questions and silently delegates others.

> **A-W3-2 — 2026-10-28 — D-29 should name the fan-out point.**
>
> *D-29 currently reads:* "Data flows sources → fan-in → transform/enrich → fan-out → sinks, with an
> explicit policy for what happens when a sink cannot keep up."
>
> *What was found.* The shape is right, but "fan-out" is underspecified in exactly the place where
> the design is decided. W2a Decision 61 forbids a source being blocked by a sink and Decision 62
> concludes that `Durable` therefore requires a local durable spool. Those two together leave no
> freedom: the fan-out point **is** the spool, every durable sink reads it through its own cursor,
> and `Latest` and `Lossy` subscribers are notified from the same append. A reader of D-29 alone
> would reasonably build a `broadcast` channel and satisfy neither decision.
>
> *What replaces it.* D-29 gains a sentence: *the fan-out point is a local durable spool; durable
> sinks consume it through independent cursors and acknowledge a prefix, while coalescing and lossy
> subscribers are notified after the append commits.* This also folds in W2a's proposed D-30a, which
> says the same thing from the sink's side.
>
> *Why this is more than inconvenient.* It is the one structural commitment in the pipeline, and it
> is currently derivable only by reading two documents and noticing that their conjunction has a
> unique solution.

> **A-W3-3 — 2026-10-28 — D-38 should record how the config payload is delivered, not only that it
> exists.**
>
> *D-38 currently reads:* the public weather website is carried forward as-is, except that
> template-generated JavaScript becomes static JavaScript plus a JSON configuration endpoint.
>
> *What was found.* W1 already filed A-3 asking D-38 to name the nine values. Implementing it
> surfaces a second gap: "a JSON configuration endpoint" read literally means the module does a
> `fetch` at initialisation, which introduces an extra round trip and an asynchronous boundary into a
> module that is synchronous today — in a page that is supposed to be visually unchanged. The page
> already contains the right pattern: `radar-config` is delivered as an inline
> `<script type="application/json">` block.
>
> *What replaces it.* D-38 gains: *the configuration is delivered inline as a
> `<script id="mw-config" type="application/json">` block, following the existing `radar-config`
> precedent; `GET /config.json` exists as an additive endpoint for other consumers. The payload is
> the nine values listed in `docs/legacy-api-contract.md` §9, with JSON keys equal to the JavaScript
> object's property names.*
>
> *Why this is more than inconvenient.* D-38 is the one decision in `DESIGN.md` that deliberately
> changes the website's bytes, so the exact replacement mechanism is load-bearing rather than detail —
> and the difference between an inline block and a `fetch` is the difference between "visually
> unchanged" and "a flash of unstyled content and an ordering bug".

> **A-W3-4 — 2026-10-28 — "one listener" should read "one router and one route table".**
>
> *What was found.* D-31 makes transport orthogonal to role and the W2a/W3 prompts both phrase the
> consequence as "one listener, not seven". Read literally, that moves the `WeatherV1` and
> `WeatherAppsV1` services from their dedicated port onto the website's port, which breaks every
> machine-to-machine peer at upgrade and makes the rewrite's first operational event a flag day. The
> property that actually matters — one place that owns listening, one route table, one TLS story, one
> shutdown path — is unaffected by how many sockets that one place binds.
>
> *What replaces it.* D-31 gains: *the singular unit is the router and the route table. One
> `http:listener` component binds the sockets configuration names — the main port, an optional
> redirecting plaintext port, an optional legacy gRPC port for existing peers — and serves one router
> on all of them.*
>
> *Why this is more than inconvenient.* Seven listener-owning sites, each with its own TLS setup and
> shutdown path, is the defect. One component binding two sockets is not that defect, and conflating
> the two would force an unnecessary compatibility break on W7.

---

## Reproducing the measurements in this document

Every count attributed to the Go implementation was taken from `chrissnell/remoteweather` at the
commit W1 recorded (`c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf`), not transcribed from a prior
document.

| Claim | How to check |
|---|---|
| `/latest` is 12 TimescaleDB round trips | `rg -n 'db.DB.Raw' internal/controllers/rainfall_calc.go` (8 across four functions) plus `restserver/handlers.go` lines 263, 308, 335, 354, 367 |
| `/snow` is 4 on a cache hit, 8 on a miss | `internal/controllers/restserver/handlers.go:406-613` — the `snow_totals_cache` read at :494 and the four function calls in the `else` arm |
| the distributor's channels are 20, 10 and **unbuffered** | `internal/managers/storage.go:37`, `internal/storage/timescaledb/timescaledb.go:35`, `internal/storage/grpcstream/grpc.go:44` |
| `RemoveEngine` closes a channel the distributor may send on | `internal/managers/storage.go:134-147` |
| `getCertificate` iterates a map and `HasSuffix`-matches wildcards | `internal/controllers/restserver/controller.go:804-836` |
| the log buffer re-parses zap's JSON back into a struct | `internal/log/log.go:46` (`LogBuffer::Write`) → `json.Unmarshal` at `:48` |
| the two buffers are 500 and 1,000 entries | `internal/log/log.go:256` and `internal/log/httplog.go:31` |
| templates are parsed per request | `internal/controllers/restserver/handlers.go:801`, `:873`, `:947` |
| `AboutStationHTML` is passed and never consumed | `rg -n 'AboutStationHTML' internal/controllers/restserver/assets/` returns nothing |
| `weather-app.js.tmpl` interpolates exactly nine values | `rg -n '{{' internal/controllers/restserver/assets/js/weather-app.js.tmpl` |
| the two hot-path handlers' extent | `handlers.go:205-403` (`/latest`) and `:406-613` (`/snow`) |
| `grpc_port` is a validated config column defaulting to 50051 | `pkg/config/provider_sqlite.go:231`, and the port-collision checks at `internal/controllers/management/handlers_controllers.go:501` and `:506` |

One claim is about this repository rather than the Go tree, and it is what settles the `ETag`
question in the Open questions section: the conformance runner asserts a **per-fixture allowlist** of
header names and never enumerates the response's headers. See `contract/conformance/src/lib.rs:320`
and the four-entry `assert.headers` array in `contract/fixtures/latest-default-station.json`.

The PostgreSQL bind-parameter ceiling (65,535) that fixes the 256-envelope batch is a protocol
constant, not a measurement; the derived row ceiling of 303 is `65535 / 216` against the registry's
current allocated column count, and §8.2 asserts it from `generated/rust/metrics.rs` so that adding
metrics fails a test rather than a deployment.
